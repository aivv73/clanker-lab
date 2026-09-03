# Paclang

Paclang analyzes the programming languages behind packages installed on an ALPM system.

The executable stores its SQLite analysis database at `$XDG_DATA_HOME/paclang/paclang.db`. If `XDG_DATA_HOME` is unset or is not an absolute path, Paclang uses `$HOME/.local/share/paclang/paclang.db`.

Run `paclang refresh --metadata-only` to read installed package metadata from `pacman` and `expac` and publish an inventory snapshot. Paclang validates both command results before it changes the published snapshot.

Run `paclang count` to report the installed total, the selected population, and missing source-language analysis from the latest published snapshot. Before the first refresh, the report returns `no_snapshot` and suggests the refresh command.

Use `--explicit` or `--dependency` to select one install reason. Repeat `--origin <name>` to select the union of named package origins. You can combine an install-reason filter with origin filters. An origin that is not in the snapshot selects zero packages. The current metadata refresh records every package origin as `unknown`, so `--origin unknown` is the only origin that can select packages from a snapshot created by this version.
