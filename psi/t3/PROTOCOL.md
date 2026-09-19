# Psi JSONL v1 — first conversation

One UTF-8 JSON object per line. Each command and reply carries `version: 1`.
Stdout is exclusively protocol traffic; stderr is diagnostics. The controller
owns stdin. EOF closes plugins and exits; there are no external tool children.
Maximum frame size is 1 MiB. Unknown operations/fields and invalid JSON are errors.

First command:

```json
{"op":"hello","version":1,"capabilities":["fixture-model","sessions","origin-command-id"]}
```

Reply includes `version`, `runtime_id`, `capabilities`, `model: "fixture"`, `pid`.
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
currently ephemeral; future durable delivery must persist them at the origin.
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
`output: "Psi fixture: hello"`. This is a completed fixture result, **not a durable
acceptance receipt**. The adapter emits the normal T3 turn/item/content events.
One FIFO response corresponds to each request. No unsolicited frames are allowed.

A repeated request with the same payload returns the prior reply plus
`duplicate: true`, without another model call or duplicate visible events.
Different content with that ID yields `request_id_conflict`; reuse of a turn ID
under a different command yields `turn_id_conflict`. Error replies contain `error`
and runtime/version. Additional errors include `handshake_required`,
`invalid_command`, `invalid_request_id`, `invalid_session_id`, `session_not_open`,
`incompatible_version`, and `incompatible_capabilities`.

All sessions, receipts and results in Psi are in memory. Closing a session removes
its conversation; request receipts remain until process exit to prevent duplicate
execution in that runtime. The fixture model returns immediately and creates no
background work. The driver rejects overlapping sendTurn calls with `busy` rather
than queuing them. Long-running turn scheduling/cancellation, resource quotas,
durable receipt/event sequencing and replay are not established by this protocol
slice and require subsequent implementation before real tools are enabled.
