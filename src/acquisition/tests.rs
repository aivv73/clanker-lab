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
fn checksums_and_url_boundaries_are_explicit() {
    for value in ["sha256:bad", "md5:0123", "sha512:SKIP"] {
        assert!(validate_checksum(value).is_err());
    }
    assert!(validate_checksum(&format!("sha512:{}", "a".repeat(128))).is_ok());
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
