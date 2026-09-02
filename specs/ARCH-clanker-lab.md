# ARCH-clanker-lab: Repository architecture

Clanker Lab contains the Paclang Rust package and project-level agent instructions.

The `paclang` executable owns command parsing and output-format selection. Its report model renders the same facts as terminal, TOON, JSON, and Markdown output. The storage boundary owns the SQLite analysis database under the XDG user data directory. Subprocess tests exercise the compiled executable with isolated user directories.

The current report path migrates an empty database and returns `no_snapshot`. It does not collect or invent installed-package data. Later inventory refresh work can publish snapshots through the storage boundary without replacing the CLI or report model.
