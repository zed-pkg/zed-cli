use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use zed_cli::binary_archive::{BinaryPackOptions, pack_binary_zip, verify_binary_zip};
use zed_interfaces::binary_artifact::BinaryPlatformV1;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const DESCRIPTOR: &str = "pkg/.zpkg-binary.json";
const PAYLOAD: &str = "pkg/bin/hello";

struct Fixture {
    _project: TempDir,
    output: TempDir,
    archive: PathBuf,
}

#[derive(Clone)]
struct Entry {
    name: String,
    bytes: Vec<u8>,
    mode: u32,
    compression: CompressionMethod,
}

fn platform() -> BinaryPlatformV1 {
    BinaryPlatformV1 {
        target: "x86_64-unknown-linux-gnu".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        libc: Some("gnu".to_owned()),
        abi: None,
    }
}

fn fixture() -> Fixture {
    let project = tempfile::tempdir().expect("project tempdir");
    let output = tempfile::tempdir().expect("output tempdir");
    fs::create_dir_all(project.path().join("bin")).expect("create bin");
    fs::write(
        project.path().join(".zpkg.toml"),
        r#"[package]
org = "acme"
name = "hello-bin-adversarial"
version = "1.2.3"
description = "adversarial binary ZIP fixture"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/hello-bin-adversarial"

[bin]
hello = "bin/hello"
"#,
    )
    .expect("write manifest");
    fs::write(project.path().join("bin/hello"), b"hello binary\n").expect("write payload");

    let packed = pack_binary_zip(
        project.path(),
        &BinaryPackOptions {
            platform: platform(),
            includes: Vec::new(),
            out_dir: Some(output.path().to_path_buf()),
            vcs_commit: Some("0123456789abcdef".to_owned()),
        },
    )
    .expect("pack valid binary fixture");
    let archive = packed.packed.path;
    verify_binary_zip(&archive, Some(&platform())).expect("fixture must verify");
    Fixture {
        _project: project,
        output,
        archive,
    }
}

fn read_entries(path: &Path) -> Vec<Entry> {
    let input = fs::File::open(path).expect("open ZIP");
    let mut archive = ZipArchive::new(input).expect("parse ZIP");
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).expect("open ZIP entry");
        assert!(file.is_file(), "packer should emit files only");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read ZIP entry");
        entries.push(Entry {
            name: file.name().to_owned(),
            bytes,
            mode: file.unix_mode().unwrap_or(0o644) & 0o777,
            compression: file.compression(),
        });
    }
    entries
}

fn write_entries(path: &Path, entries: &[Entry]) {
    let output = fs::File::create(path).expect("create mutated ZIP");
    let mut writer = ZipWriter::new(output);
    let epoch = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .expect("valid ZIP epoch");
    for entry in entries {
        let options = SimpleFileOptions::default()
            .compression_method(entry.compression)
            .unix_permissions(entry.mode)
            .last_modified_time(epoch);
        writer
            .start_file(&entry.name, options)
            .expect("start mutated ZIP entry");
        writer
            .write_all(&entry.bytes)
            .expect("write mutated ZIP entry");
    }
    let mut output = writer.finish().expect("finish mutated ZIP");
    output.flush().expect("flush mutated ZIP");
    output.sync_all().expect("sync mutated ZIP");
}

fn mutate<F>(fixture: &Fixture, name: &str, mutation: F) -> PathBuf
where
    F: FnOnce(&mut Vec<Entry>),
{
    let mut entries = read_entries(&fixture.archive);
    mutation(&mut entries);
    let path = fixture.output.path().join(name);
    write_entries(&path, &entries);
    path
}

fn descriptor_entry(entries: &mut [Entry]) -> &mut Entry {
    entries
        .iter_mut()
        .find(|entry| entry.name == DESCRIPTOR)
        .expect("descriptor entry")
}

fn payload_entry(entries: &mut [Entry]) -> &mut Entry {
    entries
        .iter_mut()
        .find(|entry| entry.name == PAYLOAD)
        .expect("payload entry")
}

fn assert_rejected(path: &Path, expected: &str) {
    let error = verify_binary_zip(path, None)
        .expect_err("adversarial archive must be rejected")
        .to_string();
    assert!(
        error.to_ascii_lowercase().contains(&expected.to_ascii_lowercase()),
        "expected error containing `{expected}`, got `{error}`"
    );
}

#[test]
fn rejects_unlisted_missing_tampered_and_mode_mismatched_payloads() {
    let fixture = fixture();

    let rewritten = mutate(&fixture, "rewritten-valid.zip", |_| {});
    verify_binary_zip(&rewritten, Some(&platform())).expect("canonical rewrite must remain valid");

    let unlisted = mutate(&fixture, "unlisted.zip", |entries| {
        entries.push(Entry {
            name: "pkg/share/unlisted.txt".to_owned(),
            bytes: b"not declared by the descriptor".to_vec(),
            mode: 0o644,
            compression: CompressionMethod::Deflated,
        });
    });
    assert_rejected(&unlisted, "unlisted payload");

    let missing = mutate(&fixture, "missing.zip", |entries| {
        entries.retain(|entry| entry.name != PAYLOAD);
    });
    assert_rejected(&missing, "missing payload");

    let tampered = mutate(&fixture, "tampered.zip", |entries| {
        payload_entry(entries).bytes.extend_from_slice(b"tampered");
    });
    assert_rejected(&tampered, "digest mismatch");

    let mode_mismatch = mutate(&fixture, "mode-mismatch.zip", |entries| {
        payload_entry(entries).mode = 0o644;
    });
    assert_rejected(&mode_mismatch, "executable mode");
}

#[test]
fn rejects_traversal_noncanonical_and_colliding_paths() {
    let fixture = fixture();
    let cases = [
        ("traversal.zip", "pkg/../escape", "escapes"),
        ("absolute.zip", "/pkg/escape", "escapes"),
        ("outside-root.zip", "outside.txt", "not beneath"),
        ("backslash.zip", "pkg\\escape", "backslash"),
        ("casefold.zip", "pkg/BIN/hello", "collide"),
    ];
    for (archive_name, entry_name, expected) in cases {
        let archive = mutate(&fixture, archive_name, |entries| {
            entries.push(Entry {
                name: entry_name.to_owned(),
                bytes: b"hostile path".to_vec(),
                mode: 0o644,
                compression: CompressionMethod::Stored,
            });
        });
        assert_rejected(&archive, expected);
    }

    let duplicate = mutate(&fixture, "duplicate.zip", |entries| {
        let duplicate = entries
            .iter()
            .find(|entry| entry.name == PAYLOAD)
            .expect("payload")
            .clone();
        entries.push(duplicate);
    });
    assert_rejected(&duplicate, "collide");
}

#[test]
fn rejects_noncanonical_invalid_and_downgraded_descriptors() {
    let fixture = fixture();

    let pretty = mutate(&fixture, "pretty-descriptor.zip", |entries| {
        let descriptor = descriptor_entry(entries);
        let value: serde_json::Value =
            serde_json::from_slice(&descriptor.bytes).expect("parse descriptor");
        descriptor.bytes = serde_json::to_vec_pretty(&value).expect("pretty descriptor");
    });
    assert_rejected(&pretty, "not canonical JSON");

    let unknown_field = mutate(&fixture, "unknown-field.zip", |entries| {
        let descriptor = descriptor_entry(entries);
        let mut value: serde_json::Value =
            serde_json::from_slice(&descriptor.bytes).expect("parse descriptor");
        value
            .as_object_mut()
            .expect("descriptor object")
            .insert("unexpected".to_owned(), serde_json::json!(true));
        descriptor.bytes = serde_json::to_vec(&value).expect("serialize descriptor");
    });
    assert_rejected(&unknown_field, "unknown field");

    let expanded_mismatch = mutate(&fixture, "expanded-size.zip", |entries| {
        let descriptor = descriptor_entry(entries);
        let mut value: serde_json::Value =
            serde_json::from_slice(&descriptor.bytes).expect("parse descriptor");
        let size = value["expanded_size"].as_u64().expect("expanded_size");
        value["expanded_size"] = serde_json::json!(size + 1);
        descriptor.bytes = serde_json::to_vec(&value).expect("serialize descriptor");
    });
    assert_rejected(&expanded_mismatch, "expanded_size");

    let alias = mutate(&fixture, "descriptor-alias.zip", |entries| {
        descriptor_entry(entries).name = "pkg/.ZPKG-BINARY.JSON".to_owned();
    });
    assert_rejected(&alias, "missing pkg/.zpkg-binary.json");
}

fn patch_zip_headers(path: &Path, mut patch: impl FnMut(&mut [u8], usize, bool)) {
    let mut bytes = fs::read(path).expect("read ZIP for header patch");
    let mut patched = 0usize;
    for offset in 0..bytes.len().saturating_sub(4) {
        let signature = &bytes[offset..offset + 4];
        if signature == b"PK\x03\x04" {
            patch(&mut bytes, offset, false);
            patched += 1;
        } else if signature == b"PK\x01\x02" {
            patch(&mut bytes, offset, true);
            patched += 1;
        }
    }
    assert!(patched >= 2, "expected local and central ZIP headers");
    fs::write(path, bytes).expect("write patched ZIP");
}

#[test]
fn rejects_encryption_unsupported_compression_and_ratio_bombs() {
    let fixture = fixture();

    let encrypted = mutate(&fixture, "encrypted.zip", |_| {});
    patch_zip_headers(&encrypted, |bytes, offset, central| {
        let flag_offset = offset + if central { 8 } else { 6 };
        let flags = u16::from_le_bytes([bytes[flag_offset], bytes[flag_offset + 1]]) | 1;
        bytes[flag_offset..flag_offset + 2].copy_from_slice(&flags.to_le_bytes());
    });
    assert_rejected(&encrypted, "encrypted");

    let unsupported = mutate(&fixture, "unsupported-compression.zip", |_| {});
    patch_zip_headers(&unsupported, |bytes, offset, central| {
        let method_offset = offset + if central { 10 } else { 8 };
        bytes[method_offset..method_offset + 2].copy_from_slice(&12_u16.to_le_bytes());
    });
    assert_rejected(&unsupported, "unsupported compression");

    let ratio_bomb = mutate(&fixture, "ratio-bomb.zip", |entries| {
        entries.push(Entry {
            name: "pkg/share/high-ratio.bin".to_owned(),
            bytes: vec![0; 8 * 1024 * 1024],
            mode: 0o644,
            compression: CompressionMethod::Deflated,
        });
    });
    assert_rejected(&ratio_bomb, "compression ratio");
}
