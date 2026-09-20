# Issue #51: cancellation with confirmed descendant cleanup

Implements [#51](https://github.com/aivv73/clanker-lab/issues/51) under the
[#43](https://github.com/aivv73/clanker-lab/issues/43) contracts. No specification
conflicts were identified. This is the local cancellation slice, not full MVP or
live-account acceptance.

## Reproduce

From `psi/`, using the prerequisites in the README, run suites sequentially because
they share the per-user admission lease:

```sh
python3 policy/build.py
cargo fmt --check
cargo check --locked
cargo test --locked
python3 t3/prepare.py /path/to/pinned/t3
(cd /path/to/pinned/t3 && node_modules/.bin/tsc -p tsconfig.psi.json --pretty false)
PSI_BINARY="$PWD/target/debug/psi" node --test --test-concurrency=1 \
  /path/to/pinned/t3/apps/server/src/psi/adapter.test.ts \
  /path/to/pinned/t3/apps/server/src/psi/inbox.test.ts \
  /path/to/pinned/t3/apps/server/src/psi/workspace.test.ts
PSI_TEST_EXEC=1 PSI_T3_CHECKOUT=/path/to/pinned/t3 PSI_CHROMIUM=/path/to/chrome \
  node --test t3/e2e.test.mjs
```

Use Node 24.13.1 and T3 dfbb11bdd7c3f1a5575cb55d3e3abb12be025727.
The Python cancellation scenario can also be run directly:
`python3 tests/fixtures/cancellation.py target/debug/psi`.
It retains a report, SQLite stores, workspace effects and runtime logs in the
printed temporary directory. Missing OS prerequisites fail rather than skip.

## Observable checks

- A07: real fork/setsid descendants run in the command cgroup. Cancellation closes
  admission, preserves file effects, reaps owned processes and drains pipes.
  The explicit populated=false observation and absence of the observed host PIDs
  are checked before accepting successful cleanup. A suppressed soft signal
  exercises the real cgroup.kill path after two seconds.
- A08: failed cgroup observation and suppressed termination retain distinct
  unknown/live resource evidence. Both block later turns in two saved sessions;
  the forbidden marker is absent. Normal parent exit with a surviving setsid
  descendant also requires empty cgroup, drained pipes and reaping.
- Exact command-process and terminal records are already present in SQLite when
  the terminal arrives. A later turn cannot overwrite or revive a cancelled turn.
  Existing continuation tests cover occupied slots, competing exec/patch rejection
  and fresh permits. Existing controller-EOF tests cover disconnect cleanup.
- An actual Bend child is stopped while a command is live, then cancellation
  cleans the command independently. A separate scenario makes Bend unavailable
  for the next continuation and terminal transition; the command still cleans up.
  Existing policy tests cover late decisions, unavailable policy, and invalidated
  permits. The same checked Bend module executes and its negative mutant fails.
- The real T3 composer, WebSocket, driver and Rust process run successful and
  failed cancellation through **Stop generation**. The browser checks cgroup/PID
  evidence, durable records, displayed resource evidence, and rejection of a
  subsequent command without creating its file.

## Timing and limits

The supervisor measures its soft and forced phases independently of transport and
UI latency. It polls the kernel at 5 ms intervals; this is not a hard real-time
scheduler guarantee. The controller gives supervisor evidence 250 ms of transport
allowance beyond the two-plus-two-second cleanup window, then treats missing
confirmation as failure. It does not wait indefinitely for a supervisor launcher.
Terminal persistence and T3 checkpoint/UI delivery follow the stop decision.

Bubblewrap can tear down its PID namespace during graceful termination even when
payload descendants ignore SIGTERM. The separate soft-signal fault forces the
hard-kill phase without substituting fake processes. On injected failure, systemd
may subsequently clean the group; this does not repair the unhealthy runtime or
retroactively change its recorded uncertainty. Restart still checks the external
lease and recorded cgroup rather than assuming that a new Rust process proves
cleanup. Safe forced termination of arbitrary blocking in-process plugins and
remote model processing are not claimed.

Results and implementation/version identity are recorded in
[evidence/issue-51.json](evidence/issue-51.json).

## Recorded result

Rust formatting/checks and all **29 Rust tests** pass. The proof build passes and
rejects the post-cancel Permit mutant. All **20 adapter/inbox/workspace tests** pass;
the targeted TypeScript check succeeds with upstream Effect suggestions. The full
T3 browser scenario passes on the pinned Node/T3 versions.

In the final process run, forced termination confirmed cleanup in 2.04 seconds;
observation and kill failures returned in 4.02 and 4.03 seconds. Cancellation with
a stopped Bend child returned in 0.28 seconds. The restart probe rejected the
later action as busy while the old cgroup was populated.

In the final browser run, the supervisor confirmed ordinary cancellation in 5 ms
and marked failed observation after 4007 ms. The corresponding click-to-visible
message times were 1.71 and 5.67 seconds, including T3 delivery/checkpoint/rendering.
The later action displayed `runtime_unhealthy` and created no file.
[The captured UI](evidence/issue-51-cancellation.png) shows the failure and rejection.

During validation, an initial concurrent test launch was interrupted so the suites
could run sequentially. A browser assertion incorrectly included UI delivery in
the supervisor's cleanup deadline; the test now records both clocks separately.
A selector matching both visible copies of the runtime error was narrowed to the
first match. These were test corrections; no failed run is counted as a pass.
