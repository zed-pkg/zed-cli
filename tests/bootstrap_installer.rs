#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::Result;
use sha2::{Digest, Sha256};

const PAYLOAD: &[u8] = b"#!/bin/sh\nprintf 'zed 1.2.3\\n'\n";

fn archive(names: &[&str], symlink: bool) -> Result<Vec<u8>> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for name in names {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o755);
        if symlink {
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_link_name("outside")?;
            header.set_size(0);
            builder.append_data(&mut header, name, &[][..])?;
        } else {
            header.set_size(PAYLOAD.len() as u64);
            builder.append_data(&mut header, name, PAYLOAD)?;
        }
    }
    Ok(builder.into_inner()?.finish()?)
}

struct Fixture {
    root: tempfile::TempDir,
    asset: String,
}

impl Fixture {
    fn new(bytes: &[u8]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let target = match std::env::consts::OS {
            "macos" => "apple-darwin",
            _ => "unknown-linux-musl",
        };
        let asset = format!("zed-{}-{target}.tar.gz", std::env::consts::ARCH);
        fs::write(root.path().join("archive"), bytes)?;
        fs::write(
            root.path().join("checksum"),
            format!("{:x}  {asset}\n", Sha256::digest(bytes)),
        )?;
        fs::create_dir(root.path().join("mock-bin"))?;
        let curl = root.path().join("mock-bin/curl");
        fs::write(
            &curl,
            r#"#!/bin/bash
set -euo pipefail
out=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done
case "$url" in
  */releases/latest) printf '%s' 'https://github.com/zed-pkg/zed-cli/releases/tag/v1.2.3' ;;
  *.sha256) cp "$FIXTURE/checksum" "$out" ;;
  *.tar.gz) cp "$FIXTURE/archive" "$out" ;;
  *) exit 90 ;;
esac
"#,
        )?;
        fs::set_permissions(curl, fs::Permissions::from_mode(0o755))?;
        Ok(Self { root, asset })
    }

    fn run(&self, install: &Path, version: Option<&str>) -> Result<Output> {
        let mut command = Command::new("/bin/bash");
        command
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
            .env_clear()
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.root.path().join("mock-bin").display()
                ),
            )
            .env("FIXTURE", self.root.path())
            .env("HOME", self.root.path())
            .env("SHELL", "/bin/sh")
            .env("ZED_INSTALL_DIR", install)
            .env("ZED_PROFILE", self.root.path().join("profile"));
        if let Some(version) = version {
            command.env("ZED_VERSION", version);
        }
        Ok(command.output()?)
    }

    fn assert_refused(&self) -> Result<()> {
        let install = self.root.path().join("bin");
        fs::create_dir(&install)?;
        fs::write(install.join("zed"), b"original")?;
        let output = self.run(&install, Some("v1.2.3"))?;
        assert!(!output.status.success(), "unexpected success: {output:?}");
        assert_eq!(fs::read(install.join("zed"))?, b"original");
        assert!(!self.root.path().join("profile").exists());
        Ok(())
    }
}

#[test]
fn verified_install_handles_shell_metacharacters_and_is_idempotent() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed", "zed-gitops"], false)?)?;
    let install = fixture
        .root
        .path()
        .join("bin ' $(touch PWNED) `touch PWNED2`");
    for version in [Some("v1.2.3"), None] {
        let output = fixture.run(&install, version)?;
        assert!(output.status.success(), "{output:?}");
    }
    assert_eq!(fs::read(install.join("zed"))?, PAYLOAD);
    assert!(!install.join("zed-gitops").exists());
    let profile = fixture.root.path().join("profile");
    let text = fs::read_to_string(&profile)?;
    assert_eq!(text.matches("export PATH=").count(), 1);
    let output = Command::new("/bin/sh")
        .args(["-c", ". \"$1\"; printf '%s' \"$PATH\"", "sh"])
        .arg(profile)
        .current_dir(fixture.root.path())
        .env_clear()
        .env("HOME", fixture.root.path())
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        format!("{}:/usr/bin:/bin", fs::canonicalize(&install)?.display())
    );
    assert!(!fixture.root.path().join("PWNED").exists());
    assert!(!fixture.root.path().join("PWNED2").exists());
    Ok(())
}

#[test]
fn missing_checksum_preserves_installed_binary() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed"], false)?)?;
    fs::remove_file(fixture.root.path().join("checksum"))?;
    fixture.assert_refused()
}

#[test]
fn corrupted_archive_preserves_installed_binary() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed"], false)?)?;
    fs::OpenOptions::new()
        .append(true)
        .open(fixture.root.path().join("archive"))?
        .write_all(b"corrupted")?;
    fixture.assert_refused()
}

#[test]
fn duplicate_checksum_preserves_installed_binary() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed"], false)?)?;
    let checksum = fixture.root.path().join("checksum");
    fs::write(&checksum, fs::read_to_string(&checksum)?.repeat(2))?;
    fixture.assert_refused()
}

#[test]
fn wrong_checksum_filename_preserves_installed_binary() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed"], false)?)?;
    let checksum = fixture.root.path().join("checksum");
    fs::write(
        &checksum,
        fs::read_to_string(&checksum)?.replace(&fixture.asset, "other.tar.gz"),
    )?;
    fixture.assert_refused()
}

#[test]
fn duplicate_binary_member_is_rejected() -> Result<()> {
    Fixture::new(&archive(&["zed", "zed"], false)?)?.assert_refused()
}

#[test]
fn symlink_binary_member_is_rejected() -> Result<()> {
    Fixture::new(&archive(&["zed"], true)?)?.assert_refused()
}

#[test]
fn unsafe_version_is_rejected_before_installation() -> Result<()> {
    let fixture = Fixture::new(&archive(&["zed"], false)?)?;
    let install = fixture.root.path().join("bin");
    assert!(
        !fixture
            .run(&install, Some("v1.2.3/../../other"))?
            .status
            .success()
    );
    assert!(!install.exists());
    Ok(())
}
