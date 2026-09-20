// @effect-diagnostics nodeBuiltinImport:off - Node test fixtures exercise real files and subprocess lifetimes.
import { strict as assert } from "node:assert";
import { test } from "node:test";
import { mkdtempSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as Effect from "effect/Effect";
import { ProviderInstanceId, ThreadId } from "@t3tools/contracts";
import { PsiBridge } from "./PsiBridge.ts";
import { PsiDriver } from "./PsiDriver.ts";

const binary = process.env.PSI_BINARY;
assert(binary, "PSI_BINARY must name the built Psi executable");

test("bridge uses a real process, validates handshake, and reaps on EOF", async () => {
  const bridge = new PsiBridge(binary);
  const hello = await bridge.hello();
  assert.equal(hello.pid, bridge.child.pid);
  await bridge.close();
  assert.equal(bridge.child.exitCode, 0);
  await bridge.close();
});

test("spawn failure rejects instead of hanging", async () => {
  const bridge = new PsiBridge("/psi/nonexistent-executable");
  await assert.rejects(bridge.hello());
  await bridge.close();
});

test("driver shares runtime between sessions and preserves originating IDs", async () => {
  await Effect.runPromise(Effect.scoped(Effect.gen(function*() {
    const instance = yield* PsiDriver.create({ instanceId: ProviderInstanceId.make("psi-test"), enabled: true, displayName: undefined, environment: [], config: { binary } });
    const a = ThreadId.make("session-a");
    const b = ThreadId.make("session-b");
    for (const threadId of [a, b]) yield* instance.adapter.startSession({ threadId, cwd: "/tmp", runtimeMode: "full-access" });
    const input = { threadId: a, requestId: "saved-command-44", input: "hello" };
    const first = yield* instance.adapter.sendTurn(input);
    const duplicate = yield* instance.adapter.sendTurn(input);
    assert.equal(first.turnId, "psi-turn:saved-command-44");
    assert.equal(duplicate.turnId, first.turnId);
    assert.equal((yield* instance.adapter.readThread(a)).turns.length, 1);
    const conflict = yield* Effect.exit(instance.adapter.sendTurn({ ...input, input: "changed" }));
    assert.equal(conflict._tag, "Failure");
    yield* instance.adapter.stopSession(a);
    assert.equal(yield* instance.adapter.hasSession(b), true);
    yield* instance.adapter.sendTurn({ threadId: b, requestId: "saved-command-45", input: "hello again" });
    const turns = (yield* instance.adapter.readThread(b)).turns;
    assert.deepEqual(turns[0]?.items, [{ type: "user", text: "hello again" }, { type: "assistant", text: "Psi fixture: hello again" }]);
    yield* instance.adapter.stopAll();
    yield* instance.adapter.stopAll();
  })));
});

for (const response of [
  { version: 2, runtime_id: "bad-version", capabilities: ["fixture-model", "sessions", "origin-command-id"], model: "fixture" },
  { version: 1, runtime_id: "bad-capabilities", capabilities: [], model: "fixture" },
]) {
  test(`bridge rejects incompatible peer ${response.runtime_id} and reaps it`, async () => {
    const dir = mkdtempSync(join(tmpdir(), "psi-peer-"));
    try {
      const executable = join(dir, "peer");
      writeFileSync(executable, `#!${process.execPath}\nprocess.stdin.once("data", () => process.stdout.write(${JSON.stringify(JSON.stringify(response) + "\n")}));\n`, { mode: 0o700 });
      const bridge = new PsiBridge(executable);
      await assert.rejects(bridge.hello());
      await bridge.exited;
      assert(bridge.child.exitCode !== null || bridge.child.signalCode !== null);
    } finally { rmSync(dir, { recursive: true, force: true }); }
  });
}

for (const agentLoop of ["single-response", "buffered-response"]) {
  test(`startup selection ${agentLoop} satisfies the same T3 contract`, async () => {
    await Effect.runPromise(Effect.scoped(Effect.gen(function*() {
      const instance = yield* PsiDriver.create({ instanceId: ProviderInstanceId.make(`psi-${agentLoop}`), enabled: true, displayName: undefined, environment: [], config: { binary, agentLoop } });
      const threadId = ThreadId.make("contract-session");
      yield* instance.adapter.startSession({ threadId, cwd: "/tmp", runtimeMode: "full-access" });
      yield* instance.adapter.sendTurn({ threadId, requestId: "contract-command", input: "contract" });
      assert.deepEqual((yield* instance.adapter.readThread(threadId)).turns[0]?.items, [{ type: "user", text: "contract" }, { type: "assistant", text: "Psi fixture: contract" }]);
    })));
  });
}

for (const startupFault of ["missing", "cycle", "duplicate", "partial"]) {
  test(`startup ${startupFault} fails readiness and reaps tracked resources`, async () => {
    const bridge = new PsiBridge(binary, { startupFault });
    let diagnostics = "";
    bridge.child.stderr.on("data", (data: Buffer) => { diagnostics += data.toString(); });
    await assert.rejects(bridge.hello());
    await bridge.exited;
    assert.equal(bridge.child.exitCode, 1);
    assert(!diagnostics.includes("runtime started"));
    if (startupFault === "partial") {
      assert(diagnostics.includes("probe resource dropped"));
      const closed = [...diagnostics.matchAll(/Psi plugin closed: (\S+)/g)].map((match) => match[1]);
      assert.deepEqual(closed, ["startup-probe", "single-response-loop", "fixture-model"]);
      assert(diagnostics.includes("registrations=0 tasks=0"));
    } else assert(!diagnostics.includes("plugin starting"));
  });
}

test("Rust rejects overlap across bridges without a queue; close cleans only its session", async () => {
  const dir = mkdtempSync(join(tmpdir(), "psi-ownership-"));
  const gate = join(dir, "release");
  const a = new PsiBridge(binary, { fixtureGate: gate });
  const b = new PsiBridge(binary);
  try {
    await a.hello(); await b.hello();
    for (const session_id of ["a", "b"]) await a.send({ op: "open", request_id: `open-${session_id}`, session_id });
    await b.send({ op: "open", request_id: "open-independent", session_id: "independent" });
    const admitted = new Promise<void>((resolve) => a.child.stderr.on("data", (data: Buffer) => { if (data.toString().includes("turn admitted")) resolve(); }));
    const pending = a.send({ op: "turn", request_id: "first", session_id: "a", turn_id: "first", input: "held" });
    // Attach a handler before close rejects the original turn.
    const cancelled = pending.then((reply) => { assert.equal(reply.status, "cancelled"); assert.equal(reply.error, "session_closed"); });
    await admitted;
    await assert.rejects(a.send({ op: "turn", request_id: "conflict", session_id: "b", turn_id: "conflict", input: "must not queue" }), /busy/);
    await assert.rejects(b.send({ op: "turn", request_id: "independent", session_id: "independent", turn_id: "independent", input: "must not queue" }), /busy/);
    await a.send({ op: "close", request_id: "close-other", session_id: "b" });
    await assert.rejects(b.send({ op: "turn", request_id: "still-busy", session_id: "independent", turn_id: "still-busy", input: "still held" }), /busy/);
    await a.send({ op: "close", request_id: "close-active", session_id: "a" });
    await cancelled;
    await a.send({ op: "close", request_id: "close-again", session_id: "a" });
    // A rejected request was never accepted or queued; an explicit retry can run.
    const reply = await b.send({ op: "turn", request_id: "independent", session_id: "independent", turn_id: "independent", input: "must not queue" });
    assert.equal(reply.output, "Psi fixture: must not queue");
    assert.equal(a.child.exitCode, null);
  } finally { await a.close(); await b.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("independent T3 provider instances cannot bypass the Rust lease", async () => {
  const dir = mkdtempSync(join(tmpdir(), "psi-instances-"));
  try {
    await Effect.runPromise(Effect.scoped(Effect.gen(function*() {
      const gate = join(dir, "release");
      const make = (id: string) => PsiDriver.create({ instanceId: ProviderInstanceId.make(id), enabled: true, displayName: undefined, environment: [], config: { binary, fixtureGate: gate } });
      const a = yield* make("independent-a"); const b = yield* make("independent-b");
      const first = ThreadId.make("first"); const second = ThreadId.make("second");
      yield* a.adapter.startSession({ threadId: first, cwd: "/tmp", runtimeMode: "full-access" });
      yield* b.adapter.startSession({ threadId: second, cwd: "/tmp", runtimeMode: "full-access" });
      const pending = Effect.runPromise(Effect.exit(a.adapter.sendTurn({ threadId: first, requestId: "held", input: "held" })));
      // Open/close commands on the same controller roundtrip after admission.
      const barrier = ThreadId.make("barrier");
      yield* a.adapter.startSession({ threadId: barrier, cwd: "/tmp", runtimeMode: "full-access" });
      const busy = yield* Effect.exit(b.adapter.sendTurn({ threadId: second, requestId: "busy", input: "busy" }));
      assert.equal(busy._tag, "Failure");
      assert.match(String(busy), /busy/);
      yield* a.adapter.stopSession(first);
      yield* a.adapter.stopSession(first);
      assert.equal((yield* Effect.promise(() => pending))._tag, "Success");
      writeFileSync(gate, "release");
      yield* b.adapter.sendTurn({ threadId: second, requestId: "after", input: "after" });
      assert.equal((yield* b.adapter.readThread(second)).turns.length, 1);
    })));
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("controller EOF joins a held turn before releasing admission", async () => {
  const dir = mkdtempSync(join(tmpdir(), "psi-eof-"));
  const a = new PsiBridge(binary, { fixtureGate: join(dir, "never") });
  let diagnostics = "";
  a.child.stderr.on("data", (data: Buffer) => { diagnostics += data.toString(); });
  try {
    await a.hello();
    await a.send({ op: "open", request_id: "open", session_id: "a" });
    const turn = assert.rejects(a.send({ op: "turn", request_id: "held", session_id: "a", turn_id: "held", input: "held" }), /process closed/);
    await a.send({ op: "open", request_id: "barrier", session_id: "barrier" });
    await a.close();
    await turn;
    assert.equal(a.child.exitCode, 0);
    assert(diagnostics.includes("turn cleaned: tasks=0"));
    assert(diagnostics.includes("registrations=0 tasks=0"));
    const b = new PsiBridge(binary);
    try {
      await b.hello();
      await b.send({ op: "open", request_id: "open", session_id: "b" });
      assert.equal((await b.send({ op: "turn", request_id: "after", session_id: "b", turn_id: "after", input: "after EOF" })).output, "Psi fixture: after EOF");
    } finally { await b.close(); }
  } finally { await a.close(); rmSync(dir, { recursive: true, force: true }); }
});

test("closing during session startup leaves no orphaned session; reopening is explicit", async () => {
  await Effect.runPromise(Effect.scoped(Effect.gen(function*() {
    const instance = yield* PsiDriver.create({ instanceId: ProviderInstanceId.make("start-close"), enabled: true, displayName: undefined, environment: [], config: { binary } });
    const threadId = ThreadId.make("starting");
    const start = Effect.runPromise(instance.adapter.startSession({ threadId, cwd: "/tmp", runtimeMode: "full-access" }));
    yield* instance.adapter.stopSession(threadId);
    yield* Effect.promise(() => start);
    assert.equal(yield* instance.adapter.hasSession(threadId), false);
    yield* instance.adapter.startSession({ threadId, cwd: "/tmp", runtimeMode: "full-access" });
    yield* instance.adapter.sendTurn({ threadId, requestId: "reopened", input: "reopened" });
    assert.equal((yield* instance.adapter.readThread(threadId)).turns.length, 1);
  })));
});
