# Security and reliability context

Paclang is a synchronous, non-interactive CLI run with the invoking user's permissions on an ALPM system. It must not request root privileges. The executable reads `XDG_DATA_HOME` and `HOME`, creates and opens its SQLite analysis database, and writes reports to standard output. It does not access the network, send telemetry, invoke package-manager subprocesses, or handle secrets.

Environment variables and an existing database are external inputs. Paclang accepts only absolute XDG or home paths. It validates the SQLite schema version before application queries and refuses databases created by newer unsupported versions without modifying them.

Concurrent CLI invocations are expected. Schema migration and future snapshot publication must acquire SQLite's write lock before checking or changing schema state. Migrations and publication must remain transactional so interruption or failure cannot expose partial state.

Invalid CLI syntax exits 2. Runtime and output failures exit 1 without panicking. Future interruptible refresh work exits 130 on interruption. Structured runtime errors go to stdout in the selected format. Stderr is reserved for diagnostics.

`PRAGMA user_version` identifies the persisted schema version. Changes to migrations or persisted tables must add compatibility tests for empty, current, malformed, and newer-version databases. Storage tests must also cover concurrent first use. CLI tests must cover XDG fallback, isolated data directories, output content, and exit status.

The current database contains no secrets. Another process running as the same user can alter it, and Paclang does not defend against that user-level compromise. Revisit file permissions and tamper resistance before storing credentials, private-source metadata, or data written by a privileged process.
