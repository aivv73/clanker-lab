use super::{
    InstallReason, InventoryError, parse_expac_records, parse_pacman_names,
    validate_same_population,
};

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
