# Fuzzing and sanitizers

This note documents the periodic dynamic-testing jobs for Paclang's
untrusted-input boundary: the structured `expac` record parser and the
`.SRCINFO` recipe-metadata parser. Ordinary `cargo test` runs stay
deterministic and offline (see below); the jobs here run on a schedule or
before releases, not on every change.

## What is covered

`paclang refresh --metadata-only` treats `pacman` and `expac` output as
untrusted until the adapter parses and validates it. The parser lives in
`src/expac_parser.rs` and enforces these allocation bounds before any large
allocation:

- Adapter stdout: 16 MiB total (`MAX_STDOUT_BYTES`).
- One `expac` record line: 16 KiB (`MAX_RECORD_BYTES`).
- One tab-delimited field: 4 KiB (`MAX_FIELD_BYTES`).
- Package records per capture: 100,000 (`MAX_PACKAGES`).
- Retained process stderr in failure messages: 2 KiB with a `[truncated]`
  marker (`MAX_RETAINED_STDERR_BYTES`).

The `.SRCINFO` parser lives in `src/srcinfo.rs` and treats fetched recipe
metadata as untrusted until validated. It enforces its own bounds before
any large allocation:

- `.SRCINFO` input: 1 MiB total (`MAX_SRCINFO_BYTES`).
- One recipe line: 8 KiB (`MAX_LINE_BYTES`).
- Package sections per document: 1,024 (`MAX_PACKAGES`).

Every malformed input yields a typed error (`ParseError`/`SrcinfoError`,
mapped to the adapter's `InventoryError`); no input panics. A failed refresh
never replaces the published snapshot.

## Ordinary tests (every change)

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

- Unit tests cover valid records, malformed rows, non-local databases,
  duplicates, and population mismatches.
- Property tests (`src/inventory/tests.rs`, `src/srcinfo.rs`) cover malformed sizes, duplicate
  records, conflicting records for one package name, arbitrary input
  ordering, and arbitrary byte strings (bounded, never panicking).
- CLI tests (`tests/cli.rs`) use fixture `pacman`/`expac` executables with
  isolated XDG directories and a real temporary SQLite database. They cover
  missing tools, nonzero exits, truncated diagnostic context, invalid UTF-8,
  malformed/duplicated/conflicting metadata, oversized output, and snapshot
  preservation. They never depend on live Arch, AUR, GitHub, or upstream
  services.

## Fuzz targets (periodic)

Targets: `fuzz/fuzz_targets/expac_parser.rs` and
`fuzz/fuzz_targets/srcinfo.rs`. Each includes its `src/` parser directly
with `#[path]`, so the targets always exercise the real parsers, and feed
each arbitrary bytes with the allocation-limit assertions inline.

```sh
cargo install cargo-fuzz
cd fuzz
cargo fuzz run expac_parser -- -max_total_time=60        # quick check
cargo fuzz run srcinfo -- -max_total_time=60             # quick check
cargo fuzz run expac_parser -- -max_total_time=3600      # periodic budget
```

`cargo fuzz` builds with AddressSanitizer by default, so the periodic
one-hour run doubles as the sanitizer pass over the parsers. Run it weekly
and before any release that touches `src/expac_parser.rs`,
`src/srcinfo.rs`, `src/inventory.rs`, or the `fuzz/` targets. Keep the
corpus in the fuzzer's default directory and check in only minimal
regression seeds as ordinary fixture tests.

## Sanitizers (periodic)

```sh
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target x86_64-unknown-linux-gnu
```

`-Zsanitizer` requires a nightly toolchain. Run the sanitizer build of the full test suite on the same weekly /
pre-release cadence as the fuzz budget. Leak detection stays enabled; triage
any report against the bounds above before changing limits.

## Miri (conditional)

Miri runs only if application-owned `unsafe` code appears. This slice has
none (`nix`, `signal-hook`, `fs2`, and `rusqlite` dependencies may contain
their own, which is out of scope), so Miri is not part of the periodic jobs.
If an `unsafe` block is ever added, gate it with a Miri run of the affected
tests and record the decision here.

## Verus

Verus is outside this slice. No Verus proofs or jobs are defined or required.
