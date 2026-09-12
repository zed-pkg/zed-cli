//! Mutually exclusive providers declared by the bytes of each selected artifact.
//! Runs over the complete install graph before native managers, hooks or wiring.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io::Read, path::Path};

pub const PROVIDER_FILE: &str = "zed-provider.toml";
const MAX_DESCRIPTOR_BYTES: u64 = 4096;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Provider {
    schema: u32,
    group: String,
    provider: String,
}

fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.as_bytes()[value.len() - 1].is_ascii_alphanumeric()
}

fn read_provider(root: &Path, key: &str) -> Result<Option<Provider>> {
    let path = root.join(PROVIDER_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot inspect provider declaration"),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("{key}: {PROVIDER_FILE} must be a regular file");
    }
    if metadata.len() > MAX_DESCRIPTOR_BYTES {
        bail!("{key}: provider declaration is too large");
    }
    let mut text = String::new();
    fs::File::open(path)?
        .take(MAX_DESCRIPTOR_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_DESCRIPTOR_BYTES {
        bail!("{key}: provider declaration is too large");
    }
    // Do not include TOML's source excerpt in diagnostics.
    let provider: Provider =
        toml::from_str(&text).map_err(|_| anyhow::anyhow!("{key}: invalid {PROVIDER_FILE}"))?;
    let components: Vec<_> = provider.group.split('/').collect();
    if provider.schema != 1
        || components.len() != 2
        || !components.iter().all(|part| slug(part))
        || !slug(&provider.provider)
    {
        bail!("{key}: invalid provider schema, group or provider name");
    }
    Ok(Some(provider))
}

/// Multiple packages may implement the SAME provider (e.g. TS plus its WASM
/// bridge). Two different providers in one group are a graph admission error.
/// Unmarked packages retain their existing behavior.
pub fn validate<'a>(sources: impl IntoIterator<Item = (&'a str, &'a Path)>) -> Result<()> {
    let mut selected: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (key, source) in sources {
        let Some(declaration) = read_provider(source, key)? else {
            continue;
        };
        if let Some((provider, owner)) = selected.get(&declaration.group) {
            if provider != &declaration.provider {
                bail!(
                    "exclusive provider conflict in {}: {} selects {}, but {} selects {}",
                    declaration.group,
                    owner,
                    provider,
                    key,
                    declaration.provider
                );
            }
        } else {
            selected.insert(declaration.group, (declaration.provider, key.to_owned()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn package(group: &str, provider: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(PROVIDER_FILE),
            format!("schema = 1\ngroup = {group:?}\nprovider = {provider:?}\n"),
        )
        .unwrap();
        dir
    }
    #[test]
    fn one_provider_and_its_bridge_are_compatible() {
        let ts = package("ores-dnd/browser", "pragmatic");
        let wasm = package("ores-dnd/browser", "pragmatic");
        validate([("ts", ts.path()), ("wasm", wasm.path())]).unwrap();
    }
    #[test]
    fn conflicting_transitive_provider_is_rejected_in_either_order() {
        let a = package("ores-dnd/browser", "pragmatic");
        let b = package("ores-dnd/browser", "sortable");
        for sources in [
            [("a", a.path()), ("b", b.path())],
            [("b", b.path()), ("a", a.path())],
        ] {
            assert!(
                validate(sources)
                    .unwrap_err()
                    .to_string()
                    .contains("exclusive provider conflict")
            );
        }
    }
    #[test]
    fn separate_runtimes_are_independent() {
        let a = package("ores-dnd/browser", "pragmatic");
        let b = package("ores-dnd/flutter", "sdk");
        validate([("a", a.path()), ("b", b.path())]).unwrap();
    }
    #[test]
    fn malformed_unknown_or_oversized_declarations_fail_without_echo() {
        let dir = tempfile::tempdir().unwrap();
        for text in [
            "schema = 2\ngroup = 'ores-dnd/browser'\nprovider = 'a'".to_owned(),
            "schema = 1\ngroup = 'ores-dnd/browser'\nprovider = 'a'\nunknown = 'private-drag-data'"
                .to_owned(),
            "schema = 1\ngroup = '../browser'\nprovider = 'a'".to_owned(),
            "private-drag-data".repeat(500),
        ] {
            fs::write(dir.path().join(PROVIDER_FILE), text).unwrap();
            let error = validate([("a", dir.path())]).unwrap_err().to_string();
            assert!(!error.contains("private-drag-data"));
        }
    }
    #[test]
    fn missing_declaration_preserves_existing_packages() {
        let dir = tempfile::tempdir().unwrap();
        validate([("unmarked", dir.path())]).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlink_declaration_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let external = package("ores-dnd/browser", "pragmatic");
        std::os::unix::fs::symlink(
            external.path().join(PROVIDER_FILE),
            dir.path().join(PROVIDER_FILE),
        )
        .unwrap();
        assert!(validate([("a", dir.path())]).is_err());
    }
}
