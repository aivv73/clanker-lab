use super::*;

fn limits() -> Limits {
    Limits {
        source_jobs: 4,
        max_download_bytes: 1_073_741_824,
        max_extracted_bytes: 4_294_967_296,
        max_source_files: 250_000,
        source_timeout_seconds: 900,
    }
}

fn tar(name: &str, content: &[u8], kind: u8, link: &str) -> Vec<u8> {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(b"0000644\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", content.len()).as_bytes());
    header[148..156].fill(b' ');
    header[156] = kind;
    header[157..157 + link.len()].copy_from_slice(link.as_bytes());
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(content);
    bytes.resize(bytes.len().div_ceil(512) * 512 + 1024, 0);
    bytes
}

#[test]
fn extraction_bounds_paths_bytes_entries_and_tracks_ignored_files() {
    let directory = tempfile::tempdir().unwrap();
    for (name, kind, link) in [
        ("../escape", b'0', ""),
        ("/absolute", b'0', ""),
        ("x/.git/config", b'0', ""),
        ("link", b'2', "../../outside"),
        ("device", b'3', ""),
        ("hard", b'1', "target"),
    ] {
        let archive = directory.path().join("input");
        fs::write(&archive, tar(name, b"", kind, link)).unwrap();
        let tree = tempfile::tempdir_in(directory.path()).unwrap();
        assert!(
            archive::extract(
                &archive,
                tree.path(),
                &limits(),
                &mut AcquisitionRecord::default()
            )
            .is_err(),
            "{name}"
        );
    }
    let archive = directory.path().join("input");
    fs::write(&archive, tar("src/lib.rs", b"fn main() {}", b'0', "")).unwrap();
    for cap in [
        Limits {
            max_extracted_bytes: 2,
            ..limits()
        },
        Limits {
            max_source_files: 1,
            ..limits()
        },
    ] {
        let tree = tempfile::tempdir_in(directory.path()).unwrap();
        assert!(matches!(
            archive::extract(
                &archive,
                tree.path(),
                &cap,
                &mut AcquisitionRecord::default()
            ),
            Err(Error::Limited(_))
        ));
    }
    let tree = tempfile::tempdir_in(directory.path()).unwrap();
    let mut record = AcquisitionRecord::default();
    archive::extract(&archive, tree.path(), &limits(), &mut record).unwrap();
    assert_eq!(record.files, 2);
    assert_eq!(record.extracted_bytes, 12);
    fs::write(tree.path().join(".gitignore"), "src/\n").unwrap();
    git::initialize(tree.path()).unwrap();
    let repo = git2::Repository::open(tree.path()).unwrap();
    assert!(
        repo.head()
            .unwrap()
            .peel_to_tree()
            .unwrap()
            .get_path(Path::new("src/lib.rs"))
            .is_ok()
    );
}

#[test]
fn archive_digests_cover_sha256_sha512_and_blake2() {
    // Reference vectors for b"abc": a wrong hasher behind any arm would
    // surface here, since all three digests differ.
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("input");
    fs::write(&archive, b"abc").unwrap();
    assert_eq!(
        archive_digests(&archive).unwrap(),
        (
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_owned(),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f".to_owned(),
            "ba80a53f981c4d0d6a2797b69f12f6e94c212f14685ac4b74b12bb6fdbffa2d17d87c5392aab792dc252d5de4533cc9518d38aa8dbf1925ab92386edd4009923".to_owned(),
        )
    );
}

#[test]
fn checksums_and_url_boundaries_are_explicit() {
    for value in ["sha256:bad", "md5:0123", "sha512:SKIP", "b2:bad", "b2:SKIP"] {
        assert!(validate_checksum(value).is_err());
    }
    assert!(validate_checksum(&format!("sha512:{}", "a".repeat(128))).is_ok());
    assert!(validate_checksum(&format!("b2:{}", "b".repeat(128))).is_ok());
    assert!(validate_checksum(&format!("b2:{}", "b".repeat(64))).is_err());
    for value in [
        "file:///tmp/source.tar",
        "http://example.org/source",
        "https://user:pass@example.org/source",
        "https://example.org/source#fragment",
    ] {
        assert!(http::validate_url(value).is_err());
    }
    assert!(http::validate_url("http://127.0.0.1:1234/source.tar").is_ok());
}

#[test]
fn compressed_tar_and_zip_expand_with_the_same_limits() {
    for bytes in [
        include_bytes!("../../tests/fixtures/source.tar.gz").as_slice(),
        include_bytes!("../../tests/fixtures/source.zip").as_slice(),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("input");
        fs::write(&archive, bytes).unwrap();
        let tree = tempfile::tempdir_in(directory.path()).unwrap();
        let mut record = AcquisitionRecord::default();
        archive::extract(&archive, tree.path(), &limits(), &mut record).unwrap();
        assert_eq!(
            fs::read(tree.path().join("source/lib.rs")).unwrap(),
            b"fn main() {}\n"
        );
        assert_eq!(record.extracted_bytes, 13);
        let tree = tempfile::tempdir_in(directory.path()).unwrap();
        assert!(matches!(
            archive::extract(
                &archive,
                tree.path(),
                &Limits {
                    max_extracted_bytes: 1,
                    ..limits()
                },
                &mut AcquisitionRecord::default()
            ),
            Err(Error::Limited(_))
        ));
    }
}

fn extract_entries(entries: &[(&str, &[u8], u8, &str)], tree: &Path, cap: &Limits) -> Result<AcquisitionRecord, Error> {
    let input = tempfile::NamedTempFile::new().unwrap();
    let mut bytes = Vec::new();
    for (name, content, kind, target) in entries {
        let mut entry = tar(name, content, *kind, target);
        entry.truncate(entry.len() - 1024);
        bytes.extend(entry);
    }
    bytes.resize(bytes.len() + 1024, 0);
    fs::write(input.path(), bytes).unwrap();
    let mut record = AcquisitionRecord::default();
    archive::extract(input.path(), tree, cap, &mut record)?;
    Ok(record)
}

#[test]
fn archive_supports_linux_names_relative_symlinks_and_forward_hardlinks() {
    let tree = tempfile::tempdir().unwrap();
    extract_entries(&[
        ("pkg/include/alsa", b"", b'2', "."),
        ("pkg/include/header", b"header", b'0', ""),
        ("pkg/src/common/windows/logo", b"", b'2', "../../logo/icon"),
        ("pkg/src/logo/icon", b"svg", b'0', ""),
        ("pkg/fontconfig-2:2.13/desc", b"epoch", b'0', ""),
        ("pkg/hard", b"", b'1', "pkg/forward"),
        ("pkg/forward", b"", b'1', "pkg/include/header"),
    ], tree.path(), &limits()).unwrap();
    assert_eq!(fs::read(tree.path().join("pkg/include/alsa/header")).unwrap(), b"header");
    assert_eq!(fs::read(tree.path().join("pkg/src/common/windows/logo")).unwrap(), b"svg");
    assert_eq!(fs::read(tree.path().join("pkg/hard")).unwrap(), b"header");
    assert_eq!(fs::read(tree.path().join("pkg/fontconfig-2:2.13/desc")).unwrap(), b"epoch");
}

#[test]
fn archive_rejects_link_chain_escapes_cycles_and_collisions() {
    for entries in [
        vec![("a/up", b"".as_slice(), b'2', ".."), ("escape", b"", b'2', "a/up/../outside")],
        vec![("a", b"".as_slice(), b'2', "b"), ("b", b"", b'2', "a")],
        vec![("a", b"".as_slice(), b'1', "b"), ("b", b"", b'1', "a")],
        vec![("a", b"".as_slice(), b'2', "target"), ("hard", b"", b'1', "a")],
        vec![("a/file", b"x".as_slice(), b'0', ""), ("a", b"", b'2', ".")],
        vec![("a", b"".as_slice(), b'2', ".git/config")],
        vec![("a", b"".as_slice(), b'1', "../outside")],
        vec![("wrapper/escape", b"".as_slice(), b'2', "..")],
    ] {
        let tree = tempfile::tempdir().unwrap();
        assert!(extract_entries(&entries, tree.path(), &limits()).is_err(), "{entries:?}");
    }
}

#[test]
fn hardlink_expansion_is_charged_to_byte_budget() {
    let tree = tempfile::tempdir().unwrap();
    assert!(matches!(extract_entries(&[("a", b"1234", b'0', ""), ("b", b"", b'1', "a")],
        tree.path(), &Limits { max_extracted_bytes: 7, ..limits() }), Err(Error::Limited(_))));
}
