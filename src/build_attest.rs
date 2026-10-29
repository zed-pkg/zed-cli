//! Sign an existing compiled artifact with Zed's enrolled publisher key.
//! This is an UNTRUSTED SELF-REPORT, not a trusted compilation attestation.
//! The Oreslang API must authenticate, durably consume the challenge and
//! independently verify builder execution before any release promotion.
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use zed_interfaces::signed_build::{
    BuildClaimsV1, SIGNED_BUILD_AUDIENCE_V1, SIGNED_BUILD_SCHEMA_V1, SignedBuildV1,
};

use crate::publisher_keys::{KeyStore, sign_preimage};

#[derive(Debug)]
pub struct BuildAttestOptions<'a> {
    pub org: &'a str,
    pub package: &'a str,
    pub version: &'a str,
    pub device_key_id: &'a str,
    pub challenge_id: &'a str,
    pub nonce_sha256: &'a str,
    pub source_commit: &'a str,
    pub source_sha256: &'a str,
    pub toolchain_sha256: &'a str,
    pub target: &'a str,
    pub expires_at_unix_ms: u64,
    pub artifact: &'a Path,
    pub output: &'a Path,
}

pub fn attest(home: &Path, input: &BuildAttestOptions<'_>) -> Result<()> {
    // Refuse symlink inputs and outputs. Store credentials in the existing
    // KeyStore, never in argv, artifact data, signed claims or logs.
    ensure!(
        input.artifact.is_file() && !input.artifact.is_symlink(),
        "artifact must be a regular non-symlink file"
    );
    ensure!(!input.output.exists() && !input.output.is_symlink(), "output already exists");
    ensure!(input.output != input.artifact, "output must differ from artifact");
    let (digest, size) = sha256_file(input.artifact)?;
    ensure!(size > 0, "empty build artifacts cannot be signed");
    let claims = BuildClaimsV1 {
        audience: SIGNED_BUILD_AUDIENCE_V1.into(),
        org: input.org.into(),
        package: input.package.into(),
        version: input.version.into(),
        challenge_id: input.challenge_id.into(),
        nonce_sha256: input.nonce_sha256.into(),
        device_key_id: input.device_key_id.into(),
        source_commit: input.source_commit.into(),
        source_sha256: input.source_sha256.into(),
        toolchain_sha256: input.toolchain_sha256.into(),
        target: input.target.into(),
        artifact_sha256: digest,
        artifact_size: size,
        expires_at_unix_ms: input.expires_at_unix_ms,
    };
    let preimage = claims.preimage().map_err(anyhow::Error::msg)?;
    let key = KeyStore::new(home).load(input.org, input.device_key_id)?;
    let signature = sign_preimage(&key, &preimage)?;
    let receipt = SignedBuildV1 {
        schema: SIGNED_BUILD_SCHEMA_V1.into(), claims, signatures: vec![signature],
    };
    receipt.validate().map_err(anyhow::Error::msg)?;
    let serialized = serde_json::to_vec_pretty(&receipt)?;
    // Create a new public receipt, never overwrite a prior one. A failed write
    // removes only the newly created file, not the build artifact.
    let mut writer = OpenOptions::new().write(true).create_new(true)
        .open(input.output).with_context(|| format!("create {}", input.output.display()))?;
    if let Err(error) = writer.write_all(&serialized).and_then(|_| writer.sync_all()) {
        drop(writer);
        let _ = fs::remove_file(input.output);
        return Err(error.into());
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut input = fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65_536];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        total = total.checked_add(count as u64).context("artifact size overflow")?;
        hash.update(&buffer[..count]);
    }
    Ok((hex::encode(hash.finalize()), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_artifact_without_creating_a_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("attestation.json");
        let opts = BuildAttestOptions {
            org:"acme",package:"widget",version:"1.0.0",device_key_id:"device",
            challenge_id:"12345678-1234-1234-1234-123456789abc",
            nonce_sha256:&"a".repeat(64),source_commit:&"b".repeat(40),
            source_sha256:&"c".repeat(64),toolchain_sha256:&"d".repeat(64),
            target:"llvm",expires_at_unix_ms:1_800_000_000_000,
            artifact:&temp.path().join("missing"),output:&output,
        };
        assert!(attest(temp.path(), &opts).is_err());
        assert!(!output.exists());
    }
}
