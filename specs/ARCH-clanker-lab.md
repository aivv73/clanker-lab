# ARCH-clanker-lab: Repository architecture

Clanker Lab contains the Paclang Rust package and project-level agent instructions.

The `paclang` executable owns command parsing and output-format selection. Its report model renders the same facts as terminal, TOON, JSON, and Markdown output. The storage boundary owns the SQLite analysis database under the XDG user data directory. Subprocess tests exercise the compiled executable with isolated user directories.

`paclang refresh --metadata-only` holds SQLite's single-writer transaction while it reads the installed package population from `pacman` and `expac`. The inventory boundary compares name-and-version fingerprints around metadata capture. A capture matching either boundary sample is coherent; a later package change can make that snapshot stale. Metadata matching neither sample is rejected as mixed. The storage boundary publishes and prunes snapshots in the same transaction. The inventory boundary records package origin as unknown when local package metadata has no authoritative origin and never substitutes current repository matches.

The storage boundary owns a sidecar snapshot lock beside the SQLite database. Reports take the sidecar's shared lock before their SQLite read transaction and hold it through report construction. Refresh takes SQLite's writer lock first and the sidecar's exclusive lock only for retention and commit. This order lets reports read the previous snapshot during inventory capture while preventing retention from overlapping a report read. The `count` report compares its committed snapshot fingerprint with the live local inventory and reports packages without source-language profiles as missing analysis.

## Alternatives

Paclang does not link to libalpm for installed-database metadata. The available accessor is specific to a pacman fork, so it cannot provide one package-origin contract across supported ALPM systems. [ADR 0010](../docs/adr/0010-do-not-depend-on-fork-specific-origin-metadata.md) records the decision and its evidence.
