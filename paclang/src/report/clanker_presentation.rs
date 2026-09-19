//! Terminal-only lore. Editing a verdict must not change analytical policy.
use std::collections::BTreeMap;
use std::sync::OnceLock;

use super::clanker_policy::Tier;

#[derive(serde::Deserialize)]
pub(super) struct Presentation {
    pub emoji: String,
    pub verdict: String,
}

fn entries() -> &'static BTreeMap<String, Presentation> {
    static ENTRIES: OnceLock<BTreeMap<String, Presentation>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        serde_json::from_str(include_str!("data/clanker-presentation.json"))
            .expect("bundled terminal presentation policy must be valid")
    })
}

pub(super) fn lookup(language: &str) -> (&'static str, &'static str) {
    entries().get(language).map_or(
        ("❓", "the council has not reviewed this language"),
        |entry| (entry.emoji.as_str(), entry.verdict.as_str()),
    )
}

pub(super) fn system_grade(grade: Option<Tier>) -> &'static str {
    match grade {
        Some(Tier::SPlus) => "🦀 THE MACHINE HAS ACHIEVED OXIDATION",
        Some(Tier::S) => "🤖 Clanker-approved computing environment",
        Some(Tier::A) => "✨ Excellent clanker habitat",
        Some(Tier::B) => "👍 Clankers may operate unsupervised",
        Some(Tier::C) => "🏭 Legacy containment successful. For now.",
        Some(Tier::D) => "⚠️ Human supervision recommended",
        Some(Tier::F) => "☣️ Clanker-hostile environment",
        None => "❓ The council declines to issue a rating",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_languages_keep_their_own_presentation() {
        assert_ne!(lookup("Python"), lookup("Python console"));
        assert_eq!(lookup("Rust"), ("🦀", "REWRITE EVERYTHING"));
        assert_eq!(lookup("C++"), ("🏭", "factory legacy containment zone"));
        assert_eq!(lookup("Perl"), ("🐪", "immediate investigation required"));
        assert!(lookup("Snakemake").1.contains("dependency"));
    }

    #[test]
    fn every_verdict_is_short_and_every_grade_has_lore() {
        assert_eq!(entries().len(), 562);
        for row in entries().values() {
            assert!(!row.emoji.is_empty());
            assert!(!row.verdict.is_empty());
            assert!(row.verdict.chars().count() <= 60, "{}", row.verdict);
        }
        for grade in [
            None,
            Some(Tier::SPlus),
            Some(Tier::S),
            Some(Tier::A),
            Some(Tier::B),
            Some(Tier::C),
            Some(Tier::D),
            Some(Tier::F),
        ] {
            assert!(!system_grade(grade).is_empty());
        }
        assert!(system_grade(None).contains("declines"));
    }
}
