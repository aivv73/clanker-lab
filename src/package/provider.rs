/// A source provider: an adapter that uses package-origin metadata to find
/// package recipes and source evidence. The name identifies the adapter
/// (for example `arch` or `aur`); unsupported origins stay visible as
/// unresolved attempts rather than being reclassified.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceProvider(String);

impl SourceProvider {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        if value.is_empty() {
            return Err("source provider is empty");
        }
        if value.chars().any(char::is_control) {
            return Err("source provider contains a control character");
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The strength of an analysis target mapping: exact, reconstructed,
/// fallback, or override. It grades one provider attempt, not the target
/// itself; the same target reached through different evidence grades
/// differently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EvidenceGrade {
    Exact,
    Reconstructed,
    Fallback,
    Override,
}

impl EvidenceGrade {
    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "exact" => Ok(Self::Exact),
            "reconstructed" => Ok(Self::Reconstructed),
            "fallback" => Ok(Self::Fallback),
            "override" => Ok(Self::Override),
            _ => Err("evidence grade is none of exact, reconstructed, fallback, override"),
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Reconstructed => "reconstructed",
            Self::Fallback => "fallback",
            Self::Override => "override",
        }
    }
}

/// A canonical source location, revision, and optional subdirectory analyzed
/// once. Several installed packages may share one analysis target; absent
/// revision or subdirectory is `None` and persists as an empty string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AnalysisTarget {
    pub(crate) location: String,
    pub(crate) revision: Option<String>,
    pub(crate) subdirectory: Option<String>,
}

/// One source provider's recorded evidence or failure while resolving an
/// installed package version. The latest outcome from each provider is
/// preserved; candidates from different providers have no winner selected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AttemptOutcome {
    Resolved {
        target: AnalysisTarget,
        grade: EvidenceGrade,
    },
    Unresolved {
        reason: String,
    },
}

/// A single provider attempt for one installed package version, keyed by
/// package name, package version, and provider. Attempts are global to the
/// database rather than attached to one snapshot, so later refreshes reuse
/// successful analyses and snapshot retention never prunes them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderAttempt {
    pub(crate) package_name: String,
    pub(crate) package_version: String,
    pub(crate) provider: SourceProvider,
    pub(crate) outcome: AttemptOutcome,
    pub(crate) recipe: Option<RecipeProvenance>,
}

/// Exact recipe commit returned by the metadata service. The attempt's
/// recorded_at timestamp records freshness; legacy attempts have no provenance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecipeProvenance {
    location: String,
    commit: String,
}

impl RecipeProvenance {
    pub(crate) fn new(location: &str, commit: &str) -> Result<Self, &'static str> {
        if location.is_empty() || location.len() > 4096 || location.chars().any(char::is_control) {
            return Err("invalid recipe location");
        }
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("recipe commit must be a full Git object ID");
        }
        Ok(Self {
            location: location.to_owned(),
            commit: commit.to_owned(),
        })
    }

    pub(crate) fn location(&self) -> &str {
        &self.location
    }
    pub(crate) fn commit(&self) -> &str {
        &self.commit
    }
}
