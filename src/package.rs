/// Source-provider evidence vocabulary (ADR 0001, ADR 0002, ADR 0005).
/// Not yet consumed: the fetch slice records and reads these through the
/// storage API (issue #18). Remove the allowance once it does.
#[allow(dead_code)]
pub(crate) mod provider;

use std::str::FromStr;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallReason {
    Explicit,
    Dependency,
}

impl InstallReason {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "explicit" => Ok(Self::Explicit),
            "dependency" => Ok(Self::Dependency),
            _ => Err("install reason is neither explicit nor dependency"),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Dependency => "dependency",
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct PackageOrigin(String);

impl PackageOrigin {
    pub(crate) fn unknown() -> Self {
        Self("unknown".to_owned())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() {
            return Err("package origin is empty");
        }
        if value.chars().any(char::is_control) {
            return Err("package origin contains a control character");
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for PackageOrigin {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotPackage {
    pub(crate) origin: PackageOrigin,
    pub(crate) install_reason: InstallReason,
}
