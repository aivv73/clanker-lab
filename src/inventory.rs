#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::process::{Command, Output};

use crate::package::{InstallReason, PackageOrigin};

// expac fields: name, version, package base, queried database, install reason,
// installed bytes. Keep this order aligned with LocalExpacRecord::parse.
const LOCAL_EXPAC_LAYOUT: &str = r"%n\t%v\t%e\t%r\t%w\t%m";

#[derive(Debug)]
pub(crate) enum InventoryError {
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
    InvalidPacmanLine(usize),
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
}

impl fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::InvalidPacmanLine(line) => {
                write!(
                    formatter,
                    "pacman returned an invalid package name on line {line}"
                )
            }
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
        }
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
    name: PackageName,
    version: PackageVersion,
    package_base: Option<PackageBase>,
    origin: PackageOrigin,
    install_reason: InstallReason,
    installed_bytes: InstalledBytes,
}

impl InstalledPackage {
    pub(crate) fn name(&self) -> &str {
        &self.name.0
    }

    pub(crate) fn version(&self) -> &str {
        &self.version.0
    }

    pub(crate) fn package_base(&self) -> Option<&str> {
        self.package_base.as_ref().map(|value| value.0.as_str())
    }

    pub(crate) fn origin(&self) -> &str {
        self.origin.as_str()
    }

    pub(crate) const fn install_reason(&self) -> &'static str {
        self.install_reason.as_str()
    }

    pub(crate) const fn installed_bytes(&self) -> i64 {
        self.installed_bytes.0
    }
}

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PackageName(String);

impl PackageName {
    fn parse(value: &str) -> Result<Self, &'static str> {
        parse_nonempty_field(value, "package name")?;
        if value.chars().any(char::is_whitespace) {
            return Err("package name contains whitespace");
        }
        Ok(Self(value.to_owned()))
    }
}

#[derive(Debug)]
struct PackageVersion(String);

impl PackageVersion {
    fn parse(value: &str) -> Result<Self, &'static str> {
        parse_nonempty_field(value, "package version")?;
        Ok(Self(value.to_owned()))
    }
}

#[derive(Debug)]
struct PackageBase(String);

impl PackageBase {
    fn parse_optional(value: &str) -> Result<Option<Self>, &'static str> {
        if value.is_empty() {
            Ok(None)
        } else {
            parse_plain_field(value, "package base")?;
            Ok(Some(Self(value.to_owned())))
        }
    }
}

#[derive(Debug)]
struct InstalledBytes(i64);

impl InstalledBytes {
    fn parse(value: &str) -> Result<Self, &'static str> {
        let bytes = value
            .parse::<u64>()
            .map_err(|_| "installed bytes is not an unsigned integer")?;
        let bytes = i64::try_from(bytes).map_err(|_| "installed bytes exceeds SQLite's range")?;
        Ok(Self(bytes))
    }
}

struct LocalExpacRecord<'a> {
    name: &'a str,
    version: &'a str,
    package_base: &'a str,
    queried_database: &'a str,
    install_reason: &'a str,
    installed_bytes: &'a str,
}

impl<'a> LocalExpacRecord<'a> {
    fn parse(line: &'a str) -> Result<Self, &'static str> {
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
            return Err("expected six tab-delimited fields");
        };
        Ok(Self {
            name,
            version,
            package_base,
            queried_database,
            install_reason,
            installed_bytes,
        })
    }

    fn into_package(self) -> Result<InstalledPackage, &'static str> {
        if self.queried_database != "local" {
            return Err("queried database is not local");
        }
        Ok(InstalledPackage {
            name: PackageName::parse(self.name)?,
            version: PackageVersion::parse(self.version)?,
            package_base: PackageBase::parse_optional(self.package_base)?,
            origin: PackageOrigin::unknown(),
            install_reason: InstallReason::parse(self.install_reason)?,
            installed_bytes: InstalledBytes::parse(self.installed_bytes)?,
        })
    }
}

fn parse_nonempty_field(value: &str, field: &'static str) -> Result<(), &'static str> {
    if value.is_empty() {
        return match field {
            "package name" => Err("package name is empty"),
            "package version" => Err("package version is empty"),
            _ => Err("required field is empty"),
        };
    }
    parse_plain_field(value, field)
}

fn parse_plain_field(value: &str, field: &'static str) -> Result<(), &'static str> {
    if value.chars().any(char::is_control) {
        return match field {
            "package base" => Err("package base contains a control character"),
            "package version" => Err("package version contains a control character"),
            _ => Err("field contains a control character"),
        };
    }
    Ok(())
}

pub(crate) fn read_installed_packages() -> Result<Vec<InstalledPackage>, InventoryError> {
    let pacman_output = run_command("pacman", &["-Qq"])?;
    let local_expac_output = run_command("expac", &["-Q", LOCAL_EXPAC_LAYOUT])?;
    let pacman_stdout = parse_stdout("pacman", &pacman_output)?;
    let local_expac_stdout = parse_stdout("expac", &local_expac_output)?;
    let pacman_names = parse_pacman_names(pacman_stdout)?;
    let packages = parse_expac_records(local_expac_stdout)?;
    validate_same_population(&pacman_names, &packages)?;

    Ok(packages.into_values().collect())
}

fn run_command(command: &'static str, arguments: &[&str]) -> Result<Output, InventoryError> {
    let output = Command::new(command)
        .args(arguments)
        .output()
        .map_err(|source| InventoryError::StartCommand { command, source })?;
    if output.status.success() {
        return Ok(output);
    }

    Err(InventoryError::CommandFailed {
        command,
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

fn parse_stdout<'a>(command: &'static str, output: &'a Output) -> Result<&'a str, InventoryError> {
    std::str::from_utf8(&output.stdout).map_err(|_| InventoryError::NonUtf8Output(command))
}

fn parse_pacman_names(stdout: &str) -> Result<BTreeSet<String>, InventoryError> {
    let mut names = BTreeSet::new();
    for (index, line) in stdout.lines().enumerate() {
        let name =
            PackageName::parse(line).map_err(|_| InventoryError::InvalidPacmanLine(index + 1))?;
        if !names.insert(name.0.clone()) {
            return Err(InventoryError::DuplicatePacmanName(name.0));
        }
    }
    Ok(names)
}

fn parse_expac_records(stdout: &str) -> Result<BTreeMap<String, InstalledPackage>, InventoryError> {
    let mut packages = BTreeMap::new();
    for (index, line) in stdout.lines().enumerate() {
        let line_number = index + 1;
        let record = LocalExpacRecord::parse(line).map_err(|message| {
            InventoryError::InvalidExpacRecord {
                line: line_number,
                message: message.to_owned(),
            }
        })?;
        let package =
            record
                .into_package()
                .map_err(|message| InventoryError::InvalidExpacRecord {
                    line: line_number,
                    message: message.to_owned(),
                })?;
        let name = package.name().to_owned();
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
