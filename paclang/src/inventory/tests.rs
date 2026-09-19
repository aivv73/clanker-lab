use proptest::prelude::*;

use super::{InventoryError, parse_expac_records, parse_pacman_names, validate_same_population};
use crate::expac_parser;
use crate::package::InstallReason;

#[test]
fn parses_typed_expac_records() {
    let packages = parse_expac_records(
        "alpha\t1.2-3\talpha\tlocal\texplicit\t42\n\
         split-docs\t2-1\tsplit\tlocal\tdependency\t100\n",
    )
    .unwrap();

    let alpha = &packages["alpha"];
    assert_eq!(alpha.version(), "1.2-3");
    assert_eq!(alpha.package_base(), Some("alpha"));
    assert_eq!(alpha.origin(), "unknown");
    assert!(matches!(alpha.install_reason, InstallReason::Explicit));
    assert_eq!(alpha.installed_bytes(), 42);
    assert_eq!(packages["split-docs"].origin(), "unknown");
}

#[test]
fn rejects_a_malformed_record_before_domain_construction() {
    let error = parse_expac_records("alpha\t1\talpha\tlocal\texplicit\n").unwrap_err();

    assert!(matches!(
        error,
        InventoryError::InvalidExpacRecord { line: 1, .. }
    ));
}

#[test]
fn rejects_a_nonlocal_query_record_instead_of_treating_it_as_origin() {
    let error = parse_expac_records("alpha\t1\talpha\textra\texplicit\t42\n").unwrap_err();

    assert!(matches!(
        error,
        InventoryError::InvalidExpacRecord { line: 1, .. }
    ));
}

#[test]
fn rejects_duplicate_pacman_names() {
    let error = parse_pacman_names("alpha\nalpha\n").unwrap_err();

    assert!(matches!(error, InventoryError::DuplicatePacmanName(name) if name == "alpha"));
}

#[test]
fn rejects_different_command_populations() {
    let names = parse_pacman_names("alpha\nbeta\n").unwrap();
    let packages = parse_expac_records("alpha\t1\talpha\tlocal\texplicit\t42\n").unwrap();

    let error = validate_same_population(&names, &packages).unwrap_err();
    assert!(matches!(
        error,
        InventoryError::PopulationMismatch {
            missing_metadata,
            unexpected_metadata,
        } if missing_metadata == ["beta"] && unexpected_metadata.is_empty()
    ));
}

fn valid_record_line(name: &str, version: &str, size: &str) -> String {
    format!("{name}\t{version}\t{name}\tlocal\texplicit\t{size}\n")
}

proptest! {
    /// Malformed sizes never panic and never publish: only canonical ASCII
    /// digits fitting in SQLite's signed range parse successfully.
    #[test]
    fn malformed_sizes_cannot_publish(
        size in prop_oneof![
            // Valid canonical sizes.
            "[0-9]{1,19}",
            // Malformed: empty, whitespace, signs, non-digits, control
            // characters, overflow, and oversized fields.
            prop_oneof![
                Just(String::new()),
                Just(" ".to_owned()),
                Just("-1".to_owned()),
                Just("+42".to_owned()),
                Just("4.2".to_owned()),
                Just("0x10".to_owned()),
                Just("18446744073709551616".to_owned()),
                Just("9223372036854775808".to_owned()),
                Just("99999999999999999999999999".to_owned()),
                Just("12\t34".to_owned()),
                Just("42\n".to_owned()),
                Just("\u{0}42".to_owned()),
                "[a-z]{1,8}",
                "[0-9]{21,40}",
            ],
        ],
    ) {
        let input = valid_record_line("alpha", "1.0-1", &size);
        let result = parse_expac_records(&input);
        let canonical = !size.is_empty()
            && size.bytes().all(|byte| byte.is_ascii_digit())
            && size.len() <= 20
            && size.parse::<u64>().is_ok_and(|value| i64::try_from(value).is_ok());
        if canonical {
            prop_assert!(result.is_ok(), "size {:?} should parse", size);
        } else {
            prop_assert!(result.is_err(), "size {:?} must not publish", size);
        }
    }

    /// Duplicate records never publish, whether the repeat is identical or
    /// carries conflicting field values.
    #[test]
    fn duplicate_records_cannot_publish(
        version_b in "[a-z0-9.+_~-]{1,12}",
        size_b in "[0-9]{1,6}",
        reason_b in prop_oneof![Just("explicit"), Just("dependency")],
    ) {
        let first = "alpha\t1.0-1\talpha\tlocal\texplicit\t42\n";
        // Identical repeat.
        let identical = format!("{first}{first}");
        let error = parse_expac_records(&identical).unwrap_err();
        prop_assert!(
            matches!(error, InventoryError::DuplicateExpacName(name) if name == "alpha"),
            "identical duplicates must not publish",
        );

        // Conflicting repeat: same name, different version, size, or reason.
        let conflicting =
            format!("{first}alpha\t{version_b}\talpha\tlocal\t{reason_b}\t{size_b}\n");
        let error = parse_expac_records(&conflicting).unwrap_err();
        prop_assert!(
            matches!(error, InventoryError::DuplicateExpacName(name) if name == "alpha"),
            "conflicting duplicates must not publish",
        );
    }

    /// Conflicting metadata for one package name is a duplicate, never a
    /// silent overwrite: parsing keeps the first record only by rejecting
    /// the whole capture.
    #[test]
    fn conflicting_metadata_is_rejected_not_merged(
        version_a in "[a-z0-9.+_~-]{1,12}",
        version_b in "[a-z0-9.+_~-]{1,12}",
    ) {
        prop_assume!(version_a != version_b);
        let input = format!(
            "alpha\t{version_a}\talpha\tlocal\texplicit\t42\n\
             alpha\t{version_b}\talpha\tlocal\tdependency\t100\n",
        );
        let error = parse_expac_records(&input).unwrap_err();
        prop_assert!(
            matches!(error, InventoryError::DuplicateExpacName(name) if name == "alpha"),
        );
    }

    /// Arbitrary input ordering parses to the same population: every
    /// permutation of distinct valid records succeeds with the same names,
    /// and the pacman/expac population check is order-invariant.
    #[test]
    fn arbitrary_input_ordering_preserves_the_population(
        count in 1_usize..12,
        seed in any::<u64>(),
    ) {
        let mut names: Vec<String> =
            (0..count).map(|index| format!("pkg-{index:03}")).collect();
        // Deterministic shuffle from the seed (xorshift64*).
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            state
        };
        for index in (1..names.len()).rev() {
            let other = (next() as usize) % (index + 1);
            names.swap(index, other);
        }
        let input = names
            .iter()
            .map(|name| valid_record_line(name, "1.0-1", "10"))
            .collect::<String>();
        let packages = parse_expac_records(&input)
            .expect("distinct valid records must parse in any order");
        prop_assert_eq!(packages.len(), count);
        let parsed_names = packages.keys().cloned().collect::<Vec<_>>();
        let mut sorted = parsed_names.clone();
        sorted.sort();
        let mut expected = names.clone();
        expected.sort();
        prop_assert_eq!(parsed_names, expected.clone());

        // The population check agrees regardless of side ordering.
        let pacman = parse_pacman_names(
            &expected.iter().map(|name| format!("{name}\n")).collect::<String>(),
        )
        .unwrap();
        prop_assert!(validate_same_population(&pacman, &packages).is_ok());
    }

    /// Arbitrary bytes never panic and never allocate without bounds: the
    /// byte-level parser returns a valid record vector or a typed error.
    #[test]
    fn arbitrary_bytes_never_panic_and_stay_bounded(
        bytes in prop::collection::vec(any::<u8>(), 0..8192),
    ) {
        let result = expac_parser::parse_expac_bytes(&bytes);
        match result {
            Ok(records) => {
                prop_assert!(records.len() <= expac_parser::MAX_PACKAGES);
                for record in &records {
                    prop_assert!(record.name.len() <= expac_parser::MAX_FIELD_BYTES);
                    prop_assert!(record.version.len() <= expac_parser::MAX_FIELD_BYTES);
                }
            }
            Err(error) => {
                // Oversized input must surface the typed bound error.
                if bytes.len() > expac_parser::MAX_STDOUT_BYTES {
                    let is_bounded = matches!(
                        error,
                        expac_parser::ParseError::OutputTooLarge { .. }
                    );
                    prop_assert!(is_bounded);
                }
            }
        }
    }
}
