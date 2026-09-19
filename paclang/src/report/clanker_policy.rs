//! Subjective policy only. Measurement and source selection live elsewhere.
//! The v1 tier overrides are frozen separately from terminal presentation.
//! Applicability and groups come from the bundled Linguist registry snapshot.
pub(super) const ID: &str = "clanker-language-tierlist-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum Tier {
    #[serde(rename = "S+")]
    SPlus,
    S,
    A,
    B,
    C,
    D,
    F,
}

impl Tier {
    pub(super) const fn weight(self) -> u8 {
        match self {
            Self::SPlus => 6,
            Self::S => 5,
            Self::A => 4,
            Self::B => 3,
            Self::C => 2,
            Self::D => 1,
            Self::F => 0,
        }
    }
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::SPlus => "S+",
            Self::S => "S",
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
            Self::F => "F",
        }
    }
}

/// Nearest ordinal tier, with half-way values rounded toward the higher tier.
/// u128 retains exact byte weighting without floating-point rounding.
pub(super) fn aggregate(weighted_sum: u128, weighted_bytes: u128) -> Option<Tier> {
    if weighted_bytes == 0 {
        return None;
    }
    let rounded = (weighted_sum + weighted_bytes / 2) / weighted_bytes;
    Some(match rounded {
        0 => Tier::F,
        1 => Tier::D,
        2 => Tier::C,
        3 => Tier::B,
        4 => Tier::A,
        5 => Tier::S,
        _ => Tier::SPlus,
    })
}

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum LanguageType {
    Programming,
    Markup,
    Prose,
    Data,
}

#[derive(serde::Deserialize)]
pub(super) struct Registry {
    pub id: String,
    pub source_sha256: String,
    languages: BTreeMap<String, Metadata>,
}

#[derive(serde::Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    language_type: LanguageType,
    group: Option<String>,
}

pub(super) fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(include_str!("data/linguist-language-types-v1.json"))
            .expect("bundled Linguist type registry must be valid")
    })
}

fn overrides() -> &'static BTreeMap<String, Option<Tier>> {
    static POLICY: OnceLock<BTreeMap<String, Option<Tier>>> = OnceLock::new();
    POLICY.get_or_init(|| {
        serde_json::from_str(include_str!("data/clanker-language-tierlist-v1.json"))
            .expect("bundled clanker tier policy must be valid")
    })
}

#[derive(Debug)]
pub(super) struct Assignment {
    pub language_type: Option<LanguageType>,
    pub tier: Option<Tier>,
    pub inherited_from: Option<&'static str>,
}

pub(super) fn lookup(language: &str) -> Assignment {
    let registry = registry();
    // Explicit project lore; it contributes only when actually measured.
    let language_type = match language {
        "Bend 2" | "Psi" => Some(LanguageType::Programming),
        _ => registry
            .languages
            .get(language)
            .map(|row| row.language_type),
    };
    let mut result = Assignment {
        language_type,
        tier: None,
        inherited_from: None,
    };
    // Unknown registry entries are not silently declared programming or ancillary.
    if language_type != Some(LanguageType::Programming) {
        return result;
    }
    let policy = overrides();
    if let Some(tier) = policy.get(language) {
        result.tier = *tier;
        return result;
    }
    let mut parent = registry
        .languages
        .get(language)
        .and_then(|row| row.group.as_deref());
    let mut visited = BTreeSet::from([language]);
    while let Some(name) = parent {
        if !visited.insert(name) {
            break;
        }
        let Some(metadata) = registry.languages.get(name) else {
            break;
        };
        if metadata.language_type != LanguageType::Programming {
            break;
        }
        if let Some(tier) = policy.get(name) {
            result.tier = *tier;
            result.inherited_from = Some(name);
            break;
        }
        parent = metadata.group.as_deref();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_policy_and_unknowns() {
        for (name, tier) in [
            ("Rust", Tier::SPlus),
            ("Smalltalk", Tier::S),
            ("Zig", Tier::A),
            ("C", Tier::C),
            ("C++", Tier::C),
            ("Python", Tier::D),
            ("Ruby", Tier::F),
            ("1C Enterprise", Tier::F),
            ("Bend 2", Tier::SPlus),
            ("Psi", Tier::SPlus),
        ] {
            assert_eq!(lookup(name).tier, Some(tier), "{name}");
        }
        for name in ["Perl", "Prolog", "Omgrofl"] {
            assert_eq!(lookup(name).tier, None);
            assert_eq!(lookup(name).language_type, Some(LanguageType::Programming));
        }
        assert_eq!(lookup("Future Unregistered Language").language_type, None);
    }

    #[test]
    fn groups_inherit_tiers_but_explicit_decisions_win() {
        for (child, parent) in [
            ("C2hs Haskell", "Haskell"),
            ("Literate Haskell", "Haskell"),
            ("Literate Agda", "Agda"),
            ("Python console", "Python"),
            ("Snakemake", "Python"),
            ("Lean 4", "Lean"),
            ("Julia REPL", "Julia"),
            ("TSX", "TypeScript"),
        ] {
            let assignment = lookup(child);
            assert_eq!(assignment.tier, lookup(parent).tier);
            assert_eq!(assignment.inherited_from, Some(parent));
        }
        for (name, tier) in [
            ("fish", Tier::B),
            ("Tcsh", Tier::D),
            ("Java Server Pages", Tier::C),
            ("JavaScript+ERB", Tier::D),
        ] {
            assert_eq!(lookup(name).tier, Some(tier));
            assert_eq!(lookup(name).inherited_from, None);
        }
        assert_eq!(
            lookup("Motorola 68K Assembly").inherited_from,
            Some("Assembly")
        );
        assert_eq!(lookup("Motorola 68K Assembly").tier, None);
    }

    #[test]
    fn registry_preserves_types_and_covers_the_editorial_tierlist() {
        assert_eq!(registry().languages.len(), 833);
        for name in ["HTML", "CSS", "MDX", "Roff"] {
            assert_eq!(lookup(name).language_type, Some(LanguageType::Markup));
            assert_eq!(lookup(name).tier, None);
        }
        assert_eq!(lookup("JSON").language_type, Some(LanguageType::Data));
        assert_eq!(lookup("Markdown").language_type, Some(LanguageType::Prose));
        let editorial = include_str!("../../clanker-index-tierlist.md");
        let table = editorial.split("\n\n## Editorial notes").next().unwrap();
        for line in table.lines().skip(2) {
            let fields: Vec<_> = line.split(" | ").collect();
            let assignment = lookup(fields[0]);
            assert_eq!(assignment.language_type, Some(LanguageType::Programming));
            assert_eq!(
                assignment.tier.map_or("???", Tier::label),
                fields[1],
                "{}",
                fields[0]
            );
        }
        for name in overrides().keys() {
            assert_eq!(lookup(name).language_type, Some(LanguageType::Programming));
        }
    }

    #[test]
    fn byte_weighted_nearest_tier_and_no_grade() {
        assert_eq!(aggregate(10 * 6 + 90 * 2, 100), Some(Tier::C));
        assert_eq!(aggregate(250, 100), Some(Tier::B));
        assert_eq!(aggregate(0, 100), Some(Tier::F));
        assert_eq!(aggregate(0, 0), None);
        assert_eq!(
            aggregate(u128::from(u64::MAX) * 6, u128::from(u64::MAX)),
            Some(Tier::SPlus)
        );
    }
}
