// @effect-diagnostics nodeBuiltinImport:off - Tests kill subprocesses at durable boundaries.
import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { DatabaseSync } from "node:sqlite";
import { PsiInbox } from "./PsiInbox.ts";

test("inbox detects gaps and content conflicts, scopes sequences, and keeps cursor behind commit", () => {
  const root = mkdtempSync(join(tmpdir(), "psi-inbox-"));
  const inbox = new PsiInbox(root);
  const frame = { session_id: "s", turn_id: "t", request_id: "r", status: "accepted", seq: 1 };
  try {
    assert.throws(() => inbox.accept("a", { ...frame, seq: 2 }), /event_gap/);
    assert.equal(inbox.cursor("a", "s"), 0);
    inbox.accept("a", frame);
    inbox.accept("a", { ...frame, runtime_id: "new", duplicate: true });
    assert.throws(() => inbox.accept("a", { ...frame, status: "completed" }), /event_conflict/);
    inbox.accept("b", frame);
    inbox.accept("a", { ...frame, session_id: "other" });
    assert.equal(inbox.pending().length, 3);
    assert.equal(inbox.cursor("a", "s"), 1);
  } finally {
    inbox.close();
    rmSync(root, { recursive: true, force: true });
  }
});

test("real Rust and T3 restarts recover inbox and projection dispatch without duplicate messages", () => {
  const root = mkdtempSync(join(tmpdir(), "psi-delivery-"));
  const binary = process.env.PSI_BINARY;
  assert(binary);
  const run = (mode: string, crash?: string) =>
    spawnSync(
      process.execPath,
      [new URL("./durability-worker.ts", import.meta.url).pathname, root, mode, binary],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          ...(crash ? { PSI_TEST_CRASH: crash, PSI_TEST_CRASH_SEQ: "2" } : {}),
        },
        timeout: 30_000,
      },
    );
  const ok = (mode: string) => {
    const r = run(mode);
    assert.equal(r.status, 0, r.stdout + r.stderr);
  };
  try {
    ok("seed");
    assert.equal(run("accept", "inbox_commit").signal, "SIGKILL");
    let inbox = new PsiInbox(join(root, "inbox"));
    assert.equal(inbox.cursor("psi", "thread"), 1);
    assert.equal(inbox.pending().length, 2);
    inbox.close();
    ok("accept");
    assert.equal(run("project", "projection_dispatch").signal, "SIGKILL");
    ok("project");
    const db = new DatabaseSync(join(root, "t3.sqlite"));
    const messages = db
      .prepare("SELECT message_id,text FROM projection_thread_messages WHERE role='assistant'")
      .all();
    assert.equal(messages.length, 1);
    assert.equal(messages[0]!.text, "Psi fixture: durable");
    const count = db.prepare("SELECT COUNT(*) AS n FROM orchestration_events").get()!.n;
    ok("accept");
    ok("project");
    assert.equal(db.prepare("SELECT COUNT(*) AS n FROM orchestration_events").get()!.n, count);
    db.close();
    inbox = new PsiInbox(join(root, "inbox"));
    assert.equal(inbox.pending().length, 0);
    assert.equal(inbox.cursor("psi", "thread"), 3);
    inbox.close();
    const rust = new DatabaseSync(join(root, "rust/history.sqlite"));
    assert.equal(
      rust.prepare("SELECT COUNT(*) AS n FROM commands WHERE turn IS NOT NULL").get()!.n,
      1,
    );
    assert.equal(
      JSON.parse(String(rust.prepare("SELECT history FROM sessions").get()!.history)).length,
      2,
    );
    rust.close();
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
