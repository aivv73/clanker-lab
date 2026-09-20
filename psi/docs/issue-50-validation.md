# Issue #50: command continuation without releasing occupancy

Implements [#50](https://github.com/aivv73/clanker-lab/issues/50) under the
[#43](https://github.com/aivv73/clanker-lab/issues/43) contracts. No specification
conflicts were identified. This is the deterministic model slice, not acceptance
of the full MVP or live Codex tool orchestration.

## Reproduce

From `psi/`, with the prerequisites and pinned T3 checkout in the README:

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
npm ci --prefix t3 --ignore-scripts
PSI_TEST_EXEC=1 PSI_T3_CHECKOUT=/path/to/pinned/t3 PSI_CHROMIUM=/path/to/chrome \
  node --test t3/e2e.test.mjs
```

Use Node 24.13.1 for the pinned T3 integration. Run turn-producing suites
sequentially: they share the actual per-user admission lease. Missing Linux
isolation prerequisites fail the tests, rather than selecting an unrestricted
executor or reporting a skip.

## Checked behavior

- **A06:** a real isolated Python process returns a handle, accepts stdin after
  read-only polling, and writes its PID and received text. The before/after PID
  matches. Competing exec and patch attempts return `tool_busy`; their control
  file never appears. A new exec succeeds after confirmed cleanup. Unknown and
  prior-turn handles are rejected. stdout is delivered once across calls.
- **Admission and history:** every attempted invocation has a distinct real Bend
  request with matching immutable arguments, model call/action ID, turn and epoch.
  Polls are included. Successful invocation results match SQLite action rows;
  completed history preserves all model call/result pairs, including rejections.
  Existing policy tests cover argument substitution, single use, stale epochs,
  cancellation ordering, unavailable/invalid policy responses and the checked
  negative proof mutation.
- **A09/A10 continuation scope:** repeated reads preserve a short original
  deadline and cumulative output totals. Output in successive stdin-triggered
  chunks exceeds 10 MiB and terminates with `output_limit`. Response truncation
  is explicit. A separate supervisor run times out with no continuation pending,
  proving that polling does not drive its deadline enforcement.
- **Cleanup:** cancellation after a running result closes admission, confirms
  cleanup and persists the final process outcome separately from the earlier
  invocation result. Existing real tests cover controller EOF, fork/setsid
  descendants, normal-exit descendants, resources and fail-closed prerequisites.
- **T3:** the ordinary composer drives exec, polling and stdin continuation.
  The browser observes running/result text and busy patch rejection; the actual
  file contains the supplied stdin, and the competing file is absent. SQLite
  confirms call/result pairing and fresh policy transitions. The existing first
  diff, isolation, rollback rejection and controller cleanup checks also run.

The first new browser assertion incorrectly expected three admission handoffs;
this counter includes the two busy attempts entering the guarded occupancy check,
so five is correct. The corrected check also verifies five distinct policy
requests and the absence of competing effects. A timing assumption that startup
output must be in the first 50 ms response was removed: the test now checks exact
combined output across responses. Neither check treats scheduling as process proof.

## Results and limits

Recorded versions, source hashes, proof bundle and final check outcomes are in
[evidence/issue-50.json](evidence/issue-50.json). The browser screenshot is
[evidence/issue-50-continuation.png](evidence/issue-50-continuation.png).

Rust formatting/checks and all 28 Rust tests pass. The enhanced continuation test
also passes with cancellation-after-yield and idle-deadline checks. All 20 adapter,
inbox and workspace tests pass. The targeted TypeScript check exits successfully
with upstream Effect suggestions. The full real T3 browser scenario passes.

PTY, patch execution, live-account tool orchestration, unknown-outcome
acknowledgement, and the exhaustive later cancellation/recovery matrix remain
outside this slice. Input backpressure can delay a write response until delivery
or the existing deadline; control writes and cancellation cleanup remain bounded.
Output is bounded text using the existing lossy UTF-8 conversion, not a binary
transport. Cancelled/failed invocation evidence is retained in durable events and
action/process records; completed call pairs enter model history. Recovery must
not interpret a running invocation result as a completed command or replay it.
