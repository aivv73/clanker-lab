// @effect-diagnostics nodeBuiltinImport:off globalDate:off - SQLite is the durable bridge boundary.
import { DatabaseSync } from "node:sqlite";
import { mkdirSync, realpathSync } from "node:fs";
import { join, relative, isAbsolute } from "node:path";
import { createHash } from "node:crypto";

export type Frame = {
  session_id: string;
  seq: number;
  request_id: string;
  turn_id: string;
  status: string;
  input?: string;
  output?: string;
  error?: string;
  event?: { kind: string; delta?: string; url?: string };
  [key: string]: unknown;
};
export const inboxRoot = (baseDir: string) => join(baseDir, "psi");
export const instanceDirectory = (root: string, instance: string) =>
  join(root, createHash("sha256").update(instance).digest("hex"));
export function outsideWorkspace(directory: string, workspace: string) {
  const rel = relative(realpathSync(workspace), realpathSync(directory));
  if (!rel || (!rel.startsWith(".." + "/") && rel !== ".." && !isAbsolute(rel)))
    throw new Error("state_directory_inside_workspace");
}
function canonical(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object")
    return `{${Object.entries(value)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([k, v]) => `${JSON.stringify(k)}:${canonical(v)}`)
      .join(",")}}`;
  return JSON.stringify(value);
}
export class PsiInbox {
  readonly db: DatabaseSync;
  readonly root: string;
  constructor(root: string) {
    this.root = root;
    mkdirSync(root, { recursive: true, mode: 0o700 });
    this.db = new DatabaseSync(join(root, "inbox.sqlite"));
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
      CREATE TABLE IF NOT EXISTS inbox(instance TEXT, session TEXT, seq INTEGER, frame TEXT NOT NULL, created TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(instance,session,seq));
      CREATE TABLE IF NOT EXISTS cursors(instance TEXT, session TEXT, seq INTEGER NOT NULL, PRIMARY KEY(instance,session));`);
  }
  cursor(instance: string, session: string): number {
    return Number(
      this.db
        .prepare("SELECT seq FROM cursors WHERE instance=? AND session=?")
        .get(instance, session)?.seq ?? 0,
    );
  }
  accept(instance: string, raw: Frame) {
    if (
      !Number.isSafeInteger(raw.seq) ||
      raw.seq < 1 ||
      !raw.session_id ||
      !raw.turn_id ||
      !raw.request_id
    )
      throw new Error("invalid_event_identity");
    const { runtime_id: _runtime, duplicate: _duplicate, replay: _replay, ...frame } = raw;
    const text = canonical(frame);
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const old = this.db
        .prepare("SELECT frame FROM inbox WHERE instance=? AND session=? AND seq=?")
        .get(instance, frame.session_id, frame.seq);
      if (old && old.frame !== text) throw new Error("event_conflict");
      const last = Number(
        this.db
          .prepare("SELECT COALESCE(MAX(seq),0) AS seq FROM inbox WHERE instance=? AND session=?")
          .get(instance, frame.session_id)!.seq,
      );
      if (!old && frame.seq !== last + 1) throw new Error("event_gap");
      if (!old)
        this.db
          .prepare("INSERT INTO inbox(instance,session,seq,frame,created) VALUES (?,?,?,?,?)")
          .run(instance, frame.session_id, frame.seq, text, new Date().toISOString());
      this.db.exec("COMMIT");
    } catch (e) {
      this.db.exec("ROLLBACK");
      throw e;
    }
    // Deliberately separate commits: replay after this boundary finds the same inbox row.
    if (!process.env.PSI_TEST_CRASH_SEQ || Number(process.env.PSI_TEST_CRASH_SEQ) === frame.seq)
      crash("inbox_commit");
    this.db
      .prepare(
        "INSERT INTO cursors VALUES (?,?,?) ON CONFLICT(instance,session) DO UPDATE SET seq=MAX(seq,excluded.seq)",
      )
      .run(instance, frame.session_id, frame.seq);
  }
  pending() {
    return this.db
      .prepare("SELECT instance,session,seq,frame,created FROM inbox WHERE done=0 ORDER BY rowid")
      .all()
      .map((row) => ({
        instance: String(row.instance),
        session: String(row.session),
        seq: Number(row.seq),
        frame: JSON.parse(String(row.frame)) as Frame,
        created: String(row.created),
      }));
  }
  done(instance: string, session: string, seq: number) {
    if (!process.env.PSI_TEST_CRASH_SEQ || Number(process.env.PSI_TEST_CRASH_SEQ) === seq)
      crash("projection_dispatch");
    this.db
      .prepare("UPDATE inbox SET done=1 WHERE instance=? AND session=? AND seq=?")
      .run(instance, session, seq);
  }
  close() {
    this.db.close();
  }
}
export function crash(boundary: string) {
  if (process.env.PSI_TEST_CRASH === boundary) process.kill(process.pid, "SIGKILL");
}
