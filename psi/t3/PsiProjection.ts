// @effect-diagnostics preferSchemaOverJson:off - Stable IDs encode a tuple, not an external payload.
import * as Effect from "effect/Effect";
import * as Schema from "effect/Schema";
import { OrchestrationCommand } from "@t3tools/contracts";
import type { OrchestrationEngineShape } from "../orchestration/Services/OrchestrationEngine.ts";
import { PsiInbox } from "./PsiInbox.ts";

/** Stateless translation: every dispatch is derived solely from durable input. */
export const drainPsiInbox = (
  inbox: PsiInbox,
  engine: Pick<OrchestrationEngineShape, "dispatch">,
) =>
  Effect.gen(function* () {
    for (const row of inbox.pending()) {
      const f = row.frame;
      const commands: unknown[] = [];
      const base = { threadId: row.session, turnId: f.turn_id, createdAt: row.created };
      const assistant = `assistant:${f.turn_id}:assistant`;
      const reasoning = `reasoning:${f.turn_id}:reasoning`;
      const delta = (messageId: string, text: string, reason = false) => {
        if (text)
          commands.push({
            ...base,
            type: reason ? "thread.message.reasoning.delta" : "thread.message.assistant.delta",
            messageId,
            delta: text,
          });
      };
      const complete = (messageId: string, reason = false) =>
        commands.push({
          ...base,
          type: reason ? "thread.message.reasoning.complete" : "thread.message.assistant.complete",
          messageId,
        });
      if (f.status === "accepted" || ["completed", "failed", "cancelled"].includes(f.status)) {
        commands.push({
          type: "thread.session.set",
          threadId: row.session,
          createdAt: row.created,
          session: {
            threadId: row.session,
            providerName: "psi",
            providerInstanceId: row.instance,
            runtimeMode: "full-access",
            status:
              f.status === "accepted"
                ? "running"
                : f.status === "cancelled"
                  ? "interrupted"
                  : f.status === "failed"
                    ? "error"
                    : "ready",
            activeTurnId: f.status === "accepted" ? f.turn_id : null,
            lastError: f.error ?? null,
            updatedAt: row.created,
          },
        });
      }
      if (f.status === "streaming") {
        if (f.event?.kind === "text") delta(assistant, f.event.delta ?? "");
        if (f.event?.kind === "reasoning") delta(reasoning, f.event.delta ?? "", true);
        if (f.event?.kind === "sign_in") {
          const id = `assistant:${f.turn_id}:sign-in:${row.seq}`;
          delta(id, `Sign in to Psi: ${f.event.url}\n\n`);
          complete(id);
        }
      }
      if (["completed", "failed", "cancelled"].includes(f.status)) {
        const prior = inbox.db
          .prepare("SELECT frame FROM inbox WHERE instance=? AND session=? AND seq<? ORDER BY seq")
          .all(row.instance, row.session, row.seq)
          .map((r) => JSON.parse(String(r.frame)))
          .filter((r) => r.turn_id === f.turn_id);
        const streamed = prior
          .filter((r) => r.event?.kind === "text")
          .map((r) => r.event.delta ?? "")
          .join("");
        if (f.status === "completed") {
          if (!(f.output ?? "").startsWith(streamed)) throw new Error("terminal_text_conflict");
          delta(assistant, (f.output ?? "").slice(streamed.length));
        } else
          delta(
            assistant,
            f.status === "cancelled"
              ? "\nCancelled locally. Server processing and billing may continue."
              : `\nPsi failed: ${f.error ?? "model_failed"}`,
          );
        complete(assistant);
        if (prior.some((r) => r.event?.kind === "reasoning" && r.event.delta))
          complete(reasoning, true);
        // Lifecycle completion follows message completion, even after replay.
        commands.push(commands.shift()!);
      }
      for (const [ordinal, command] of commands.entries()) {
        const commandId = `psi:${JSON.stringify([row.instance, row.session, row.seq, ordinal])}`;
        const decoded = yield* Schema.decodeUnknownEffect(OrchestrationCommand)({
          ...(command as object),
          commandId,
        });
        yield* engine.dispatch(decoded);
      }
      inbox.done(row.instance, row.session, row.seq);
    }
  });
