//! `zed mirror` and `zed key`: making the fallback path inspectable.
//!
//! A resilience mechanism nobody can see is a resilience mechanism nobody
//! trusts, and one nobody exercises is usually broken. `zed mirror check` is
//! the important command here: it probes every fallback for every pinned
//! package while things are healthy, so the answer arrives on an ordinary
//! Tuesday rather than during an incident.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::Serialize;
use zed_interfaces::lockfile::Lockfile;
use zed_interfaces::mirror::{
    MIRROR_BOOTSTRAP_PATH, MirrorDescriptorV1, MirrorKindV1, default_public_mirrors,
};
use zed_interfaces::paths::{LOCKFILE_FILE, MANIFEST_FILE};
use zed_interfaces::registry::{self, OrgKeysRequest, OrgKeysResponse};
use zed_interfaces::signing::{PublisherKeySetV1, PublisherKeyV1};

use crate::cli::{KeyCmd, MirrorCmd};
use crate::config::Config;
use crate::mirror::{MirrorClient, merge_mirrors, registry_mirror};
use crate::publisher_keys::KeyStore;
use crate::registry::max_artifact_bytes;

pub fn run_mirror(cwd: &Path, cfg: &Config, cmd: MirrorCmd) -> Result<()> {
    match cmd {
        MirrorCmd::List { json } => list(cwd, cfg, json),
        MirrorCmd::Check { package, json } => check(cwd, cfg, package.as_deref(), json),
        MirrorCmd::Bootstrap { url } => bootstrap(cfg, url.as_deref()),
        MirrorCmd::Sync { output } => sync(cwd, cfg, &output),
    }
}

pub fn run_key(cfg: &Config, cmd: KeyCmd) -> Result<()> {
    let store = KeyStore::new(&cfg.home);
    match cmd {
        KeyCmd::Generate { org, key_id } => {
            let (stored, path) = store.generate(&org, &key_id)?;
            println!("created signing key `{}/{}`", org, key_id);
            println!("private key: {}", path.display());
            println!();
            println!("Enroll the public half so consumers can verify what mirrors serve.");
            println!("Add to .zpkg.toml:");
            println!();
            println!("  [[signing.key]]");
            println!("  key_id = \"{key_id}\"");
            println!("  algorithm = \"ed25519\"");
            println!(
                "  public_key_multibase = \"{}\"",
                stored.public_key_multibase
            );
            println!("  state = \"active\"");
            println!();
            println!("Then: zed key enroll --org {org} --key-id {key_id}");
            Ok(())
        }
        KeyCmd::List { org } => {
            let keys = store.list(&org)?;
            if keys.is_empty() {
                println!("no signing keys for `{org}` on this machine");
                return Ok(());
            }
            for key in keys {
                println!("{}\t{}", key.key_id, key.public_key_multibase);
            }
            Ok(())
        }
        KeyCmd::Show { org, key_id } => {
            let stored = store.load(&org, &key_id)?;
            println!("{}", stored.public_key_multibase);
            Ok(())
        }
        KeyCmd::Enroll { org, key_id } => enroll(cfg, &store, &org, &key_id),
    }
}

#[derive(Debug, Serialize)]
struct MirrorRow {
    id: String,
    kind: String,
    priority: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    urls: Vec<String>,
    serves: Vec<&'static str>,
}

fn describe(mirror: &MirrorDescriptorV1) -> MirrorRow {
    let mut serves = Vec::new();
    if mirror.serves.artifacts {
        serves.push("artifacts");
    }
    if mirror.serves.metadata {
        serves.push("metadata");
    }
    if mirror.serves.index {
        serves.push("index");
    }
    let mut urls = mirror.base_urls();
    if urls.is_empty()
        && let Ok(repo) = mirror.repo_ref()
    {
        urls.push(format!(
            "https://{}/{}/{}",
            repo.host, repo.owner, repo.repo
        ));
    }
    if let Some(path) = mirror.path.as_deref() {
        urls.push(format!("file://{path}"));
    }
    MirrorRow {
        id: mirror.identifier(),
        kind: mirror.kind.as_str().to_owned(),
        priority: mirror.effective_priority(),
        urls,
        serves,
    }
}

type ProjectMirrors = (
    Vec<MirrorDescriptorV1>,
    BTreeMap<String, Vec<MirrorDescriptorV1>>,
);

/// Everything that could serve this project, ambient and per-package.
fn project_mirrors(cwd: &Path, cfg: &Config) -> Result<ProjectMirrors> {
    let canonical: Vec<MirrorDescriptorV1> = registry_mirror(&cfg.registry).into_iter().collect();
    let ambient = merge_mirrors(&[&cfg.mirrors, &canonical])?;

    let mut per_package = BTreeMap::new();
    if let Some(lock) = read_lock(cwd)? {
        for package in &lock.packages {
            let merged = merge_mirrors(&[&package.mirrors, &ambient])?;
            per_package.insert(
                format!("{}@{}", package.full_name(), package.version),
                merged,
            );
        }
    }
    Ok((ambient, per_package))
}

fn read_lock(cwd: &Path) -> Result<Option<Lockfile>> {
    let Some(root) = cwd
        .ancestors()
        .find(|candidate| candidate.join(LOCKFILE_FILE).is_file())
    else {
        return Ok(None);
    };
    let text = fs::read_to_string(root.join(LOCKFILE_FILE))?;
    Ok(Some(Lockfile::parse(&text)?))
}

fn lock_root(cwd: &Path) -> Result<PathBuf> {
    cwd.ancestors()
        .find(|candidate| candidate.join(LOCKFILE_FILE).is_file())
        .map(Path::to_path_buf)
        .with_context(|| format!("no {LOCKFILE_FILE} at or above {}", cwd.display()))
}

fn list(cwd: &Path, cfg: &Config, json: bool) -> Result<()> {
    let (ambient, per_package) = project_mirrors(cwd, cfg)?;
    if json {
        #[derive(Serialize)]
        struct Output {
            ambient: Vec<MirrorRow>,
            packages: BTreeMap<String, Vec<MirrorRow>>,
        }
        let output = Output {
            ambient: ambient.iter().map(describe).collect(),
            packages: per_package
                .iter()
                .map(|(key, mirrors)| (key.clone(), mirrors.iter().map(describe).collect()))
                .collect(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("sources tried for every package, in order:");
    for row in ambient.iter().map(describe) {
        println!(
            "  {:>3}  {:<28} {:<16} {}",
            row.priority,
            row.id,
            row.serves.join(","),
            row.urls.join(" ")
        );
    }
    if per_package.is_empty() {
        println!();
        println!("no {LOCKFILE_FILE} here, so no package-specific mirrors are pinned yet");
        return Ok(());
    }
    for (package, mirrors) in &per_package {
        // Only the entries this package adds beyond the ambient set: repeating
        // the shared list once per dependency makes the specific ones harder
        // to see, not easier.
        let extra: Vec<_> = mirrors
            .iter()
            .filter(|mirror| {
                !ambient
                    .iter()
                    .any(|shared| shared.identifier() == mirror.identifier())
            })
            .map(describe)
            .collect();
        if extra.is_empty() {
            continue;
        }
        println!();
        println!("{package} additionally:");
        for row in extra {
            println!(
                "  {:>3}  {:<28} {:<16} {}",
                row.priority,
                row.id,
                row.serves.join(","),
                row.urls.join(" ")
            );
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ProbeResult {
    package: String,
    mirror: String,
    url: String,
    ok: bool,
    detail: String,
}

fn check(cwd: &Path, cfg: &Config, only: Option<&str>, json: bool) -> Result<()> {
    let lock =
        read_lock(cwd)?.with_context(|| format!("`zed mirror check` needs a {LOCKFILE_FILE}"))?;
    let (ambient, _) = project_mirrors(cwd, cfg)?;
    let client = MirrorClient::new(max_artifact_bytes())?;

    let mut results = Vec::new();
    let mut reachable_packages = 0_usize;
    let mut checked_packages = 0_usize;

    for locked in &lock.packages {
        if let Some(filter) = only
            && locked.full_name() != filter
        {
            continue;
        }
        checked_packages += 1;
        let mirrors = merge_mirrors(&[&locked.mirrors, &ambient])?;
        let coord = locked.mirror_coordinate();
        let package = format!("{}@{}", locked.full_name(), locked.version);
        let mut any = false;
        for mirror in &mirrors {
            if !mirror.serves.artifacts {
                continue;
            }
            let urls = match mirror.artifact_urls(&coord) {
                Ok(urls) => urls,
                Err(error) => {
                    results.push(ProbeResult {
                        package: package.clone(),
                        mirror: mirror.identifier(),
                        url: String::new(),
                        ok: false,
                        detail: error.to_string(),
                    });
                    continue;
                }
            };
            for url in urls {
                let (ok, detail) = match client.probe(&url) {
                    // A 2xx means the object is there. 403 on a signed-URL
                    // store is expected for an unauthenticated HEAD and is not
                    // evidence either way, so it is reported as-is rather than
                    // scored.
                    Ok(status) if (200..300).contains(&status) => (true, format!("HTTP {status}")),
                    Ok(status) => (false, format!("HTTP {status}")),
                    Err(error) => (false, error.to_string()),
                };
                any |= ok;
                results.push(ProbeResult {
                    package: package.clone(),
                    mirror: mirror.identifier(),
                    url,
                    ok,
                    detail,
                });
            }
        }
        if any {
            reachable_packages += 1;
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        for result in &results {
            println!(
                "{}  {:<28} {:<10} {}",
                if result.ok { "ok  " } else { "FAIL" },
                result.mirror,
                result.detail,
                result.package
            );
        }
        println!();
        println!(
            "{reachable_packages}/{checked_packages} package(s) have at least one working source"
        );
    }

    ensure!(
        checked_packages == 0 || reachable_packages == checked_packages,
        "{} package(s) have no working source; an outage would block this project",
        checked_packages - reachable_packages
    );
    Ok(())
}

fn bootstrap(cfg: &Config, url: Option<&str>) -> Result<()> {
    let client = MirrorClient::new(max_artifact_bytes())?;
    let urls: Vec<String> = match url {
        Some(explicit) => vec![format!(
            "{}{MIRROR_BOOTSTRAP_PATH}",
            explicit.trim_end_matches('/')
        )],
        None => {
            let canonical: Vec<MirrorDescriptorV1> =
                registry_mirror(&cfg.registry).into_iter().collect();
            merge_mirrors(&[
                &canonical,
                &cfg.mirrors,
                &default_public_mirrors(&cfg.registry),
            ])?
            .iter()
            .flat_map(MirrorDescriptorV1::bootstrap_urls)
            .collect()
        }
    };
    ensure!(!urls.is_empty(), "no host to ask for a mirror bootstrap");
    let (document, hit) = client.fetch_bootstrap(&urls)?;
    println!("# recovered from {}", hit.url);
    println!("# generated {}", document.generated_at);
    println!("registry = \"{}\"", document.registry_url);
    for mirror in &document.mirrors {
        let row = describe(mirror);
        println!(
            "{:>3}  {:<28} {:<16} {}",
            row.priority,
            row.id,
            row.serves.join(","),
            row.urls.join(" ")
        );
    }
    Ok(())
}

fn sync(cwd: &Path, cfg: &Config, output: &Path) -> Result<()> {
    let root = lock_root(cwd)?;
    let lock = read_lock(cwd)?.context("reading the project lockfile")?;
    let (ambient, _) = project_mirrors(cwd, cfg)?;
    let client = MirrorClient::new(max_artifact_bytes())?;

    // The `artifacts/<sha>.<ext>` layout is deliberately the same one the
    // production bucket and the `file://` registry use, so this directory is
    // simultaneously an offline registry, an air-gap bundle, and something a
    // bucket sync can push straight into.
    let artifacts = output.join("artifacts");
    fs::create_dir_all(&artifacts).with_context(|| format!("creating {}", artifacts.display()))?;

    let mut written = 0_usize;
    let mut present = 0_usize;
    for locked in &lock.packages {
        let destination =
            artifacts.join(format!("{}.{}", locked.sha256, locked.format.extension()));
        if destination.is_file() {
            present += 1;
            continue;
        }
        let mirrors = merge_mirrors(&[&locked.mirrors, &ambient])?;
        let coord = locked.mirror_coordinate();
        let staging = destination.with_extension("partial");
        client
            .fetch_artifact(&mirrors, &coord, locked.size, &staging)
            .with_context(|| format!("mirroring {}@{}", locked.full_name(), locked.version))?;
        // Rename only after the digest check inside `fetch_artifact` passed,
        // so a killed sync never leaves a file that looks complete.
        fs::rename(&staging, &destination)?;
        written += 1;
    }

    println!(
        "mirrored {written} artifact(s) ({present} already present) from {} to {}",
        root.join(LOCKFILE_FILE).display(),
        output.display()
    );
    println!();
    println!("use it with:");
    println!(
        "  zed install --frozen --registry file://{}",
        output.display()
    );
    Ok(())
}

fn enroll(cfg: &Config, store: &KeyStore, org: &str, key_id: &str) -> Result<()> {
    let stored = store.load(org, key_id)?;
    let token = cfg
        .resolve_token()?
        .context("enrolling a signing key needs an org `owner` token; run `zed login`")?;

    // Send the whole set, merging the new key into whatever is already
    // enrolled. A client that could only append could never express a
    // revocation, and one that replaced blindly would drop a co-owner's key.
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("zed-cli/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let url = format!("{}{}", cfg.registry, registry::org_keys_path(org));

    let mut keys: Vec<PublisherKeyV1> = client
        .get(&url)
        .send()
        .ok()
        .filter(|response| response.status().is_success())
        .and_then(|response| response.json::<OrgKeysResponse>().ok())
        .map(|response| response.keys)
        .unwrap_or_default();
    let public = stored.public();
    if let Some(existing) = keys.iter_mut().find(|key| key.key_id == public.key_id) {
        ensure!(
            existing.public_key_multibase == public.public_key_multibase,
            "key id `{key_id}` is already enrolled for `{org}` with different key material; \
             choose a new key id rather than replacing it — replacing invalidates every \
             signature already made under that name"
        );
    } else {
        keys.push(public);
    }

    let set = PublisherKeySetV1 {
        schema: zed_interfaces::signing::PUBLISHER_KEYS_SCHEMA_V1.to_owned(),
        org: org.to_owned(),
        keys: keys.clone(),
    };
    set.validate().map_err(|error| anyhow!(error))?;

    let response = client
        .put(&url)
        .bearer_auth(token)
        .json(&OrgKeysRequest { keys })
        .send()?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("enrolling `{org}/{key_id}` failed with HTTP {status}: {body}");
    }
    println!("enrolled `{org}/{key_id}`");
    println!();
    println!("Consumers pin this key the first time they resolve one of your packages.");
    println!("Declare it in {MANIFEST_FILE} too, so it travels with the source.");
    Ok(())
}

/// The mirror set a package publishes with: whatever the manifest resolves to.
pub fn publish_mirrors(
    manifest: &zed_interfaces::manifest::Manifest,
) -> Result<Vec<MirrorDescriptorV1>> {
    let mirrors = manifest.resolved_mirrors()?;
    // A `directory` mirror is a local path. Publishing one would put a route
    // in every consumer's lockfile that resolves to a different machine's
    // filesystem — at best useless, at worst pointing at something else.
    Ok(mirrors
        .into_iter()
        .filter(|mirror| mirror.kind != MirrorKindV1::Directory)
        .collect())
}
