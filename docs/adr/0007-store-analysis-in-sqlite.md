# Store analysis in SQLite

Paclang stores inventory snapshots, package mappings, provider attempts, analysis targets, evidence grades, source-language profiles, and tool versions in SQLite through `rusqlite` with bundled SQLite. The database lives under the XDG user data directory, while disposable downloads use the cache directory and user overrides remain under the config directory. Foreign keys, schema migrations, transactions, and a single-writer lock protect coherent snapshot publication. JSON files remain output artifacts rather than internal storage.
