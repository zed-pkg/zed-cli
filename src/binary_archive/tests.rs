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
}
