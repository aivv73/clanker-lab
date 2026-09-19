# Libalpm package-origin evaluation

Issue [#9](https://github.com/aivv73/clanker-lab/issues/9) asks whether Paclang can treat installed-database metadata as authoritative package-origin evidence.

## Decision

Paclang will not add libalpm as a runtime dependency or package-origin source. No released upstream libalpm version exposes installed-database metadata. The available API is a CachyOS fork extension, so a binary that links to it does not run against the corresponding upstream libalpm ABI.

Paclang continues to record package origin as unknown. It must not derive historical package origin from a package's presence in a current sync database.

## API and version evidence

CachyOS added `const char *alpm_pkg_get_installed_db(alpm_pkg_t *pkg)` in commit [`a8a7114`](https://github.com/CachyOS/pacman/commit/a8a7114a6ec9f9a1cab2e4b91c649142f084a74f) on 2022-10-22. The same commit added an `installed_db` field to `alpm_pkg_t`, read and wrote `%INSTALLED_DB%` in the local database, and copied the source sync-database name into a package during a libalpm transaction.

The extension is present in the CachyOS fork's maintenance branches for [libalpm 13](https://github.com/CachyOS/pacman/blob/c95951992afdc717eb4f4aa51c3082ff576e7003/lib/libalpm/alpm.h), [libalpm 14](https://github.com/CachyOS/pacman/blob/daf180a7d5a7fe90cd47c9ac1432a16162d899f5/lib/libalpm/alpm.h), [libalpm 15](https://github.com/CachyOS/pacman/blob/ddf769f121d3f173f3b166cc964646e4d1ac2e57/lib/libalpm/alpm.h), and [libalpm 16](https://github.com/CachyOS/pacman/blob/4056cd687f6379e61e7decb9b66e9b57cb3949a9/lib/libalpm/alpm.h). These links pin the branch heads inspected on 2026-09-03. This is a fork compatibility range, not an upstream API guarantee.

The upstream [libalpm 16 header from pacman 7.1.0](https://gitlab.archlinux.org/pacman/pacman/-/blob/v7.1.0/lib/libalpm/alpm.h) does not declare `alpm_pkg_get_installed_db`. It declares `alpm_pkg_get_origin`, but that accessor reports whether the in-memory package object came from a file, the local database, or a sync database. It does not report the repository that installed a local package. The released [alpm-db-desc version 2 specification](https://github.com/archlinux/alpm/blob/alpm-db/0.2.2/alpm-db/resources/specification/alpm-db-descv2.5.md) does not define `%INSTALLED_DB%` either. Therefore, the set of supported upstream libalpm versions that expose installed-database metadata is empty as of 2026-09-03. The living [`libalpm_packages(3)` reference](https://man.archlinux.org/man/libalpm_packages.3.en) can be checked for later upstream additions.

## What the fork API can prove

A non-null `alpm_pkg_get_installed_db` result is authoritative evidence that the CachyOS-patched package manager recorded a sync-database name during that package transaction. Paclang could map that value to `Repository(name)` after validating the name.

A null result has several meanings. The local entry can have no recorded value because the package came from a file or predates `%INSTALLED_DB%`. The accessor can also return null when libalpm fails to load the local `desc` metadata. The accessor does not give its caller a reliable way to distinguish these cases. It cannot distinguish a foreign package from missing historical data or failed validation. A current sync-database lookup does not repair that ambiguity because repositories change after installation.

The evidence source and the package origin are separate facts. If Paclang revisits this integration, the domain model should keep them separate:

```text
OriginObservation = Unavailable | Available(RecordedRepository(name) | NotRecorded)
PackageOrigin = Repository(name) | Foreign | Unknown
read_origin() -> Result<OriginObservation, OriginReadError>
```

`Foreign` requires affirmative historical evidence from an authoritative source. Neither a null fork-API result nor absence from current sync databases qualifies. An unreadable or malformed local entry is `OriginReadError`, not `Unknown`. Paclang's refresh boundary must reject that input before publishing a snapshot.

## Required compatibility fixtures

Any future adapter must use recorded local-database fixtures and cover the following cases:

| Fixture | API result | Required outcome | Reason |
| --- | --- | --- | --- |
| `%INSTALLED_DB%` contains `extra` | `"extra"` | `Repository("extra")` | The local entry records the transaction source. |
| Package installed from a file | null | `Unknown` | The API provides no affirmative foreign marker. |
| Local entry predates `%INSTALLED_DB%` | null | `Unknown` | The API cannot distinguish this case from a file install. |
| Local `desc` is unreadable or malformed | null can mask the failure | `OriginReadError` before constructing `PackageOrigin` | Refresh must fail validation and preserve the previous snapshot. The fork API cannot guarantee this result. |
| Upstream or older libalpm lacks the symbol | unavailable | `Unknown` | The evidence source is unavailable. |
| Package name exists only outside current sync databases | not consulted | unchanged | Current repository membership is not historical evidence. |

The selected API cannot produce `Foreign`, and its null result cannot reliably separate missing evidence from a metadata-read failure. The malformed fixture therefore disqualifies this API from Paclang's complete-population validation boundary. A future source must preserve read failure as an error and must not encode a guess for `Foreign`.

## Dependency and migration cost

Direct linking would add a system libalpm development dependency and bind Paclang to libalpm's changing ABI. The fork reuses upstream SONAMEs while adding a symbol. A binary linked against the fork can fail at program startup on upstream libalpm with the same ABI version because the symbol is missing.

Optional runtime symbol loading would avoid that startup failure, but it would require handwritten or patched Rust FFI, capability detection, libalpm handle and configuration lifecycle code, and tests against both upstream and fork builds. Paclang would also need to review the distribution terms for binaries linked to libalpm, which pacman 7.1.0 publishes under GPL-2.0-or-later in its [`COPYING`](https://gitlab.archlinux.org/pacman/pacman/-/blob/v7.1.0/COPYING) file.

Persisting the richer model would require a new SQLite migration. The current `origin TEXT NOT NULL` column stores `unknown` and cannot record the evidence source separately. Compatibility tests would need empty, current, malformed, and newer-version databases, plus migration fixtures for old `unknown` rows. Old rows must remain `Unknown`; a migration cannot reconstruct evidence that was never stored.

These costs do not buy a portable contract. Paclang should reconsider the decision only if upstream libalpm or the standard ALPM local-database format adds stable installed-origin metadata with semantics that distinguish recorded repository origin from missing evidence.
