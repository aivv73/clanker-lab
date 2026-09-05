use base64::Engine;

use super::http::RecipeEndpoint;
use super::recipe::{Recipe, RecipePackage, project_path};
use crate::package::provider::{AttemptOutcome, EvidenceGrade};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn api_body(text: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "file_path": ".SRCINFO", "encoding": "base64", "commit_id": COMMIT,
        "content": base64::engine::general_purpose::STANDARD.encode(text)
    }))
    .unwrap()
}

fn recipe(text: &str) -> Recipe {
    Recipe::decode(
        &api_body(text.as_bytes()),
        &"https://gitlab.archlinux.org/api/v4/projects/test/repository/files/.SRCINFO?ref=1.0-1"
            .parse()
            .unwrap(),
    )
    .unwrap()
}

fn resolve(source: &str) -> AttemptOutcome {
    recipe(&format!(
        "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = {source}\npkgname = alpha\n"
    ))
    .resolve(RecipePackage {
        name: "alpha",
        base: "alpha",
        version: "1.0-1",
    })
}

#[test]
fn pinned_git_and_versioned_archives_are_reconstructed_evidence() {
    for (source, location, revision) in [
        (
            "git+https://example.org/alpha.git#tag=v1.0",
            "https://example.org/alpha.git",
            Some("tag:v1.0"),
        ),
        (
            "alpha::git+https://example.org/alpha.git#commit=0123456789abcdef0123456789abcdef01234567",
            "https://example.org/alpha.git",
            Some("commit:0123456789abcdef0123456789abcdef01234567"),
        ),
        (
            "alpha.tar.gz::https://example.org/releases/1.0/archive.tar.gz",
            "https://example.org/releases/1.0/archive.tar.gz",
            None,
        ),
    ] {
        let AttemptOutcome::Resolved { target, grade } = resolve(source) else {
            panic!("{source}");
        };
        assert_eq!(grade, EvidenceGrade::Reconstructed);
        assert_eq!(target.location, location);
        assert_eq!(target.revision.as_deref(), revision);
        assert_eq!(target.subdirectory, None);
    }
}

#[test]
fn uncertain_sources_never_become_current_upstream_fallbacks() {
    for source in [
        "git+https://example.org/alpha.git",
        "git+https://example.org/alpha.git#branch=main",
        "git+https://example.org/alpha.git#commit=abcd",
        "git+https://example.org/alpha.git#commit=zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        "https://example.org/latest.tar.gz",
        "https://example.org/11.0.tar.gz",
        "https://example.org/1.0.1.tar.gz",
        "alpha-1.0.tar.gz::https://example.org/latest.tar.gz",
        "https://example.org/$pkgver.tar.gz",
        "https://user:secret@example.org/1.0.tar.gz",
        "http://example.org/1.0.tar.gz",
        "file:///tmp/1.0.tar.gz",
        "patch.diff",
        "https://example.org/1.0.patch",
        "https://example.org/1.0.tar.gz#other",
        "https://example.org/a b/1.0.tar.gz",
    ] {
        assert!(
            matches!(resolve(source), AttemptOutcome::Unresolved { .. }),
            "{source}"
        );
    }
}

#[test]
fn remote_patches_do_not_veto_a_primary_tree_or_supply_one() {
    for auxiliary in [
        "https://example.org/fix.patch",
        "https://example.org/fix.diff?download=1",
        "fix.patch::https://example.org/download/123",
        "fix.diff::https://example.org/download/456",
    ] {
        assert_eq!(
            resolve(auxiliary),
            AttemptOutcome::Unresolved {
                reason: "primary source is missing or ambiguous in recipe metadata".to_owned(),
            },
            "{auxiliary}"
        );
        for primary in [
            "https://example.org/alpha-1.0.tar.gz",
            "git+https://example.org/alpha.git#tag=v1.0",
        ] {
            assert_eq!(
                resolve(&format!("{primary}\n\tsource = {auxiliary}")),
                resolve(primary),
                "{primary} with {auxiliary}"
            );
        }
    }
    for auxiliary in [
        "http://example.org/fix.patch",
        "https://user:secret@example.org/fix.diff",
        "https://example.org/$pkgver.patch",
        "https://example.org/unknown.bin",
        "git+https://example.org/fix.patch",
        "https://example.org/other-1.0.tar.gz",
    ] {
        assert!(
            matches!(
                resolve(&format!(
                    "https://example.org/alpha-1.0.tar.gz\n\tsource = {auxiliary}"
                )),
                AttemptOutcome::Unresolved { .. }
            ),
            "{auxiliary}"
        );
    }
}

#[test]
fn matching_split_members_share_targets_and_keep_recipe_commit() {
    let recipe = recipe(
        "pkgbase = suite\n\tpkgver = 2.0\n\tpkgrel = 1\n\tepoch = 3\n\tsource = https://example.org/suite-2.0.tar.gz\n\tsource = https://example.org/suite-2.0.tar.gz.sig\n\tsource = fix.patch\npkgname = suite-cli\npkgname = suite-docs\n",
    );
    assert_eq!(recipe.provenance().commit(), COMMIT);
    assert!(
        recipe
            .provenance()
            .location()
            .ends_with(&format!("ref={COMMIT}"))
    );
    assert_eq!(
        recipe.resolve(RecipePackage {
            name: "suite-cli",
            base: "suite",
            version: "3:2.0-1"
        }),
        recipe.resolve(RecipePackage {
            name: "suite-docs",
            base: "suite",
            version: "3:2.0-1"
        })
    );
    for query in [
        RecipePackage {
            name: "suite-cli",
            base: "other",
            version: "3:2.0-1",
        },
        RecipePackage {
            name: "other",
            base: "suite",
            version: "3:2.0-1",
        },
        RecipePackage {
            name: "suite-cli",
            base: "suite",
            version: "2.0-1",
        },
    ] {
        assert!(matches!(
            recipe.resolve(query),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}

#[test]
fn multiple_or_architecture_specific_sources_remain_unresolved() {
    for sources in [
        "\tsource = https://example.org/a-1.0.tar.gz\n\tsource = https://example.org/b-1.0.tar.gz\n",
        "\tsource = https://example.org/a-1.0.tar.gz\n\tsource_x86_64 = https://example.org/b-1.0.tar.gz\n",
    ] {
        let recipe = recipe(&format!(
            "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n{sources}pkgname = alpha\n"
        ));
        assert!(matches!(
            recipe.resolve(RecipePackage {
                name: "alpha",
                base: "alpha",
                version: "1.0-1"
            }),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}

#[test]
fn malformed_api_metadata_is_rejected_and_malformed_recipe_keeps_provenance() {
    let url = "https://example.org/api/v4".parse().unwrap();
    for body in [
        b"not json".as_slice(),
        br#"{"file_path":"PKGBUILD","encoding":"base64","commit_id":"abc","content":""}"#,
    ] {
        assert!(Recipe::decode(body, &url).is_err());
    }
    for text in [
        b"not srcinfo".as_slice(),
        &[255],
        &vec![b'x'; crate::srcinfo::MAX_SRCINFO_BYTES + 1],
    ] {
        let recipe = Recipe::decode(&api_body(text), &url).unwrap();
        assert_eq!(recipe.provenance().commit(), COMMIT);
        assert!(matches!(
            recipe.resolve(RecipePackage {
                name: "alpha",
                base: "alpha",
                version: "1.0-1"
            }),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}

#[test]
fn api_urls_encode_project_and_ref_and_reject_unsafe_endpoints() {
    let endpoint: RecipeEndpoint = "https://gitlab.archlinux.org/api/v4".parse().unwrap();
    let url = endpoint.file_url("archlinux/packaging/packages/foo", "1:2~3-4");
    assert!(
        url.path()
            .contains("archlinux%2Fpackaging%2Fpackages%2Ffoo")
    );
    assert_eq!(url.query_pairs().next().unwrap().1, "1:2~3-4");
    for value in [
        "http://example.org",
        "file:///tmp/foo",
        "https://user:secret@example.org",
        "https://example.org?secret=x",
        "https://example.org#fragment",
    ] {
        assert!(value.parse::<RecipeEndpoint>().is_err());
    }
    assert!(
        "http://127.0.0.1:1234/api/v4"
            .parse::<RecipeEndpoint>()
            .is_ok()
    );
}

#[test]
fn project_paths_follow_arch_packaging_names() {
    for (base, path) in [
        ("tree", "unix-tree"),
        ("gtk+3", "gtkplus3"),
        ("libsigc++", "libsigcplusplus"),
        ("foo+bar", "foo-bar"),
        ("a+b+c", "a-bplusc"),
        ("a__--b", "a-b"),
        ("a_b", "a_b"),
    ] {
        assert_eq!(project_path(base), path);
    }
}

#[test]
fn archive_checksums_follow_source_positions_and_survive_auxiliary_inputs() {
    let digest = "a".repeat(64);
    let AttemptOutcome::Resolved { target, .. } = resolve(&format!(
        "https://example.org/fix.patch\n\tsource = alpha.tar::https://example.org/alpha-1.0.tar\n\tsha256sums = SKIP\n\tsha256sums = {digest}"
    )) else {
        panic!("valid checksum metadata did not resolve");
    };
    assert_eq!(target.checksum, Some(format!("sha256:{digest}")));
    let digest = "B".repeat(128);
    let AttemptOutcome::Resolved { target, .. } = resolve(&format!(
        "https://example.org/alpha-1.0.tar\n\tsha512sums = {digest}"
    )) else {
        panic!("SHA-512 did not resolve");
    };
    assert_eq!(
        target.checksum,
        Some(format!("sha512:{}", digest.to_ascii_lowercase()))
    );
    for sums in [
        "\tsha256sums = invalid",
        "\tsha256sums = SKIP\n\tsha256sums = SKIP",
    ] {
        assert!(matches!(
            resolve(&format!("https://example.org/alpha-1.0.tar\n{sums}")),
            AttemptOutcome::Unresolved { .. }
        ));
    }
    let AttemptOutcome::Resolved { target, .. } =
        resolve("https://example.org/alpha-1.0.tar\n\tsha256sums = SKIP")
    else {
        panic!("SKIP should preserve resolution, not verification");
    };
    assert_eq!(target.checksum, None);
}

#[test]
fn matching_recipes_without_declared_sources_are_source_free() {
    for (name, version, release) in [("base", "3", "3"), ("base-devel", "1", "2")] {
        let recipe = recipe(&format!(
            "pkgbase = {name}\n\tpkgver = {version}\n\tpkgrel = {release}\n\tdepends = glibc\npkgname = {name}\n"
        ));
        assert_eq!(
            recipe.resolve(RecipePackage {
                name,
                base: name,
                version: &format!("{version}-{release}")
            }),
            AttemptOutcome::SourceFree
        );
        assert!(matches!(
            recipe.resolve(RecipePackage {
                name,
                base: name,
                version: "99-1"
            }),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}

#[test]
fn source_free_does_not_hide_unsupported_or_incomplete_source_metadata() {
    for metadata in [
        "\tsource = local.patch\n",
        "\tsource_x86_64 = https://example.org/a.tar.gz\n",
        "\tsha256sums = SKIP\n",
        "\tsource = git+https://example.org/a#tag=v1\n\tsource = git+https://example.org/b#tag=v1\n",
    ] {
        assert!(matches!(
            recipe(&format!(
                "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n{metadata}pkgname = alpha\n"
            ))
            .resolve(RecipePackage {
                name: "alpha",
                base: "alpha",
                version: "1.0-1"
            }),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}
