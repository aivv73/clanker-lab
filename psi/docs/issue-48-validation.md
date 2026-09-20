# Issue #48: durable conversations and delivery

Scope: A20, completed-conversation A02, and the inbox/projection portion of A21.
This does not accept checkpoint recovery, file effects, or the full MVP.

## Implementation

Psi commits turn identity, original command, and acceptance in one SQLite
transaction before spawning model work. Retries compare parsed command content,
return the original acceptance and saved result, and never call the model again.
Busy requests are not accepted. Exact provider items remain JSON values, preserving
IDs, call/result relationships, encrypted items, and argument strings. A completed
turn commits history, result, terminal-processing intent, and its sequenced event
in one transaction. Streaming provider items are retained even on stream failure.

T3 owns a separate SQLite inbox beneath its base directory. It durably inserts a
frame before advancing its cursor in a separate commit; replay can repair a crash
between those commits. Sequence gaps, conflicting content, and cursors beyond the
Rust log fail explicitly. Identities include the provider instance and session.
Projection commands have deterministic IDs derived from instance/session/sequence/
ordinal. The real T3 engine's command receipts make replay after dispatch safe.
Inbox completion follows all dispatches. Accepted frames need not be rendered yet.

Psi conversation projection deliberately uses the token path without transient
text buffers. It ignores the adapter's hot PubSub copy; a scoped worker drains the
durable inbox instead. Sign-in messages complete independently. Restarting T3
reconnects saved Psi instances and replays their sessions without a new prompt.
Other providers retain their existing ingestion path. Checkpoints and richer tool
projections remain later slices.

SQLite uses WAL and synchronous=FULL. Runtime storage is locked per directory;
one controller cannot recover another controller's live store. Existing global
turn admission remains in force across stores. T3 rejects its state root inside a
workspace, and Rust checks the canonical workspace against its own directory.
Session close retains history. Authentication tokens remain memory-only.

An accepted unfinished turn is recorded as interrupted with unknown outcome on
restart, never executed again, and blocks ordinary continuation in that session.
This is not proof of external cleanup. Durable action intent/result/unknown and
terminal pending/done APIs provide extension points for #49. Intent and external
file changes are not one atomic transaction. Unknown acknowledgement and real
file-changing tools are not implemented here.

## Reproduce

From `psi/`, prepare the pinned T3 checkout as described in the README, then run
turn-producing suites sequentially:

```sh
cargo fmt --check
cargo check --locked
cargo test --locked
python3 t3/prepare.py /path/to/t3
(cd /path/to/t3 && node_modules/.bin/tsc -p tsconfig.psi.json --pretty false)
PSI_BINARY="$PWD/target/debug/psi" node --test --test-concurrency=1 \
  /path/to/t3/apps/server/src/psi/adapter.test.ts \
  /path/to/t3/apps/server/src/psi/inbox.test.ts
PSI_T3_CHECKOUT=/path/to/t3 PSI_CHROMIUM=/path/to/chrome \
  node --test t3/e2e.test.mjs
```

The Rust process scenario completes a turn, restarts, compares exact history,
retries the originating command, verifies conflict rejection, and checks no model
admission occurred after restart. It then SIGKILLs a gated fixture after durable
acceptance and confirms unknown outcome, no execution, and blocked continuation.
The OAuth fixture also restarts against the same SQLite history, checks the
next HTTP model request retains the exact earlier items, requires fresh sign-in,
and scans SQLite/WAL files for synthetic access and refresh tokens.
The store scenario preserves opaque provider items and call/result relationships,
and checks that unknown action records cannot be overwritten with invented results.

The Node crash scenario uses real Rust and the pinned T3 engine, SQLite event store,
command receipts, and projection pipeline in separate processes. It SIGKILLs the
receiver after inbox commit but before cursor advancement, then SIGKILLs projection
after dispatch but before inbox completion. Recovery produces exactly one assistant
message, unchanged event counts on repeated delivery, and one Rust turn record.
`PSI_TEST_CRASH=inbox_commit|projection_dispatch` and optional
`PSI_TEST_CRASH_SEQ` are explicit test-only process-kill hooks.

## Recorded results

See [machine-readable evidence](evidence/issue-48.json) for actual versions,
implementation hashes, and outcomes. No live account, Bend execution, sandbox,
file changes, checkpoint crash, or unknown-outcome acknowledgement was tested;
those do not count as passing this slice or the MVP. No product-scope changes or
conflicts with the accepted specification were identified.
