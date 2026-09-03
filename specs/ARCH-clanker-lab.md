# ARCH-clanker-lab: Repository architecture

Clanker Lab contains the Paclang Rust package and project-level agent instructions.

The `paclang` executable owns command parsing and output-format selection. Its report model renders the same facts as terminal, TOON, JSON, and Markdown output. The storage boundary owns the SQLite analysis database under the XDG user data directory. Subprocess tests exercise the compiled executable with isolated user directories.

`paclang refresh --metadata-only` reads the installed package population from `pacman` and `expac`. The inventory boundary validates both command results before the storage boundary publishes one SQLite snapshot in a transaction. The inventory boundary records package origin as unknown when local package metadata has no authoritative origin and never substitutes current repository matches. The `count` report reads only the latest published snapshot and reports packages without source-language profiles as missing analysis.

## Alternatives

Paclang does not link to libalpm for installed-database metadata. The available accessor is specific to a pacman fork, so it cannot provide one package-origin contract across supported ALPM systems. [ADR 0010](../docs/adr/0010-do-not-depend-on-fork-specific-origin-metadata.md) records the decision and its evidence.
