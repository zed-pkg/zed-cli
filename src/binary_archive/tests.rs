#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        fs::create_dir_all(project.path().join("bin")).unwrap();
        fs::write(
            project.path().join(MANIFEST_FILE),
            r#"[package]
org = "acme"
name = "hello-bin"
version = "1.2.3"
description = "test binary"
license = "MIT"

[package.repository]
vcs = "git"
url = "https://github.com/acme/hello-bin"

[bin]
hello = "bin/hello"
"#,
        )
        .unwrap();
        fs::write(project.path().join("bin/hello"), b"hello binary\n").unwrap();
        project
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

    #[test]
    fn binary_zip_roundtrip_is_deterministic_and_self_describing() {
        let project = fixture_project();
        let output = tempfile::tempdir().unwrap();
        let options = BinaryPackOptions {
            platform: platform(),
            includes: Vec::new(),
            out_dir: Some(output.path().to_path_buf()),
            vcs_commit: Some("0123456789abcdef".to_owned()),
        };
        let first = pack_binary_zip(project.path(), &options).unwrap();
        let first_bytes = fs::read(&first.packed.path).unwrap();
        let second = pack_binary_zip(project.path(), &options).unwrap();
        let second_bytes = fs::read(&second.packed.path).unwrap();
        assert_eq!(first_bytes, second_bytes);
        assert_eq!(first.packed.sha256, second.packed.sha256);

        let verified = verify_binary_zip(&first.packed.path, Some(&platform())).unwrap();
        assert_eq!(verified.descriptor.entrypoints["hello"], "bin/hello");
        assert_eq!(verified.descriptor.platform.target, platform().target);
        assert_eq!(verified.manifest.package.version, "1.2.3");
    }

    #[test]
    fn verifier_rejects_a_self_extracting_prefix() {
        let project = fixture_project();
        let output = tempfile::tempdir().unwrap();
        let packed = pack_binary_zip(
            project.path(),
            &BinaryPackOptions {
                platform: platform(),
                includes: Vec::new(),
                out_dir: Some(output.path().to_path_buf()),
                vcs_commit: None,
            },
        )
        .unwrap();
        let original = fs::read(&packed.packed.path).unwrap();
        let prefixed = output.path().join("prefixed.zip");
        let mut bytes = b"MZ".to_vec();
        bytes.extend(original);
        fs::write(&prefixed, bytes).unwrap();
        assert!(
            verify_binary_zip(&prefixed, None)
                .unwrap_err()
                .to_string()
                .contains("self-extracting")
        );
    }

    #[test]
    fn packer_rejects_symlinked_payloads() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let project = fixture_project();
            fs::remove_file(project.path().join("bin/hello")).unwrap();
            fs::write(project.path().join("real-hello"), b"hello").unwrap();
            symlink("../real-hello", project.path().join("bin/hello")).unwrap();
            let error = pack_binary_zip(
                project.path(),
                &BinaryPackOptions {
                    platform: platform(),
                    includes: Vec::new(),
                    out_dir: None,
                    vcs_commit: None,
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("symlink"), "{error}");
        }
    }

    #[test]
    fn packer_never_clobbers_a_conflicting_output() {
        let project = fixture_project();
        let output = tempfile::tempdir().unwrap();
        let destination = output
            .path()
            .join("acme-hello-bin-1.2.3-x86_64-unknown-linux-gnu.zip");
        fs::write(&destination, b"operator-owned conflicting bytes").unwrap();

        let error = pack_binary_zip(
            project.path(),
            &BinaryPackOptions {
                platform: platform(),
                includes: Vec::new(),
                out_dir: Some(output.path().to_path_buf()),
                vcs_commit: None,
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("refusing to overwrite conflicting"));
        assert_eq!(
            fs::read(destination).unwrap(),
            b"operator-owned conflicting bytes"
        );
    }

    #[test]
    fn qualified_file_registry_roundtrip_keeps_release_and_target_separate() {
        use crate::registry::{FileRegistry, Registry};

        let project = fixture_project();
        let output = tempfile::tempdir().unwrap();
        let registry_root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let packed = pack_binary_zip(
            project.path(),
            &BinaryPackOptions {
                platform: platform(),
                includes: Vec::new(),
                out_dir: Some(output.path().to_path_buf()),
                vcs_commit: Some("0123456789abcdef".to_owned()),
            },
        )
        .unwrap();
        let cfg = Config {
            registry: format!("file://{}", registry_root.path().display()),
            home: home.path().to_path_buf(),
            token: None,
            auth_url: "http://127.0.0.1/unused".to_owned(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        };

        publish_binary_zip_with_route(
            &cfg,
            &packed,
            false,
            BinaryRegistryRoute::Qualified,
        )
        .unwrap();
        let registry = FileRegistry::new(registry_root.path().to_path_buf());
        assert!(registry.get_version("acme", "hello-bin", "1.2.3").is_err());
        let qualified = registry
            .get_binary_artifact(
                "acme",
                "hello-bin",
                "1.2.3",
                &platform().target,
                BinaryArchiveFormatV1::Zip,
            )
            .unwrap();
        assert_eq!(qualified.sha256, packed.packed.sha256);
        let descriptor_sha256 = hex::encode(Sha256::digest(
            packed.descriptor.canonical_json_bytes().unwrap(),
        ));
        assert_eq!(qualified.descriptor_sha256, descriptor_sha256);

        let publish_meta = BinaryArtifactPublishMetaV1 {
            schema: BINARY_ARTIFACT_PUBLISH_META_SCHEMA_V1.to_owned(),
            manifest: packed.manifest.clone(),
            platform: platform(),
            format: BinaryArchiveFormatV1::Zip,
            sha256: packed.packed.sha256.clone(),
            size: packed.packed.size,
            descriptor_sha256: "f".repeat(64),
            vcs_tag: packed.manifest.vcs_tag(),
            vcs_commit: packed
                .descriptor
                .source
                .as_ref()
                .and_then(|source| source.vcs_commit.clone()),
            attachments: Vec::new(),
        };
        let conflict = registry
            .publish_binary_artifact(&publish_meta, &packed.packed.path, None)
            .unwrap_err();
        assert!(format!("{conflict:#}").contains("immutable"));
        assert_eq!(
            registry
                .get_binary_artifact(
                    "acme",
                    "hello-bin",
                    "1.2.3",
                    &platform().target,
                    BinaryArchiveFormatV1::Zip,
                )
                .unwrap()
                .descriptor_sha256,
            descriptor_sha256
        );

        let destination = output.path().join("downloaded.zip");
        let downloaded = download_binary_zip_with_route(
            &cfg,
            "acme/hello-bin@1.2.3",
            &destination,
            Some(&platform().target),
            BinaryRegistryRoute::Qualified,
        )
        .unwrap();
        assert_eq!(downloaded.sha256, packed.packed.sha256);
    }

    #[cfg(unix)]
    #[test]
    fn packer_rejects_a_payload_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("tool");
        fs::write(&source, b"reviewed payload").unwrap();
        let mut files = BTreeMap::new();
        let mut portable_paths = BTreeMap::new();
        insert_source_file(
            &mut files,
            &mut portable_paths,
            "bin/tool",
            source.clone(),
            true,
        )
        .unwrap();

        fs::rename(&source, root.path().join("original-inode")).unwrap();
        fs::write(&source, b"replacement payload").unwrap();
        let archive = root.path().join("snapshot.zip");
        let error = write_binary_zip(&archive, &files, b"{}").unwrap_err();
        assert!(format!("{error:#}").contains("changed while being opened"));
    }

    #[cfg(unix)]
    #[test]
    fn promotion_rejects_symlink_destinations_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging.zip");
        let victim = root.path().join("victim");
        let destination = root.path().join("artifact.zip");
        fs::write(&staging, b"verified bytes").unwrap();
        fs::write(&victim, b"do not touch").unwrap();
        symlink(&victim, &destination).unwrap();
        let (sha256, size) = sha256_file(&staging).unwrap();

        let error = promote_verified_noclobber(
            &staging,
            &destination,
            &sha256,
            size,
            "test artifact",
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("non-symlink"));
        assert_eq!(fs::read(victim).unwrap(), b"do not touch");
    }
}
