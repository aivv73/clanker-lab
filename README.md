# Paclang

Paclang analyzes the programming languages behind packages installed on an ALPM system.

The executable stores its SQLite analysis database at `$XDG_DATA_HOME/paclang/paclang.db`. If `XDG_DATA_HOME` is unset or is not an absolute path, Paclang uses `$HOME/.local/share/paclang/paclang.db`.

This release cannot create an inventory snapshot. Until a later release adds metadata refresh, the report returns `no_snapshot` and suggests the future `paclang refresh --metadata-only` command.
