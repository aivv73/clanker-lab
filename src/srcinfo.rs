//! Bounded, panic-free parser for Arch `.SRCINFO` recipe metadata.
//!
//! This module is intentionally dependency-free (only `std` and `core`) so
//! the `cargo fuzz` target in `fuzz/` can include it with `#[path]` and feed
//! arbitrary bytes into [`parse_srcinfo_bytes`] without pulling in the rest
//! of the binary. The Arch source provider calls it with recipe text; it
//! performs no network or filesystem access itself.
//!
//! The parser reads metadata only and never evaluates PKGBUILDs or other
//! recipe code (ADR 0005). It extracts the evidence a provider needs to
//! identify a packaged source revision: the package base, the
//! `pkgver`/`pkgrel`/`epoch` triple, every `source` entry, and the package
//! names of a split package. All other keys are ignored so new recipe fields
//! stay forward-compatible.
//!
//! Allocation is bounded at every step: total input, individual lines, and
//! the number of package sections are all limited before any large
//! allocation happens. Every stored value is a substring of one bounded
//! line, so total retained bytes never exceed the input size. Parsing never
//! panics; every malformed input yields a typed [`SrcinfoError`].

/// Maximum accepted `.SRCINFO` input in bytes (1 MiB). Real recipe metadata
/// is a few KiB; this limit is generous while preventing unbounded capture
/// from exhausting memory.
pub const MAX_SRCINFO_BYTES: usize = 1024 * 1024;

/// Maximum accepted bytes for one `.SRCINFO` line. Source URLs can be long;
/// this still keeps every retained value small.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// Maximum accepted package sections in one `.SRCINFO` document. Real split
/// packages hold a handful of members; this rejects section-count abuse.
pub const MAX_PACKAGES: usize = 1024;

/// Validated `.SRCINFO` document: the package base, its packaged revision,
/// and the member packages of a (possibly split) package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSrcinfo {
    /// Validated package base from the leading `pkgbase` line.
    pub pkgbase: String,
    /// Validated `pkgver` from the package-base section.
    pub pkgver: String,
    /// Validated `pkgrel` from the package-base section.
    pub pkgrel: String,
    /// Validated `epoch` from the package-base section, if present.
    pub epoch: Option<String>,
    /// Base-level `source` entries in document order.
    pub sources: Vec<String>,
    pub sha256sums: Vec<String>,
    pub sha512sums: Vec<String>,
    /// Architecture-specific source arrays require a later architecture-aware
    /// resolver; their presence must not silently disappear from evidence.
    pub has_arch_specific_sources: bool,
    /// Member packages in document order; always nonempty.
    pub packages: Vec<ParsedSrcinfoPackage>,
}

/// One `pkgname` section: a member package and its own `source` entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSrcinfoPackage {
    /// Validated package name.
    pub name: String,
    /// Package-level `source` entries in document order.
    pub sources: Vec<String>,
    pub sha256sums: Vec<String>,
    pub sha512sums: Vec<String>,
}

/// Typed parse failure. Every variant carries enough context to diagnose the
/// offending input without echoing unbounded bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SrcinfoError {
    /// Input is not valid UTF-8.
    NotUtf8,
    /// Total input exceeds [`MAX_SRCINFO_BYTES`].
    InputTooLarge {
        /// Enforced limit in bytes.
        limit: usize,
        /// Observed size in bytes.
        actual: usize,
    },
    /// One line exceeds [`MAX_LINE_BYTES`].
    LineTooLong {
        /// 1-based line number.
        line: usize,
        /// Enforced limit in bytes.
        limit: usize,
    },
    /// Input holds more package sections than [`MAX_PACKAGES`].
    TooManyPackages {
        /// Enforced limit.
        limit: usize,
    },
    /// A line is structurally invalid or holds an invalid value.
    InvalidLine {
        /// 1-based line number.
        line: usize,
        /// Static description; never echoes unbounded input.
        message: &'static str,
    },
    /// A package name appears more than once.
    DuplicatePackage {
        /// 1-based line number of the repeat.
        line: usize,
        /// Repeated package name, bounded by [`MAX_LINE_BYTES`].
        name: String,
    },
    /// A required field is absent after the full document was read.
    MissingField {
        /// Static description of the absent field.
        message: &'static str,
    },
}

impl core::fmt::Display for SrcinfoError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotUtf8 => formatter.write_str("input is not valid UTF-8"),
            Self::InputTooLarge { limit, actual } => write!(
                formatter,
                "input is {actual} bytes, which exceeds the {limit}-byte limit"
            ),
            Self::LineTooLong { line, limit } => {
                write!(formatter, "line {line} exceeds the {limit}-byte limit")
            }
            Self::TooManyPackages { limit } => {
                write!(formatter, "input holds more than {limit} package sections")
            }
            Self::InvalidLine { line, message } => {
                write!(formatter, "invalid line {line}: {message}")
            }
            Self::DuplicatePackage { line, name } => {
                write!(
                    formatter,
                    "package {name:?} appears more than once (line {line})"
                )
            }
            Self::MissingField { message } => {
                write!(formatter, "missing required field: {message}")
            }
        }
    }
}

impl std::error::Error for SrcinfoError {}

/// Parse raw `.SRCINFO` bytes into a validated document.
///
/// Returns the document or a typed [`SrcinfoError`]. Never panics and never
/// allocates beyond the documented bounds.
pub fn parse_srcinfo_bytes(bytes: &[u8]) -> Result<ParsedSrcinfo, SrcinfoError> {
    if bytes.len() > MAX_SRCINFO_BYTES {
        return Err(SrcinfoError::InputTooLarge {
            limit: MAX_SRCINFO_BYTES,
            actual: bytes.len(),
        });
    }
    let text = core::str::from_utf8(bytes).map_err(|_| SrcinfoError::NotUtf8)?;
    parse_srcinfo_text(text)
}

/// Document assembly state while scanning lines.
#[derive(Default)]
struct DocumentBuilder {
    pkgbase: Option<String>,
    pkgver: Option<String>,
    pkgrel: Option<String>,
    epoch: Option<String>,
    sources: Vec<String>,
    sha256sums: Vec<String>,
    sha512sums: Vec<String>,
    has_arch_specific_sources: bool,
    packages: Vec<ParsedSrcinfoPackage>,
}

impl DocumentBuilder {
    /// Index of the package section subsequent indented keys belong to, or
    /// `None` while still in the package-base section.
    fn current_package(&self) -> Option<usize> {
        self.packages.len().checked_sub(1)
    }

    fn finish(self) -> Result<ParsedSrcinfo, SrcinfoError> {
        let Some(pkgbase) = self.pkgbase else {
            return Err(SrcinfoError::MissingField {
                message: "the first line is not a pkgbase definition",
            });
        };
        let Some(pkgver) = self.pkgver else {
            return Err(SrcinfoError::MissingField {
                message: "the package-base section has no pkgver",
            });
        };
        let Some(pkgrel) = self.pkgrel else {
            return Err(SrcinfoError::MissingField {
                message: "the package-base section has no pkgrel",
            });
        };
        if self.packages.is_empty() {
            return Err(SrcinfoError::MissingField {
                message: "the document has no pkgname section",
            });
        }
        Ok(ParsedSrcinfo {
            pkgbase,
            pkgver,
            pkgrel,
            epoch: self.epoch,
            sources: self.sources,
            sha256sums: self.sha256sums,
            sha512sums: self.sha512sums,
            has_arch_specific_sources: self.has_arch_specific_sources,
            packages: self.packages,
        })
    }
}

/// Parse validated UTF-8 `.SRCINFO` text into a document.
fn parse_srcinfo_text(text: &str) -> Result<ParsedSrcinfo, SrcinfoError> {
    let mut document = DocumentBuilder::default();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.len() > MAX_LINE_BYTES {
            return Err(SrcinfoError::LineTooLong {
                line: line_number,
                limit: MAX_LINE_BYTES,
            });
        }
        // Blank lines separate `makepkg --printsrcinfo` sections; they carry
        // no meaning and are ignored.
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            parse_indented_line(&mut document, line, line_number)?;
        } else {
            parse_section_line(&mut document, line, line_number)?;
        }
    }
    document.finish()
}

/// Handle an unindented line: the leading `pkgbase` definition or a new
/// `pkgname` section. No other key may appear at column zero.
fn parse_section_line(
    document: &mut DocumentBuilder,
    line: &str,
    line_number: usize,
) -> Result<(), SrcinfoError> {
    let (key, value) = split_key_value(line, line_number)?;
    match key {
        "pkgbase" => {
            if document.pkgbase.is_some() {
                return Err(invalid(line_number, "the package base is redefined"));
            }
            if !document.packages.is_empty() {
                return Err(invalid(line_number, "the package base is redefined"));
            }
            document.pkgbase = Some(parse_package_base(value, line_number)?);
            Ok(())
        }
        "pkgname" => {
            if document.pkgbase.is_none() {
                return Err(invalid(
                    line_number,
                    "a pkgname section precedes the pkgbase definition",
                ));
            }
            if document.packages.len() >= MAX_PACKAGES {
                return Err(SrcinfoError::TooManyPackages {
                    limit: MAX_PACKAGES,
                });
            }
            let name = parse_package_name(value, line_number)?;
            if document.packages.iter().any(|package| package.name == name) {
                return Err(SrcinfoError::DuplicatePackage {
                    line: line_number,
                    name,
                });
            }
            document.packages.push(ParsedSrcinfoPackage {
                name,
                sources: Vec::new(),
                sha256sums: Vec::new(),
                sha512sums: Vec::new(),
            });
            Ok(())
        }
        _ => Err(invalid(
            line_number,
            "only pkgbase and pkgname may appear unindented",
        )),
    }
}

/// Handle an indented `key = value` line within the current section. Keys
/// outside the extracted evidence set are ignored for forward compatibility.
fn parse_indented_line(
    document: &mut DocumentBuilder,
    line: &str,
    line_number: usize,
) -> Result<(), SrcinfoError> {
    let (key, value) = split_key_value(line, line_number)?;
    if document.pkgbase.is_none() {
        return Err(invalid(
            line_number,
            "a key precedes the pkgbase definition",
        ));
    }
    // `source` is valid in both sections; singletons below are restricted to
    // the package-base section, where `makepkg --printsrcinfo` emits them.
    let current = document.current_package();
    if key.starts_with("source_") {
        document.has_arch_specific_sources = true;
        return Ok(());
    }
    if matches!(key, "sha256sums" | "sha512sums") {
        let checksums = match (current, key) {
            (Some(index), "sha256sums") => &mut document.packages[index].sha256sums,
            (Some(index), _) => &mut document.packages[index].sha512sums,
            (None, "sha256sums") => &mut document.sha256sums,
            (None, _) => &mut document.sha512sums,
        };
        checksums.push(parse_source_value(value, line_number)?);
        return Ok(());
    }
    if key == "source" {
        let source = parse_source_value(value, line_number)?;
        match current {
            Some(index) => document.packages[index].sources.push(source),
            None => document.sources.push(source),
        }
        return Ok(());
    }
    if current.is_some() {
        return match key {
            "pkgver" | "pkgrel" | "epoch" | "pkgbase" | "pkgname" => Err(invalid(
                line_number,
                "a package-base key appears inside a pkgname section",
            )),
            _ => Ok(()),
        };
    }
    match key {
        "pkgver" => {
            if document.pkgver.is_some() {
                return Err(invalid(line_number, "pkgver appears more than once"));
            }
            document.pkgver = Some(parse_version_value(value, "pkgver", line_number)?);
            Ok(())
        }
        "pkgrel" => {
            if document.pkgrel.is_some() {
                return Err(invalid(line_number, "pkgrel appears more than once"));
            }
            document.pkgrel = Some(parse_version_value(value, "pkgrel", line_number)?);
            Ok(())
        }
        "epoch" => {
            if document.epoch.is_some() {
                return Err(invalid(line_number, "epoch appears more than once"));
            }
            document.epoch = Some(parse_epoch_value(value, line_number)?);
            Ok(())
        }
        "pkgbase" | "pkgname" => Err(invalid(
            line_number,
            "a section header appears inside the package-base section",
        )),
        _ => Ok(()),
    }
}

/// Split a line at the first `=` and trim both sides. Surrounding whitespace
/// is insignificant in `makepkg --printsrcinfo` output.
fn split_key_value(line: &str, line_number: usize) -> Result<(&str, &str), SrcinfoError> {
    let (raw_key, raw_value) = line
        .split_once('=')
        .ok_or(invalid(line_number, "the line has no key-value separator"))?;
    let key = raw_key.trim();
    let value = raw_value.trim();
    if key.is_empty() || key.chars().any(char::is_whitespace) || key.chars().any(char::is_control) {
        return Err(invalid(line_number, "the key is empty or invalid"));
    }
    if value.is_empty() {
        return Err(invalid(line_number, "the value is empty"));
    }
    Ok((key, value))
}

/// Validate a package base: nonempty, bounded by the line limit, and
/// restricted to the portable package-name alphabet shared with the `expac`
/// record parser.
fn parse_package_base(value: &str, line_number: usize) -> Result<String, SrcinfoError> {
    validate_name_like(value, "the package base is invalid")
        .map(|()| value.to_owned())
        .map_err(|message| invalid(line_number, message))
}

/// Validate a package name with the same alphabet as the `expac` parser so
/// both reject the same invalid inputs.
fn parse_package_name(value: &str, line_number: usize) -> Result<String, SrcinfoError> {
    validate_name_like(value, "the package name is invalid")
        .map(|()| value.to_owned())
        .map_err(|message| invalid(line_number, message))
}

fn validate_name_like(value: &str, message: &'static str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err(message);
    }
    if value.len() > MAX_LINE_BYTES {
        return Err(message);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(message);
    }
    if value.chars().any(char::is_control) {
        return Err(message);
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'.' | b'_' | b'+' | b'-')
    }) {
        return Err(message);
    }
    Ok(())
}

/// Validate a `pkgver` or `pkgrel` value: nonempty, bounded, and free of
/// whitespace and control characters.
fn parse_version_value(
    value: &str,
    kind: &'static str,
    line_number: usize,
) -> Result<String, SrcinfoError> {
    let message = if kind == "pkgver" {
        "pkgver is empty or invalid"
    } else {
        "pkgrel is empty or invalid"
    };
    if value.is_empty() || value.len() > MAX_LINE_BYTES {
        return Err(invalid(line_number, message));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(invalid(line_number, message));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(line_number, message));
    }
    Ok(value.to_owned())
}

/// Validate an `epoch` value: canonical ASCII digits.
fn parse_epoch_value(value: &str, line_number: usize) -> Result<String, SrcinfoError> {
    if value.is_empty()
        || value.len() > MAX_LINE_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(line_number, "epoch is not an unsigned integer"));
    }
    Ok(value.to_owned())
}

/// Validate a `source` entry: nonempty and free of control characters. URLs
/// may contain any other characters, including `=` and whitespace-adjacent
/// forms the key splitter already trimmed.
fn parse_source_value(value: &str, line_number: usize) -> Result<String, SrcinfoError> {
    if value.is_empty() || value.len() > MAX_LINE_BYTES {
        return Err(invalid(line_number, "the source entry is empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(line_number, "the source entry is invalid"));
    }
    Ok(value.to_owned())
}

fn invalid(line_number: usize, message: &'static str) -> SrcinfoError {
    SrcinfoError::InvalidLine {
        line: line_number,
        message,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{
        MAX_LINE_BYTES, MAX_PACKAGES, MAX_SRCINFO_BYTES, SrcinfoError, parse_srcinfo_bytes,
    };

    /// Real `makepkg --printsrcinfo` output for a split package, captured on
    /// pacman 7.1.0 from a recipe with `pkgbase`, `epoch`, base-level
    /// sources, and per-package overrides.
    const SPLIT_SAMPLE: &str = "pkgbase = sample-suite\n\
         \tpkgdesc = A sample split package\n\
         \tpkgver = 2.0\n\
         \tpkgrel = 1\n\
         \tepoch = 1\n\
         \turl = https://example.invalid\n\
         \tarch = x86_64\n\
         \tlicense = MIT\n\
         \tmakedepends = git\n\
         \tdepends = glibc\n\
         \tsource = https://example.invalid/sample-2.0.tar.gz\n\
         \tsha256sums = SKIP\n\
         \n\
         pkgname = sample-cli\n\
         \tpkgdesc = CLI tool\n\
         \tdepends = glibc\n\
         \tdepends = sample-lib\n\
         \n\
         pkgname = sample-docs\n\
         \tpkgdesc = Docs\n\
         \tarch = any\n";

    /// Real `makepkg --printsrcinfo` output for a non-split package: the
    /// package base defaults to the package name and the `pkgname` section
    /// carries no indented keys.
    const SINGLE_SAMPLE: &str = "pkgbase = lone-tool\n\
         \tpkgdesc = A lone tool\n\
         \tpkgver = 3.1\n\
         \tpkgrel = 2\n\
         \tarch = x86_64\n\
         \tlicense = Apache-2.0\n\
         \tsource = https://example.invalid/lone.tar.gz\n\
         \tsha256sums = SKIP\n\
         \n\
         pkgname = lone-tool\n";

    #[test]
    fn parses_a_split_package_recipe() {
        let parsed = parse_srcinfo_bytes(SPLIT_SAMPLE.as_bytes()).unwrap();
        assert_eq!(parsed.pkgbase, "sample-suite");
        assert_eq!(parsed.pkgver, "2.0");
        assert_eq!(parsed.pkgrel, "1");
        assert_eq!(parsed.epoch.as_deref(), Some("1"));
        assert_eq!(
            parsed.sources,
            ["https://example.invalid/sample-2.0.tar.gz"]
        );
        assert_eq!(parsed.packages.len(), 2);
        assert_eq!(parsed.packages[0].name, "sample-cli");
        assert!(parsed.packages[0].sources.is_empty());
        assert_eq!(parsed.packages[1].name, "sample-docs");
    }

    #[test]
    fn parses_a_non_split_package_recipe() {
        let parsed = parse_srcinfo_bytes(SINGLE_SAMPLE.as_bytes()).unwrap();
        assert_eq!(parsed.pkgbase, "lone-tool");
        assert_eq!(parsed.pkgver, "3.1");
        assert_eq!(parsed.pkgrel, "2");
        assert_eq!(parsed.epoch, None);
        assert_eq!(parsed.packages.len(), 1);
        assert_eq!(parsed.packages[0].name, "lone-tool");
    }

    #[test]
    fn keeps_a_source_url_containing_equals() {
        let input = "pkgbase = queried\n\
             \tpkgver = 1.0\n\
             \tpkgrel = 1\n\
             \tsource = https://example.invalid/fetch?token=a%3Db&file=x.tar.gz\n\
             \n\
             pkgname = queried\n";
        let parsed = parse_srcinfo_bytes(input.as_bytes()).unwrap();
        assert_eq!(
            parsed.sources,
            ["https://example.invalid/fetch?token=a%3Db&file=x.tar.gz"]
        );
    }

    #[test]
    fn ignores_unknown_keys_for_forward_compatibility() {
        let input = "pkgbase = future\n\
             \tpkgver = 1.0\n\
             \tpkgrel = 1\n\
             \tsome-new-key = some-new-value\n\
             \n\
             pkgname = future\n\
             \tsome-new-key = some-new-value\n";
        let parsed = parse_srcinfo_bytes(input.as_bytes()).unwrap();
        assert_eq!(parsed.pkgbase, "future");
        assert_eq!(parsed.packages.len(), 1);
    }

    #[test]
    fn rejects_non_utf8_without_panicking() {
        let error = parse_srcinfo_bytes(b"pkgbase = \xff\n").unwrap_err();
        assert_eq!(error, SrcinfoError::NotUtf8);
    }

    #[test]
    fn rejects_oversized_input_before_parsing() {
        let error = parse_srcinfo_bytes(&vec![b'a'; MAX_SRCINFO_BYTES + 1]).unwrap_err();
        assert!(matches!(error, SrcinfoError::InputTooLarge { .. }));
    }

    #[test]
    fn rejects_an_oversized_line() {
        let input = format!("pkgbase = {}\n", "b".repeat(MAX_LINE_BYTES));
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::LineTooLong { line: 1, .. }));
    }

    #[test]
    fn accepts_a_line_of_exactly_the_limit() {
        // `\tsource = ` is 10 bytes, so the value pads the line to exactly
        // MAX_LINE_BYTES; one more byte must be rejected.
        let accepted = format!(
            "pkgbase = boundary\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = {}\n\npkgname = boundary\n",
            "s".repeat(MAX_LINE_BYTES - 10)
        );
        let parsed = parse_srcinfo_bytes(accepted.as_bytes()).unwrap();
        assert_eq!(parsed.sources.len(), 1);
        let rejected = accepted.replace("\tsource = s", "\tsource = ss");
        let error = parse_srcinfo_bytes(rejected.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::LineTooLong { .. }));
    }

    #[test]
    fn rejects_input_not_starting_with_pkgbase() {
        let error = parse_srcinfo_bytes("pkgname = orphan\n".as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::InvalidLine { line: 1, .. }));
    }

    #[test]
    fn rejects_an_unindented_non_section_key() {
        let input = "pkgbase = broken\nurl = https://example.invalid\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::InvalidLine { line: 2, .. }));
    }

    #[test]
    fn rejects_a_missing_pkgver() {
        let input = "pkgbase = incomplete\n\tpkgrel = 1\n\npkgname = incomplete\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::MissingField { .. }));
    }

    #[test]
    fn rejects_a_missing_pkgname_section() {
        let input = "pkgbase = lonely\n\tpkgver = 1.0\n\tpkgrel = 1\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::MissingField { .. }));
    }

    #[test]
    fn rejects_a_duplicate_package() {
        let input = "pkgbase = suite\n\
             \tpkgver = 1.0\n\
             \tpkgrel = 1\n\
             \n\
             pkgname = member\n\
             \n\
             pkgname = member\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(
            error,
            SrcinfoError::DuplicatePackage { line: 7, .. }
        ));
    }

    #[test]
    fn rejects_a_version_key_inside_a_package_section() {
        let input = "pkgbase = suite\n\
             \tpkgver = 1.0\n\
             \tpkgrel = 1\n\
             \n\
             pkgname = member\n\
             \tpkgver = 9.9\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::InvalidLine { line: 6, .. }));
    }

    #[test]
    fn rejects_an_invalid_package_name() {
        let input = "pkgbase = suite\n\
             \tpkgver = 1.0\n\
             \tpkgrel = 1\n\
             \n\
             pkgname = bad name\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::InvalidLine { line: 5, .. }));
    }

    #[test]
    fn rejects_a_non_numeric_epoch() {
        let input =
            "pkgbase = dated\n\tpkgver = 1.0\n\tpkgrel = 1\n\tepoch = 1a\n\npkgname = dated\n";
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert!(matches!(error, SrcinfoError::InvalidLine { line: 4, .. }));
    }

    #[test]
    fn rejects_too_many_package_sections() {
        let mut input = String::from("pkgbase = crowded\n\tpkgver = 1.0\n\tpkgrel = 1\n");
        for index in 0..=MAX_PACKAGES {
            input.push_str(&format!("pkgname = member-{index}\n"));
        }
        let error = parse_srcinfo_bytes(input.as_bytes()).unwrap_err();
        assert_eq!(
            error,
            SrcinfoError::TooManyPackages {
                limit: MAX_PACKAGES
            }
        );
    }

    proptest! {
        /// Arbitrary bytes never panic and never allocate without bounds: the
        /// byte-level parser returns a valid document or a typed error.
        #[test]
        fn arbitrary_bytes_never_panic_and_stay_bounded(
            bytes in prop::collection::vec(any::<u8>(), 0..8192),
        ) {
            let result = parse_srcinfo_bytes(&bytes);
            match result {
            Ok(document) => {
                prop_assert!(document.packages.len() <= MAX_PACKAGES);
                prop_assert!(document.pkgbase.len() <= MAX_LINE_BYTES);
                prop_assert!(document.pkgver.len() <= MAX_LINE_BYTES);
                prop_assert!(document.pkgrel.len() <= MAX_LINE_BYTES);
                if let Some(epoch) = &document.epoch {
                    prop_assert!(epoch.len() <= MAX_LINE_BYTES);
                }
                for source in &document.sources {
                    prop_assert!(source.len() <= MAX_LINE_BYTES);
                }
                for package in &document.packages {
                    prop_assert!(package.name.len() <= MAX_LINE_BYTES);
                    for source in &package.sources {
                        prop_assert!(source.len() <= MAX_LINE_BYTES);
                    }
                }
            }
                Err(error) => {
                    // Oversized input must surface the typed bound error.
                    if bytes.len() > MAX_SRCINFO_BYTES {
                        let is_bounded =
                            matches!(error, SrcinfoError::InputTooLarge { .. });
                        prop_assert!(is_bounded);
                    }
                }
            }
        }
    }
}
