# Issue #44 — first conversation validation

Date: 2026-09-19. Result: **the local fixture slice for #44 passed**.
This is a new run of the Psi implementation, not a carryover of prototype results.

[Machine-readable report with versions and hashes](evidence/issue-44.json).
At the time of validation, the source files had not yet been committed:
`source_revision` identifies the base commit; `implementation_sha256` records the
validated files of the current implementation.

## Environment

| Component | Validated version |
| --- | --- |
| T3 | `dfbb11bdd7c3f1a5575cb55d3e3abb12be025727` + `t3/t3.patch` and the Psi overlay |
| Node | `24.13.1` (satisfies upstream `^24.13.1`) |
| Rust | rustc `1.97.1 (8bab26f4f 2026-07-14)`, Cargo `1.97.1 (c980f4866 2026-06-30)` |
| Browser | Google Chrome `152.0.7977.82`, separate headless profile |
| Playwright Core | `1.60.0`, npm lockfile in `t3/` |
| OS | Linux `7.2.2-1-cachyos`, x86_64 |

T3 used an already installed local dependency set from the same pin; workspace links
were redirected to a separate test checkout. The report records the SHA-256 hash of
the unchanged `pnpm-lock.yaml`. A clean installation of all upstream dependencies
from scratch was not tested in this run. Rust was built with `--locked`. The initial
diagnostic UI run used Node 26.8.1; the final automated run used Node 24.13.1.

## Checks

| Check | Result |
| --- | --- |
| `cargo fmt --check`, `cargo check --locked`, `cargo test --locked` from `psi/` | Passed, 6 tests |
| `tsc -p tsconfig.psi.json --pretty false` in the prepared T3 checkout | Passed: strict checking of the bridge, driver, adapter tests, and their imports |
| `node --test apps/server/src/psi/adapter.test.ts` with `PSI_BINARY` | Passed, 5 tests on Node 24.13.1 |
| `node --test t3/e2e.test.mjs` with a separate T3 instance | Passed, 1 end-to-end scenario, approximately 59 seconds |
| Python launcher/preparation | Compilation and actual execution passed |

The Rust tests cover dependency ordering, repeated close, partial-start cleanup,
invalid graph rejection, a real JSONL process, Unicode/newlines, incompatible
handshakes, repeated/conflicting IDs, independent sessions, and EOF. Adapter tests
also cover spawn failure, process shutdown, incompatible server versions/capabilities,
and preservation of the originating command's identity.

In the end-to-end scenario, the normal composer in the real T3 application sent
`Hello Psi issue 44` over the standard WebSocket. Exactly one response appeared in
the conversation: `Psi fixture: Hello Psi issue 44`. The test matched the submitted
`commandId`, the persisted `thread.turn-start-requested` event, and
`psi-turn:<commandId>` in the assistant-message projection. T3 had one standalone
Psi child process. Navigating the tab to `about:blank` preserved the runtime;
stopping the server triggered EOF, plugin shutdown, and process reaping. There
were no page errors.

The automated test saves a screenshot, `report.json`, and a log with the pairing URL
removed in the temporary directory whose path it prints. The last run of this session
used `/tmp/psi-t3-uizyos3k/`. These temporary files are not required to rerun the test.

Validation exposed and fixed cleanup of the Vite child server in the test launcher;
the final scenario passed again after the fix. Normal T3 shutdown first closes the
controlled Rust process; the launcher's own child process groups are used to clean
up remaining test-application processes.

## Not tested or claimed

The full T3 production bundle build, repository-wide T3 tests/typecheck,
desktop/mobile, remote/relay, a live account, and the full MVP A01–A28 were not tested.
This slice has no real tools, Bend, sandbox/cgroup policy, cancellation of long-running
work, durable inbox/replay, or model-history recovery. Acceptance of the full MVP
remains separate. No `paclang` changes were required, and its checks were not run
for this slice.
