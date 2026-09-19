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
