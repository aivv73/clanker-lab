use super::{AppError, Format};

#[test]
fn redirected_stdout_selects_toon() {
    assert_eq!(Format::for_terminal(false), Format::Toon);
}

#[test]
fn tty_stdout_selects_terminal() {
    assert_eq!(Format::for_terminal(true), Format::Terminal);
}

#[test]
fn json_error_has_stable_schema() {
    let rendered = Format::Json.render_error(&AppError::NoSnapshot).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

    assert_eq!(value["schema_version"], "1.0.0");
    assert_eq!(value["error"]["code"], "no_snapshot");
    assert_eq!(
        value["error"]["next_command"],
        "paclang refresh --metadata-only"
    );
}

#[test]
fn toon_serializer_escapes_grammar_characters() {
    let error = AppError::storage("line one:\n\"quoted\" \\ path");
    let rendered = Format::Toon.render_error(&error).unwrap();
    let value: serde_json::Value = toon_format::decode_default(&rendered).unwrap();

    assert_eq!(value["error"]["message"], "line one:\n\"quoted\" \\ path");
}
