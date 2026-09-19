# clanker-lab

A monorepo for personal experiments. Each experiment lives in its own directory with its own dependencies, documentation, and development commands.

## Packages

| Package | Description | Stack |
| --- | --- | --- |
| [paclang](paclang/README.md) | Analyze the programming languages used in the source code of installed packages on Arch Linux and other ALPM-based systems. | Rust |
| [Psi](psi/README.md) | A standalone Rust agent runtime; the first conversation through T3 with a fixture model. | Rust, T3; Bend planned for the MVP |

## paclang

`paclang` collects information about installed packages, locates their source code, and produces programming-language reports. Results are stored locally in SQLite. Exact analysis uses GitHub Linguist; fast mode retrieves approximate data through the GitHub Languages API.

Building requires Rust with edition 2024 support, Cargo, C build tools, and the libcurl and libarchive development libraries. Reading installed packages requires `pacman` and `expac`; exact analysis requires `github-linguist`. Integration tests use Git.

From the monorepo root:

```sh
cd paclang
cargo build
cargo run -- --help
```

Create a snapshot of installed packages without network requests and view the summary:

```sh
cargo run -- refresh --metadata-only
cargo run -- count
```

A metadata snapshot alone contains no language analysis results. To download source code and analyze it using an installed GitHub Linguist:

```sh
cargo run -- refresh --sources-only --linguist-executable github-linguist
cargo run -- count
```

See the [package README](paclang/README.md) for details on modes, limitations, and reports.

## Development

There is no shared Cargo workspace or unified build command yet. Run commands inside the relevant package.

For `paclang`:

```sh
cd paclang
cargo fmt --check
cargo check
cargo test
```

Instructions for coding agents are in [AGENTS.md](AGENTS.md). When adding an experiment, create its own directory and README, then update the package table above.
