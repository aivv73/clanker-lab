#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};

use sha2::{Digest, Sha256};

use crate::expac_parser;
use crate::interrupt;
use crate::package::{InstallReason, PackageOrigin, SnapshotPackage};

// expac fields: name, version, package base, queried database, install reason,
// installed bytes. Keep this order aligned with expac_parser::parse_record_line.
const LOCAL_EXPAC_LAYOUT: &str = r"%n\t%v\t%e\t%r\t%w\t%m";

#[derive(Debug)]
pub(crate) enum InventoryError {
    Interrupted,
    StartCommand {
        command: &'static str,
        source: std::io::Error,
    },
    CommandFailed {
        command: &'static str,
        status: String,
        stderr: String,
    },
    NonUtf8Output(&'static str),
    OutputTooLarge {
        command: &'static str,
        limit: usize,
        actual: usize,
    },
    TooManyPackages {
        command: &'static str,
        limit: usize,
    },
    ReadCommandOutput {
        command: &'static str,
        source: std::io::Error,
    },
    InvalidPacmanLine(usize),
    InvalidPacmanFingerprintLine(usize),
    DuplicatePacmanName(String),
    InvalidExpacRecord {
        line: usize,
        message: String,
    },
    DuplicateExpacName(String),
    PopulationMismatch {
        missing_metadata: Vec<String>,
        unexpected_metadata: Vec<String>,
    },
    InventoryChanged,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Interrupted => formatter.write_str("inventory refresh was interrupted"),
            Self::StartCommand { command, source } => {
                write!(formatter, "cannot start {command}: {source}")
            }
            Self::CommandFailed {
                command,
                status,
                stderr,
            } => {
                write!(formatter, "{command} exited with {status}")?;
                if !stderr.is_empty() {
                    write!(formatter, ": {stderr}")?;
                }
                Ok(())
            }
            Self::NonUtf8Output(command) => {
                write!(formatter, "{command} returned output that is not UTF-8")
            }
            Self::OutputTooLarge {
                command,
                limit,
                actual,
            } => write!(
                formatter,
                "{command} returned {actual} bytes, which exceeds the {limit}-byte capture limit"
            ),
            Self::TooManyPackages { command, limit } => {
                write!(formatter, "{command} returned more than {limit} packages")
            }
            Self::ReadCommandOutput { command, source } => {
                write!(formatter, "cannot read {command} output: {source}")
            }
            Self::InvalidPacmanLine(line) => {
                write!(
                    formatter,
                    "pacman returned an invalid package name on line {line}"
                )
            }
            Self::InvalidPacmanFingerprintLine(line) => write!(
                formatter,
                "pacman returned an invalid name and version on line {line}"
            ),
            Self::DuplicatePacmanName(name) => {
                write!(formatter, "pacman returned package {name:?} more than once")
            }
            Self::InvalidExpacRecord { line, message } => {
                write!(
                    formatter,
                    "expac returned an invalid record on line {line}: {message}"
                )
            }
            Self::DuplicateExpacName(name) => {
                write!(formatter, "expac returned package {name:?} more than once")
            }
            Self::PopulationMismatch {
                missing_metadata,
                unexpected_metadata,
            } => write!(
                formatter,
                "pacman and expac returned different package populations (missing metadata: {}; unexpected metadata: {})",
                display_names(missing_metadata),
                display_names(unexpected_metadata)
            ),
            Self::InventoryChanged => formatter
                .write_str("the installed package inventory changed while refresh was reading it"),
        }
    }
}

impl InventoryError {
    pub(crate) const fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

#[derive(Debug)]
pub(crate) struct InstalledPackage {
    name: String,
    version: String,
    package_base: Option<String>,
    origin: PackageOrigin,
    install_reason: InstallReason,
    installed_bytes: i64,
}

/// SHA-256 digest over sorted `name + NUL + version + newline` entries.
/// A lightweight sample of the live `pacman -Q` population used to detect
/// package changes around metadata capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InventoryFingerprint(String);

impl InventoryFingerprint {
    fn from_name_versions<'a>(values: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut digest = Sha256::new();
        for (name, version) in values {
            digest.update(name.as_bytes());
            digest.update([0]);
            digest.update(version.as_bytes());
            digest.update(b"\n");
        }
        Self(format!("{:x}", digest.finalize()))
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("inventory fingerprint is not a SHA-256 digest");
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A coherent package capture: its fingerprint matched at least one of the
/// lightweight `pacman -Q` samples taken before and after the metadata read.
/// `stale` is true when the capture matched the earlier sample only, meaning
/// packages changed after the snapshot was read.
#[derive(Debug)]
pub(crate) struct CapturedInventory {
    packages: Vec<InstalledPackage>,
    fingerprint: InventoryFingerprint,
    stale: bool,
}

impl CapturedInventory {
    pub(crate) fn packages(&self) -> &[InstalledPackage] {
        &self.packages
    }

    pub(crate) fn fingerprint(&self) -> &InventoryFingerprint {
        &self.fingerprint
    }

    pub(crate) const fn is_stale(&self) -> bool {
        self.stale
    }

    pub(crate) fn snapshot_packages(&self) -> Vec<SnapshotPackage> {
        self.packages
            .iter()
            .map(|package| SnapshotPackage {
                name: package.name.clone(),
                origin: package.origin.clone(),
                install_reason: package.install_reason,
            })
            .collect()
    }
}

impl InstalledPackage {
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    pub(crate) fn package_base(&self) -> Option<&str> {
        self.package_base.as_deref()
    }

    pub(crate) fn origin(&self) -> &str {
        self.origin.as_str()
    }

    pub(crate) const fn install_reason(&self) -> &'static str {
        self.install_reason.as_str()
    }

    pub(crate) const fn installed_bytes(&self) -> i64 {
        self.installed_bytes
    }
}

pub(crate) fn capture_installed_inventory() -> Result<CapturedInventory, InventoryError> {
    let before = read_live_fingerprint()?;
    let pacman_output = run_command("pacman", &["-Qq"])?;
    let local_expac_output = run_command("expac", &["-Q", LOCAL_EXPAC_LAYOUT])?;
    let pacman_stdout = parse_stdout("pacman", &pacman_output.stdout)?;
    let local_expac_stdout = parse_stdout("expac", &local_expac_output.stdout)?;
    let pacman_names = parse_pacman_names(pacman_stdout)?;
    let packages = parse_expac_records(local_expac_stdout)?;
    validate_same_population(&pacman_names, &packages)?;
    let fingerprint = InventoryFingerprint::from_name_versions(
        packages
            .values()
            .map(|package| (package.name(), package.version())),
    );
    let after = read_live_fingerprint()?;
    if interrupt::is_requested() {
        return Err(InventoryError::Interrupted);
    }
    if fingerprint != before && fingerprint != after {
        return Err(InventoryError::InventoryChanged);
    }
    let stale = fingerprint != after;

    Ok(CapturedInventory {
        packages: packages.into_values().collect(),
        fingerprint,
        stale,
    })
}

pub(crate) fn read_live_fingerprint() -> Result<InventoryFingerprint, InventoryError> {
    let output = run_command("pacman", &["-Q"])?;
    let stdout = parse_stdout("pacman", &output.stdout)?;
    let mut packages = BTreeMap::new();
    for (index, line) in stdout.lines().enumerate() {
        let line_number = index + 1;
        if line.len() > expac_parser::MAX_FIELD_BYTES * 2 {
            return Err(InventoryError::InvalidPacmanFingerprintLine(line_number));
        }
        let (name, version) = line
            .split_once(' ')
            .ok_or(InventoryError::InvalidPacmanFingerprintLine(line_number))?;
        expac_parser::validate_package_name(name)
            .map_err(|_| InventoryError::InvalidPacmanFingerprintLine(line_number))?;
        expac_parser::validate_package_version(version)
            .map_err(|_| InventoryError::InvalidPacmanFingerprintLine(line_number))?;
        if packages
            .insert(name.to_owned(), version.to_owned())
            .is_some()
        {
            return Err(InventoryError::DuplicatePacmanName(name.to_owned()));
        }
        if packages.len() > expac_parser::MAX_PACKAGES {
            return Err(InventoryError::TooManyPackages {
                command: "pacman",
                limit: expac_parser::MAX_PACKAGES,
            });
        }
    }
    Ok(InventoryFingerprint::from_name_versions(
        packages
            .iter()
            .map(|(name, version)| (name.as_str(), version.as_str())),
    ))
}

fn run_command(command: &'static str, arguments: &[&str]) -> Result<Output, InventoryError> {
    if interrupt::is_requested() {
        return Err(InventoryError::Interrupted);
    }
    let child = Command::new(command)
        .args(arguments)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| InventoryError::StartCommand { command, source })?;
    let active_group = interrupt::ActiveProcessGroup::register(child.id());
    let output = child
        .wait_with_output()
        .map_err(|source| InventoryError::ReadCommandOutput { command, source })?;
    drop(active_group);
    if interrupt::is_requested() {
        return Err(InventoryError::Interrupted);
    }
    if output.status.success() {
        if output.stdout.len() > expac_parser::MAX_STDOUT_BYTES {
            return Err(InventoryError::OutputTooLarge {
                command,
                limit: expac_parser::MAX_STDOUT_BYTES,
                actual: output.stdout.len(),
            });
        }
        return Ok(output);
    }

    Err(InventoryError::CommandFailed {
        command,
        status: output.status.to_string(),
        stderr: expac_parser::truncate_stderr_context(&output.stderr),
    })
}

fn parse_stdout<'a>(command: &'static str, stdout: &'a [u8]) -> Result<&'a str, InventoryError> {
    if stdout.len() > expac_parser::MAX_STDOUT_BYTES {
        return Err(InventoryError::OutputTooLarge {
            command,
            limit: expac_parser::MAX_STDOUT_BYTES,
            actual: stdout.len(),
        });
    }
    std::str::from_utf8(stdout).map_err(|_| InventoryError::NonUtf8Output(command))
}

fn parse_pacman_names(stdout: &str) -> Result<BTreeSet<String>, InventoryError> {
    if stdout.len() > expac_parser::MAX_STDOUT_BYTES {
        return Err(InventoryError::OutputTooLarge {
            command: "pacman",
            limit: expac_parser::MAX_STDOUT_BYTES,
            actual: stdout.len(),
        });
    }
    let mut names = BTreeSet::new();
    for (index, line) in stdout.lines().enumerate() {
        let line_number = index + 1;
        if line.len() > expac_parser::MAX_FIELD_BYTES {
            return Err(InventoryError::InvalidPacmanLine(line_number));
        }
        expac_parser::validate_package_name(line)
            .map_err(|_| InventoryError::InvalidPacmanLine(line_number))?;
        if !names.insert(line.to_owned()) {
            return Err(InventoryError::DuplicatePacmanName(line.to_owned()));
        }
        if names.len() > expac_parser::MAX_PACKAGES {
            return Err(InventoryError::TooManyPackages {
                command: "pacman",
                limit: expac_parser::MAX_PACKAGES,
            });
        }
    }
    Ok(names)
}

fn parse_expac_records(stdout: &str) -> Result<BTreeMap<String, InstalledPackage>, InventoryError> {
    let parsed =
        expac_parser::parse_expac_bytes(stdout.as_bytes()).map_err(|error| match error {
            expac_parser::ParseError::NotUtf8 => InventoryError::NonUtf8Output("expac"),
            expac_parser::ParseError::OutputTooLarge { limit, actual } => {
                InventoryError::OutputTooLarge {
                    command: "expac",
                    limit,
                    actual,
                }
            }
            expac_parser::ParseError::TooManyPackages { limit } => {
                InventoryError::TooManyPackages {
                    command: "expac",
                    limit,
                }
            }
            expac_parser::ParseError::RecordTooLong { line, limit } => {
                InventoryError::InvalidExpacRecord {
                    line,
                    message: format!("record exceeds the {limit}-byte limit"),
                }
            }
            expac_parser::ParseError::FieldTooLong { line, limit } => {
                InventoryError::InvalidExpacRecord {
                    line,
                    message: format!("a field exceeds the {limit}-byte limit"),
                }
            }
            expac_parser::ParseError::InvalidRecord { line, message } => {
                InventoryError::InvalidExpacRecord {
                    line,
                    message: message.to_owned(),
                }
            }
            expac_parser::ParseError::DuplicateName { name, .. } => {
                InventoryError::DuplicateExpacName(name)
            }
        })?;
    let mut packages = BTreeMap::new();
    for record in parsed {
        let package = InstalledPackage {
            name: record.name.clone(),
            version: record.version,
            package_base: record.package_base,
            origin: PackageOrigin::unknown(),
            install_reason: match record.install_reason {
                expac_parser::ParsedInstallReason::Explicit => InstallReason::Explicit,
                expac_parser::ParsedInstallReason::Dependency => InstallReason::Dependency,
            },
            installed_bytes: record.installed_bytes,
        };
        let name = package.name().to_owned();
        // The bounded parser already rejects duplicates, so this insert
        // cannot fail; keep the check as defense in depth.
        if packages.insert(name.clone(), package).is_some() {
            return Err(InventoryError::DuplicateExpacName(name));
        }
    }
    Ok(packages)
}

fn validate_same_population(
    pacman_names: &BTreeSet<String>,
    packages: &BTreeMap<String, InstalledPackage>,
) -> Result<(), InventoryError> {
    let expac_names = packages.keys().cloned().collect::<BTreeSet<_>>();
    let missing_metadata = pacman_names
        .difference(&expac_names)
        .cloned()
        .collect::<Vec<_>>();
    let unexpected_metadata = expac_names
        .difference(pacman_names)
        .cloned()
        .collect::<Vec<_>>();
    if missing_metadata.is_empty() && unexpected_metadata.is_empty() {
        Ok(())
    } else {
        Err(InventoryError::PopulationMismatch {
            missing_metadata,
            unexpected_metadata,
        })
    }
}
