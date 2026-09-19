# Issue #45 — session ownership and plugin lifecycle

Validation date: 2026-09-20 (Europe/Ulyanovsk). This report covers the local fixture
slice, not acceptance of the full Psi MVP. No paclang files or shared infrastructure
were changed. No specification conflicts were identified.

## Implemented contract

Rust owns admission while the JSONL reader remains responsive. A conflicting turn
receives `busy` immediately; it is not put in a queue or saved as accepted. A fixed
per-user kernel file lock prevents another adapter or independent process from
bypassing admission. The lock is held until cooperative turn work has been joined.
It is stronger than a per-workspace limit: all participating Psi processes for the
local user coordinate through the same lock inode.

Execution owners (runtime, session, turn, invocation) and supplying plugins are
separate. Session close joins only that session's tasks, preserves other sessions
and shared plugins, and is repeatable. Reopening requires an explicit open and
cannot reuse old turn identities. The T3 adapter also handles close during session
startup without leaving an orphaned session.

Assembly selects `single-response` or `buffered-response` through one `AgentLoop`
contract. Each configured plugin starts once. Graph validation rejects missing
services, cycles, duplicate providers and duplicate plugin IDs before startup.
Partial startup cleanup includes the failing plugin, tracked work and registrations,
then proceeds through consumers to providers. Stopping an in-use provider is
rejected. Close errors are identified by plugin and reported after remaining cleanup.

T3 correlates replies by request ID, allowing lifecycle and busy replies while a
turn is pending. Startup diagnostics are bounded to the last 8192 characters and
included in the startup error, so an early pipe close cannot hide the graph failure.

## Checks and evidence

The final machine-readable reports are in [issue-45.json](evidence/issue-45.json).
They record the base revision and hashes of the uncommitted implementation, Rust
and Cargo versions, Node, OS, T3 pin and lockfile. Browser screenshots and redacted
logs remain in the temporary roots recorded in the report.

| Check | Result |
| --- | --- |
| `cargo fmt --check`, `cargo check --locked`, `cargo test --locked` in `psi/` | Passed; 10 tests |
| Strict targeted T3 `tsc -p tsconfig.psi.json --pretty false` | Passed |
| T3 adapter tests with a real Rust process | Passed; 15 tests |
| Full T3 browser scenarios | Passed; 6 scenarios (four startup faults, buffered loop, session ownership) |

The adapter suite checks both selected loops with the same scenario, all four
startup faults, cross-session conflicts, independent bridges and independent
ProviderInstances, no queued execution, repeated close, controller EOF while a
turn is held, and reopen after close during startup. It verifies returned identities,
history, process exit, cleanup diagnostics and the ability to admit subsequent work.

The full application scenarios use ordinary composer requests over the standard
WebSocket, real ProviderDriver/Adapter instances, and real Rust processes. The
lifecycle case holds the first model call on a fixture gate, observes busy in another
conversation, issues two standard `thread.session.stop` commands, and verifies the
surviving conversation's visible response and durable T3 projection. Shared plugin
shutdown must not occur during session close. Startup-failure scenarios check the
visible error, persisted error session, absence of an assistant result, absence of
a surviving Psi child, and cleanup diagnostics. No projections or responses are
substituted by the tests.

Provider-stop protection, explicit invocation ownership, and returned close errors
are additionally checked at the public lifecycle API. No plugin-specific diagnostic
RPC was added solely to make those checks reachable from T3.

The local A03 scenario passed with both startup-selected loops. A04 passed graph
rejection and partial-start cleanup. A05 passed the full application session scenario,
with additional real-process tests for cross-instance admission and public-core tests
for provider protection. All six final browser reports match the current implementation
file hashes; no source changes were made during those final runs.

## Environment and limits

T3 pin: `dfbb11bdd7c3f1a5575cb55d3e3abb12be025727` plus the repository overlay.
Node: `24.13.1`. Rust/Cargo: `1.97.1`. Chrome: `152.0.7977.82`.
Playwright Core: `1.60.0`. Linux: `7.2.2-1-cachyos`, x86_64.
The disposable `/tmp/psi-t3-44` checkout reused the pinned upstream dependency set;
a fresh installation of all upstream dependencies was not tested. Installing Psi's
Playwright dependency emitted an engine warning under ambient Node 26; all actual
TypeScript and browser tests ran with Node 24.13.1.

An initial missing-dependency browser check failed because it expected a specific
process-close message while the OS delivered EPIPE first. The Rust graph rejection
was recorded correctly. The bridge now preserves startup stderr diagnostics in the
error, and the final checks verify the failure in T3's durable state as well as UI.

Safe forced termination of arbitrary blocking or hung in-process plugin code is not
promised. Task joining proves cleanup of these cooperative fixture futures, not of
external processes or arbitrary plugin effects. A returned plugin close error is
reported, not represented as proof that its external resources disappeared.
The fixed lock coordinates trusted processes; it is not a security boundary against
same-user code unlinking the file or using a different mount namespace.

No real tools, Bend, cgroups/seccomp, live account, durable Psi receipts/history,
crash recovery, full upstream T3 build/test suite, desktop/mobile or remote acceptance
was run or claimed. The reserved invocation ownership type does not imply tool
implementation. A process crash releasing the lock does not establish cleanup of
future external children. Those concerns remain in their subsequent MVP slices.
