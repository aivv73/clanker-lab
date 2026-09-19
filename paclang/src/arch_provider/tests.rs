use base64::Engine;

use super::http::RecipeEndpoint;
use super::recipe::{Recipe, RecipePackage, project_path};
use crate::package::provider::{AttemptOutcome, EvidenceGrade};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn archive_recipe(text: &str) -> Recipe {
    let indented = text.lines().map(|line| {
        if line.starts_with("pkgbase =") || line.starts_with("pkgname =") {
            line.to_owned()
        } else { format!("\t{line}") }
    }).collect::<Vec<_>>().join("\n");
    recipe(&indented)
}

#[test]
fn audited_supplemental_checkouts_require_the_expected_primary() {
    for base in ["coreutils", "cpio", "diffutils", "findutils", "grep", "grub", "gzip", "inetutils", "libtool", "m4", "patch", "tar", "wget", "enchant", "glib2", "lib32-glib2", "glycin", "graphene", "libnghttp2", "lib32-libnghttp2", "libnghttp3", "lib32-libnghttp3", "libngtcp2", "lib32-libngtcp2", "p11-kit", "lib32-p11-kit", "pcre2", "lib32-pcre2", "libbluray", "libjxl", "libldac", "malcontent", "libshout", "libtorrent-rasterbar", "lutris", "mbedtls", "mbedtls3", "obs-studio", "ostree", "pahole", "python-lark-parser", "python-numpy", "python-rapidfuzz", "python-referencing", "qt6-tools", "qt6-webengine", "qtcreator", "gamescope"] {
        let rule = super::source_roles::rule(base).unwrap();
        let package = RecipePackage { name: base, base, version: "1.0-1" };
        let secondary = rule.supplemental.iter().enumerate().map(|(i, url)| format!("\tsource = checkout-{i}::git+{url}.git?signed\n")).collect::<String>();
        let primary = format!("\tsource = main::git+{}.git?signed#tag=v1.0\n", rule.primary);
        let resolve = |sources: &str| recipe(&format!("pkgbase = {base}\n\tpkgver = 1.0\n\tpkgrel = 1\n{sources}pkgname = {base}\n")).resolve(package);
        for sources in [format!("{primary}{secondary}"), format!("{secondary}{primary}")] {
            assert!(matches!(resolve(&sources), AttemptOutcome::Resolved { target, .. } if target.location == format!("{}.git", rule.primary) && target.revision.as_deref() == Some("tag:v1.0")), "{base}");
        }
        assert!(matches!(resolve(&secondary), AttemptOutcome::Unresolved { .. }), "{base}: unpinned inputs alone");
        assert!(matches!(resolve(&format!("{primary}{secondary}\tsource = git+https://example.org/new-component.git\n")), AttemptOutcome::Unresolved { reason } if reason.contains("unclassified unpinned secondary") && reason.contains("new-component")), "{base}: unknown input");
        assert!(matches!(resolve(&format!("{primary}{secondary}\tsource = git+https://example.org/new-component.git#tag=v1\n")), AttemptOutcome::Unresolved { reason } if reason.contains("ambiguous primary source")), "{base}: pinned extra component");
    }
}

#[test]
fn secondary_roles_do_not_relax_selectors_origins_or_version_checks() {
    let package = RecipePackage { name: "glib2", base: "glib2", version: "1.0-1" };
    for secondary in [
        "git+https://gitlab.gnome.org/GNOME/gvdb.git#branch=main",
        "git+https://gitlab.gnome.org/GNOME/gvdb.git#commit=abcdef",
        "git+https://gitlab.gnome.org/GNOME/gvdb.git?unexpected=1",
        "git+https://gitlab.gnome.org:8443/GNOME/gvdb.git",
        "git+https://gitlab.gnome.org.attacker.example/GNOME/gvdb.git",
        "git+http://gitlab.gnome.org/GNOME/gvdb.git",
        "git+https://user@gitlab.gnome.org/GNOME/gvdb.git",
        "git+https://gitlab.gnome.org/GNOME/other.git",
        "git+https://gitlab.gnome.org/GNOME/gvdb.git.git",
    ] {
        let doc = recipe(&format!("pkgbase = glib2\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = git+https://gitlab.gnome.org/GNOME/glib.git#tag=1.0\n\tsource = {secondary}\npkgname = glib2\n"));
        assert!(matches!(doc.resolve(package), AttemptOutcome::Unresolved { .. }), "{secondary}");
    }
    let doc = recipe("pkgbase = glib2\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = git+https://gitlab.gnome.org/GNOME/glib.git#tag=1.0\n\tsource = git+https://gitlab.gnome.org/GNOME/gvdb.git\npkgname = glib2\n");
    assert!(matches!(doc.resolve(RecipePackage { version: "2.0-1", ..package }), AttemptOutcome::Unresolved { reason } if reason.contains("version")));
}

#[test]
fn gnu_alternate_distribution_is_not_a_second_primary() {
    let resolve = |extra: &str| recipe(&format!("pkgbase = coreutils\n\tpkgver = 9.11\n\tpkgrel = 2\n\tsource = git+https://git.savannah.gnu.org/git/coreutils.git?signed#tag=v9.11\n\tsource = git+https://git.savannah.gnu.org/git/gnulib.git\n\tsource = https://ftp.gnu.org/gnu/coreutils/coreutils-9.11.tar.gz\n{extra}pkgname = coreutils\n")).resolve(RecipePackage { name: "coreutils", base: "coreutils", version: "9.11-2" });
    assert!(matches!(resolve(""), AttemptOutcome::Resolved { target, .. } if target.revision.as_deref() == Some("tag:v9.11")));
    assert!(matches!(resolve("\tsource = https://ftp.gnu.org/gnu/other/other-9.11.tar.gz\n"), AttemptOutcome::Unresolved { reason } if reason.contains("ambiguous")));
    assert!(matches!(resolve("\tsource = https://ftp.gnu.org/gnu/coreutils/coreutils-extra-9.11.tar.gz\n"), AttemptOutcome::Unresolved { reason } if reason.contains("ambiguous")));
    assert!(matches!(resolve("\tsource = git+https://git.savannah.gnu.org/git/coreutils.git#tag=v9.10\n"), AttemptOutcome::Unresolved { .. }));
}

#[test]
fn primary_roles_ignore_known_docs_but_keep_unknown_code_ambiguous() {
    let make = |extra: &str| recipe(&format!("pkgbase = gstreamer\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = git+https://gitlab.freedesktop.org/gstreamer/gstreamer.git#tag=1.0\n\tsource = https://gstreamer.freedesktop.org/src/gstreamer-docs/gstreamer-docs-1.0.tar.xz\n{extra}pkgname = gstreamer\n"));
    let package = RecipePackage { name: "gstreamer", base: "gstreamer", version: "1.0-1" };
    assert!(matches!(make("").resolve(package), AttemptOutcome::Resolved { target, .. } if target.location.ends_with("/gstreamer/gstreamer.git")));
    assert!(matches!(make("\tsource = https://example.org/extra-1.0.tar.gz\n").resolve(package), AttemptOutcome::Unresolved { reason } if reason.contains("3 candidates") && reason.contains("extra-1.0")));
    assert!(matches!(make("\tsource = https://example.org/extra-1.0.tar.gz\n").resolve(RecipePackage { version: "2.0-1", ..package }), AttemptOutcome::Unresolved { reason } if reason.contains("version")));
}

#[test]
fn local_sources_use_pinned_packaging_tree_and_directory() {
    use crate::package::provider::RecipeProvenance;
    let text = b"pkgbase = hwdetect\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = hwdetect\npkgname = hwdetect\n";
    for (location, directory) in [
        (format!("https://gitlab.archlinux.org/api/v4/projects/archlinux%2Fpackaging%2Fpackages%2Fhwdetect/repository/files/.SRCINFO?ref={COMMIT}"), None),
        (format!("https://github.com/CachyOS/CachyOS-PKGBUILDS/blob/{COMMIT}/hwdetect/.SRCINFO"), Some("hwdetect")),
    ] {
        let doc = Recipe::from_srcinfo(text, RecipeProvenance::new(&location, COMMIT).unwrap());
        let AttemptOutcome::Resolved { target, .. } = doc.resolve(RecipePackage { name: "hwdetect", base: "hwdetect", version: "1.0-1" }) else { panic!("{location}"); };
        assert_eq!(target.revision, Some(format!("commit:{COMMIT}")));
        assert_eq!(target.subdirectory.as_deref(), directory);
    }
    let doc = Recipe::from_srcinfo(text, RecipeProvenance::new(&format!("https://github.com/unrelated/repo/blob/{COMMIT}/.SRCINFO"), COMMIT).unwrap());
    assert!(matches!(doc.resolve(RecipePackage { name: "hwdetect", base: "hwdetect", version: "1.0-1" }), AttemptOutcome::Unresolved { reason } if reason.contains("recipe-local")));
}

#[test]
fn archive_identity_can_come_from_matching_recipe_digest() {
    for (version, url, kind, length) in [
        ("26.03", "https://7-zip.org/a/7z2603-src.tar.xz", "sha512", 128),
        ("3.53.4", "https://sqlite.org/2026/sqlite-src-3530400.zip", "sha256", 64),
        ("1.0", "https://example.org/snapshot.tar.gz", "b2", 128),
    ] {
        let hash = "a".repeat(length);
        let doc = archive_recipe(&format!("pkgbase = alpha\npkgver = {version}\npkgrel = 1\nsource = {url}\n{kind}sums = {hash}\npkgname = alpha\n"));
        let packaged = format!("{version}-1");
        let AttemptOutcome::Resolved { target, grade } = doc.resolve(RecipePackage {
            name: "alpha", base: "alpha", version: &packaged,
        }) else { panic!("{url}"); };
        assert_eq!(target.location, url);
        assert_eq!(target.checksum, Some(format!("{kind}:{hash}")));
        assert_eq!(target.revision, None);
        assert_eq!(grade, EvidenceGrade::Reconstructed);
        assert!(matches!(doc.resolve(RecipePackage { name: "alpha", base: "alpha", version: "999-1" }), AttemptOutcome::Unresolved { .. }));
    }
}

#[test]
fn unversioned_archive_requires_its_own_valid_digest() {
    for sums in [String::new(), "sha256sums = SKIP\n".into(), "sha256sums = invalid\n".into(),
        format!("sha256sums = SKIP\nsha256sums = {}\n", "a".repeat(64))] {
        let doc = archive_recipe(&format!("pkgbase = alpha\npkgver = 1.0\npkgrel = 1\nsource = https://example.org/snapshot.tar.gz\n{sums}pkgname = alpha\n"));
        assert!(matches!(doc.resolve(RecipePackage { name: "alpha", base: "alpha", version: "1.0-1" }), AttemptOutcome::Unresolved { .. }));
    }
    let unrelated = archive_recipe(&format!("pkgbase = alpha\npkgver = 1.0\npkgrel = 1\nsource = https://example.org/snapshot.tar.gz\nsource = local.patch\nsha256sums = SKIP\nsha256sums = {}\npkgname = alpha\n", "a".repeat(64)));
    assert!(matches!(unrelated.resolve(RecipePackage { name: "alpha", base: "alpha", version: "1.0-1" }), AttemptOutcome::Unresolved { reason } if reason.contains("archive URL")));
    // Checksums do not authorize choosing one of multiple source archives.
    let doc = archive_recipe(&format!("pkgbase = alpha\npkgver = 1.0\npkgrel = 1\nsource = https://example.org/a.tar.gz\nsource = https://example.org/b.tar.gz\nsha256sums = {}\nsha256sums = {}\npkgname = alpha\n", "a".repeat(64), "b".repeat(64)));
    assert!(matches!(doc.resolve(RecipePackage { name: "alpha", base: "alpha", version: "1.0-1" }), AttemptOutcome::Unresolved { reason } if reason.contains("ambiguous")));
}

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
            "git+https://example.org/alpha.git#tag=v1.0?signed",
            "https://example.org/alpha.git",
            Some("tag:v1.0"),
        ),
        (
            "git+https://example.org/alpha.git?signed#tag=v1.0",
            "https://example.org/alpha.git",
            Some("tag:v1.0"),
        ),
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
                reason: "no primary source candidate: only auxiliary source inputs".to_owned(),
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
    let digest = "C".repeat(128);
    let AttemptOutcome::Resolved { target, .. } = resolve(&format!(
        "https://example.org/alpha-1.0.tar\n\tb2sums = {digest}"
    )) else {
        panic!("BLAKE2 did not resolve");
    };
    assert_eq!(
        target.checksum,
        Some(format!("b2:{}", digest.to_ascii_lowercase()))
    );
    // Strongest verification evidence wins: SHA-512, then BLAKE2, then SHA-256.
    let AttemptOutcome::Resolved { target, .. } = resolve(&format!(
        "https://example.org/alpha-1.0.tar\n\tsha256sums = {}\n\tsha512sums = {}\n\tb2sums = {}",
        "d".repeat(64),
        "e".repeat(128),
        "f".repeat(128),
    )) else {
        panic!("checksum precedence did not resolve");
    };
    assert_eq!(target.checksum, Some(format!("sha512:{}", "e".repeat(128))));
    let AttemptOutcome::Resolved { target, .. } = resolve(&format!(
        "https://example.org/alpha-1.0.tar\n\tsha256sums = {}\n\tb2sums = {}",
        "d".repeat(64),
        "f".repeat(128),
    )) else {
        panic!("BLAKE2-over-SHA-256 precedence did not resolve");
    };
    assert_eq!(target.checksum, Some(format!("b2:{}", "f".repeat(128))));
    for sums in [
        "\tb2sums = invalid",
        "\tb2sums = SKIP\n\tb2sums = SKIP",
        "\tb2sums = short",
    ] {
        assert!(matches!(
            resolve(&format!("https://example.org/alpha-1.0.tar\n{sums}")),
            AttemptOutcome::Unresolved { .. }
        ));
    }
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
