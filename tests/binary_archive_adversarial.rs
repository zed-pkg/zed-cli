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
    let epoch = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("valid ZIP epoch");
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
    let error = verify_binary_zip(path, None).expect_err("adversarial archive must be rejected");
    let error = format!("{error:#}");
    assert!(
        error
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
        "expected error containing `{expected}`, got `{error}`"
    );
}

fn assert_rejected_for_any(path: &Path, expected: &[&str]) {
    let error = verify_binary_zip(path, None).expect_err("adversarial archive must be rejected");
    let error = format!("{error:#}");
    let normalized = error.to_ascii_lowercase();
    assert!(
        expected
            .iter()
            .any(|meaning| normalized.contains(&meaning.to_ascii_lowercase())),
        "expected an error containing one of {expected:?}, got `{error}`"
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
    assert_rejected_for_any(
        &tampered,
        &["digest mismatch", "size mismatch", "expanded byte count"],
    );

    let mode_mismatch = mutate(&fixture, "mode-mismatch.zip", |entries| {
        payload_entry(entries).mode = 0o644;
    });
    assert_rejected(&mode_mismatch, "executable mode");
}

#[test]
fn rejects_traversal_noncanonical_and_colliding_paths() {
    let fixture = fixture();
    let cases: [(&str, &str, &[&str]); 5] = [
        (
            "traversal.zip",
            "pkg/../escape",
            &["escapes", "not beneath"],
        ),
        (
            "absolute.zip",
            "/pkg/escape",
            &["escapes", "not beneath", "not canonically encoded"],
        ),
        ("outside-root.zip", "outside.txt", &["not beneath"]),
        ("backslash.zip", "pkg\\escape", &["backslash"]),
        ("casefold.zip", "pkg/BIN/hello", &["collide"]),
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
        assert_rejected_for_any(&archive, expected);
    }

    let duplicate = mutate(&fixture, "duplicate.zip", |entries| {
        entries.push(Entry {
            name: "pkg/dup/hello".to_owned(),
            bytes: b"duplicate path payload".to_vec(),
            mode: 0o755,
            compression: CompressionMethod::Stored,
        });
    });
    let mut duplicate_bytes = fs::read(&duplicate).expect("read duplicate fixture");
    let needle = b"pkg/dup/hello";
    let replacement = b"pkg/bin/hello";
    let mut replacements = 0;
    for offset in 0..=duplicate_bytes.len().saturating_sub(needle.len()) {
        if &duplicate_bytes[offset..offset + needle.len()] == needle {
            duplicate_bytes[offset..offset + replacement.len()].copy_from_slice(replacement);
            replacements += 1;
        }
    }
    assert_eq!(replacements, 2, "patch local and central names");
    fs::write(&duplicate, duplicate_bytes).expect("write duplicate fixture");
    assert_rejected_for_any(&duplicate, &["duplicate filename", "collide"]);

    let local_name_mismatch = mutate(&fixture, "local-name-mismatch.zip", |_| {});
    let mut mismatch_bytes = fs::read(&local_name_mismatch).expect("read local-name fixture");
    let central_name = b"pkg/bin/hello";
    let hostile_local_name = b"pkg/../escape";
    assert_eq!(central_name.len(), hostile_local_name.len());
    let offset = mismatch_bytes
        .windows(central_name.len())
        .position(|window| window == central_name)
        .expect("local payload filename");
    mismatch_bytes[offset..offset + hostile_local_name.len()].copy_from_slice(hostile_local_name);
    fs::write(&local_name_mismatch, mismatch_bytes).expect("write local-name fixture");
    assert_rejected_for_any(
        &local_name_mismatch,
        &["local and central filenames disagree"],
    );
}

#[test]
fn rejects_nonportable_device_unicode_and_directory_payload_paths() {
    let fixture = fixture();
    let long_component = "a".repeat(256);
    let cases: Vec<(&str, &str, &[u8], &[&str])> = vec![
        (
            "device.zip",
            "pkg/share/CON.txt",
            b"device",
            &["device name", "invalid value"],
        ),
        (
            "trailing-dot.zip",
            "pkg/share/name.",
            b"dot",
            &["trailing dot or space", "invalid value"],
        ),
        (
            "reserved-char.zip",
            "pkg/share/name?.txt",
            b"question",
            &["reserved character", "invalid value"],
        ),
        (
            "alternate-stream.zip",
            "pkg/share/name:stream",
            b"alternate data stream",
            &["reserved character", "invalid"],
        ),
        (
            "directory-data.zip",
            "pkg/share/",
            b"hidden directory data",
            &["directory", "payload bytes", "unsupported unix file type"],
        ),
        (
            "long-component.zip",
            &long_component,
            b"long",
            &["255-byte", "invalid value"],
        ),
    ];
    for (archive_name, entry_name, bytes, meanings) in cases {
        let archive = mutate(&fixture, archive_name, |entries| {
            entries.push(Entry {
                name: if archive_name == "long-component.zip" {
                    format!("pkg/{entry_name}")
                } else {
                    entry_name.to_owned()
                },
                bytes: bytes.to_vec(),
                mode: 0o644,
                compression: CompressionMethod::Stored,
            });
        });
        assert_rejected_for_any(&archive, meanings);
    }

    let unicode_collision = mutate(&fixture, "unicode-casefold.zip", |entries| {
        for name in ["pkg/share/Ä.txt", "pkg/share/ä.txt"] {
            entries.push(Entry {
                name: name.to_owned(),
                bytes: b"unicode collision".to_vec(),
                mode: 0o644,
                compression: CompressionMethod::Stored,
            });
        }
    });
    assert_rejected(&unicode_collision, "collide");

    let file_directory_collision = mutate(&fixture, "file-directory.zip", |entries| {
        for name in ["pkg/share", "pkg/share/child"] {
            entries.push(Entry {
                name: name.to_owned(),
                bytes: b"ambiguous hierarchy".to_vec(),
                mode: 0o644,
                compression: CompressionMethod::Stored,
            });
        }
    });
    assert_rejected_for_any(
        &file_directory_collision,
        &["nested beneath an existing file", "existing child path"],
    );
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

fn add_unnecessary_zip64_directory(path: &Path) {
    let bytes = fs::read(path).expect("read ZIP64 fixture");
    let eocd = bytes
        .windows(4)
        .rposition(|window| window == b"PK\x05\x06")
        .expect("EOCD");
    assert_eq!(eocd + 22, bytes.len(), "fixture has no ZIP comment");
    let entries = u16::from_le_bytes(bytes[eocd + 10..eocd + 12].try_into().unwrap()) as u64;
    let central_size = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as u64;
    let central_offset = u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().unwrap()) as u64;

    let mut zip64 = Vec::with_capacity(56);
    zip64.extend_from_slice(b"PK\x06\x06");
    zip64.extend_from_slice(&44_u64.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&45_u16.to_le_bytes());
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&0_u32.to_le_bytes());
    zip64.extend_from_slice(&entries.to_le_bytes());
    zip64.extend_from_slice(&entries.to_le_bytes());
    zip64.extend_from_slice(&central_size.to_le_bytes());
    zip64.extend_from_slice(&central_offset.to_le_bytes());
    assert_eq!(zip64.len(), 56);

    let mut locator = Vec::with_capacity(20);
    locator.extend_from_slice(b"PK\x06\x07");
    locator.extend_from_slice(&0_u32.to_le_bytes());
    locator.extend_from_slice(&(eocd as u64).to_le_bytes());
    locator.extend_from_slice(&1_u32.to_le_bytes());
    assert_eq!(locator.len(), 20);

    let mut ordinary_eocd = bytes[eocd..].to_vec();
    ordinary_eocd[8..12].fill(0xff);
    let mut rewritten = bytes[..eocd].to_vec();
    rewritten.extend_from_slice(&zip64);
    rewritten.extend_from_slice(&locator);
    rewritten.extend_from_slice(&ordinary_eocd);
    fs::write(path, rewritten).expect("write ZIP64 fixture");
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
    assert_rejected_for_any(&encrypted, &["encrypted", "password required"]);

    let data_descriptor = mutate(&fixture, "data-descriptor.zip", |_| {});
    patch_zip_headers(&data_descriptor, |bytes, offset, central| {
        let flag_offset = offset + if central { 8 } else { 6 };
        let flags = u16::from_le_bytes([bytes[flag_offset], bytes[flag_offset + 1]]) | (1 << 3);
        bytes[flag_offset..flag_offset + 2].copy_from_slice(&flags.to_le_bytes());
    });
    assert_rejected_for_any(
        &data_descriptor,
        &["data descriptor", "local and central", "checksum"],
    );

    let zip64 = mutate(&fixture, "unnecessary-zip64.zip", |_| {});
    add_unnecessary_zip64_directory(&zip64);
    assert_rejected(&zip64, "ZIP64 even though ordinary ZIP limits suffice");

    let unsupported = mutate(&fixture, "unsupported-compression.zip", |_| {});
    patch_zip_headers(&unsupported, |bytes, offset, central| {
        let method_offset = offset + if central { 10 } else { 8 };
        bytes[method_offset..method_offset + 2].copy_from_slice(&12_u16.to_le_bytes());
    });
    assert_rejected_for_any(
        &unsupported,
        &[
            "unsupported compression",
            "compression method not supported",
        ],
    );

    let symlink = mutate(&fixture, "symlink.zip", |_| {});
    patch_zip_headers(&symlink, |bytes, offset, central| {
        if central {
            let attributes_offset = offset + 38;
            let attributes = (0o120777_u32) << 16;
            bytes[attributes_offset..attributes_offset + 4]
                .copy_from_slice(&attributes.to_le_bytes());
        }
    });
    assert_rejected_for_any(&symlink, &["symlink", "unsupported Unix file type"]);

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
