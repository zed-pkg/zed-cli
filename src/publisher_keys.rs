//! Publisher signing keys: generation, storage, signing, and the trust
//! decision that lets a mirror's answer be believed.
//!
//! The asymmetry worth keeping in mind: a *pinned* artifact needs no
//! signature, because the lockfile's sha256 already decides what is
//! acceptable. Signatures exist for the unpinned case — resolving a range,
//! adding a dependency for the first time — where the answer is the metadata
//! itself and a mirror could otherwise choose it.
//!
//! Trust is anchored in three independent places so that no single one has to
//! be reachable: the registry serves the org's keys, the package's own
//! `.zpkg.toml` declares them, and the consumer's `.zpkg.lock` pins whichever
//! key actually signed, on first use. During a registry outage the first is
//! gone and the second may be too — the pin is what remains, and it is the
//! strongest of the three anyway, because it was established over TLS against
//! the canonical registry at a time when nothing was degraded.
//!
//! Private keys are never read by anything except [`sign_preimage`], never
//! logged, and never travel over the network.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use zed_interfaces::signing::{
    DetachedSignatureV1, ED25519_PUBLIC_KEY_BYTES, ED25519_SIGNATURE_BYTES, PublisherKeyStateV1,
    PublisherKeyV1, SIGNING_ALGORITHM, decode_multibase_base58btc, encode_multibase_base58btc,
};

/// Schema marker on a stored private key file.
pub const PRIVATE_KEY_SCHEMA_V1: &str = "zpkg.publisher-private-key/v1";
/// Environment variable carrying a private key directly, for CI runners that
/// have a secret store but no home directory worth trusting.
pub const SIGNING_KEY_ENV: &str = "ZED_PKG_SIGNING_KEY";

/// A private key as stored on disk.
///
/// Deliberately self-describing: a bare 32-byte blob in a file is impossible
/// to attribute later, and an operator holding several keys needs to know
/// which org and key id a file belongs to without loading it into a signer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPrivateKey {
    pub schema: String,
    pub org: String,
    pub key_id: String,
    pub algorithm: String,
    pub public_key_multibase: String,
    /// Multibase base58btc of the 32-byte Ed25519 seed.
    pub private_key_multibase: String,
}

impl StoredPrivateKey {
    pub fn public(&self) -> PublisherKeyV1 {
        PublisherKeyV1 {
            key_id: self.key_id.clone(),
            algorithm: SIGNING_ALGORITHM.to_owned(),
            public_key_multibase: self.public_key_multibase.clone(),
            state: PublisherKeyStateV1::Active,
            enrolled_at: None,
            revoked_reason: None,
        }
    }

    fn signing_key(&self) -> Result<SigningKey> {
        ensure!(
            self.algorithm == SIGNING_ALGORITHM,
            "unsupported signing algorithm `{}`",
            self.algorithm
        );
        let seed = decode_multibase_base58btc(&self.private_key_multibase)
            .map_err(|error| anyhow!("stored private key is unreadable: {error}"))?;
        let seed: [u8; ED25519_PUBLIC_KEY_BYTES] = seed
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("stored private key is not {ED25519_PUBLIC_KEY_BYTES} bytes"))?;
        let signing = SigningKey::from_bytes(&seed);
        // A file whose halves disagree is a file that will produce signatures
        // nobody can verify. Catching it here turns a silent, permanent
        // publishing failure into an immediate, local one.
        let derived = encode_multibase_base58btc(signing.verifying_key().as_bytes());
        ensure!(
            derived == self.public_key_multibase,
            "stored private key does not match its recorded public key"
        );
        Ok(signing)
    }
}

/// Where private keys live, and how they are found.
pub struct KeyStore {
    root: PathBuf,
}

impl KeyStore {
    /// `<zed home>/keys`. Kept out of the content-addressed store: nothing
    /// under `store/` is secret, and a key that lands there would be copied by
    /// every tool that syncs a store.
    pub fn new(zed_home: &Path) -> Self {
        Self {
            root: zed_home.join("keys"),
        }
    }

    pub fn path_for(&self, org: &str, key_id: &str) -> PathBuf {
        self.root.join(org).join(format!("{key_id}.json"))
    }

    /// Create a new key pair and persist the private half.
    ///
    /// Refuses to overwrite: silently replacing a signing key would make every
    /// previously published signature unverifiable, with no way back.
    pub fn generate(&self, org: &str, key_id: &str) -> Result<(StoredPrivateKey, PathBuf)> {
        let path = self.path_for(org, key_id);
        ensure!(
            !path.exists(),
            "a signing key for `{org}/{key_id}` already exists at {}; \
             choose another key id rather than overwriting it",
            path.display()
        );
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        let stored = StoredPrivateKey {
            schema: PRIVATE_KEY_SCHEMA_V1.to_owned(),
            org: org.to_owned(),
            key_id: key_id.to_owned(),
            algorithm: SIGNING_ALGORITHM.to_owned(),
            public_key_multibase: encode_multibase_base58btc(signing.verifying_key().as_bytes()),
            private_key_multibase: encode_multibase_base58btc(&signing.to_bytes()),
        };
        self.write(&path, &stored)?;
        Ok((stored, path))
    }

    /// Load a key: the environment variable first, then the on-disk store.
    ///
    /// The environment wins so a CI job can inject a key without writing one
    /// to a runner's disk, where it would outlive the job.
    pub fn load(&self, org: &str, key_id: &str) -> Result<StoredPrivateKey> {
        if let Ok(raw) = std::env::var(SIGNING_KEY_ENV) {
            let stored = parse_env_key(&raw, org, key_id)?;
            if stored.key_id == key_id && stored.org == org {
                return Ok(stored);
            }
        }
        let path = self.path_for(org, key_id);
        let bytes = fs::read(&path).with_context(|| {
            format!(
                "no signing key for `{org}/{key_id}`; run `zed key generate --org {org} --key-id {key_id}` \
                 or set {SIGNING_KEY_ENV}"
            )
        })?;
        let stored: StoredPrivateKey = serde_json::from_slice(&bytes)
            .with_context(|| format!("reading signing key {}", path.display()))?;
        ensure!(
            stored.schema == PRIVATE_KEY_SCHEMA_V1,
            "{} is not a {PRIVATE_KEY_SCHEMA_V1} document",
            path.display()
        );
        Ok(stored)
    }

    /// Every key this machine holds for an org.
    pub fn list(&self, org: &str) -> Result<Vec<StoredPrivateKey>> {
        let dir = self.root.join(org);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "json")
                && let Ok(bytes) = fs::read(entry.path())
                && let Ok(stored) = serde_json::from_slice::<StoredPrivateKey>(&bytes)
            {
                out.push(stored);
            }
        }
        out.sort_by(|left, right| left.key_id.cmp(&right.key_id));
        Ok(out)
    }

    fn write(&self, path: &Path, stored: &StoredPrivateKey) -> Result<()> {
        let parent = path.parent().context("key path has a parent")?;
        fs::create_dir_all(parent)?;
        restrict_permissions(parent, 0o700)?;
        let json = serde_json::to_string_pretty(stored)? + "\n";
        // Create with the restricted mode from the start. Writing first and
        // chmod-ing after leaves a window in which the key is world-readable.
        write_private(path, json.as_bytes())?;
        Ok(())
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    // Windows has no mode bits to set at create time; the file inherits the
    // parent directory's ACL, and the parent is under the user profile.
    fs::write(path, bytes).with_context(|| format!("creating {}", path.display()))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

fn parse_env_key(raw: &str, org: &str, key_id: &str) -> Result<StoredPrivateKey> {
    let trimmed = raw.trim();
    // Two accepted spellings: a full key document, or the bare seed for the
    // key the caller already named. The bare form is what a secret manager
    // usually holds.
    if trimmed.starts_with('{') {
        let stored: StoredPrivateKey = serde_json::from_str(trimmed)
            .with_context(|| format!("{SIGNING_KEY_ENV} is not a valid key document"))?;
        return Ok(stored);
    }
    let seed = decode_multibase_base58btc(trimmed)
        .map_err(|error| anyhow!("{SIGNING_KEY_ENV} is not a multibase key: {error}"))?;
    let seed: [u8; ED25519_PUBLIC_KEY_BYTES] = seed.as_slice().try_into().map_err(|_| {
        anyhow!("{SIGNING_KEY_ENV} must decode to {ED25519_PUBLIC_KEY_BYTES} bytes")
    })?;
    let signing = SigningKey::from_bytes(&seed);
    Ok(StoredPrivateKey {
        schema: PRIVATE_KEY_SCHEMA_V1.to_owned(),
        org: org.to_owned(),
        key_id: key_id.to_owned(),
        algorithm: SIGNING_ALGORITHM.to_owned(),
        public_key_multibase: encode_multibase_base58btc(signing.verifying_key().as_bytes()),
        private_key_multibase: encode_multibase_base58btc(&signing.to_bytes()),
    })
}

/// Publisher keys this machine has learned, cached across runs.
///
/// Populated whenever the canonical registry answers — that contact happens
/// over TLS against the authoritative host, which is the best moment there
/// will ever be to learn what an org signs with. The cache is what makes a
/// *later* degraded resolution possible: by the time the registry is
/// unreachable, there is no way left to ask.
///
/// It is a cache, not an authority. A lockfile pin always overrides it, and a
/// missing entry degrades to "cannot verify" rather than to "trust anyway".
#[derive(Debug, Clone)]
pub struct TrustCache {
    root: PathBuf,
}

impl TrustCache {
    pub fn new(zed_home: &Path) -> Self {
        Self {
            root: zed_home.join("keys").join("trusted"),
        }
    }

    fn path_for(&self, org: &str) -> PathBuf {
        self.root.join(format!("{org}.json"))
    }

    /// Keys known for an org, newest write wins. Never fails: an unreadable or
    /// corrupt cache is treated as an empty one, because a broken cache should
    /// cost a verification, not an install.
    pub fn keys_for(&self, org: &str) -> Vec<PublisherKeyV1> {
        let Ok(bytes) = fs::read(self.path_for(org)) else {
            return Vec::new();
        };
        let Ok(set) = serde_json::from_slice::<zed_interfaces::signing::PublisherKeySetV1>(&bytes)
        else {
            return Vec::new();
        };
        if set.org != org || set.validate().is_err() {
            return Vec::new();
        }
        set.keys
    }

    /// Record what the registry says an org signs with.
    ///
    /// A revocation must never be lost to a stale write, so states merge
    /// toward the more restrictive value: once a key is seen revoked, no later
    /// document can quietly return it to active.
    pub fn remember(&self, org: &str, keys: &[PublisherKeyV1]) {
        let mut merged: BTreeMap<String, PublisherKeyV1> = self
            .keys_for(org)
            .into_iter()
            .map(|key| (key.key_id.clone(), key))
            .collect();
        for key in keys {
            if key.validate().is_err() {
                continue;
            }
            match merged.get(&key.key_id) {
                Some(existing) if existing.state == PublisherKeyStateV1::Revoked => continue,
                _ => {
                    merged.insert(key.key_id.clone(), key.clone());
                }
            }
        }
        let set = zed_interfaces::signing::PublisherKeySetV1 {
            schema: zed_interfaces::signing::PUBLISHER_KEYS_SCHEMA_V1.to_owned(),
            org: org.to_owned(),
            keys: merged.into_values().collect(),
        };
        // Best effort by design: failing an install because a cache directory
        // is read-only would trade a real capability for a nicety.
        let _ = fs::create_dir_all(&self.root);
        if let Ok(json) = serde_json::to_string_pretty(&set) {
            let _ = fs::write(self.path_for(org), json + "\n");
        }
    }
}

/// Sign a preimage produced by `zed_interfaces::signing`.
pub fn sign_preimage(stored: &StoredPrivateKey, preimage: &[u8]) -> Result<DetachedSignatureV1> {
    let signing = stored.signing_key()?;
    let signature: Signature = signing.sign(preimage);
    Ok(DetachedSignatureV1::new(
        &stored.key_id,
        &signature.to_bytes(),
    ))
}

/// What a caller learned by verifying a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The key that actually verified — this is what gets pinned.
    pub key: PublisherKeyV1,
    /// True when the key was already pinned in the lockfile, i.e. this was a
    /// continuity check rather than a first encounter.
    pub was_pinned: bool,
}

/// Verify a detached signature set against a trusted key set.
///
/// `pinned` is the lockfile's trust-on-first-use record. When present, it is
/// the *only* acceptable signer — not merely a preferred one. A mirror that
/// answers with a document signed by a different enrolled key of the same org
/// is refused, because "the org rotated" and "an attacker enrolled a key" look
/// identical from here, and the safe reading of an ambiguous signer change
/// during an outage is to stop.
///
/// Rotation is therefore an explicit act: the lockfile entry changes, in a
/// diff a human reviews.
pub fn verify(
    preimage: &[u8],
    signatures: &[DetachedSignatureV1],
    trusted: &[PublisherKeyV1],
    pinned: Option<&PublisherKeyV1>,
) -> Result<Verified> {
    ensure!(
        !signatures.is_empty(),
        "document carries no publisher signature"
    );

    let candidates: Vec<&PublisherKeyV1> = match pinned {
        Some(pin) => {
            // Match on the key *bytes*, not the id. An id is a label the
            // answering party chooses; only the bytes are the identity.
            let matches_pin = trusted
                .iter()
                .find(|key| key.public_key_multibase == pin.public_key_multibase);
            match matches_pin {
                Some(key) if key.state == PublisherKeyStateV1::Revoked => {
                    bail!(
                        "the publisher key pinned in .zpkg.lock (`{}`) has been revoked{}; \
                         re-resolve this dependency against the registry before installing",
                        pin.key_id,
                        key.revoked_reason
                            .as_deref()
                            .map(|reason| format!(": {reason}"))
                            .unwrap_or_default()
                    );
                }
                // Not in the trusted set is fine: an offline client may have
                // no key set at all, and the pin is itself a trusted key.
                _ => vec![pin],
            }
        }
        None => trusted
            .iter()
            .filter(|key| key.state.verifies())
            .collect::<Vec<_>>(),
    };
    ensure!(
        !candidates.is_empty(),
        "no publisher key is available to verify this document"
    );

    let mut last_error: Option<String> = None;
    for signature in signatures {
        if signature.algorithm != SIGNING_ALGORITHM {
            last_error = Some(format!(
                "unsupported signature algorithm `{}`",
                signature.algorithm
            ));
            continue;
        }
        let Some(key) = candidates
            .iter()
            .find(|candidate| candidate.key_id == signature.key_id)
        else {
            last_error = Some(format!("no trusted key named `{}`", signature.key_id));
            continue;
        };
        match verify_one(preimage, signature, key) {
            Ok(()) => {
                return Ok(Verified {
                    key: (*key).clone(),
                    was_pinned: pinned.is_some(),
                });
            }
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(anyhow!(
        "no publisher signature verified{}",
        last_error
            .map(|reason| format!(": {reason}"))
            .unwrap_or_default()
    ))
}

fn verify_one(
    preimage: &[u8],
    signature: &DetachedSignatureV1,
    key: &PublisherKeyV1,
) -> Result<()> {
    let public = key
        .public_key()
        .map_err(|error| anyhow!("key `{}` is unusable: {error}", key.key_id))?;
    let verifying = VerifyingKey::from_bytes(&public)
        .map_err(|_| anyhow!("key `{}` is not a valid Ed25519 point", key.key_id))?;
    let bytes: [u8; ED25519_SIGNATURE_BYTES] = signature
        .signature_bytes()
        .map_err(|error| anyhow!("signature by `{}` is malformed: {error}", key.key_id))?;
    let signature = Signature::from_bytes(&bytes);
    // `verify_strict` rejects small-order and non-canonical public keys, so
    // one signature cannot be made to verify under two different keys.
    if verifying.verify_strict(preimage, &signature).is_ok() {
        return Ok(());
    }
    // Distinguish "wrong signature" from "signature that only the permissive
    // check accepts". Both fail, but only the second tells an operator their
    // signer is producing something zed will never accept.
    if verifying.verify(preimage, &signature).is_ok() {
        bail!(
            "signature by `{}` verifies only under the permissive check and is refused",
            key.key_id
        );
    }
    bail!("signature by `{}` does not verify", key.key_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::signing::encode_multibase_base58btc;

    fn key_pair(key_id: &str) -> (StoredPrivateKey, PublisherKeyV1) {
        let signing = SigningKey::from_bytes(&[7_u8; 32]);
        let stored = StoredPrivateKey {
            schema: PRIVATE_KEY_SCHEMA_V1.to_owned(),
            org: "acme".to_owned(),
            key_id: key_id.to_owned(),
            algorithm: SIGNING_ALGORITHM.to_owned(),
            public_key_multibase: encode_multibase_base58btc(signing.verifying_key().as_bytes()),
            private_key_multibase: encode_multibase_base58btc(&signing.to_bytes()),
        };
        let public = stored.public();
        (stored, public)
    }

    #[test]
    fn a_valid_signature_verifies() {
        let (stored, public) = key_pair("acme-2026");
        let signature = sign_preimage(&stored, b"payload").expect("sign");
        let verified = verify(b"payload", &[signature], &[public], None).expect("verify");
        assert_eq!(verified.key.key_id, "acme-2026");
        assert!(!verified.was_pinned);
    }

    #[test]
    fn a_tampered_payload_does_not_verify() {
        let (stored, public) = key_pair("acme-2026");
        let signature = sign_preimage(&stored, b"payload").expect("sign");
        assert!(verify(b"payload!", &[signature], &[public], None).is_err());
    }

    #[test]
    fn a_pin_refuses_a_different_key_from_the_same_org() {
        let (stored, public) = key_pair("acme-2026");
        let other_signing = SigningKey::from_bytes(&[9_u8; 32]);
        let other = PublisherKeyV1 {
            key_id: "acme-rogue".to_owned(),
            algorithm: SIGNING_ALGORITHM.to_owned(),
            public_key_multibase: encode_multibase_base58btc(
                other_signing.verifying_key().as_bytes(),
            ),
            state: PublisherKeyStateV1::Active,
            enrolled_at: None,
            revoked_reason: None,
        };
        let rogue_stored = StoredPrivateKey {
            schema: PRIVATE_KEY_SCHEMA_V1.to_owned(),
            org: "acme".to_owned(),
            key_id: "acme-rogue".to_owned(),
            algorithm: SIGNING_ALGORITHM.to_owned(),
            public_key_multibase: other.public_key_multibase.clone(),
            private_key_multibase: encode_multibase_base58btc(&other_signing.to_bytes()),
        };
        let signature = sign_preimage(&rogue_stored, b"payload").expect("sign");
        let error = verify(
            b"payload",
            &[signature],
            &[public.clone(), other],
            Some(&public),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("no trusted key named"),
            "{error}"
        );
    }

    #[test]
    fn a_revoked_pinned_key_stops_the_install() {
        let (stored, public) = key_pair("acme-2026");
        let signature = sign_preimage(&stored, b"payload").expect("sign");
        let revoked = PublisherKeyV1 {
            state: PublisherKeyStateV1::Revoked,
            revoked_reason: Some("laptop compromised".to_owned()),
            ..public.clone()
        };
        // The signature itself is perfectly valid. Revocation is what stops
        // it, which is the whole difference between revoked and retired.
        let error = verify(b"payload", &[signature], &[revoked], Some(&public)).unwrap_err();
        assert!(error.to_string().contains("revoked"), "{error}");
        assert!(
            error.to_string().contains("laptop compromised"),
            "the reason should reach the operator: {error}"
        );
    }

    #[test]
    fn a_retired_key_still_verifies_history() {
        let (stored, public) = key_pair("acme-2026");
        let signature = sign_preimage(&stored, b"payload").expect("sign");
        let retired = PublisherKeyV1 {
            state: PublisherKeyStateV1::Retired,
            ..public
        };
        assert!(verify(b"payload", &[signature], &[retired], None).is_ok());
    }

    #[test]
    fn a_key_file_whose_halves_disagree_is_refused() {
        let (mut stored, _) = key_pair("acme-2026");
        stored.public_key_multibase = encode_multibase_base58btc(&[0_u8; 32]);
        let error = stored.signing_key().unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
    }
}

/// Current time as the `YYYY-MM-DDTHH:MM:SSZ` form the signing contract uses.
///
/// Hand-rolled rather than pulling a date crate in for one function, and
/// second-resolution rather than sub-second: the value is inside a signed
/// payload, so every producer has to agree on its spelling exactly, and fewer
/// digits is fewer ways to disagree.
pub fn utc_now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    format_utc(seconds)
}

/// Civil time from a Unix timestamp, by Howard Hinnant's `civil_from_days`.
/// Proleptic Gregorian, no leap seconds — which is what Unix time already is.
fn format_utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let time_of_day = seconds % 86_400;
    let (hour, minute, second) = (
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60,
    );

    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod time_tests {
    use super::format_utc;

    #[test]
    fn known_instants_round_trip() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where a naive day-count conversion goes wrong.
        assert_eq!(format_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_utc(1_735_689_599), "2024-12-31T23:59:59Z");
    }
}
