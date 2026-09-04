//! Bounded, panic-free parser for structured `expac` output.
//!
//! This module is intentionally dependency-free (only `std`) so the
//! `cargo fuzz` target in `fuzz/` can include it with `#[path]` and feed
//! arbitrary bytes into [`parse_expac_bytes`] without pulling in the rest of
//! the binary. `src/inventory.rs` wraps it and maps [`ParseError`] onto the
//! typed [`InventoryError`](crate::inventory::InventoryError).
//!
//! Allocation is bounded at every step: total input, individual records,
//! individual fields, and the number of parsed packages are all limited before
//! any large allocation happens. Parsing never panics; every malformed input
//! yields a typed [`ParseError`].

/// Maximum accepted adapter stdout in bytes (16 MiB). A large ALPM system
/// lists a few thousand packages; this limit is generous while preventing
/// unbounded capture from exhausting memory.
pub const MAX_STDOUT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum accepted bytes for one `expac` record line.
pub const MAX_RECORD_BYTES: usize = 16 * 1024;

/// Maximum accepted bytes for one tab-delimited field.
pub const MAX_FIELD_BYTES: usize = 4 * 1024;

/// Maximum accepted number of package records in one capture.
pub const MAX_PACKAGES: usize = 100_000;

/// Bytes of process stderr retained for diagnostic context in failure
/// messages. The remainder is discarded so a noisy child cannot bloat
/// reports.
pub const MAX_RETAINED_STDERR_BYTES: usize = 2 * 1024;

/// One validated `expac` record, ready for domain construction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedExpacRecord {
    /// Validated package name.
    pub name: String,
    /// Validated package version.
    pub version: String,
    /// Validated package base, if present.
    pub package_base: Option<String>,
    /// Validated install reason.
    pub install_reason: ParsedInstallReason,
    /// Validated installed size in bytes.
    pub installed_bytes: i64,
}

/// Validated install reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsedInstallReason {
    /// Explicitly installed package.
    Explicit,
    /// Package installed as a dependency.
    Dependency,
}

/// Typed parse failure. Every variant carries enough context to diagnose the
/// offending input without echoing unbounded bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// Input is not valid UTF-8.
    NotUtf8,
    /// Total input exceeds [`MAX_STDOUT_BYTES`].
    OutputTooLarge {
        /// Enforced limit in bytes.
        limit: usize,
        /// Observed size in bytes.
        actual: usize,
    },
    /// Input holds more package records than [`MAX_PACKAGES`].
    TooManyPackages {
        /// Enforced limit.
        limit: usize,
    },
    /// One record line exceeds [`MAX_RECORD_BYTES`].
    RecordTooLong {
        /// 1-based line number.
        line: usize,
        /// Enforced limit in bytes.
        limit: usize,
    },
    /// One field exceeds [`MAX_FIELD_BYTES`].
    FieldTooLong {
        /// 1-based line number.
        line: usize,
        /// Enforced limit in bytes.
        limit: usize,
    },
    /// A record is malformed or holds an invalid value.
    InvalidRecord {
        /// 1-based line number.
        line: usize,
        /// Static description; never echoes unbounded input.
        message: &'static str,
    },
    /// A package name appears more than once, whether the repeated fields
    /// are identical or conflicting.
    DuplicateName {
        /// 1-based line number of the repeat.
        line: usize,
        /// Repeated package name, bounded by [`MAX_FIELD_BYTES`].
        name: String,
    },
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotUtf8 => formatter.write_str("output is not valid UTF-8"),
            Self::OutputTooLarge { limit, actual } => write!(
                formatter,
                "output is {actual} bytes, which exceeds the {limit}-byte limit"
            ),
            Self::TooManyPackages { limit } => {
                write!(formatter, "output holds more than {limit} package records")
            }
            Self::RecordTooLong { line, limit } => write!(
                formatter,
                "record on line {line} exceeds the {limit}-byte limit"
            ),
            Self::FieldTooLong { line, limit } => write!(
                formatter,
                "a field on line {line} exceeds the {limit}-byte limit"
            ),
            Self::InvalidRecord { line, message } => {
                write!(formatter, "invalid record on line {line}: {message}")
            }
            Self::DuplicateName { line, name } => {
                write!(
                    formatter,
                    "package {name:?} appears more than once (line {line})"
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse raw adapter stdout bytes into validated records.
///
/// Returns the records in input order, or a typed [`ParseError`]. Never
/// panics and never allocates beyond the documented bounds.
pub fn parse_expac_bytes(bytes: &[u8]) -> Result<Vec<ParsedExpacRecord>, ParseError> {
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(ParseError::OutputTooLarge {
            limit: MAX_STDOUT_BYTES,
            actual: bytes.len(),
        });
    }
    let text = core::str::from_utf8(bytes).map_err(|_| ParseError::NotUtf8)?;
    parse_expac_text(text)
}

/// Parse validated UTF-8 adapter stdout into records.
fn parse_expac_text(text: &str) -> Result<Vec<ParsedExpacRecord>, ParseError> {
    // Empty output means zero installed packages. The population check in the
    // caller compares against `pacman -Qq`; an empty capture is only accepted
    // when both sides agree.
    let mut records = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.len() > MAX_RECORD_BYTES {
            return Err(ParseError::RecordTooLong {
                line: line_number,
                limit: MAX_RECORD_BYTES,
            });
        }
        let record = parse_record_line(line, line_number)?;
        if !names.insert(record.name.clone()) {
            return Err(ParseError::DuplicateName {
                line: line_number,
                name: record.name,
            });
        }
        records.push(record);
        if records.len() > MAX_PACKAGES {
            return Err(ParseError::TooManyPackages {
                limit: MAX_PACKAGES,
            });
        }
    }
    Ok(records)
}

/// Split one line into six tab-delimited fields and validate each value.
fn parse_record_line(line: &str, line_number: usize) -> Result<ParsedExpacRecord, ParseError> {
    // The per-field count is bounded by the per-record length: a 16 KiB line
    // holds at most ~16k tabs, so this temporary vector stays small.
    let fields = line.split('\t').collect::<Vec<_>>();
    let [
        name,
        version,
        package_base,
        queried_database,
        install_reason,
        installed_bytes,
    ] = fields.as_slice()
    else {
        return Err(ParseError::InvalidRecord {
            line: line_number,
            message: "expected six tab-delimited fields",
        });
    };
    for field in &fields {
        if field.len() > MAX_FIELD_BYTES {
            return Err(ParseError::FieldTooLong {
                line: line_number,
                limit: MAX_FIELD_BYTES,
            });
        }
    }
    if *queried_database != "local" {
        return Err(ParseError::InvalidRecord {
            line: line_number,
            message: "queried database is not local",
        });
    }
    Ok(ParsedExpacRecord {
        name: parse_package_name(name).map_err(|error| with_line(error, line_number))?,
        version: parse_package_version(version).map_err(|error| with_line(error, line_number))?,
        package_base: parse_package_base(package_base)
            .map_err(|error| with_line(error, line_number))?,
        install_reason: parse_install_reason(install_reason)
            .map_err(|error| with_line(error, line_number))?,
        installed_bytes: parse_installed_bytes(installed_bytes)
            .map_err(|error| with_line(error, line_number))?,
    })
}

/// Validate a package name: nonempty, bounded, no whitespace or control
/// characters, and restricted to the portable package-name alphabet.
fn parse_package_name(value: &str) -> Result<String, ParseError> {
    // Line numbers are attached by the caller through `parse_record_line`;
    // these helpers return the static message and the caller wraps it.
    validate_package_name(value)
        .map(|()| value.to_owned())
        .map_err(record_message)
}

/// Validate a package name without allocating: nonempty, bounded, no
/// whitespace or control characters, restricted to the portable
/// package-name alphabet. Shared by the `expac` record parser and the
/// `pacman -Qq` name parser so both reject the same invalid inputs.
pub fn validate_package_name(value: &str) -> Result<(), &'static str> {
    validate_name_like(value, "package name")
}

/// Validate a package base without allocating. Empty is rejected here;
/// callers treat empty as absent before calling.
pub fn validate_package_base(value: &str) -> Result<(), &'static str> {
    validate_name_like(value, "package base")
}

fn validate_name_like(value: &str, kind: &'static str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err(if kind == "package base" {
            "package base is empty"
        } else {
            "package name is empty"
        });
    }
    if value.len() > MAX_FIELD_BYTES {
        return Err(if kind == "package base" {
            "package base exceeds the field limit"
        } else {
            "package name exceeds the field limit"
        });
    }
    if value.chars().any(char::is_whitespace) {
        return Err(if kind == "package base" {
            "package base contains whitespace"
        } else {
            "package name contains whitespace"
        });
    }
    if value.chars().any(char::is_control) {
        return Err(if kind == "package base" {
            "package base contains a control character"
        } else {
            "package name contains a control character"
        });
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'+' | b'-')
    }) {
        return Err(if kind == "package base" {
            "package base contains an unexpected character"
        } else {
            "package name contains an unexpected character"
        });
    }
    Ok(())
}

/// Validate a package version without allocating: nonempty, bounded, no
/// whitespace or control characters. Shared with the `pacman -Q`
/// fingerprint parser so live-inventory sampling rejects the same invalid
/// inputs.
pub fn validate_package_version(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("package version is empty");
    }
    if value.len() > MAX_FIELD_BYTES {
        return Err("package version exceeds the field limit");
    }
    if value.chars().any(char::is_whitespace) {
        return Err("package version contains whitespace");
    }
    if value.chars().any(char::is_control) {
        return Err("package version contains a control character");
    }
    Ok(())
}

/// Validate a package version: nonempty, bounded, no whitespace or control
/// characters.
fn parse_package_version(value: &str) -> Result<String, ParseError> {
    validate_package_version(value)
        .map(|()| value.to_owned())
        .map_err(record_message)
}

/// Validate an optional package base: empty means absent, otherwise the same
/// alphabet as a package name.
fn parse_package_base(value: &str) -> Result<Option<String>, ParseError> {
    if value.is_empty() {
        return Ok(None);
    }
    validate_package_base(value)
        .map(|()| Some(value.to_owned()))
        .map_err(record_message)
}

/// Validate an install reason.
fn parse_install_reason(value: &str) -> Result<ParsedInstallReason, ParseError> {
    match value {
        "explicit" => Ok(ParsedInstallReason::Explicit),
        "dependency" => Ok(ParsedInstallReason::Dependency),
        _ => Err(record_message(
            "install reason is neither explicit nor dependency",
        )),
    }
}

/// Validate installed bytes: canonical ASCII digits that fit in SQLite's
/// signed 64-bit integer range.
fn parse_installed_bytes(value: &str) -> Result<i64, ParseError> {
    if value.is_empty() || value.len() > 20 {
        return Err(record_message("installed bytes is not an unsigned integer"));
    }
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(record_message("installed bytes is not an unsigned integer"));
    }
    let bytes = value
        .parse::<u64>()
        .map_err(|_| record_message("installed bytes is not an unsigned integer"))?;
    i64::try_from(bytes).map_err(|_| record_message("installed bytes exceeds SQLite's range"))
}

/// Build an [`ParseError::InvalidRecord`] message placeholder. Field helpers
/// emit `line: 0` as a sentinel; [`with_line`] replaces it with the real
/// 1-based line number at the single call site in [`parse_record_line`].
fn record_message(message: &'static str) -> ParseError {
    ParseError::InvalidRecord { line: 0, message }
}

/// Attach the 1-based line number to an error produced by a field helper.
/// Helpers above emit [`ParseError::InvalidRecord`] with `line: 0` as a
/// sentinel; this function replaces the sentinel with the real line.
fn with_line(error: ParseError, line: usize) -> ParseError {
    match error {
        ParseError::InvalidRecord { line: 0, message } => {
            ParseError::InvalidRecord { line, message }
        }
        other => other,
    }
}

/// Truncate retained stderr context to [`MAX_RETAINED_STDERR_BYTES`] on a
/// character boundary, appending a truncation marker when trimmed.
pub fn truncate_stderr_context(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim();
    if trimmed.len() <= MAX_RETAINED_STDERR_BYTES {
        return trimmed.to_owned();
    }
    let mut end = MAX_RETAINED_STDERR_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", trimmed[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_FIELD_BYTES, MAX_PACKAGES, MAX_RECORD_BYTES, MAX_RETAINED_STDERR_BYTES,
        MAX_STDOUT_BYTES, ParseError, parse_expac_bytes, truncate_stderr_context,
    };

    fn record(name: &str, version: &str, base: &str, reason: &str, bytes: &str) -> String {
        format!("{name}\t{version}\t{base}\tlocal\t{reason}\t{bytes}\n")
    }

    #[test]
    fn parses_a_valid_record() {
        let records =
            parse_expac_bytes(record("alpha", "1.0-1", "alpha", "explicit", "42").as_bytes())
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "alpha");
        assert_eq!(records[0].installed_bytes, 42);
        assert_eq!(records[0].package_base.as_deref(), Some("alpha"));
    }

    #[test]
    fn rejects_non_utf8_without_panicking() {
        let error = parse_expac_bytes(b"\xff\xfe\n").unwrap_err();
        assert_eq!(error, ParseError::NotUtf8);
    }

    #[test]
    fn rejects_oversized_stdout_before_parsing() {
        let error = parse_expac_bytes(&vec![b'a'; MAX_STDOUT_BYTES + 1]).unwrap_err();
        assert!(matches!(error, ParseError::OutputTooLarge { .. }));
    }

    #[test]
    fn rejects_an_oversized_record_line() {
        let mut line = record("a", "1", "a", "explicit", "1");
        assert!(line.ends_with('\n'));
        line.pop();
        line.push_str(&"x".repeat(MAX_RECORD_BYTES));
        line.push('\n');
        let error = parse_expac_bytes(line.as_bytes()).unwrap_err();
        assert!(matches!(error, ParseError::RecordTooLong { .. }));
    }

    #[test]
    fn rejects_an_oversized_field() {
        let big = "n".repeat(MAX_FIELD_BYTES + 1);
        let error =
            parse_expac_bytes(record(&big, "1", "", "explicit", "1").as_bytes()).unwrap_err();
        assert!(matches!(error, ParseError::FieldTooLong { .. }));
    }

    #[test]
    fn rejects_too_many_packages() {
        let one = record("pkg", "1", "", "explicit", "1");
        let mut input = String::new();
        for index in 0..=MAX_PACKAGES {
            input.push_str(&one.replace("pkg", &format!("pkg-{index}")));
        }
        let error = parse_expac_bytes(input.as_bytes()).unwrap_err();
        assert_eq!(
            error,
            ParseError::TooManyPackages {
                limit: MAX_PACKAGES
            }
        );
    }

    #[test]
    fn truncates_stderr_context() {
        let raw = vec![b'x'; MAX_RETAINED_STDERR_BYTES + 100];
        let kept = truncate_stderr_context(&raw);
        assert!(kept.ends_with("…[truncated]"));
        assert!(kept.len() <= MAX_RETAINED_STDERR_BYTES + "…[truncated]".len());
    }
}
