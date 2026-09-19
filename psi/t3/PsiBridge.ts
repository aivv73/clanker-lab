import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import * as Schema from "effect/Schema";

const Reply = Schema.Struct({
  version: Schema.Number,
  runtime_id: Schema.String,
  request_id: Schema.optional(Schema.String),
  error: Schema.optional(Schema.String),
  capabilities: Schema.optional(Schema.Array(Schema.String)),
  pid: Schema.optional(Schema.Number),
  model: Schema.optional(Schema.String),
  session_id: Schema.optional(Schema.String),
  turn_id: Schema.optional(Schema.String),
  status: Schema.optional(Schema.String),
  output: Schema.optional(Schema.String),
  duplicate: Schema.optional(Schema.Boolean),
});
export type PsiReply = typeof Reply.Type;
const capabilities = ["fixture-model", "sessions", "origin-command-id"];

/** One controller process per provider instance, shared by every session. */
export class PsiBridge {
  readonly child: ChildProcessWithoutNullStreams;
  readonly exited: Promise<void>;
  runtimeId?: string;
  private buffer = "";
  private failure?: Error;
  private closing = false;
  private pending: {
    requestId: string | undefined;
    resolve: (reply: PsiReply) => void;
    reject: (error: Error) => void;
    timer: ReturnType<typeof setTimeout>;
  }[] = [];

  constructor(binary: string) {
    this.child = spawn(binary, [], { stdio: "pipe" });
    // Never treat stderr as JSONL. Forward diagnostics without buffering them.
    this.child.stderr.on("data", (data: Buffer) => process.stderr.write(data));
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (data: string) => {
      this.buffer += data;
      if (Buffer.byteLength(this.buffer) > 1024 * 1024) {
        this.fail(new Error("Psi protocol frame exceeds 1 MiB"));
        return;
      }
      for (;;) {
        const end = this.buffer.indexOf("\n");
        if (end < 0) break;
        const line = this.buffer.slice(0, end);
        this.buffer = this.buffer.slice(end + 1);
        try {
          const reply = Schema.decodeUnknownSync(Reply)(JSON.parse(line));
          const next = this.pending[0];
          if (!next || reply.version !== 1 || (this.runtimeId && reply.runtime_id !== this.runtimeId)) {
            throw new Error("Psi incompatible or unsolicited reply");
          }
          if (!reply.error && next.requestId !== reply.request_id) throw new Error("Psi request identity mismatch");
          this.pending.shift();
          clearTimeout(next.timer);
          if (reply.error) next.reject(new Error(`Psi: ${reply.error}`));
          else next.resolve(reply);
        } catch (error) { this.fail(error instanceof Error ? error : new Error(String(error))); }
      }
    });
    this.child.stdin.on("error", (error) => this.fail(error));
    this.child.on("error", (error) => this.fail(error));
    this.exited = new Promise((resolve) => this.child.on("close", (code, signal) => {
      this.fail(new Error(`Psi process closed (${code ?? signal})`), false);
      resolve();
    }));
  }

  private fail(error: Error, terminate = true) {
    this.failure ??= error;
    for (const next of this.pending.splice(0)) {
      clearTimeout(next.timer);
      next.reject(this.failure);
    }
    if (terminate) this.child.kill("SIGKILL");
  }

  async hello() {
    try {
      const reply = await this.send({ op: "hello", capabilities });
      if (!reply.capabilities || capabilities.some((c) => !reply.capabilities!.includes(c)) || reply.model !== "fixture") {
        throw new Error("Psi incompatible capabilities");
      }
      this.runtimeId = reply.runtime_id;
      return reply;
    } catch (error) {
      await this.close();
      throw error;
    }
  }

  send(command: { op: string; request_id?: string; [key: string]: unknown }): Promise<PsiReply> {
    if (this.failure || this.closing) return Promise.reject(this.failure ?? new Error("Psi is closing"));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.fail(new Error("Psi response timeout")), 10_000);
      this.pending.push({ requestId: command.request_id, resolve, reject, timer });
      this.child.stdin.write(JSON.stringify({ ...command, version: 1 }) + "\n");
    });
  }

  async close() {
    if (!this.closing) {
      this.closing = true;
      this.child.stdin.end();
    }
    const timer = setTimeout(() => this.child.kill("SIGKILL"), 2_000);
    try { await this.exited; } finally { clearTimeout(timer); }
  }
}

// Non-turn lifecycle commands have separate identities. Turn IDs come from T3.
export const lifecycleId = () => `psi-lifecycle:${randomUUID()}`;
