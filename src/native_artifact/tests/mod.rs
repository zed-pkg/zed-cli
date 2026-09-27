use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use tar::Builder;
use zed_interfaces::artifact::ArtifactFormat;

use super::*;

fn native_version(url: &str, sha256: String) -> VersionMetadata {
    return VersionMetadata {
        org: "npm".to_string(),
        name: "left-pad".to_string(),
        version: "1.3.0".to_string(),
        sha256,
        size: 0,
        format: ArtifactFormat::TarGz,
        vcs_tag: "1.3.0".to_string(),
        vcs_commit: None,
        download_url: url.to_string(),
        published_at: "1970-01-01T00:00:00Z".to_string(),
        yanked: false,
        mirrors: Vec::new(),
        signatures: Vec::new(),
    };
}

fn npm_style_archive(path: &Path) {
    let file = fs::File::create(path).unwrap();
    let encoder = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(encoder);
    let bytes = b"module.exports = 1;\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, "package/index.js", &bytes[..])
        .unwrap();
    tar.finish().unwrap();
}

fn nested_package(path: &Path, member: &str) {
    let temp = tempfile::tempdir().unwrap();
    let payload = temp.path().join("payload.tar.gz");
    {
        let file = fs::File::create(&payload).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut inner = Builder::new(encoder);
        let bytes = b"native\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        inner
            .append_data(&mut header, "lib/source.txt", &bytes[..])
            .unwrap();
        inner.finish().unwrap();
    }
    let payload_bytes = fs::read(&payload).unwrap();
    let file = fs::File::create(path).unwrap();
    let mut outer = Builder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_size(payload_bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    outer
        .append_data(&mut header, member, &payload_bytes[..])
        .unwrap();
    outer.finish().unwrap();
}

#[test]
fn npm_single_root_is_normalized_without_changing_the_pinned_digest() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("left-pad.tgz");
    npm_style_archive(&archive);
    let (sha256, _) = sha256_file(&archive).unwrap();
    let store = Store::new(&temp.path().join("home"));
    let version = native_version(
        "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz",
        sha256.clone(),
    );

    let package = add_if_native(&store, &archive, &version)
        .unwrap()
        .expect("npm is an admitted native source");
    assert_eq!(package, store.pkg_dir(&sha256));
    assert!(package.join("index.js").is_file());
    assert!(!package.join("package").exists());
}

#[test]
fn arbitrary_https_hosts_do_not_weaken_the_zed_archive_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("package.tgz");
    npm_style_archive(&archive);
    let (sha256, _) = sha256_file(&archive).unwrap();
    let store = Store::new(&temp.path().join("home"));
    let version = native_version("https://example.invalid/package.tgz", sha256);
    assert!(add_if_native(&store, &archive, &version).unwrap().is_none());
}

#[test]
fn multi_root_zip_is_wrapped_under_pkg_instead_of_dropping_entries() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("package.nupkg");
    {
        let file = fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("lib/a.dll", options).unwrap();
        zip.write_all(b"dll").unwrap();
        zip.start_file("package.nuspec", options).unwrap();
        zip.write_all(b"metadata").unwrap();
        zip.finish().unwrap();
    }
    let (sha256, _) = sha256_file(&archive).unwrap();
    let store = Store::new(&temp.path().join("home"));
    let mut version = native_version(
        "https://api.nuget.org/v3-flatcontainer/acme/1.0.0/acme.1.0.0.nupkg",
        sha256.clone(),
    );
    version.format = ArtifactFormat::Zip;

    let package = add_if_native(&store, &archive, &version)
        .unwrap()
        .expect("NuGet is an admitted native source");
    assert!(package.join("lib/a.dll").is_file());
    assert!(package.join("package.nuspec").is_file());
}

#[test]
fn rubygem_nested_data_archive_is_normalized() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("demo.gem");
    nested_package(&archive, "data.tar.gz");
    let (sha256, _) = sha256_file(&archive).unwrap();
    let store = Store::new(&temp.path().join("home"));
    let version = native_version("https://rubygems.org/gems/demo-1.3.0.gem", sha256.clone());

    let package = add_if_native(&store, &archive, &version)
        .unwrap()
        .expect("RubyGems is an admitted native source");
    assert!(package.join("lib/source.txt").is_file());
    assert!(!package.join("source.txt").exists());
}

#[test]
fn hex_nested_contents_archive_is_normalized() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("demo.tar");
    nested_package(&archive, "contents.tar.gz");
    let (sha256, _) = sha256_file(&archive).unwrap();
    let store = Store::new(&temp.path().join("home"));
    let version = native_version(
        "https://repo.hex.pm/tarballs/demo-1.3.0.tar",
        sha256.clone(),
    );

    let package = add_if_native(&store, &archive, &version)
        .unwrap()
        .expect("Hex is an admitted native source");
    assert!(package.join("lib/source.txt").is_file());
    assert!(!package.join("source.txt").exists());
}

#[test]
fn single_directory_zip_retains_its_package_relative_path() {
    for url in [
        "https://api.nuget.org/v3-flatcontainer/acme/1.0.0/acme.1.0.0.nupkg",
        "https://files.pythonhosted.org/packages/acme-1.0.0-py3-none-any.whl",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("package.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        zip.start_file("lib/payload", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"payload").unwrap();
        zip.finish().unwrap();
        let (sha256, _) = sha256_file(&archive).unwrap();
        let version = native_version(url, sha256.clone());
        let store = Store::new(&temp.path().join("home"));
        let package = add_if_native(&store, &archive, &version).unwrap().unwrap();
        assert!(package.join("lib/payload").is_file());
        assert!(!package.join("payload").exists());
        assert_eq!(sha256_file(&archive).unwrap().0, sha256);
    }
}

#[test]
fn only_the_expected_archive_wrapper_is_removed() {
    for (expected, stripped) in [(None, false), (Some("package"), false), (Some("lib"), true)] {
        let temp = tempfile::tempdir().unwrap();
        let unpacked = temp.path().join("unpacked");
        fs::create_dir_all(unpacked.join("lib")).unwrap();
        fs::write(unpacked.join("lib/source"), b"source").unwrap();
        let package = temp.path().join("pkg");
        normalize_extracted_tree(&unpacked, &package, expected).unwrap();
        assert_eq!(package.join("source").exists(), stripped);
        assert_eq!(package.join("lib/source").exists(), !stripped);
    }
}

#[test]
fn native_urls_reject_nonstandard_ports_and_fragments() {
    for url in [
        "https://rubygems.org:8443/gems/demo.gem",
        "https://repo.hex.pm/tarballs/demo.tar#fragment",
    ] {
        assert_eq!(native_layout(&native_version(url, "a".repeat(64))), None);
    }
}

#[test]
fn duplicate_nested_payload_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("duplicate.gem");
    let mut outer = Builder::new(fs::File::create(&archive).unwrap());
    for _ in 0..2 {
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        outer
            .append_data(&mut header, "data.tar.gz", std::io::empty())
            .unwrap();
    }
    outer.finish().unwrap();
    let error = extract_nested_tar_gzip(
        &archive,
        "data.tar.gz",
        temp.path(),
        &temp.path().join("pkg"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("duplicate payload"));
    assert!(!temp.path().join("pkg").exists());
}

#[test]
fn oversized_native_envelope_is_rejected_before_parsing() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("oversized.tar");
    fs::File::create(&archive)
        .unwrap()
        .set_len(MAX_NATIVE_ENVELOPE_BYTES + 1)
        .unwrap();
    let error = extract_nested_tar_gzip(
        &archive,
        "contents.tar.gz",
        temp.path(),
        &temp.path().join("pkg"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("byte cap"));
}

#[test]
fn native_envelope_entry_budget_is_exact() {
    for count in [MAX_NATIVE_ENVELOPE_ENTRIES, MAX_NATIVE_ENVELOPE_ENTRIES + 1] {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("envelope.tar");
        {
            let mut outer = Builder::new(fs::File::create(&archive).unwrap());
            for index in 0..count {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o644);
                header.set_cksum();
                let name = if index + 1 == count {
                    "contents.tar.gz"
                } else {
                    "metadata"
                };
                outer
                    .append_data(&mut header, name, std::io::empty())
                    .unwrap();
            }
            outer.finish().unwrap();
        }
        let result = copy_nested_payload(&archive, "contents.tar.gz", &temp.path().join("payload"));
        if count == MAX_NATIVE_ENVELOPE_ENTRIES {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().to_string().contains("entry cap"));
        }
    }
}

#[test]
fn nested_payload_cannot_be_a_link() {
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("link.gem");
    {
        let mut outer = Builder::new(fs::File::create(&archive).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o644);
        outer
            .append_link(&mut header, "data.tar.gz", "outside")
            .unwrap();
        outer.finish().unwrap();
    }
    let result = copy_nested_payload(&archive, "data.tar.gz", &temp.path().join("payload"));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    assert!(!temp.path().join("payload").exists());
}
