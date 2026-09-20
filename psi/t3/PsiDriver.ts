// @effect-diagnostics nodeBuiltinImport:off - Durable adapter paths use Node filesystem semantics.
import { existsSync } from "node:fs";
import { join } from "node:path";
import * as Option from "effect/Option";
import { ServerConfig } from "../config.ts";
import { PsiInbox, instanceDirectory, outsideWorkspace } from "./PsiInbox.ts";
import * as DateTime from "effect/DateTime";
import * as Effect from "effect/Effect";
import * as PubSub from "effect/PubSub";
import * as Stream from "effect/Stream";
import * as Schema from "effect/Schema";
import {
  ProviderDriverKind, ProviderRuntimeEvent, ServerProvider, TurnId, TextGenerationError,
  type ThreadId, type ProviderSession,
} from "@t3tools/contracts";
import { createModelCapabilities } from "@t3tools/shared/model";
import { ProviderAdapterValidationError, ProviderDriverError, type ProviderAdapterError } from "../provider/Errors.ts";
import type { ProviderDriver } from "../provider/ProviderDriver.ts";
import type { ProviderAdapterShape } from "../provider/Services/ProviderAdapter.ts";
import { PsiBridge, lifecycleId } from "./PsiBridge.ts";

const kind = ProviderDriverKind.make("psi");
const Config = Schema.Struct({ stateDir: Schema.optional(Schema.String), binary: Schema.String, model: Schema.optional(Schema.String), oauthFixture: Schema.optional(Schema.String), agentLoop: Schema.optional(Schema.String), fixtureGate: Schema.optional(Schema.String), startupFault: Schema.optional(Schema.String) });

export const PsiDriver: ProviderDriver<typeof Config.Type> = {
  driverKind: kind,
  metadata: { displayName: "Psi", supportsMultipleInstances: false },
  configSchema: Config,
  defaultConfig: () => ({ binary: "psi" }),
  create: ({ instanceId, displayName, enabled, config }) => Effect.gen(function*() {
    const server = yield* Effect.serviceOption(ServerConfig);
    const stateRoot = Option.isSome(server) ? join(server.value.baseDir, "psi") : config.stateDir;
    if (!stateRoot) return yield* Effect.fail(new ProviderDriverError({driver:kind,instanceId,detail:"stateDir required outside T3"}));
    const inbox = new PsiInbox(stateRoot);
    yield* Effect.addFinalizer(() => Effect.sync(() => inbox.close()));
    const stateDir = instanceDirectory(stateRoot,instanceId);
    const events = yield* PubSub.unbounded<ProviderRuntimeEvent>();
    const sessions = new Map<ThreadId, ProviderSession>();
    const sessionStarts = new Map<ThreadId, Promise<ProviderSession>>();
    let stopped = false;
    const model = config.model ?? "fixture";
    let bridge: PsiBridge | undefined;
    let starting: Promise<PsiBridge> | undefined;

    const fail = (operation: string, issue: string) => new ProviderAdapterValidationError({ provider: kind, operation, issue });
    const wrap = <A>(operation: string, run: () => Promise<A>) => Effect.tryPromise({ try: run, catch: (e) => fail(operation, String(e)) });
    const runtime = async () => {
      if (bridge) return bridge;
      starting ??= (async () => {
        const next = new PsiBridge(config.binary, { ...config, stateDir, onFrame: frame => inbox.accept(instanceId,frame) });
        try {
          const hello = await next.hello();
          for (const session of hello.sessions ?? []) await next.send({op:"replay",request_id:lifecycleId(),session_id:session,after:inbox.cursor(instanceId,session)});
          bridge = next;
          return next;
        } catch (error) {
          await next.close();
          throw error;
        }
      })();
      try { return await starting; } finally { starting = undefined; }
    };
    const stopAll = async () => {
      stopped = true;
      await Promise.allSettled(sessionStarts.values());
      const current = bridge ?? await starting?.catch(() => undefined);
      if (current) await current.close();
      bridge = undefined;
      sessions.clear();
    };
    yield* Effect.addFinalizer(() => Effect.promise(stopAll));
    // Reconnect and drain persisted events even without a new user message.
    if (enabled && existsSync(join(stateDir,"history.sqlite"))) yield* Effect.tryPromise({try:runtime,catch:e=>new ProviderDriverError({driver:kind,instanceId,detail:String(e)})});
    const unsupported = (operation: string) => Effect.fail(fail(operation, "unsupported by Psi fixture"));
    const adapter: ProviderAdapterShape<ProviderAdapterError> = {
      provider: kind,
      capabilities: { sessionModelSwitch: "unsupported", supportsConversationRollback: false },
      startSession: (input) => wrap("startSession", async () => {
        if (stopped) throw new Error("runtime_closed");
        if (sessions.has(input.threadId)) return sessions.get(input.threadId)!;
        const pending = sessionStarts.get(input.threadId);
        if (pending) return pending;
        const start = (async () => {
          if (!input.cwd) throw new Error("workspace_required");
          outsideWorkspace(stateRoot,input.cwd);
          const current = await runtime();
          await current.send({ op: "open", request_id: lifecycleId(), session_id: input.threadId, workspace: input.cwd });
          await current.send({op:"replay",request_id:lifecycleId(),session_id:input.threadId,after:inbox.cursor(instanceId,input.threadId)});
          const now = DateTime.formatIso(DateTime.nowUnsafe());
          const session: ProviderSession = { provider: kind, providerInstanceId: instanceId, threadId: input.threadId, cwd: input.cwd, runtimeMode: input.runtimeMode, status: "ready", createdAt: now, updatedAt: now };
          sessions.set(input.threadId, session);
          return session;
        })();
        sessionStarts.set(input.threadId, start);
        try { return await start; } finally { sessionStarts.delete(input.threadId); }
      }),
      sendTurn: (input) => wrap("sendTurn", async () => {
        if (!input.requestId) throw new Error("originating_command_id_required");
        if (!sessions.has(input.threadId) || !bridge) throw new Error("session_not_started");
        if (input.attachments?.length) throw new Error("attachments_unsupported");
        if (input.modelSelection && input.modelSelection.model !== model) throw new Error("model_unsupported");
        const current = bridge;
        const turnId = TurnId.make(`psi-turn:${input.requestId}`);
        let itemId = `${turnId}:assistant`;
        let ordinal = 0;
        let started = false;
        let streamed = "";
        let delivery = Promise.resolve();
        const emit = async (type: string, payload: unknown) => {
          const event = Schema.decodeUnknownSync(ProviderRuntimeEvent)({ eventId: `psi:${instanceId}:${input.threadId}:${input.requestId}:${ordinal++}`, provider: kind, providerInstanceId: instanceId, threadId: input.threadId, turnId, itemId, createdAt: DateTime.formatIso(DateTime.nowUnsafe()), type, payload });
          await Effect.runPromise(PubSub.publish(events, event));
        };
        const start = async () => {
          if (started) return;
          started = true;
          await emit("turn.started", { model });
          await emit("item.started", { itemType: "assistant_message", status: "inProgress" });
        };
        const reply = await current.send({ op: "turn", request_id: input.requestId, session_id: input.threadId, turn_id: turnId, input: input.input ?? "" }, (frame) => {
          delivery = delivery.then(async () => {
            if (frame.turn_id !== turnId || frame.session_id !== input.threadId) throw new Error("invalid_stream_identity");
            if (frame.event?.kind === "sign_in") itemId = `${turnId}:sign-in`;
            await start();
            if (frame.event?.kind === "sign_in") {
              await emit("content.delta", { streamKind: "assistant_text", delta: `Sign in to Psi: ${frame.event.url}\n\n` });
              // Flush this actionable link even when T3 buffers whole answers.
              await emit("item.completed", { itemType: "assistant_message", status: "completed" });
              itemId = `${turnId}:assistant`;
              await emit("item.started", { itemType: "assistant_message", status: "inProgress" });
            } else if (frame.event?.kind === "text") {
              const delta = frame.event.delta ?? "";
              streamed += delta;
              await emit("content.delta", { streamKind: "assistant_text", delta });
            } else if (frame.event?.kind === "reasoning") {
              await emit("content.delta", { streamKind: "reasoning_text", delta: frame.event.delta ?? "" });
            }
          });
          // Observe immediately; await below still propagates failures.
          void delivery.catch(() => undefined);
        });
        await delivery;
        if (reply.turn_id !== turnId || reply.session_id !== input.threadId) throw new Error("invalid_turn_reply");
        if (!reply.duplicate) {
          await start();
          if (reply.status !== "completed") {
            const cancelled = reply.status === "cancelled";
            await emit("content.delta", { streamKind: "assistant_text", delta: cancelled ? "\nCancelled locally. Server processing and billing may continue." : `\nPsi failed: ${reply.error ?? "model_failed"}` });
            await emit("item.completed", { itemType: "assistant_message", status: "failed" });
            await emit("turn.completed", { state: cancelled ? "interrupted" : "failed" });
            return { threadId: input.threadId, turnId };
          }
          if (typeof reply.output !== "string") throw new Error("invalid_turn_reply");
          if (!streamed) await emit("content.delta", { streamKind: "assistant_text", delta: reply.output });
          await emit("item.completed", { itemType: "assistant_message", status: "completed" });
          await emit("turn.completed", { state: "completed" });
        }
        return { threadId: input.threadId, turnId };
      }),
      interruptTurn: (input, turnId) => wrap("interruptTurn", async () => {
        if (bridge) await bridge.send({ op: "cancel", request_id: lifecycleId(), session_id: input, ...(turnId ? { turn_id: turnId } : {}) });
      }),
      stopSession: (id) => wrap("stopSession", async () => {
        await sessionStarts.get(id)?.catch(() => undefined);
        const existed = sessions.delete(id);
        if (bridge && existed) await bridge.send({ op: "close", request_id: lifecycleId(), session_id: id });
      }),
      stopAll: () => wrap("stopAll", stopAll),
      listSessions: () => Effect.sync(() => [...sessions.values()]),
      hasSession: (id) => Effect.sync(() => sessions.has(id)),
      readThread: (id) => wrap("readThread", async () => {
        if (!sessions.has(id)) throw new Error("session_not_started");
        const reply = await bridge!.send({op:"history",request_id:lifecycleId(),session_id:id});
        return {threadId:id,turns:(reply.turns ?? []).filter(t=>t.status==="completed").map(t=>({id:TurnId.make(t.turn_id),items:[{type:"user" as const,text:t.input ?? ""},{type:"assistant" as const,text:t.output ?? ""}]}))};
      }),
      rollbackThread: () => unsupported("rollbackThread"),
      respondToRequest: () => unsupported("respondToRequest"),
      respondToUserInput: () => unsupported("respondToUserInput"),
      streamEvents: Stream.fromPubSub(events),
    };
    const snapshot = yield* Schema.decodeUnknownEffect(ServerProvider)({ instanceId, driver: kind, displayName: displayName ?? "Psi", enabled, installed: true, version: "0.1.0", status: "ready", auth: { status: model === "fixture" ? "authenticated" : "unknown" }, checkedAt: DateTime.formatIso(DateTime.nowUnsafe()), supportsConversationRollback: false, supportsTextGeneration: false, models: [{ slug: model, name: model === "fixture" ? "Psi fixture" : `Psi ${model}`, isCustom: false, capabilities: createModelCapabilities({ optionDescriptors: [] }) }] }).pipe(Effect.mapError(() => new ProviderDriverError({ driver: kind, instanceId, detail: "invalid Psi provider configuration" })));
    const text = () => Effect.fail(new TextGenerationError({ operation: "psi", detail: "Psi fixture has no auxiliary text generation" }));
    return {
      instanceId, driverKind: kind, displayName, enabled,
      continuationIdentity: { driverKind: kind, continuationKey: `${kind}:${instanceId}` }, adapter,
      snapshot: { getSnapshot: Effect.succeed(snapshot), refresh: Effect.succeed(snapshot), streamChanges: Stream.empty, applyUsageLimits: () => Effect.void, resolveMaintenance: () => Effect.succeed({ provider: kind, packageName: null, update: null }) },
      textGeneration: { generateCommitMessage: text, generatePrContent: text, generateBranchName: text, generateThreadTitle: text },
    };
  }),
};
