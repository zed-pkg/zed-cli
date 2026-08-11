//! Secure native-binary ZIP packing, verification, publication, and download.
//!
//! Binary artifacts are intentionally separate from source packages at the
//! artifact layer, not the release layer: `org/name/version` remains the
//! release identity, while `.zpkg-binary.json` records one normalized platform
//! for the ZIP. The live v1 registry still stores one artifact per version, so
//! this module fails loudly rather than encoding platforms into SemVer.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zed_interfaces::artifact::ArtifactFormat;
use zed_interfaces::binary_artifact::{
    BINARY_ARCHIVE_ROOT, BINARY_ARTIFACT_SCHEMA_V1, BINARY_DESCRIPTOR_PATH,
    BINARY_PACKAGE_MANIFEST_PATH, BinaryArchiveFormatV1, BinaryArtifactManifestV1, BinaryFileV1,
    BinaryPackageIdentityV1, BinaryPlatformV1, BinarySourceProvenanceV1,
    validate_safe_relative_path,
};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{MANIFEST_FILE, PACK_OUT_DIR};
use zed_interfaces::registry::{PublishMeta, PublishResponse, VersionMetadata};

use crate::config::{Config, read_manifest};
use crate::interactive;
use crate::pack::{PackResult, sha256_file};
use crate::registry::registry_for;

const DEFAULT_MAX_BINARY_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_BINARY_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_MAX_BINARY_ENTRIES: usize = 200_000;
const DEFAULT_MAX_BINARY_COMPRESSION_RATIO: u64 = 1_000;
const MAX_DESCRIPTOR_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PACKAGE_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct BinaryPackOptions {
    pub platform: BinaryPlatformV1,
    pub includes: Vec<PathBuf>,
    pub out_dir: Option<PathBuf>,
    pub vcs_commit: Option<String>,
}

#[derive(Debug)]
pub struct BinaryPackResult {
    pub manifest: Manifest,
    pub descriptor: BinaryArtifactManifestV1,
    pub packed: PackResult,
}

#[derive(Debug)]
pub struct VerifiedBinaryArtifact {
    pub manifest: Manifest,
    pub descriptor: BinaryArtifactManifestV1,
    pub sha256: String,
    pub size: u64,
    pub file_count: usize,
}

#[derive(Debug)]
struct CollectedFile {
    path: String,
    source: CollectedSource,
    sha256: String,
    size: u64,
    executable: bool,
}

#[derive(Debug)]
enum CollectedSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

include!("binary_archive/pack.rs");
include!("binary_archive/verify.rs");
include!("binary_archive/registry_io.rs");
include!("binary_archive/write.rs");
include!("binary_archive/collect.rs");
include!("binary_archive/validate.rs");
include!("binary_archive/tests.rs");
