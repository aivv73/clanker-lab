# Issue #46: checked Bend action admission

This slice adds a policy plugin and the `policy-fixture` agent loop. An ordinary
T3 turn requests one immutable `fixture_counter` action. The real checked Bend
CPU executable supplies the decision; an admitted handoff increments a harmless
in-memory counter. No workspace command or patch runs in this slice.

## Build and repeat

From `psi/`, on Linux with Python 3, Git, Bun, Clang and Rust:

```sh
python3 policy/build.py
cargo fmt --check
cargo check --locked
cargo test --locked
```

The build fetches Bend **2.0.16**, commit
`15ae0c86f3193b8f645b4bedbc438655b648d0da`, into ignored `policy/.build/bend`.
`--bend-source /absolute/path` uses an existing clean checkout at that exact pin.
Telemetry is disabled. It requires the exact successful checker report, rejects
an intentionally broken post-cancellation Permit for the expected law mismatch,
and compiles `Main.bend`, which imports the same `Policy.bend` that was checked.
It records compiler/module/executable identities in `.build/manifest.json`.
Policy tests require this build and fail if it is missing; they do not skip or
substitute a Rust implementation of the rule.

Prepare the pinned disposable T3 checkout as described in the package README.
Use Node 24.13.1 and the existing T3 lockfile. Then:

```sh
python3 t3/prepare.py /absolute/path/to/t3
PSI_BINARY="$PWD/target/debug/psi" node --test /absolute/path/to/t3/apps/server/src/psi/adapter.test.ts
PSI_T3_CHECKOUT=/absolute/path/to/t3 PSI_CHROMIUM=/absolute/path/to/chrome \
  PSI_TEST_POLICY=normal node --test t3/e2e.test.mjs
```

Repeat the browser command with `cancel`, `missing`, `hang`, `invalid`, `mismatch`,
and `failed`. These use the normal composer and WebSocket, real T3 server/driver,
a real Rust process, the checked executable, and actual SQLite records. They
verify the visible result, handoff count, correlated terminal record, and absence
of every recorded policy child after cleanup. `cancel` presses **Stop generation**
after a valid Permit is received but before handoff, then releases the fixture
late and verifies that cancellation persists.

For interactive use:

```sh
python3 t3/run.py /absolute/path/to/t3 --agent-loop policy-fixture
python3 t3/run.py /absolute/path/to/t3 --agent-loop policy-fixture --hold-policy
```

The latter waits for the printed scratch root's `release-policy` file. Cancel
through T3 or create the file to allow the already-admitted fixture handoff.
`--policy-bundle /absolute/path` selects a separately built bundle. T3 configuration
uses `policyBundle`, `policyGate`, and `policyFault`; Rust uses `--policy-bundle`,
`--policy-gate`, and `--policy-fault`. These are trusted startup configuration,
never prompt-selected options. Existing fixture and Codex loops remain text-only.

## Formal boundary and audit

The entire proof graph is `PROOF.bend` → `LAWS.bend` → `Policy.bend`. It imports no
Base, IO, template, external library, unsafe construct, axiom, or assumed proof.
All four declared laws have checked definitions. `cancelled_sequence` uses
structural induction over arbitrary finite event lists and the actual `step`
function; it is not a bounded enumeration. `cancellation_closes_active` connects
Cancel in Active to that invariant. Finished sequences also deny every action.
The dependency/import audit and negative mutation are mandatory build gates.

`Main.bend`, Base/IO, the compiler, C runtime, Bun/Clang, Linux, serialization,
Rust, and SQLite are outside the proved property. All 12 transitions are also
executed on CPU in tests; Rust's test oracle is not used by runtime admission.
The bundle manifest is build evidence, not a signature or a defense against a
malicious installer replacing both binary and manifest. The installation is
trusted. At startup Rust verifies the pin and compiled-in module hashes against
the manifest and copies the verified executable bytes into a sealed Linux memfd.
Each transition executes those same bytes, avoiding pathname replacement between
hashing and execution. Bundle verification occurs before accepting turns.

## Runtime ordering and limits

B1 uses arguments `request_id turn_id action_id epoch state event` and exactly one
seven-field tab-separated response starting with `B1`. A fresh transition request
ID correlates the reply; the turn identifies its durable originating command.
Action IDs bind private immutable names and JSON arguments. Epochs are newly
randomized per policy-plugin startup. A permit is private, move-only, tracked by
its issuing gate, and consumed once. Wrong request/turn/action/epoch, changed
arguments, repeated action IDs, and duplicate use cannot hand off.

Each transition has a 250 ms deadline, 4096-byte stdout and stderr bounds, null
stdin, an empty environment, `--threads 1 --gpu off`, and no shell. Nonzero exit, timeout, unavailable or
mismatched bundle, malformed output, and identity mismatch deny admission. There
is no Rust decision fallback. `hang` stops the real child with SIGSTOP; `failed`
gives the real B1 executable an invalid argument count; `invalid` and `mismatch`
corrupt its returned bytes at the transport boundary. Missing-policy injection
prevents launch. These explicit fault switches are for acceptance testing.

Request registration, decision acceptance, permit consumption and fixture handoff,
and admission closure serialize through one gate mutex. No mutex is held across
Bend execution. Cancel closes admission and invalidates issued permits before
awaiting anything; a late response cannot restore it. The active child is killed,
pipes dropped, and wait/reaping confirmed by a supervisor task owned independently
of the abortable agent task. Session/controller shutdown runs the same cleanup.
A terminal Cancel/Finish transition runs after active work is stopped; its failure
is recorded and cannot reopen admission. Cleanup failure marks the controller
unhealthy and rejects further turns. Terminal records follow cleanup and SQLite
commit and include transitions, correlation identities, handoffs, and epoch.
The policy plugin also owns and drains its outstanding gates on close.

This supervisor handles only the pinned, trusted, non-forking policy executable.
It is not the cgroup-based executor for arbitrary commands required by #49.
A counter resets at runtime restart; durable old counter results remain intact
and replay does not increment it. Crashed turns use the existing unknown-outcome
recovery behavior rather than automatic execution.

## Evidence

The recorded run passed A17–A19 within this slice: 26 Rust tests, 17 adapter tests,
2 durable-inbox tests, and all 7 browser scenarios passed without skips. Formatting,
`cargo check --locked`, and the targeted T3 TypeScript check passed; the latter
reported non-failing Effect suggestions. Each final browser report's implementation
and executable hashes were checked against the final working tree.

The [checked-in evidence file](evidence/issue-46.json) records this implementation's validation results.
Browser reports contain the T3 pin, Node/Chrome/Rust/OS versions, source and lockfile
hashes, and durable policy evidence. The focused policy tests cover all transitions,
immutable argument and identity rejection, single use, old epochs, cancellation
before Request/between decision and handoff/after effect, and policy failures.
The controller integration test additionally observes real child PIDs, cancellation
and EOF during a hung transition, replay, restart epochs, and late fixture release.

These checks establish the #46 fixture slice, not arbitrary tool isolation or
full MVP acceptance. No real account is needed for this slice. No change to the
accepted product boundary is intended; #49 remains the next tool-execution slice.
