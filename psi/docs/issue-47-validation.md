# Issue #47: independent sign-in and streamed model answers

Scope: [#47](https://github.com/aivv73/clanker-lab/issues/47), implementing the
A26 local model boundary and A28 authentication portion from
[#43](https://github.com/aivv73/clanker-lab/issues/43). This is not acceptance of the
entire MVP or the mandatory live tool roundtrip.

## Implementation

`CodexModelPlugin` implements the shared model service without the installed Codex
client, its credential files, or IPC. OAuth uses 256-bit random state and PKCE S256,
a bounded loopback callback, code exchange, and rotating refresh. The model service
alone owns access/refresh tokens. A failed/cancelled rotation discards the previous
tokens; a 401 clears authentication and does not retry the model request. Sign-in
and refresh failures use distinct fixed codes; HTTP error bodies and library errors
are not logged or returned.

The model boundary accepts exact JSON input items, returns lossless output items
and response identity, and emits text, reasoning, sign-in, and completed output
items. Output-item events remain available even if a later event fails. The Rust
controller uses successful exact items for follow-up requests; it does not reconstruct
context from T3 text. Durable history and context budgets remain separate slices.

The SSE decoder supports arbitrary byte fragmentation, UTF-8, BOM, CR/LF/CRLF,
comments and multiline data. Explicit completion is required; EOF, `[DONE]` alone,
incomplete status, explicit incompatible MIME, malformed data, inconsistent output, and
broken transport are failures. Output items preserve encrypted reasoning and exact
function-call IDs/arguments. No tool is advertised or executed in this slice.

T3 shows a separate completed sign-in-link item and then streams the answer. This
keeps the link actionable even with whole-turn buffering. The launcher selects token
streaming. T3's ordinary interrupt command cancels and joins local work, preserves
the session, and reports that server processing/billing may continue. An optional
turn identity prevents an old interrupt from targeting a newer turn. Controller EOF
also drops the local stream/callback and joins owned cooperative work.

## Reproducible checks

Run commands are in [README](../README.md#account-free-validation).
Machine-readable evidence is in [issue-47.json](evidence/issue-47.json).

| Check | Expected and observed result | Outcome |
| --- | --- | --- |
| Rust format/check/tests | Format and compile pass; 16 tests pass, including prior lifecycle/protocol checks | Passed |
| T3 TypeScript | Pinned checkout with overlay passes `tsc -p tsconfig.psi.json`; Effect suggestions remain non-errors | Passed |
| Existing T3 adapter suite | 15 tests pass with real Rust processes, both loops, identity replay, busy, and cleanup | Passed |
| A26 OAuth fixtures | Peer validates PKCE verifier/challenge and one-use code; wrong/duplicate state never exchanges; sign-in/refresh denial is explicit | Passed |
| A26 refresh/401 | Three sequential process requests across restart rotate tokens; expired tokens use the latest refresh; 401 never auto-replays and the next explicit request signs in | Passed |
| A26 streaming | Every UTF-8/CRLF split plus actual fragmented HTTP, CR-only, multiline SSE, item fallback, reasoning and call identity preserved; all failure scenarios return distinct failures | Passed |
| A28 authentication | Real new process requires sign-in; failed rotation reauthenticates; synthetic home unchanged; inotify sees no installed-credential directory access; stdout/stderr contain no synthetic tokens | Passed |
| T3 OAuth answer | Normal composer → ordinary WebSocket → Psi driver → Rust → local OAuth/HTTP/SSE; browser follows displayed link; answer and reasoning appear, answer is persisted | Passed |
| T3 local cancellation | Visible delta arrives while server holds completion; ordinary Stop generation interrupts; persisted text says local-only cancellation; runtime stays alive until controller shutdown and is then reaped | Passed |
| Real account / A27 | Real sign-in, refresh and integrated live tool roundtrip | Not run; belongs to live acceptance |

Fixtures use synthetic tokens and no account. Rust tests call Python 3's standard
library HTTP server. Model tests use HTTP at the model boundary, not a plugin-specific
JSONL test interface. The integrated scenarios use the real web application,
standard WebSocket, T3 driver and standalone Rust process. The held fixture proves
that a displayed answer delta does not depend on successful stream completion.

The credential check observes a synthetic `.codex` directory with Linux inotify,
compares home file bytes before/after, checks sign-in after a real restart, and checks
protocol/stderr for synthetic tokens. It is not a whole-filesystem syscall trace,
a proof of memory erasure, or a tool sandbox test. Tools do not exist in this slice.
T3 history is independent of authentication; durable Psi context recovery remains
unimplemented and is not claimed by these checks.

## Environment and limits

T3 `dfbb11bdd7c3f1a5575cb55d3e3abb12be025727`, Node 24.13.1, pnpm 11.10.0,
Playwright Core 1.60.0, Chrome 152.0.7977.82, Rust/Cargo 1.97.1,
Linux 7.2.2-1-cachyos x86_64. The disposable T3 checkout was freshly installed with
its frozen pnpm lockfile. Installation ran under ambient Node 26.8.1 and printed an
engine warning; actual type/adapter/browser checks used Node 24.13.1. The binary's
Cargo dependencies are pinned by `psi/Cargo.lock`. Evidence records source base and
hashes of the working implementation, not a claim that the base revision contains
these changes. Bend and cgroup/seccomp are not involved in this model-only slice.

The first local HTTP run exposed inherited proxy configuration in the test browser;
fixture clients now bypass proxies. The first held browser scenario timed out
because T3's default paragraph mode buffered the final short paragraph. The launcher
now selects token mode and sign-in has an independently completed item, with final
browser validation covering the revised implementation. No failed attempt is counted
as a pass.

Live endpoints/client parameters follow the issue's pinned
[OAuth prototype](https://github.com/aivv73/clanker-lab/tree/62144a3/harness/prototypes/codex-auth),
whose protocol reference is Pi `36b60d2e8985899743c4cf5bd5f8929832a3f05d`.
[Official authentication documentation](https://learn.chatgpt.com/docs/auth) was
checked during implementation; it does not establish compatibility of this
independent client's wire protocol. The real-account launcher path is documented,
but account/model availability and live backend compatibility are unverified here.

The initial implementation rejected missing MIME despite the pinned prototype's
known tolerance. The MIME correction below restores that compatibility while
continuing to require SSE framing and explicit completion. No product boundary
change is intended. Memory-only auth does not guarantee absence of swap or erasure.
Remote processing/billing may continue after local cancellation. Safe termination of
arbitrary hung in-process plugins and external tool children is outside these checks.

## Sign-in proxy correction

A subsequent user run completed browser sign-in but recorded `sign_in_failed` in
T3. Investigation found that the client also disabled proxies for live traffic.
A credential-free GET probe of the token endpoint returned HTTP 403 directly and
HTTP 405 through the configured proxy (GET is not the token exchange method).
Rust bypassed the available working proxy; browser success did not exercise
Rust's token exchange. The original local
fixture tests did not catch this live-network configuration defect.

Live requests now honor standard proxy environment settings; only explicit local
fixtures bypass them. A subprocess-isolated regression test uses two independent
loopback listeners to verify that the live client reaches the configured proxy and
the fixture client reaches its origin directly. Format, check, all 17 Rust tests,
and the binary build pass after the correction. Neither branch contacts an external
service, even if the regression returns. The proxy environment is not logged.

The original browser reports retain their original tested hashes; the
machine-readable evidence records the correction separately. Real-account sign-in
with the corrected client still needs confirmation. Restart the running Psi/T3
launcher to load the rebuilt binary before retrying sign-in.

## Missing-MIME compatibility correction

After the proxy correction, the user reported `model_mime`, which means token
exchange succeeded and the model HTTP response was successful but failed MIME
validation. The old error did not distinguish absent from incompatible metadata.
The pinned working prototype explicitly tolerates missing Content-Type. That known
compatibility case is now handled by this client too: parsing and explicit completed
response validation remain mandatory. Explicit incompatible Content-Type still fails,
with credential-free classes `model_mime_json`, `model_mime_html`, or
`model_mime_other` to distinguish this from missing metadata.

HTTP fixtures now verify a successful headerless SSE response and reject headerless
unfinished SSE, JSON, and HTML. They also reject explicit JSON and HTML MIME. Format/check, all 17 Rust tests,
and the rebuilt binary pass. The
original browser evidence retains its original snapshot; this correction is recorded
separately in the evidence JSON. A live retry is still needed to confirm the user's
actual response is the missing-header case; no account credentials were inspected or
reused to investigate it.
