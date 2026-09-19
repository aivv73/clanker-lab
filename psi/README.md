# Psi

A standalone Rust agent runtime, separate from `paclang`. The first implemented
slice ([#44](https://github.com/aivv73/clanker-lab/issues/44)) lets you send an ordinary
message in the real T3 Web application and see `Psi fixture: <message>` in response.
The model is deterministic; no account or tools are required. The
[#45](https://github.com/aivv73/clanker-lab/issues/45) slice adds runtime-owned turn
admission, execution scopes, tracked plugin work, and startup loop selection.

This is **not the full MVP**: tools, Bend, Codex sign-in, durable Psi history,
recovery, process-tree cancellation, and sandboxing are not implemented yet.
The full contract is in the [specification](docs/mvp-spec.md) and [glossary](CONTEXT.md).

## Build and checks

From `psi/` (there is no shared Cargo workspace):

```sh
cargo build --locked
cargo fmt --check
cargo check --locked
cargo test --locked
```

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
  `SingleResponsePlugin`; the model belongs to `FixtureModelPlugin`. Both expose
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
  1 MiB; the bridge times out a stalled response after 10 seconds and allows 2 seconds
  for process shutdown before forcing termination if needed. These limits apply to
  the fixture and do not establish cancellation guarantees for future tools.
- Repeated IDs are deduplicated **only in the current process's memory**. There is no
  durable acknowledgement, inbox, or replay. After a restart, T3 conversation history
  remains in T3, but it is not recovered model history. Do not use this slice for
  actions with side effects.
- Model support, attachments, rollback, and auxiliary generation are not silently
  broadened: only `fixture` is supported; attachments and unsupported operations are
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
Bend admission, process-tree cancellation, unhealthy-runtime recovery, and durable
records remain subsequent slices. Killing the process is not proof of external
resource cleanup. No conflicts with the existing MVP specification were identified.

[Issue #45 validation](docs/issue-45-validation.md).
