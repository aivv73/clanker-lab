# Psi JSONL v1 — conversations and ownership

One UTF-8 JSON object per line. Each command and reply carries `version: 1`.
Stdout is exclusively protocol traffic; stderr is diagnostics. The controller
owns stdin. EOF closes admission, joins active cooperative work, reaps policy children, closes plugins and exits; there are no external tool children.
Maximum frame size is 1 MiB. Unknown operations/fields and invalid JSON are errors.

First command:

```json
{"op":"hello","version":1,"capabilities":["sessions","origin-command-id","model-stream","durable-events"]}
```

Reply includes `version`, `runtime_id`, `capabilities`, the configured `model`, and `pid`.
`model-stream` is required: an older peer that does not request it is rejected
before work. `fixture-model` is only advertised in fixture mode; `memory-oauth`
identifies the memory-only authentication contract.
A requested unsupported capability or version returns an error and closes the
connection before accepting a turn. The adapter independently verifies all
required capabilities and version; no compatibility fallback exists.

| Identity | Meaning |
| --- | --- |
| `runtime_id` | Fresh UUID for one process lifetime; validates replies against the connected runtime. |
| T3 `providerInstanceId` | Owner of the shared process; included in derived event identities. |
| `session_id` | T3 thread ID, separate conversation scope within the shared runtime. |
| `request_id` | Identity of the originating command, not prompt text or a delivery attempt. Turn requests use the persisted T3 `commandId` (persisted `eventId` fallback). |
| `turn_id` | Work scope for one turn, derived as `psi-turn:<originating command ID>`. Distinct field and role from the request identity. |
| `invocation_id` | Reserved for a future tool execution inside a turn. This slice invokes no tools and fabricates no invocation IDs. Model responses are not tool invocations. |

Lifecycle requests have independent `psi-lifecycle:<UUID>` identities. They are
generated for lifecycle operations and stored by Psi; turn retries use the original
T3 command ID, never a newly generated ID.
Turn command identities already survive the real T3 command/schema/reactor/adapter
path. Derived event IDs include instance, session, originating command and ordinal.

After hello:

```json
{"op":"open","version":1,"request_id":"open-command-1","session_id":"thread-1"}
{"op":"turn","version":1,"request_id":"saved-t3-command-1","session_id":"thread-1","turn_id":"psi-turn:saved-t3-command-1","input":"hello"}
{"op":"close","version":1,"request_id":"close-command-1","session_id":"thread-1"}
{"op":"shutdown","version":1,"request_id":"shutdown-command-1"}
```

Open returns `status: ready`, close `closed`, shutdown `shutdown`. A turn returns
`status: completed`, the session/turn/request/runtime IDs and
`output: "Psi fixture: hello"`. Before execution it emits `status: accepted` with
the same IDs and a session sequence number, after committing command and acceptance.
The terminal result is separate. T3 stores these frames in its durable inbox.
One terminal response corresponds to each request, correlated by `request_id`; replies may
arrive out of order. A busy response can precede the held turn's result. A replay operation delivers saved frames with their original request IDs and
`replay: true`; other unsolicited frames are rejected. The initial hello has no request ID.

A repeated request with the same payload returns the prior reply plus
`duplicate: true`, without another model call or duplicate visible events.
Different content with that ID yields `request_id_conflict`; reuse of a turn ID
under a different command yields `turn_id_conflict`. Error replies contain `error`
and runtime/version, plus the originating request ID for parsed commands. Additional errors include `handshake_required`,
`invalid_command`, `invalid_request_id`, `invalid_session_id`, `session_not_open`,
`incompatible_version`, and `incompatible_capabilities`.

Sessions, command receipts, exact model history, and results are persisted in SQLite. Closing a session first
joins its active work, returns that turn with `status: cancelled` and
`error: session_closed`, then returns the close acknowledgement. Other sessions and
shared plugins remain available. Repeated close is safe. An explicit open may reopen
a closed conversation; old turn IDs and request receipts remain reserved across restart.
Closing a session does not erase T3's durable conversation.

The runtime owns the active-turn slot. A second turn returns `busy` immediately and
is neither remembered as accepted nor queued; a later explicit retry may succeed.
The same rule holds across independent Psi processes via a fixed per-user kernel
lock. The slot remains held until tracked turn work has been joined. A repeated
request still in flight returns its original `accepted` frame with `duplicate: true`;
different content with that ID returns `request_id_conflict`. After completion,
retry also returns the saved terminal result without repeating execution. Model errors produce `status: failed` with a fixed, credential-free error code.

`shutdown` cancels and joins active cooperative work before acknowledging; EOF does
the same without a reply. Plugin teardown follows in reverse dependency order.
Startup failure never reaches hello readiness. Stderr records plugin start/close,
registration/task counts, turn admission, and cleanup, but not model input.

`Runtime → Session → Turn → Invocation` ownership is represented separately from
plugin ownership. No tool invocation is fabricated for a model call. The normal
loops run the selected model; the policy fixture runs its harmless counter action.
Arbitrary tool execution, tool process-tree cleanup, and tool resource quotas
remain subsequent slices. Forced safe termination of
arbitrary in-process plugin code is not promised.

## Streaming and authentication

Before its terminal reply, a turn can emit any number of correlated frames:

```json
{"version":1,"runtime_id":"runtime","request_id":"command","session_id":"thread","turn_id":"turn","status":"streaming","event":{"kind":"text","delta":"Hello"}}
```

`event.kind` is `text`, `reasoning` (both carry `delta`), `sign_in` (carries a
browser authorization `url`, never access/refresh tokens or an authorization code),
or `output_item` (carries the output `index` and lossless provider `item`, including
items completed before a later stream failure).
The bridge keeps the request pending until its terminal reply and serializes event
delivery to T3. Replay of a terminal receipt emits no new streaming frames.

Completed replies also carry lossless `provider_output` and optional `response_id`.
The Rust controller appends the exact user input and those output items to its
durable session history in the same transaction as its terminal result. T3 rendering is not the source for model context.
Failed or cancelled streams do not append a fabricated assistant completion.

```json
{"op":"cancel","version":1,"request_id":"cancel-command","session_id":"thread-1"}
```

Cancel joins the active turn's owned future before returning its terminal
`status: cancelled`, `error: local_cancelled`, and
`cancellation_scope: local_only_server_processing_and_billing_unknown`. It then
acknowledges the cancel command with `status: cancel_requested`. The session remains
open. An optional `turn_id` scopes cancellation to that turn, so a delayed
interrupt cannot cancel a newer turn. T3 maps its ordinary interrupt command to this operation. Close/shutdown
cancellation has the same local-only scope. No remote processing/billing claim is made.

Representative failure codes: `sign_in_state_mismatch`, `sign_in_denied`,
`sign_in_failed`, `sign_in_timeout`, `refresh_failed`, `model_unauthorized`,
`model_mime_json`, `model_mime_html`, `model_mime_other`, `model_incomplete`, `model_unfinished_stream`, `model_transport`,
`model_invalid_utf8`, `model_invalid_json`, `model_item_mismatch`,
`model_text_mismatch`, and `model_stream_limit`. Neither provider error bodies nor
credential-bearing requests are included. Authentication has no persistent receipt
or storage operation: only the model service owns tokens.

Absent Content-Type is allowed only with valid SSE and explicit completion, matching
the pinned Codex reference. Explicit incompatible MIME is rejected with one of the
fixed `model_mime_*` classes; raw response headers/bodies are never included.

## Durable delivery

Hello also lists saved `sessions`; the bridge reconnects and drains them on T3
restart. The updated bridge requires `durable-events`. Every accepted, streaming,
and terminal turn frame has a monotonically increasing per-session `seq`, committed
before emission. Replay preserves those sequence numbers and payloads; only the
connection's `runtime_id` and delivery flags change.

```json
{"op":"replay","version":1,"request_id":"replay-1","session_id":"thread-1","after":0}
{"op":"history","version":1,"request_id":"history-1","session_id":"thread-1"}
```

Replay emits saved frames after the cursor, then `status: replayed` correlated to
its own request. A cursor beyond the stored log or a log gap fails explicitly.
History returns `status: history`, exact provider `history`, and saved terminal
`turns`. Model requests use only Psi history; UI snapshots are not model input.
`open` accepts `workspace`, whose canonical path must not contain the state store.
The adapter supplies it. Standalone fixture clients may omit it; no tools exist.

The T3 inbox scopes sequences by instance/session and rejects gaps or conflicting
payloads. It commits the frame, then advances the cursor. Projection completion is
a separate boundary after all deterministic derived commands have been accepted
by the real T3 engine. Their IDs encode instance/session/sequence/ordinal; replay
uses its command receipts, including after dispatch but before inbox completion.

Rust stores action intent before an external handoff and result after it through
explicit store APIs. An unfinished action becomes `unknown` on restart; it cannot
be overwritten with a fabricated result. Accepted unfinished turns receive
`interrupted_outcome_unknown`; their session rejects new turns with
`session_outcome_unknown`. Terminal-processing pending/done records are available
for later checkpoint integration. No external effect is made atomic by SQLite.


## Checked policy fixture (#46)

`--agent-loop policy-fixture` selects one harmless `fixture_counter` action per
ordinary turn. It uses the same turn/cancel/close protocol, driver, durable inbox,
and UI. No special action test endpoint is added. The policy service is a plugin;
Bend supplies admission decisions and Rust correlates and consumes permits.

Its terminal frame adds `policy` with `epoch`, `turn`, `state`, `closed`,
`handoffs`, `runtime_counter`, and `transitions`. Each transition records its B1
request identity, immutable action/arguments, turn and epoch, event, and result.
This B1 request identity belongs to a single policy process call; the outer JSONL
request identity still refers to the persisted originating T3 command. The
harmless counter is process-local. Replaying an old terminal returns its stored
counter without incrementing the new runtime's counter.

Cancellation sets `cancellation_scope: local_policy`; T3 shows that admission is
closed. Text-only model cancellation retains its existing remote-processing
qualification. Failed policy decisions terminate with explicit `policy_*` errors
and zero new handoffs. Missing bundles, failed processes, malformed/mismatched
responses, and timeouts never grant fallback authorization. Cleanup failure adds
`cleanup_error`, fails the turn, and rejects new turns with `runtime_unhealthy`.

The [policy validation document](../docs/issue-46-validation.md) describes the
250 ms B1 deadline, proof boundary, build provenance, startup settings, and tests.
