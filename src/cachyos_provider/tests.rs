use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const BLOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn tree(entries: serde_json::Value, truncated: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"tree": entries, "truncated": truncated})).unwrap()
}

#[test]
fn repository_distinguishes_absent_custom_and_srcinfo_paths() {
    let repo = Repository::decode_tree(
        &tree(
            serde_json::json!([
                {"path":"alpha/.SRCINFO","type":"blob","sha":BLOB},
                {"path":"custom/PKGBUILD","type":"blob","sha":BLOB},
                {"path":"kernel","type":"commit","sha":COMMIT}
            ]),
            false,
        ),
        COMMIT.to_owned(),
    )
    .unwrap();
    assert_eq!(repo.srcinfo.get("alpha").unwrap().1, BLOB);
    assert!(repo.bases.contains("custom"));
    assert!(repo.bases.contains("kernel"));
    assert!(!repo.bases.contains("wget"));
}

#[test]
fn untrustworthy_indexes_cannot_enable_arch_fallback() {
    assert!(
        Repository::decode_tree(&tree(serde_json::json!([]), true), COMMIT.to_owned()).is_err()
    );
    assert!(
        Repository::decode_tree(
            &tree(
                serde_json::json!([
                    {"path":"alpha/.SRCINFO","type":"blob","sha":"bad"}
                ]),
                false
            ),
            COMMIT.to_owned()
        )
        .is_err()
    );
    let entry = serde_json::json!({"path":"alpha/.SRCINFO","type":"blob","sha":BLOB});
    assert!(
        Repository::decode_tree(
            &tree(serde_json::json!([entry, entry]), false),
            COMMIT.to_owned()
        )
        .is_err()
    );
}

#[test]
fn rebuild_mapping_only_removes_one_numeric_release_component() {
    for (installed, expected) in [
        ("1:7.2.7-1.1", "1:7.2.7-1"),
        ("3.0.23_2-13.1", "3.0.23_2-13"),
        ("0.4-2.2", "0.4-2"),
    ] {
        assert_eq!(rebuild_version(installed).as_deref(), Some(expected));
    }
    for version in [
        "1.2.3-1",
        "1.2.3",
        "1.2-1.",
        "1.2-.1",
        "1.2-1.x",
        "1.2-1.1.1",
    ] {
        assert_eq!(rebuild_version(version), None);
    }
}

fn recipe(version: &str, release: &str) -> Recipe {
    Recipe::from_srcinfo(format!("pkgbase = suite\n\tpkgver = {version}\n\tpkgrel = {release}\n\tsource = git+https://example.org/suite#tag=v{version}\npkgname = suite-cli\npkgname = suite-docs\n").as_bytes(), RecipeProvenance::new("https://example.org/recipe", COMMIT).unwrap())
}

#[test]
fn exact_and_rebuild_matches_have_distinct_grades() {
    let recipe = recipe("2.0", "1");
    for (version, grade) in [
        ("2.0-1", EvidenceGrade::Reconstructed),
        ("2.0-1.1", EvidenceGrade::Fallback),
    ] {
        for name in ["suite-cli", "suite-docs"] {
            assert!(
                matches!(resolve(&recipe, RecipePackage { name, base: "suite", version }), AttemptOutcome::Resolved { grade: actual, .. } if actual == grade)
            );
        }
    }
    for (name, base, version) in [
        ("suite-cli", "suite", "2.1-1.1"),
        ("suite-cli", "suite", "2.0-2.1"),
        ("suite-cli", "suite", "1:2.0-1.1"),
        ("other", "suite", "2.0-1.1"),
        ("suite-cli", "other", "2.0-1.1"),
    ] {
        assert!(matches!(
            resolve(
                &recipe,
                RecipePackage {
                    name,
                    base,
                    version
                }
            ),
            AttemptOutcome::Unresolved { .. }
        ));
    }
}

#[test]
fn github_blob_uses_commit_provenance_and_bounded_parser() {
    let repo = Repository {
        commit: COMMIT.to_owned(),
        bases: BTreeSet::new(),
        srcinfo: BTreeMap::new(),
        recipes: BTreeMap::new(),
    };
    let endpoint = "https://api.github.com/repos/CachyOS/CachyOS-PKGBUILDS"
        .parse()
        .unwrap();
    let content = b"pkgbase = alpha\n\tpkgver = 1\n\tpkgrel = 1\npkgname = alpha\n";
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);
    let wrapped = encoded
        .as_bytes()
        .chunks(60)
        .map(|line| std::str::from_utf8(line).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let bytes =
        serde_json::to_vec(&serde_json::json!({"sha":BLOB,"encoding":"base64","content":wrapped}))
            .unwrap();
    let recipe = repo
        .decode_recipe(&bytes, &endpoint, "alpha", BLOB)
        .unwrap();
    assert_eq!(recipe.provenance().commit(), COMMIT);
    assert!(
        recipe
            .provenance()
            .location()
            .ends_with(&format!("/contents/alpha/.SRCINFO?ref={COMMIT}"))
    );
    assert!(
        repo.decode_recipe(&bytes, &endpoint, "alpha", COMMIT)
            .is_err()
    );
    let bytes = serde_json::to_vec(&serde_json::json!({"sha":BLOB,"encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(vec![b'x'; crate::srcinfo::MAX_SRCINFO_BYTES + 1])})).unwrap();
    let recipe = repo
        .decode_recipe(&bytes, &endpoint, "alpha", BLOB)
        .unwrap();
    assert!(matches!(
        recipe.resolve(RecipePackage {
            name: "alpha",
            base: "alpha",
            version: "1-1"
        }),
        AttemptOutcome::Unresolved { .. }
    ));
}

#[test]
fn nested_recipes_keep_their_full_directory_and_custom_presence() {
    let repo = Repository::decode_tree(
        &tree(
            serde_json::json!([
                {"path":"toolchain/gcc/.SRCINFO","type":"blob","sha":BLOB},
                {"path":"mesa/lib32-mesa/PKGBUILD","type":"blob","sha":BLOB}
            ]),
            false,
        ),
        COMMIT.to_owned(),
    )
    .unwrap();
    assert_eq!(
        repo.srcinfo.get("gcc"),
        Some(&("toolchain/gcc".to_owned(), BLOB.to_owned()))
    );
    assert!(repo.bases.contains("lib32-mesa"));
    for path in ["../alpha/.SRCINFO", "a/./.SRCINFO", "a//.SRCINFO"] {
        assert!(
            Repository::decode_tree(
                &tree(
                    serde_json::json!([
                        {"path":path,"type":"blob","sha":BLOB}
                    ]),
                    false
                ),
                COMMIT.to_owned()
            )
            .is_err()
        );
    }
}

#[test]
fn rebuild_fallback_reports_the_actual_source_failure() {
    let recipe = Recipe::from_srcinfo(
        b"pkgbase = wget\n\tpkgver = 1.25.0\n\tpkgrel = 6\n\tsource = git+https://gitlab.com/gnuwget/wget.git?signed#tag=v1.25.0\n\tsource = git+https://git.savannah.gnu.org/git/gnulib.git\npkgname = wget\n",
        RecipeProvenance::new("https://example.org/recipe", COMMIT).unwrap(),
    );
    assert_eq!(
        resolve(
            &recipe,
            RecipePackage {
                name: "wget",
                base: "wget",
                version: "1.25.0-6.1"
            }
        ),
        AttemptOutcome::Unresolved {
            reason: "unpinned Git source is unsupported".to_owned()
        }
    );
}

fn snapshot_bytes(commit: &str, files: &[(&str, &[u8])], dirs: &[&str]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for dir in dirs {
        builder
            .append_dir(format!("CachyOS-PKGBUILDS-{commit}/{dir}"), "/tmp")
            .unwrap();
    }
    for (path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                format!("CachyOS-PKGBUILDS-{commit}/{path}"),
                *content,
            )
            .unwrap();
    }
    let encoder = builder.into_inner().unwrap();
    encoder.finish().unwrap()
}

#[test]
fn snapshot_decodes_recipes_and_distinguishes_absent_bases() {
    let srcinfo = b"pkgbase = foo\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = https://example.org/foo-1.0.tar.gz\npkgname = foo\n";
    let bytes = snapshot_bytes(
        COMMIT,
        &[
            ("foo/.SRCINFO", srcinfo.as_slice()),
            ("custom/PKGBUILD", b"pkgname=custom".as_slice()),
        ],
        &["", "foo", "custom"],
    );
    let repo = Repository::decode_snapshot(&bytes, COMMIT.to_owned()).unwrap();
    assert_eq!(repo.commit, COMMIT);
    assert!(repo.recipes.contains_key("foo"));
    assert!(repo.bases.contains("custom"));
    assert!(!repo.bases.contains("wget"));
    assert!(!repo.srcinfo.contains_key("wget"));
}

#[test]
fn snapshot_rejects_entries_outside_the_commit_prefix() {
    let bytes = snapshot_bytes(
        COMMIT,
        &[("foo/.SRCINFO", b"pkgbase = foo\npkgname = foo\n".as_slice())],
        &[""],
    );
    // Corrupt the prefix by renaming the top directory.
    let mut decoder = flate2::read::GzDecoder::new(bytes.as_slice());
    let mut expanded = Vec::new();
    use std::io::Read;
    decoder.read_to_end(&mut expanded).unwrap();
    let evil = format!("CachyOS-PKGBUILDS-{COMMIT}/").into_bytes();
    let mut fake = evil.clone();
    fake[0] = if fake[0] == b'C' { b'X' } else { b'C' };
    for i in 0..expanded.len().saturating_sub(evil.len()) {
        if expanded[i..i + evil.len()] == evil {
            expanded[i..i + evil.len()].copy_from_slice(&fake);
            break;
        }
    }
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&expanded).unwrap();
    let bytes = encoder.finish().unwrap();
    assert!(Repository::decode_snapshot(&bytes, COMMIT.to_owned()).is_err());
}

#[test]
fn git_advertisement_accepts_only_the_master_ref() {
    fn pkt(line: &str) -> Vec<u8> {
        format!("{:04x}{}", line.len() + 4, line).into_bytes()
    }
    let oid = "2d4245489a7c99520f2423e3b395fb989949aaf7";
    let mut refs = b"001e# service=git-upload-pack\n0000".to_vec();
    refs.extend(pkt(&format!("{oid} HEAD\0multi_ack symref=HEAD:refs/heads/master\n")));
    refs.extend(pkt(&format!("{oid} refs/heads/master\n")));
    assert_eq!(advertised_master(&refs).as_deref(), Ok(oid));
    assert!(advertised_master(b"001e# service=git-upload-pack\n0000").is_err());
    let mut dup = refs.clone();
    dup.extend(pkt(&format!("{oid} refs/heads/master\n")));
    assert!(advertised_master(&dup).is_err());
}
