#![no_main]

//! Fuzz target for the structured `expac` record parser.
//!
//! Feeds arbitrary bytes into the same bounded parser used by
//! `paclang refresh --metadata-only` (`src/expac_parser.rs`, included here
//! with `#[path]` so the target always exercises the real code, not a copy).
//! The parser must return a valid record vector or a typed error; it must
//! never panic or allocate without bounds.
//!
//! Run briefly during development:
//!
//! ```sh
//! cargo install cargo-fuzz
//! cd fuzz
//! cargo fuzz run expac_parser -- -max_total_time=60
//! ```
//!
//! Periodic runs (see `docs/fuzzing-and-sanitizers.md`) use a longer budget
//! with AddressSanitizer. The in-tree property test
//! `inventory::tests::arbitrary_bytes_never_panic_and_stay_bounded` covers
//! the same invariant in ordinary `cargo test` runs.

#[path = "../../src/expac_parser.rs"]
mod expac_parser;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match expac_parser::parse_expac_bytes(data) {
        Ok(records) => {
            // Bounded allocation: the record count and every field stay
            // within the documented limits.
            assert!(records.len() <= expac_parser::MAX_PACKAGES);
            for record in &records {
                assert!(record.name.len() <= expac_parser::MAX_FIELD_BYTES);
                assert!(record.version.len() <= expac_parser::MAX_FIELD_BYTES);
            }
        }
        Err(_) => {}
    }
});
