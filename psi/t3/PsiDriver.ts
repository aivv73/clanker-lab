import * as Effect from "effect/Effect";
import * as PubSub from "effect/PubSub";
import * as Stream from "effect/Stream";
import * as Schema from "effect/Schema";
import {
  ProviderDriverKind, ProviderRuntimeEvent, ServerProvider, TurnId, TextGenerationError,
  type ThreadId, type ProviderSession,
} from "@t3tools/contracts";
import { createModelCapabilities } from "@t3tools/shared/model";
import { ProviderAdapterValidationError, type ProviderAdapterError } from "../provider/Errors.ts";
import type { ProviderDriver } from "../provider/ProviderDriver.ts";
import type { ProviderAdapterShape, ProviderThreadSnapshot } from "../provider/Services/ProviderAdapter.ts";
import { PsiBridge, lifecycleId } from "./PsiBridge.ts";

const kind = ProviderDriverKind.make("psi");
const Config = Schema.Struct({ binary: Schema.String });

export const PsiDriver: ProviderDriver<typeof Config.Type> = {
  driverKind: kind,
  metadata: { displayName: "Psi", supportsMultipleInstances: false },
  configSchema: Config,
  defaultConfig: () => ({ binary: "psi" }),
  create: ({ instanceId, displayName, enabled, config }) => Effect.gen(function*() {
    const events = yield* PubSub.unbounded<ProviderRuntimeEvent>();
    const sessions = new Map<ThreadId, ProviderSession>();
    const history = new Map<ThreadId, ProviderThreadSnapshot["turns"]>();
    let bridge: PsiBridge | undefined;
    let starting: Promise<PsiBridge> | undefined;
    let active = false;
    const fail = (operation: string, issue: string) => new ProviderAdapterValidationError({ provider: kind, operation, issue });
    const wrap = <A>(operation: string, run: () => Promise<A>) => Effect.tryPromise({ try: run, catch: (e) => fail(operation, String(e)) });
    const runtime = async () => {
      if (bridge) return bridge;
      starting ??= (async () => {
        const next = new PsiBridge(config.binary);
        await next.hello();
        bridge = next;
        return next;
      })();
      try { return await starting; } finally { starting = undefined; }
    };
    const stopAll = async () => {
      const current = bridge ?? await starting;
      if (current) await current.close();
      bridge = undefined;
      sessions.clear();
      history.clear();
    };
    yield* Effect.addFinalizer(() => Effect.promise(stopAll));
    const unsupported = (operation: string) => Effect.fail(fail(operation, "unsupported by Psi fixture"));
    const adapter: ProviderAdapterShape<ProviderAdapterError> = {
      provider: kind,
      capabilities: { sessionModelSwitch: "unsupported", supportsConversationRollback: false },
      startSession: (input) => wrap("startSession", async () => {
        if (sessions.has(input.threadId)) return sessions.get(input.threadId)!;
        if (!input.cwd) throw new Error("workspace_required");
        const current = await runtime();
        await current.send({ op: "open", request_id: lifecycleId(), session_id: input.threadId });
        const now = new Date().toISOString();
        const session: ProviderSession = { provider: kind, providerInstanceId: instanceId, threadId: input.threadId, cwd: input.cwd, runtimeMode: input.runtimeMode, status: "ready", createdAt: now, updatedAt: now };
        sessions.set(input.threadId, session);
        history.set(input.threadId, []);
        return session;
      }),
      sendTurn: (input) => wrap("sendTurn", async () => {
        if (!input.requestId) throw new Error("originating_command_id_required");
        if (!sessions.has(input.threadId) || !bridge) throw new Error("session_not_started");
        if (active) throw new Error("busy");
        if (input.attachments?.length) throw new Error("attachments_unsupported");
        if (input.modelSelection && input.modelSelection.model !== "fixture") throw new Error("model_unsupported");
        const current = bridge;
        const turnId = TurnId.make(`psi-turn:${input.requestId}`);
        active = true;
        try {
          const reply = await current.send({ op: "turn", request_id: input.requestId, session_id: input.threadId, turn_id: turnId, input: input.input ?? "" });
          if (reply.turn_id !== turnId || reply.session_id !== input.threadId || reply.status !== "completed" || typeof reply.output !== "string") throw new Error("invalid_turn_reply");
          if (!reply.duplicate) {
            const itemId = `${turnId}:assistant`;
            const emit = async (ordinal: number, type: string, payload: unknown) => {
              const event = Schema.decodeUnknownSync(ProviderRuntimeEvent)({ eventId: `psi:${instanceId}:${input.threadId}:${input.requestId}:${ordinal}`, provider: kind, providerInstanceId: instanceId, threadId: input.threadId, turnId, itemId, createdAt: new Date().toISOString(), type, payload });
              await Effect.runPromise(PubSub.publish(events, event));
            };
            await emit(0, "turn.started", { model: "fixture" });
            await emit(1, "item.started", { itemType: "assistant_message", status: "inProgress" });
            await emit(2, "content.delta", { streamKind: "assistant_text", delta: reply.output });
            await emit(3, "item.completed", { itemType: "assistant_message", status: "completed" });
            await emit(4, "turn.completed", { state: "completed" });
            history.set(input.threadId, [...(history.get(input.threadId) ?? []), { id: turnId, items: [{ type: "user", text: input.input ?? "" }, { type: "assistant", text: reply.output }] }]);
          }
          return { threadId: input.threadId, turnId };
        } finally { active = false; }
      }),
      interruptTurn: () => unsupported("interruptTurn"),
      stopSession: (id) => wrap("stopSession", async () => {
        if (bridge && sessions.has(id)) await bridge.send({ op: "close", request_id: lifecycleId(), session_id: id });
        sessions.delete(id);
        history.delete(id);
      }),
      stopAll: () => wrap("stopAll", stopAll),
      listSessions: () => Effect.sync(() => [...sessions.values()]),
      hasSession: (id) => Effect.sync(() => sessions.has(id)),
      readThread: (id) => wrap("readThread", async () => {
        if (!sessions.has(id)) throw new Error("session_not_started");
        return { threadId: id, turns: history.get(id) ?? [] };
      }),
      rollbackThread: () => unsupported("rollbackThread"),
      respondToRequest: () => unsupported("respondToRequest"),
      respondToUserInput: () => unsupported("respondToUserInput"),
      streamEvents: Stream.fromPubSub(events),
    };
    const snapshot = Schema.decodeUnknownSync(ServerProvider)({ instanceId, driver: kind, displayName: displayName ?? "Psi", enabled, installed: true, version: "0.1.0", status: "ready", auth: { status: "authenticated" }, checkedAt: new Date().toISOString(), supportsConversationRollback: false, supportsTextGeneration: false, models: [{ slug: "fixture", name: "Psi fixture", isCustom: false, capabilities: createModelCapabilities({ optionDescriptors: [] }) }] });
    const text = () => Effect.fail(new TextGenerationError({ operation: "psi", detail: "Psi fixture has no auxiliary text generation" }));
    return {
      instanceId, driverKind: kind, displayName, enabled,
      continuationIdentity: { driverKind: kind, continuationKey: `${kind}:${instanceId}` }, adapter,
      snapshot: { getSnapshot: Effect.succeed(snapshot), refresh: Effect.succeed(snapshot), streamChanges: Stream.empty, applyUsageLimits: () => Effect.void, resolveMaintenance: () => Effect.succeed({ provider: kind, packageName: null, update: null }) },
      textGeneration: { generateCommitMessage: text, generatePrContent: text, generateBranchName: text, generateThreadTitle: text },
    };
  }),
};
