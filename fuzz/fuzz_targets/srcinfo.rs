#![no_main]

//! Fuzz target for the `.SRCINFO` recipe-metadata parser.
//!
//! Feeds arbitrary bytes into the same bounded parser a future source
//! provider will use for Arch recipe evidence (`src/srcinfo.rs`, included
//! here with `#[path]` so the target always exercises the real code, not a
//! copy). The parser must return a valid document or a typed error; it must
//! never panic or allocate without bounds.
//!
//! Run briefly during development:
//!
//! ```sh
//! cargo install cargo-fuzz
//! cd fuzz
//! cargo fuzz run srcinfo -- -max_total_time=60
//! ```
//!
//! Periodic runs (see `docs/fuzzing-and-sanitizers.md`) use a longer budget
//! with AddressSanitizer. The in-tree property test
//! `srcinfo::tests::arbitrary_bytes_never_panic_and_stay_bounded` covers the
//! same invariant in ordinary `cargo test` runs.

#[path = "../../src/srcinfo.rs"]
mod srcinfo;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match srcinfo::parse_srcinfo_bytes(data) {
        Ok(document) => {
            // Bounded allocation: the section count and every retained value
            // stay within the documented limits.
            assert!(document.packages.len() <= srcinfo::MAX_PACKAGES);
            assert!(document.pkgbase.len() <= srcinfo::MAX_LINE_BYTES);
            assert!(document.pkgver.len() <= srcinfo::MAX_LINE_BYTES);
            assert!(document.pkgrel.len() <= srcinfo::MAX_LINE_BYTES);
            if let Some(epoch) = &document.epoch {
                assert!(epoch.len() <= srcinfo::MAX_LINE_BYTES);
            }
            for source in &document.sources {
                assert!(source.len() <= srcinfo::MAX_LINE_BYTES);
            }
            for package in &document.packages {
                assert!(package.name.len() <= srcinfo::MAX_LINE_BYTES);
                for source in &package.sources {
                    assert!(source.len() <= srcinfo::MAX_LINE_BYTES);
                }
            }
        }
        Err(_) => {}
    }
});
