# Keep the first release local and read-only

Paclang is a read-only analyzer for public sources behind packages on ALPM systems. It sends no telemetry and never installs runtimes, executes PKGBUILDs, modifies packages, or configures coding agents automatically. The first release excludes private repositories, non-ALPM package managers, binary disassembly, installed-file language detection, dependency graphs, and actionable rewrite recommendations. An optional agent skill and on-demand status command provide agent guidance without adding ambient context to unrelated sessions.
