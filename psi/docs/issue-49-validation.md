# Issue #49: isolated command execution and T3 checkpoints

This slice implements [#49](https://github.com/aivv73/clanker-lab/issues/49), under
[#43](https://github.com/aivv73/clanker-lab/issues/43). It is not acceptance of the
entire MVP. The ordinary text-only and live OAuth paths retain their behavior;
`--agent-loop exec-fixture` selects the deterministic command model and loop.
No paclang files or shared dependencies change.

## Reproduce

Use the pinned T3 checkout and Node setup from the [README](../README.md). From
`psi/`, run turn-producing suites **sequentially** because they share the per-user
admission lock:

```sh
python3 policy/build.py
cargo fmt --check
cargo check --locked
cargo test --locked
python3 t3/prepare.py /path/to/t3
(cd /path/to/t3 && node_modules/.bin/tsc -p tsconfig.psi.json --pretty false)
PSI_BINARY="$PWD/target/debug/psi" node --test --test-concurrency=1 \
  /path/to/t3/apps/server/src/psi/adapter.test.ts \
  /path/to/t3/apps/server/src/psi/inbox.test.ts \
  /path/to/t3/apps/server/src/psi/workspace.test.ts
npm ci --prefix t3 --ignore-scripts
PSI_TEST_EXEC=1 PSI_T3_CHECKOUT=/path/to/t3 PSI_CHROMIUM=/path/to/chrome \
  node --test t3/e2e.test.mjs
```

The OS tests require Linux x86-64, bubblewrap, unified cgroup v2, `cgroup.kill`,
a working user systemd manager with delegation, seccomp and user namespaces.
Missing test prerequisites fail the integration test; they are not skipped or
reported as success. No account is needed for this slice.

## Implementation and evidence boundaries

- **A11–A13, exec scope:** the full browser uses the ordinary composer/WebSocket,
  real T3, Rust, checked Bend, systemd and bubblewrap. An immediate write changes
  `target.txt` from `value=old\n` to `value=new\n`. A separate control file is
  unchanged; independently running TCP and AF_UNIX listeners receive no connection.
  The payload tests inaccessible home/control trees, a symlink escape, read-only
  system/Git metadata, socket/socketpair, io_uring and inherited standard FDs.
- **Preflight:** a second T3 command has an inaccessible workdir and produces a
  launch failure with no startup marker. Focused OS tests additionally mask the
  real bubblewrap executable and cgroup filesystem, restrict namespaces and
  seccomp/prctl, disable service delegation, and remove access to the user manager.
  None produces its marker. These checks exercise actual OS restrictions, without
  a permissive test executor. Individual memory/pids-controller and `cgroup.kill`
  absence variants are not separately masked in this run; their required checks
  are implemented, and real controller readback/cgroup.kill cleanup are exercised.
- **Initial bounds:** readback is 2147483648 memory bytes, zero swap, 128 tasks,
  and 268435456 temporary bytes. Tests exercise thread exhaustion, temporary
  ENOSPC, response truncation, aggregate output termination, and a short deadline.
  The implementation defaults to 120 seconds and rejects requests over 600 seconds;
  tests do not wait ten minutes or force a 2 GiB OOM. Those exhaustive limit
  diagnostics and continuation are later slices.
- **Cleanup:** normal exit with a surviving setsid descendant and controller EOF
  during forked work both require an empty cgroup, pipe handling and reaping before
  terminal. Intent/result rows and call/result IDs are inspected. A supervisor-held
  lease and persisted cgroup record survive loss of the controlling Rust process;
  a restart cannot clear live or unreadable cleanup state. The exhaustive crash,
  cancellation-race and injected-unconfirmed-cleanup matrix remains later work.
- **A24:** baseline capture is an awaited API before `sendTurn`. The completion
  checkpoint runs through the same standard T3 handler before inbox done/session
  ready. Replayed capture preserves an existing ref. The test compares baseline
  and completion Git blobs/diff, opens and expands the rendered diff in T3,
  verifies the old line is visible, and tests both
  `thread.checkpoint.revert` and `thread.conversation.revert` over the real socket.
  Rejection receipts are required; files, messages and refs must remain unchanged.
  The UI's Edit-from-here control is absent.
- **Git boundary:** commands cannot write the mounted `.git` metadata. T3 rejects
  mutable config/includes and external filter/diff/fsmonitor programs before
  handing off a Psi command, preventing these settings from turning a file edit
  into host-side execution during checkpoint/status/diff processing. Focused real
  Git fixtures test existing and future mutable includes and configured filters.

An earlier browser attempt overlapped the Rust suite and correctly received busy;
it was rerun sequentially. Strengthening rollback assertions also caught a malformed
WebSocket test envelope; the corrected test requires actual engine rejection
receipts, not just unchanged files. Neither failed attempt is counted as a pass.

## Recorded environment and limits of the result

The host is Linux 7.2.2-1-cachyos x86-64, bubblewrap 0.12.0, systemd 261.2,
Rust/Cargo 1.97.1, Node 24.13.1 and the T3 pin
`dfbb11bdd7c3f1a5575cb55d3e3abb12be025727`. Bend 2.0.16 remains pinned to
`15ae0c86f3193b8f645b4bedbc438655b648d0da`; the proof passed and the deliberate
post-cancel Permit mutation was rejected. The checked/executed Policy.bend SHA-256
is `43d01758a533eaf2954dea62984dc3d244e53a7805e7188e4a5a92abd048da29`.

The final run passed 27 Rust tests, 20 Node adapter/inbox/workspace tests and the
full browser scenario, with no skips. Formatting, `cargo check --locked`, and the
expanded T3 TypeScript check passed (non-failing Effect suggestions remain).
[Rendered first diff](evidence/issue-49-first-diff.png).

[Machine-readable evidence](evidence/issue-49.json) records the source base,
implementation/lockfile/binary hashes, proof manifest, versions, test counts and
browser report. Source hashes identify this uncommitted implementation; prior
prototype results are not carried forward. The browser scratch directory retains
screenshots, SQLite state, refs and a log with the pairing link redacted.

This does not claim full A01–A28 acceptance. Live model tool use, write_stdin,
apply_patch, explicit unknown-outcome acknowledgement, exhaustive limit/race
matrices, other architectures and other hosts are not validated here. No conflict
with the accepted MVP boundary is intended; reduced tool arguments and the
fail-closed Git workspace requirements are documented in the README/protocol.
