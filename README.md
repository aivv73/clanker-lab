# Paclang

Paclang analyzes the programming languages behind packages installed on an ALPM system.

The executable stores its SQLite analysis database at `$XDG_DATA_HOME/paclang/paclang.db`. If `XDG_DATA_HOME` is unset or is not an absolute path, Paclang uses `$HOME/.local/share/paclang/paclang.db`.

Run `paclang refresh --metadata-only` to read installed package metadata from `pacman` and `expac` and publish an inventory snapshot. Paclang checks the captured names and versions against lightweight samples taken around the metadata read. A package change may make the new coherent snapshot stale; metadata that matches neither sample is rejected as mixed. Failure or interruption leaves the previous snapshot active, and interruption exits 130. Paclang retains the latest ten published snapshots.

Run `paclang refresh --recipes-only` to capture inventory and fetch Arch `.SRCINFO` metadata for the selected packages. This mode records source-provider evidence; it does not download source trees or run Linguist, so analysis coverage remains missing. Package-base/version groups share one request. Missing recipes, network failures, mismatched versions, dynamic or unpinned sources, architecture-specific source arrays, and unclear primary sources are recorded as unresolved. Matching pinned HTTPS Git sources or versioned HTTPS archives receive reconstructed evidence because package origin remains unknown. Recipe commits and attempt timestamps are stored for provenance and freshness. A final inventory sample marks the snapshot stale if packages changed during fetching.

Recipe requests use the installed version's Arch packaging tag and never fall back to the current branch. They run with at most 8 concurrent connections, limited to 20 seconds and 2 MiB per API response, with a 1 MiB decoded `.SRCINFO` limit. Members with an already-resolved attempt reuse the stored evidence without refetching; unresolved attempts are retried. When standard error is a terminal, recipe fetching shows a live progress block with completed groups, downloaded bytes, resolved and unresolved counts, elapsed time, and ETA; piped output stays silent so structured reports on standard output are unaffected. SIGINT cancels the request and rolls back all attempts from that refresh along with publication. Successful partial refreshes publish normally. `--metadata-only` remains offline. `--explicit`, `--dependency`, and `--origin` limit recipe fetching and report selection; the complete inventory is still published.

The default recipe endpoint is `https://gitlab.archlinux.org/api/v4`. `--arch-api-url` selects a trusted Arch-compatible GitLab API and requires `--recipes-only`; HTTPS is required except for literal loopback HTTP addresses used by local fixtures. Redirects, URL credentials, and proxy environment variables are not used. Building the HTTP adapter requires libcurl development files; recipe fetching requires an asynchronous-DNS libcurl build. Provider inspection and language analysis remain future slices.

Run `paclang count` to report the installed total, the selected population, and missing source-language analysis from the latest published snapshot. The report compares the snapshot with `pacman -Q` and marks it `current` or `stale`. Snapshots created by an older database version report `unknown` until the next refresh. Before the first refresh, the report returns `no_snapshot` and suggests the refresh command.

Use `--explicit` or `--dependency` to select one install reason. Repeat `--origin <name>` to select the union of named package origins. You can combine an install-reason filter with origin filters. An origin that is not in the snapshot selects zero packages. The current metadata refresh records every package origin as `unknown`, so `--origin unknown` is the only origin that can select packages from a snapshot created by this version.

## Report formats and stability

One typed report model feeds all four formats: `terminal`, `toon`, `json`, and `markdown`. Format selection never changes the reported facts. A TTY selects `terminal`, redirected stdout selects `toon`, and an explicit `--format` always wins.

JSON is the durable interchange contract for automation: working-draft TOON changes never break it. Every structured success and error carries `schema_version` (currently `1.2.0`); reject output whose schema version you do not understand. TOON output additionally declares `toon_specification_version` (currently `3.0`, the working-draft TOON specification the serializer targets).

Structured output carries integer numerators and denominators, so agents and scripts can compute exact shares. Human output shows percentages rounded deterministically to one decimal place; the coverage rows total 100.0% for any non-empty selection (an empty selection reports 0.0% rows). Coverage rows sort by count descending and state name ascending, and their counts always sum to the coverage denominator.

Pass `--min-coverage <percent>` (0 to 100) to make a report fail when analyzed coverage is below the threshold. The report facts are unchanged; only the exit status becomes 1. An empty selection is vacuously complete and always satisfies the threshold.
