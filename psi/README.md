# Psi

A standalone Rust agent runtime, separate from `paclang`. The first implemented
slice ([#44](https://github.com/aivv73/clanker-lab/issues/44)) lets you send an ordinary
message in the real T3 Web application and see `Psi fixture: <message>` in response.
The default model is deterministic; no account or tools are required. The
[#45](https://github.com/aivv73/clanker-lab/issues/45) slice adds runtime-owned turn
admission, execution scopes, tracked plugin work, and startup loop selection.

The [#47](https://github.com/aivv73/clanker-lab/issues/47) slice adds independent
ChatGPT OAuth/PKCE, memory-only refresh, and streamed model responses through T3.

The [#48](https://github.com/aivv73/clanker-lab/issues/48) slice adds SQLite model
history, durable command acceptance, sequenced replay, and a T3 inbox with
idempotent conversation projection.

The [#46](https://github.com/aivv73/clanker-lab/issues/46) slice adds a checked Bend
policy plugin and a deterministic, harmless action through the real T3 path.
See [the proof, runtime contract, and validation](docs/issue-46-validation.md).

This is **not the full MVP**: tools, checkpoint/unknown-outcome recovery,
process-tree cancellation, and sandboxing are not implemented yet.
The full contract is in the [specification](docs/mvp-spec.md) and [glossary](CONTEXT.md).

## Build and checks

From `psi/` (there is no shared Cargo workspace):

```sh
python3 policy/build.py
cargo build --locked
cargo fmt --check
cargo check --locked
cargo test --locked
```

The policy build requires Git, Python 3, Bun and Clang. It fetches the pinned
Bend source into an ignored build directory, checks the proof and its negative
mutation, and builds the CPU executable required by policy tests. Normal text-only
turns can still run without this bundle; action admission then fails closed.

[Protocol and identity roles](t3/PROTOCOL.md).

The executable is `target/debug/psi`. It reads JSONL from stdin, writes only
protocol traffic to stdout, and sends diagnostics to stderr. EOF closes the runtime
and its plugins.

## Run with the real T3 application

The supported T3 base revision is
`dfbb11bdd7c3f1a5575cb55d3e3abb12be025727`. Use Node **24.13.1** and pnpm **11.10.0**
(the package-manager version specified by T3). Python 3, Git, Rust, and T3's build
dependencies are required. Use a separate T3 checkout; the overlay is not intended
for a live installation.

From `psi/`:

```sh
git clone https://github.com/pingdotgg/t3code t3/.checkout
git -C t3/.checkout checkout --detach dfbb11bdd7c3f1a5575cb55d3e3abb12be025727
(cd t3/.checkout && pnpm install --frozen-lockfile)
python3 t3/run.py t3/.checkout
```

The launcher verifies the pin, applies `t3/t3.patch`, copies the driver and bridge,
builds Rust, and creates a new temporary T3 home with a Git workspace. Only the Psi
provider is enabled. The server listens on `127.0.0.1:8844`; the web application uses
`127.0.0.1:8845`. Open the printed pairing link, changing the port to **8845**.
Complete onboarding without importing projects, create a conversation in the test
workspace, and send a message with **Psi fixture** selected. Ctrl-C stops the
launched processes. Existing T3 data is not used; the temporary directory remains
available for inspection.

Other T3 versions are rejected. Updating the pin requires reviewing the patch and
running the end-to-end scenario again. Installing all upstream dependencies may be
substantial; it creates no dependencies between Psi and other experiments in this
repository.

## Automated integration check

Prepare T3 using the commands above. Then, from `psi/`:

```sh
python3 t3/prepare.py t3/.checkout
(cd t3/.checkout && node_modules/.bin/tsc -p tsconfig.psi.json --pretty false)
PSI_BINARY="$PWD/target/debug/psi" node --test t3/.checkout/apps/server/src/psi/adapter.test.ts
npm ci --prefix t3 --ignore-scripts
PSI_T3_CHECKOUT="$PWD/t3/.checkout" PSI_CHROMIUM=/absolute/path/to/chrome \
  node --test t3/e2e.test.mjs
```

The E2E test uses an installed Chromium/Chrome through Playwright Core 1.60.0, a
fresh browser profile, the real T3 web application/server, and Rust. Ports 8854–8855
must be free by default; `PSI_TEST_PORT` changes the first port. The test sends a
message through the normal composer and checks the visible response, WebSocket
command, its persisted ID, and the corresponding turn ID in the T3 projection.
It then checks the single Psi child process, tab disconnection, and process exit
and reaping after server shutdown. It does not substitute responses or projections.
The printed temporary directory retains `report.json`, a screenshot, and a log with
the pairing token removed. The report includes versions, the pin, and SHA-256 hashes
of implementation files.

[Recorded validation results](docs/issue-44-validation.md).

## Implementation boundaries

- The core manages service contracts and plugin lifecycles: it validates the
  dependency graph before startup, starts providers first, and closes plugins in
  reverse order, including a partially started plugin. Orchestration belongs to
  `SingleResponsePlugin`; the model belongs to `FixtureModelPlugin` or
  `CodexModelPlugin`. Both expose
  replaceable trait contracts. `Context::spawner()` associates tracked tasks with
  the supplying plugin and a runtime/session/turn/invocation owner. Closing a scope
  aborts and joins its cooperative futures before releasing admission. Plugin close
  errors are reported after remaining reverse-order cleanup; no hot restart is offered.
- One process per ProviderInstance serves all of its sessions. Rust rejects
  overlapping turns with `busy`, without accepting or queuing them. A fixed per-user
  kernel lock also protects against independent adapters/processes (including other
  workspaces). Closing a session cleans its work but preserves other sessions and
  shared plugins. Repeated close is safe; an explicit open can reopen the conversation.
- Both sides validate the handshake before accepting work. Frames are limited to
  1 MiB; the bridge allows 10 seconds for lifecycle replies, 660 seconds for a turn
  (including sign-in), and 2 seconds
  for process shutdown before forcing termination if needed. These limits apply to
  the fixture and do not establish cancellation guarantees for future tools.
- Turn commands and acceptance are durable before model execution. Identical retries
  return the saved acceptance/result; changed content conflicts. Completed model
  history survives restart, independently of T3 text. Sequenced events enter a
  durable T3 inbox before its cursor advances; deterministic projection commands
  prevent duplicate messages after dispatch/restart. This does not promise atomic
  exactly-once external effects.
- Model support, attachments, rollback, and auxiliary generation are not silently
  broadened: `fixture` and the configured ChatGPT model are supported; attachments and unsupported operations are
  rejected. Full verification of disabled rollback and checkpoints belongs to later slices.

## Session ownership and lifecycle checks (#45)

Choose `--agent-loop single-response` (default) or `--agent-loop buffered-response`
when launching Rust. Both plugins implement `AgentLoop` and use the same model
service; the latter buffers its response before returning it. T3 provider configuration
uses `agentLoop`, and the test launcher accepts the same `--agent-loop` option.
No core or model changes are needed to select an implementation.

The admission lock is `/tmp/psi-active-turn-<effective uid>.lock`, opened without
following symlinks and with owner/mode validation. Do not delete it while Psi is
running: the kernel lock belongs to its open inode. There is deliberately no
per-adapter, per-workspace, or environment override. Admission fails closed if the
lock cannot be opened safely. The file may remain after shutdown; the kernel releases
the lock when its handle closes. This is coordination between trusted local runtimes,
not isolation from malicious code running as the same user.

`--fixture-gate /absolute/path` holds deterministic model work until that file exists.
`--startup-fault missing|cycle|duplicate|partial` enables explicit lifecycle fixtures.
T3 configuration exposes these as `fixtureGate` and `startupFault`; they are startup
settings, never prompt instructions. `run.py --hold-fixture` uses the printed scratch
root's `release-fixture` file. The partial-start fixture registers a service and owns
a tracked pending task whose resource drop is visible on stderr before plugin close.

After preparing T3 and building Rust, run the adapter tests above. Full browser
scenarios use the same `PSI_T3_CHECKOUT` and `PSI_CHROMIUM` environment variables:

```sh
PSI_TEST_LOOP=buffered-response node --test t3/e2e.test.mjs
PSI_TEST_LIFECYCLE=1 node --test t3/e2e.test.mjs
for fault in missing cycle duplicate partial; do
  PSI_TEST_STARTUP_FAULT="$fault" node --test t3/e2e.test.mjs
done
```

Run turn-producing test suites sequentially: they intentionally share the per-user
admission lock. The lifecycle browser scenario uses the ordinary composer in two
threads, observes busy, sends repeated standard `thread.session.stop` WebSocket
commands, and verifies a new response in the surviving conversation. Adapter tests
also exercise independent instances/processes and controller EOF during held work.
Core tests verify provider protection and separate invocation ownership, without
adding a plugin-specific test protocol.

Safe forced termination of arbitrary blocking or hung in-process plugin code is
**not guaranteed**. Cooperative future cleanup is verified; external tool children,
process-tree cancellation, and unhealthy-runtime recovery for arbitrary tools
remain subsequent slices. Bend policy-child cleanup is covered by #46. Killing the process is not proof of external
resource cleanup. No conflicts with the existing MVP specification were identified.

[Issue #45 validation](docs/issue-45-validation.md).

## ChatGPT sign-in and streamed answers (#47)

Use the same pinned T3 preparation, then choose a model available to your account:

```sh
python3 t3/run.py t3/.checkout --model gpt-5.6-luna
```

Send a normal message in T3. Psi displays a sign-in link in the response; open it
on the same machine, complete browser sign-in, and return to the conversation.
The runtime listens on `127.0.0.1:1455`, using the registered
`http://localhost:1455/auth/callback` redirect. An occupied callback port fails
explicitly. Sign-in expires after five minutes. No installed Codex process,
credential file, keychain, or API key is used. Model availability is account-specific;
the named model follows the pinned prototype, not a live availability guarantee.

T3 provider settings `model` and `oauthFixture` correspond to Rust's `--model` and
`--oauth-fixture` options. Only the chosen model is advertised. Authentication is
reported as unknown in the provider snapshot; the turn displays sign-in/failure
state. This slice initiates sign-in from the conversation, not T3's provider setup
wizard. Credentials never travel through T3. After a Rust restart, send a new
message and sign in again; existing T3 conversation records remain independent.
Psi's exact model history is recovered from its SQLite store, independently of T3 text.

The launcher selects T3 token streaming; other T3 streaming preferences remain
available. Sign-in is its own completed message item so that buffering cannot hide
the actionable link. The normal loop streams text and reasoning as received. The buffered loop forwards
sign-in immediately but buffers model deltas. Completed provider output items,
including encrypted reasoning, item IDs, call IDs and exact argument strings, are
preserved in `ModelResponse` and in subsequent model input. Completed
items also cross the model event boundary before any later stream failure; failed
streams do not append a fabricated successful response to session history. This slice
advertises no tools and does not execute tool calls. Context budget checks and integrated tool use remain later slices.

Access/refresh tokens live only in the Rust model service. Refresh replaces both
tokens; failed or cancelled rotation drops the old token. A 401 is explicit, clears
authentication, and is never transparently replayed. The next turn signs in again.
Error bodies and HTTP library errors are not sent to diagnostics or conversation
history. Memory-only storage does not promise erasure from RAM or protection from
swap. The live client honors standard proxy environment settings (`HTTPS_PROXY`,
`HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`, including lowercase variants). Redirects
remain disabled. Only explicit HTTP `127.0.0.1` fixtures bypass proxies. Endpoints
are fixed for live use; a fixture origin must be explicitly selected.

When Content-Type is present, the client requires `text/event-stream`
(case-insensitive, parameters allowed). It
handles UTF-8 split across chunks, LF/CRLF/CR lines, multiline SSE data, and preserved
output items. Missing Content-Type is tolerated, matching the pinned backend
reference; explicitly incompatible MIME is an error. Every stream must contain an
explicit completed response with consistent output; EOF, `[DONE]` alone, incomplete
responses, broken transport, malformed events, and provider failures never become
success. Responses are bounded to 512 KiB of SSE and five minutes; token responses
to 64 KiB and 30 seconds. These are current client limits, separate from later tool
limits. Absence of MIME metadata never substitutes for stream validation or completion.
Live backend compatibility has not yet been confirmed end to end.

Use T3's **Stop generation** to close the local request. The turn reports that
server processing and billing may continue. Cancellation also closes a pending
sign-in callback listener; session close and controller EOF clean up owned futures.
There is no claim of remote cancellation.

### Account-free validation

`cargo test --locked` requires Python 3 for loopback HTTP fixtures. It checks PKCE,
state/duplicate-state rejection, sign-in/refresh failure, refresh rotation, lossless
items, follow-up requests, SSE framing and error classification. Real process tests
check restart sign-in, cancellation, unchanged synthetic home files, inotify evidence
that installed credentials were not accessed, and absence of synthetic tokens in
protocol output and stderr. No account is needed.

After preparing T3, run these browser scenarios **sequentially**:

```sh
PSI_TEST_OAUTH=1 PSI_T3_CHECKOUT="$PWD/t3/.checkout" \
  PSI_CHROMIUM=/absolute/path/to/chrome node --test t3/e2e.test.mjs
PSI_TEST_OAUTH=hold PSI_T3_CHECKOUT="$PWD/t3/.checkout" \
  PSI_CHROMIUM=/absolute/path/to/chrome node --test t3/e2e.test.mjs
```

The first follows the actual link shown in T3 through synthetic OAuth and receives
an HTTP/SSE answer. The second holds completion after a visible delta, then clicks
**Stop generation**, verifying the ordinary interrupt WebSocket command and local
cancellation in persisted T3 conversation text. Both use the real Rust process.

For a manual fixture run, start `python3 tests/fixtures/oauth_server.py` and pass its
printed origin to `run.py --model fixture-codex --oauth-fixture <origin>`. The fixture
accepts synthetic sign-in without an account; never use it for a real account.

[Issue #47 validation and remaining live checks](docs/issue-47-validation.md).

## Durable conversation storage (#48)

The T3 integration stores its inbox under `<T3 base directory>/psi/inbox.sqlite`
and Psi history under `psi/<SHA-256 of provider instance ID>/history.sqlite`.
Both locations are outside the selected tool workspace. The standalone binary
accepts `--state-dir /absolute/path` (or `PSI_STATE_DIR`); the default is
`$XDG_STATE_HOME/psi/default`, falling back to `$HOME/.local/state/psi/default`.
Do not share one directory between simultaneous controllers; startup rejects it.
Closing a session keeps its saved history. Reopening restores it.

After an ambiguous crash, accepted unfinished work is marked with unknown outcome
and is never automatically executed again. That session blocks continuation;
explicit recovery acknowledgement belongs to #49. Access and refresh tokens are
not persisted, so model authentication is still required after restart.

Psi projects streamed conversation text directly from the durable inbox, using
T3 command receipts. Its durable path currently uses token streaming regardless
of the global buffering preference. The inbox's cursor records durable acceptance,
not UI rendering. Action intent/result and terminal-processing records are extension
points for tools and checkpoints; they do not establish atomic file effects.

[Protocol](t3/PROTOCOL.md) and [validation with crash tests](docs/issue-48-validation.md).
