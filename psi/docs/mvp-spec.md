# Psi MVP specification

The product name is **Psi** (with an initial capital letter); “harness” describes the type of system.

Status: **accepted by the user, ready for implementation**, 2026-09-19. The product implementation has not yet been accepted.

The purpose of this document is to hand off the agreed boundaries, contracts, and verifiable outcomes for implementation. The decisions come from comments on the [decision map's](https://github.com/aivv73/clanker-lab/issues/29) issues; this document consolidates them into a specification without changing the accepted decisions. Subsequent contract changes require explicit agreement. Terms are defined in the [Psi glossary](../CONTEXT.md).

All identified product decisions have been agreed, including storing OAuth credentials only in runtime memory. The user confirmed that this document is sufficient to begin implementation. The API shapes below define mandatory semantics, not ready-made Rust type declarations.

## 1. Outcome and scope

The MVP lets one local user request a file change in T3. A standalone Rust harness accesses the model through Codex/ChatGPT sign-in, performs reads and patches, and returns a response; T3 displays history and the diff. A separate scenario verifies cancellation of a hung tool and its descendants.

Required components: T3, a standalone Rust harness, one provider with Codex sign-in, a replaceable agent loop, tools, context/history, and a Bend rule that is actually executed. Replacing a required component with another product does not count as implementing this specification.

The target environment is Linux with the mandatory isolation mechanisms below. Multiple saved sessions are allowed. At most one turn is active across the entire runtime; there is no hidden queue. Tools run sequentially within a turn. One workspace is assigned to a session before work begins.

The decision map ends with a specification sufficient to begin implementation. Accepting the document is not MVP acceptance and does not transfer results from throwaway prototypes to the future product.

## 2. Components and interfaces

| Component | Responsibility | Boundary |
| --- | --- | --- |
| Rust core | Lifetime scopes, registrations, dependencies, task tracking, startup/shutdown order | Does not choose the next tool or implement the domain admission rule |
| Agent loop | Sequence of model requests and tool invocations | Selected at startup through a common contract; another implementation can be connected without changing the core or other plugins |
| Model plugin | OAuth/refresh, provider requests and response streaming | Network access is separate from tools; the official Codex CLI is not a required intermediary |
| Tool plugin | exec_command, write_stdin, apply_patch, supervisor | Validates arguments, occupancy, limits, and isolation; executes only admitted actions |
| History/context plugins | Exact model history and construction of the next request | Context comes from harness records, not the text displayed by T3 |
| TypeScript T3 adapter | ProviderDriver/Instance/Adapter, JSONL, durable ingestion | Connects command and event identities to persisted T3 operations |
| T3 | User conversation, log, Git checkpoints, and diff | History/file rollback is disabled in the MVP |
| Bend rule plugin | Pure transition and admission decision | The sole implementation of the selected rule; not responsible for stopping processes |

Plugins are Rust modules/crates in one binary. One configured instance of each plugin lives for the lifetime of the runtime. The contract includes an identifier, provided services, required dependencies, asynchronous `start(context)`, and a repeatable `close()`. The context provides dependencies, service registration, and spawning of tasks tracked by the core. Registrations belong to the plugin scope.

Missing dependencies, cycles, and conflicting providers are checked before work begins: each service has one provider. Providers start before consumers and close after them. A startup error triggers cleanup of the partially started plugin, then previously started plugins in reverse order; registrations are removed. A partially operational mode is forbidden. Stopping a provider separately is rejected while dependent plugins are running.

Execution hierarchy: `Runtime → Session → Turn → Tool invocation`; plugin scopes are separate. Work belongs to an execution scope and is associated with the plugin that provided the capability. Closing a session cleans up its work while preserving shared plugins and other sessions.

Source: [core and plugins](https://github.com/aivv73/clanker-lab/issues/33#issuecomment-5742783758).

## 3. States and shutdown

The states in this table describe contract semantics; their encoding does not have to match T3 status names.

| Situation | Required behavior |
| --- | --- |
| Runtime is starting | Turns are not accepted until all plugins have started successfully |
| Runtime is ready and the session is not blocked | One turn may be accepted |
| A turn is active or its cleanup is incomplete | Another request receives an explicit busy response; no queue is created |
| Cancellation is accepted | Admission of new work closes immediately; active work receives a cancellation signal |
| The result is persisted and cleanup is confirmed | Turn completion is produced; the slot is released |
| Cleanup is unconfirmed | The runtime is unhealthy and rejects new turns; the reason and remaining resources are displayed |
| Unknown action outcome | New turns in that session are forbidden until the user explicitly checks the consequences |
| Consequences of the unknown outcome are acknowledged | The session is unblocked; the outcome does not become success, and the action is not repeated |
| Runtime is restarted | Records are recovered, but the old turn does not resume automatically; old permits are invalid |

Acknowledging an unknown outcome does not clear an unhealthy runtime state or replace cleanup. A restart alone does not prove that no external process remains. Safe forced termination of arbitrary hung plugin work inside a shared process is not guaranteed.

A late result does not revive a cancelled turn. Cancellation does not roll back changes already made. The UI may indicate that cancellation has begun, but it displays completed cancellation only after confirmed cleanup; cleanup failure is shown separately.

## 4. Tools and isolation

### 4.1. Tool set and compatibility

The supported tools are `exec_command`, `write_stdin`, and `apply_patch`; reading and searching use the shell. Names, semantics of supported arguments, patch format, and primary Codex results are preserved. Full compatibility is not claimed. The implementation publishes the exact supported parameter schema; unsupported parameters are not advertised and are explicitly rejected.

Use ordinary stdin/stdout/stderr, without a PTY. User shell profiles are not loaded; the environment is set explicitly. Returning a running process identifier from exec_command does not release the slot: while it runs, write_stdin is allowed for that process, while a new exec_command or apply_patch receives busy. The slot remains occupied during cleanup of surviving descendants.

### 4.2. Access

Writes are allowed in the session workspace and a separate temporary area. Required system programs and libraries are available read-only. The full home directory, SSH keys, harness credentials, host sockets, and cgroup management are not exposed. Tool networking is disabled. Allowed actions run automatically; requests to expand access, enable networking, or elevate privileges during a turn are rejected.

All three tools, including apply_patch and symlink traversal, obey the same boundary. All mandatory restrictions must be established before user code starts. Failure to establish isolation means rejection; there is no ordinary-shell fallback.

Profile: bubblewrap, a separate cgroup v2 for each command, delegation through a user systemd service/scope, and a seccomp ban on Unix sockets, including bypasses through io_uring. Inherited host socket descriptors are not passed through. Programs requiring Unix IPC, networking, or hidden home caches do not receive access automatically.

Mandatory prerequisites: user namespaces, bubblewrap, unified cgroup v2 with cgroup.kill, a user systemd manager, and effectively delegated memory/pids controllers. The supervisor stays outside the command's cgroup; code starts only after placement in the correct cgroup.

### 4.3. Limits and cleanup

| Parameter | MVP contract |
| --- | --- |
| Command deadline | 120 seconds by default; requested increases are capped at 10 minutes; reading output does not extend the deadline |
| Graceful termination | Up to 2 seconds after cancellation/timeout |
| Forced termination | Then the entire cgroup subtree; after another 2 seconds, unconfirmed cleanup makes the runtime unhealthy |
| Response to one invocation | At most 16000 tokens, with explicit truncation reporting |
| Total command stdout/stderr | At most 10 MiB including all write_stdin calls; exceeding the limit stops the command with output_limit |
| Memory | 2 GiB per command; swap is forbidden |
| Processes and threads | At most 128 combined per command |
| Temporary area | Up to 256 MiB |
| Workspace size | No separate MVP quota; the output limit does not constrain file writes |

The user configures memory, task count, and temporary storage before startup; the model does not. Resource-limit violations have an explicit reason. Confirming cleanup requires `populated=0`, completed supervisor cleanup, handled pipes, and waiting for processes to exit. Sending a signal or writing to cgroup.kill alone is insufficient. Surviving descendants are also cleaned up after the main command exits normally.

### 4.4. Patches and results

The entire patch, its paths, and its applicability are validated before the first write. A transaction across multiple files is not promised. If an error or cancellation occurs after partial application, return an exact list of applied and unapplied changes; there is no automatic rollback. Cancellation is checked between file changes. If the process crashes before an exact result can be saved, the unknown-outcome rule in section 6 applies.

Distinguish exit code, launch failure, timeout, cancellation, output/resource limits, isolation violation, and cleanup failure. A nonzero exit code is the result of a command that ran; launch failure means the command did not start.

Sources: [tools](https://github.com/aivv73/clanker-lab/issues/34#issuecomment-5743006862), [Linux profile](https://github.com/aivv73/clanker-lab/issues/40#issuecomment-5743592209).

## 5. Bend rule

Rule: after cancellation is accepted, no subsequent sequence of events for that turn grants a new permit to start or continue a tool. Resource cleanup is not a new model-requested action.

Pure transition: `state + event → next_state + Permit/Deny`. States: Active, Cancelled, Finished; events: Request, Cancel, Finish, Late. Only Request in Active can yield Permit. Terminal states cannot return to Active. A new turn has its own state.

Admission is mandatory for exec_command, apply_patch, and every write_stdin, including reading another chunk of output. It is bound to a specific turn and immutable action, is single-use, and cannot be deferred for later application. Rust separately checks arguments, occupancy, and isolation: Permit is necessary but not sufficient.

One short-lived CPU Bend process handles a transition. Rust owns the state. Protocol B1 carries request_id, turn_id, action_id, epoch, state/event, and the response next_state + Permit/Deny. The initial timeout is 250 ms. There is no parallel implementation of the rule or permissive fallback in Rust.

Cancellation, receipt of the decision, and handoff to the executor are ordered. If cancellation is accepted first, the action does not start. If the action has already been handed off, cleanup begins. Waiting for Bend does not delay cancellation: Rust closes admission of new work, begins cleanup, and rejects a late response. Incorrect identifiers, a stale epoch, an invalid response, unavailability, or a Bend timeout deny the action and fail the turn. Cleanup does not depend on Bend being healthy.

The same transition module is checked and executed. The mandatory property covers every finite sequence of events after cancellation; unsafe constructs and unchecked assumptions in the proof or its dependencies are forbidden. A successful checker exit code alone is insufficient. A mutation that permits an action after cancellation must be rejected because it violates the property. The module version is pinned together with the compiler.

The proof does not cover the compiler, serialization, IO, Rust, or the OS. Permit binding, single use, the build, races, and actual cleanup are checked separately.

Sources: [rule](https://github.com/aivv73/clanker-lab/issues/36#issuecomment-5743220695), [accepted process boundary](https://github.com/aivv73/clanker-lab/issues/37#issuecomment-5743335407).

## 6. History, JSONL, and recovery

The harness stores exact model history, tool calls and results, commands, events, and action states in SQLite outside the tool workspace. Intent is recorded before an action and its result afterward. SQLite does not make a file write atomic with committing the result. Model context is built from these records, preserving required provider output items and call/result relationships.

The T3 adapter launches a standalone Rust process and exchanges versioned JSONL over stdin/stdout. Stdout is reserved for protocol traffic; stderr carries diagnostics. The handshake checks version and capabilities before accepting turns. The adapter must not create independent active runtimes that bypass the global single-active-turn constraint.

| Contract element | Semantics |
| --- | --- |
| Command request_id | Stable identity of the original persisted command; not a new UUID for every attempt or a hash of prompt text alone |
| session_id / turn_id / invocation_id | Distinct identifiers for the corresponding execution scopes |
| Repeated ID with the same content | Previous acceptance status, without another execution |
| Repeated ID with different content | Explicit conflict error |
| Acceptance acknowledgement | The command is persisted, but work is not necessarily complete |
| Event seq | Increases within a session; the event is persisted before sending |
| Delivery | Read after the last accepted sequence number; gaps are not hidden, and conflicting content for the same event is rejected |
| Cursor | Persisted after T3 durably accepts the event |
| Terminal | Produced after the result is persisted and cleanup finishes; cleanup failure is not disguised as successful completion |

A durable inbox on the T3 side ensures accepted events reach projections. Derived commands have stable identities; redelivery does not create duplicate messages. Event identity must include the session and provider instance to prevent collisions between logs. Acknowledgement of durable acceptance does not mean the browser has already rendered the event.

Work continues when the browser disconnects. Loss of the controlling T3–Rust connection triggers cancellation and cleanup. After a Rust crash, records are recovered; the interrupted turn is not restarted automatically.

If an action may have occurred but no durable result exists, its outcome remains unknown. The user sees the action, arguments, and known facts. Their explicit acknowledgement that they checked the consequences unblocks the session without repeating the action or inventing success. Model history receives an explicitly unknown result with a valid call/result relationship; a new ordinary request cannot bypass acknowledgement.

There is no automatic compaction or silent message removal. The context budget is checked before a model request. If the history does not fit, the turn stops with a clear reason; the user may start a new session.

Source: [history contract](https://github.com/aivv73/clanker-lab/issues/35#issuecomment-5743088901).

## 7. T3 and Git checkpoints

T3 owns the conversation, log, and Git checkpoints. History and diff viewing remain available. History and file rollback is disabled in the UI and rejected by the server before any files, messages, or refs change. Checking only that a button is absent is insufficient.

**Before admitting an action that can change files, wait for T3's initial Git snapshot.** A start notification without waiting does not ensure a correct first diff.

**Recovery of a terminal event includes checkpoint processing.** Once the message is saved, the checkpoint must not be lost because the cursor advances or the server crashes. In the accepted prototype, an inbox entry becomes processed after the standard checkpoint handler, and replay is idempotent. The production implementation must preserve this guarantee; the prototype's global callback injection is not an approved API.

The adapter maps capabilities, the model, states, errors, and requests to check an unknown outcome onto standard T3 contracts. If the pinned revision is incompatible, the contract must be explicitly reconsidered rather than silently weakened.

Source: [accepted full integration](https://github.com/aivv73/clanker-lab/issues/41#issuecomment-5744176238).

## 8. Provider, sign-in, and credentials

A standalone Rust client for Codex/ChatGPT sign-in was selected. It requires correct OAuth/PKCE and callback-state validation, refresh, SSE handling, preservation of output items including required reasoning items, and correct forwarding of tool results into the next request.

An interrupted or incomplete stream must not be presented as a completed response. UTF-8/SSE fragmentation, missing Content-Type, and incompatible MIME types are checked separately. HTTP 401 and refresh errors are distinguished from model refusal and local tool errors. Closing the stream locally is not claimed as proof that server-side computation or billing has stopped.

**Accepted MVP policy:** access/refresh tokens are stored only in runtime memory. Sign-in occurs after process startup, and refresh operates within that process lifetime; a restart requires signing in again. The harness does not read credentials from an installed Codex or create its own persistent token store. Session history is retained independently of credentials. In-memory storage does not guarantee cryptographic memory erasure or the absence of OS swap. Persistent sign-in is deferred beyond the MVP.

Credentials are inaccessible to tools and excluded from JSONL diagnostics, model history, and acceptance reports. Testing with a real account requires user participation; local tests use synthetic tokens.

Sources: [accepted standalone client and limitations](https://github.com/aivv73/clanker-lab/issues/39#issuecomment-5743495723), [credential storage policy](https://github.com/aivv73/clanker-lab/issues/42#issuecomment-5744283395).

## 9. Environment, versions, and evidence boundaries

These are pinned experiment snapshots, not a promise to support arbitrary future versions.

| Component | Pinned base | Known results and remaining checks |
| --- | --- | --- |
| T3 | dfbb11bdd7c3f1a5575cb55d3e3abb12be025727 | The full local web path was tested with a fixture; desktop/mobile and remote were not tested |
| Node for T3 | 26.8.1 in the run; upstream requires ^24.13.1 | The product run must record the actual supported environment; the earlier run does not prove Node 24 compatibility |
| Bend | 2.0.16, 15ae0c86f3193b8f645b4bedbc438655b648d0da | The same module was checked and executed; prototype performance is not an SLA |
| Linux profile | Linux 7.2.2-1-cachyos x86_64, bubblewrap 0.12.0 | One host was tested; moving to another requires preflight and repeated checks |
| OAuth/SSE reference | Pi 36b60d2e8985899743c4cf5bd5f8929832a3f05d | Protocol reference, not a guarantee of third-party backend stability |
| Prototype live model | gpt-5.6-luna on the account used | No guarantee of availability on another account or compatibility with all models |

Each implementation report records the source revision, lockfiles, Rust/compiler/tool versions, the hash of the checked and executed Bend module, T3/Node, the OS, and cgroup/seccomp settings. A new pin requires rerunning the affected checks; it cannot silently replace a required component.

Feasibility artifacts:

- [Rust–Bend, 5de2322](https://github.com/aivv73/clanker-lab/blob/5de232209e7f805853fee9e76d48f8a3898d106a/harness/prototypes/rust-bend/README.md): pure property, CPU process, and controlled races; no real tools or shared SQLite/T3 integration.
- [Codex sign-in, 62144a3](https://github.com/aivv73/clanker-lab/tree/62144a3/harness/prototypes/codex-auth): live OAuth/refresh and an SSE tool roundtrip; not durable token storage/recovery or an end-to-end file change through T3.
- [Linux isolation, 3bc6c5d](https://github.com/aivv73/clanker-lab/tree/3bc6c5d/harness/prototypes/linux-isolation): boundaries, descendants, and resources; not a full implementation of apply_patch, general graceful termination, output limits, or all races.
- [T3–harness, 960bb65](https://github.com/aivv73/clanker-lab/blob/960bb65/harness/prototypes/t3-recovery/native/FULL-APP.md): real web, checkpoints, and crash/replay with a synthetic model; model history was checked structurally. The Rust fixture did not spawn children; process Ctrl-C and unknown-outcome recovery are not separate proof of graceful EOF.

The T3 prototype passed strict adapter checking. A broader targeted TypeScript check reported errors in unchanged upstream HostPowerMonitor.ts:69 and NativeTelemetryClient.ts:509; an earlier frontend check reported an error in openPullRequestLink.ts:242. These limitations do not constitute a clean check of the future implementation and must be accounted for during its build and acceptance.

## 10. Implementation acceptance matrix

Every row is mandatory within its stated scope. `Local` means a reproducible fixture without an account; the real OS, Rust, Bend, and T3 are used wherever they are the subject of the check. A fixture-model test does not prove that a live scenario passes.

| ID / scope | Setup and scenario | Expected result | Verifiable evidence |
| --- | --- | --- | --- |
| A01 / live | Small Git project: target.txt contains `value=old\n`, a control file is unchanged; ask T3 to replace old with new | The real model invokes reading and apply_patch through Rust; target.txt is exactly `value=new\n`; other working files are unchanged; T3 shows the response and correct diff | Request/tool identifiers without credentials, file bytes/hashes, diff, UI history, and database |
| A02 / local + continuation of A01 | Restart after a completed turn | History and diff are preserved; the action is not repeated | Compare messages, refs, files, and execution counts before/after |
| A03 / local | Two agent-loop implementations through one contract | Replacement at startup without changing the core or other plugins | Configurations and results from the same contract scenario |
| A04 / local | Missing dependency, cycle, two providers, failure partway through start | No turns before readiness; explicit errors; partial startup cleaned up in reverse order, registrations removed | Ordering log, resource registry, absence of leftover tasks |
| A05 / local | Multiple sessions, second turn while the first is active; close one session; close twice; stop a provider in use | Busy without queuing; other sessions/shared plugins preserved; repeated close is safe; provider is protected | Scope states, start/close calls, and execution counters |
| A06 / local | exec_command returns a process ID, followed by write_stdin and attempts to use other tools | The same process can continue; a new exec/patch receives busy until exit and cleanup | Process, slot, and invocation events; absence of a second effect |
| A07 / local | Hung tool with fork/setsid, cancellation from T3; late result | Bend closes admission; no new work; the tree stops within the 2+2 second bound; a late result does not revive the turn | Real Bend calls, cgroup populated=0, pipes/reaping, timing, and terminal/UI |
| A08 / local | Unconfirmed cleanup; normal parent exit with a live descendant | Cleanup failure blocks the entire runtime; normal exit also cleans up descendants and holds the slot until then | Injected observation/cleanup failure, resource list, rejection of a new turn |
| A09 / local | Command timeout; repeated output reads | 120 seconds by default, requested maximum 10 minutes; reading does not extend the deadline; cleanup is mandatory | Controlled time plus an integration run of the supervisor, stop reasons |
| A10 / local | Output chunks through exec/write_stdin, one long response | A total of 10 MiB triggers output_limit; response ≤16000 tokens with explicit truncation | Total byte counter, response measurement, reason, and cleanup |
| A11 / local | Attempts to read home/credentials, write system paths, or escape through symlinks; all three tools | The boundary holds; apply_patch cannot bypass it through a separate path | External control files unchanged; rejected operations and closed FDs |
| A12 / local | TCP/networking; host socket in the workspace; socketpair and an alternative io_uring path | Networking and forbidden Unix IPC are unavailable; no fallback or access expansion | An independent host listener receives no connection, syscall results |
| A13 / local | Make namespace/bwrap/systemd/controller/cgroup.kill/seccomp unavailable one at a time | Tool code does not start; explicit preflight error | No startup marker; the reason identifies the mechanism |
| A14 / local | Memory, swap, pids/threads, and tmp limits | Actual values of 2 GiB/0/128/256 MiB; violations explicitly diagnosed | Read back settings; OOM with a smaller test limit, pids, and ENOSPC; no invented workspace restrictions |
| A15 / local | Invalid patch; failure/cancellation after the first of multiple files | Invalid patch makes no writes; partial result lists changes exactly; no rollback | Each file's bytes compared with the applied/unapplied lists |
| A16 / local | Nonzero exit, launch failure, timeout/cancellation/limit/isolation | Reasons are distinguishable; launch failure is not presented as an executed command | Typed results and actual side effects |
| A17 / local | Proof checker, dependency audit, module execution; deliberate mutation permitting Permit after Cancel | Property checked for every sequence without unsafe/assumptions; mutation rejected for violating the property; the same module is executed | Checker output, dependency audit, module/compiler/binary hashes, negative result |
| A18 / local | All Bend states/events; double use, wrong ID, old epoch; cancellation before/after handoff to the executor | Only a valid single-use Permit in Active; no admission after Cancel/Finish; race ordering respected | Controlled ordering, real Bend responses, and tool-handoff counter |
| A19 / local | Bend missing, hung, returning an invalid response/ID, or failing; cancellation while waiting | Timeout after 250 ms or explicit error; no action or permissive substitute; cancellation and cleanup do not wait for Bend to recover | No action marker, rejected late responses, cleanup |
| A20 / local | Incompatible handshake; repeated request_id with the same/different content | Incompatibility detected before turns; prior acceptance without repeating the action; conflict rejected | JSONL and persisted commands, effect count |
| A21 / local | Crash after inbox commit but before cursor advancement; after dispatch but before done; after checkpoint but before acknowledgement | Replay restores projections/checkpoints without duplicates; seq/cursor agree; gaps are not hidden | Real restarts, inbox/receipt rows, messages and refs before/after |
| A22 / local | File changed, result not saved; restart; ordinary continuation attempt; explicit acknowledgement | Unknown blocks the session; no automatic retry; arguments/facts displayed; acknowledgement preserves unknown and call/result pairing | File, model history, action records, T3 acknowledgement request, and execution counters |
| A23 / local | Actual tab disconnection; separately, controlling-channel EOF and Rust crash | Tab disconnection does not cancel; EOF triggers cancellation/cleanup; restart does not resume the interrupted action | Files, process/cgroup state, log, and UI after reconnection |
| A24 / local | Fast first change; rollback from the UI and a direct server command | Baseline before writing, correct first diff; rollback unavailable/rejected before changes | Baseline/exec order, diff, unchanged files/messages/refs on rollback attempt |
| A25 / local | Context does not fit; history contains tool output and unknown outcomes | Explicit stop before the request, no hidden compression; context built from harness records with correct relationships | Serialized request without secrets, comparison with history, no network request on overflow |
| A26 / local | PKCE/state, refresh rotation, fragmented UTF-8/SSE, output items, incomplete/interrupted stream, MIME, 401 | Invalid state rejected; tokens refreshed according to contract; incompleteness not hidden; required items preserved | HTTP/SSE fixtures and result classification without credentials |
| A27 / live | Sign-in and refresh, real tool roundtrip in A01, local stream cancellation | Standalone client works with the selected account/model; local cancellation is not presented as server-side cancellation | Anonymized report with model, timing, statuses, and call relationships |
| A28 / local | Sign-in and refresh with synthetic tokens; shutdown and a new process; sign-in/refresh failure | Tokens not written to persistent storage; new process requires sign-in while history is retained; explicit failure; tools/logs receive no secrets | Inspect file writes, FDs, requests, and output; compare history; no reading of installed Codex credentials |

The A01 reference case concretizes the agreed line replacement. The fixture is created before the run; manual edits or substituted model responses after sending the request do not count. Changes to T3/harness service data are kept separate from changes to working files; their stores are outside the tool workspace.

Crash/cleanup checks require real files, durable records, and processes, not just UI text. Test clocks may accelerate deadline-calculation checks, but do not replace measuring actual process-tree cleanup. A missing account cannot be substituted with fixture success.

Criteria sources: [two levels](https://github.com/aivv73/clanker-lab/issues/38#issuecomment-5744195271), [reference file](https://github.com/aivv73/clanker-lab/issues/38#issuecomment-5744201485), [cancellation/Bend](https://github.com/aivv73/clanker-lab/issues/38#issuecomment-5744212588), [recovery](https://github.com/aivv73/clanker-lab/issues/38#issuecomment-5744218533). The other rows operationalize mandatory requirements of previously accepted contracts; they do not claim that implementation checks have already passed.

## 11. Reporting and readiness criteria

For each matrix row, the report records the environment/versions, input fixture, run command, expected and observed behavior, evidence links, and outcome: passed, implementation defect, external failure/unavailability, or not run. Reports exclude secrets.

A model refusal or unavailable service does not by itself prove a harness defect, but the run does not count as passed. A transport interruption must not be presented as a model refusal, nor a success message as proof of a file change. A skipped mandatory check does not become a success.

**Specification readiness:** all identified decisions are agreed, criteria are unambiguous, and the user has accepted the document; decision references and limitations are preserved. Final acceptance was received on 2026-09-19; the criteria issue and decision map are closed with a reference to this document.

**MVP implementation readiness:** mandatory local and live checks pass against a pinned implementation snapshot, and evidence is available. Prototype results do not replace repeated product verification. Implementation beyond the agreed scope is not required.

## 12. Outside the MVP

- Product implementation/release within this planning map.
- MCP, multiple agents, hot plugin loading, dynamic libraries/WASM; pause/restart/reload as plugin-contract operations.
- Multiple providers, a multi-user service, parallel active turns or tools.
- PTY, tool networking, privilege elevation, and sandbox expansion during a turn.
- Persistent OAuth credential storage and retaining sign-in across runtime restarts.
- Automatic compaction, silent history truncation, and history/file rollback through T3.
- A supported Codex fork, GPU acceleration of Bend as a goal, or a proof of safety for the entire harness.
- WebSocket on the T3–Rust boundary: it uses JSONL. The standard browser–T3 WebSocket remains part of T3 and is used by the checks.

Desktop/mobile and remote were not tested by the prototype; this limits the evidence and does not claim that those surfaces work. Acceptance here defines a local T3 scenario; expanding the tested surface requires a separate decision.
