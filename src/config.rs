use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{MANIFEST_FILE, ZED_HOME_DIR_NAME};

use crate::cli::Globals;

#[derive(Debug, Clone)]
pub struct Config {
    pub registry: String,
    pub home: PathBuf,
    pub token: Option<String>,
}

impl Config {
    pub fn from_globals(globals: &Globals) -> Result<Self> {
        let home = match &globals.home {
            Some(h) => h.clone(),
            None => dirs::home_dir()
                .context("could not determine home directory; set ZED_PKG_HOME")?
                .join(ZED_HOME_DIR_NAME),
        };
        Ok(Self {
            registry: globals.registry.trim_end_matches('/').to_string(),
            home,
            token: globals.token.clone(),
        })
    }

    /// Explicit token flag/env wins; otherwise saved credentials.
    pub fn resolve_token(&self) -> Option<String> {
        if self.token.is_some() {
            return self.token.clone();
        }
        Credentials::load(&self.home)
            .ok()
            .and_then(|c| c.token_for(&self.registry))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub registries: BTreeMap<String, RegistryCredentials>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RegistryCredentials {
    pub token: String,
}

impl Credentials {
    fn path(home: &Path) -> PathBuf {
        home.join("credentials.toml")
    }

    pub fn load(home: &Path) -> Result<Self> {
        let path = Self::path(home);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        Ok(toml::from_str(&text)?)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        fs::create_dir_all(home)?;
        let path = Self::path(home);
        let text = toml::to_string_pretty(self)?;
        // Create with 0600 from the first byte — a write-then-chmod leaves a
        // window where the token file is readable at the default umask.
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            // An existing file keeps its old mode; enforce 0600 either way.
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
        }
        #[cfg(not(unix))]
        fs::write(&path, text)?;
        Ok(())
    }

    pub fn token_for(&self, registry: &str) -> Option<String> {
        self.registries
            .get(registry.trim_end_matches('/'))
            .map(|r| r.token.clone())
    }

    pub fn set_token(&mut self, registry: &str, token: String) {
        self.registries.insert(
            registry.trim_end_matches('/').to_string(),
            RegistryCredentials { token },
        );
    }
}

pub fn read_manifest(project: &Path) -> Result<Manifest> {
    let path = project.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no {MANIFEST_FILE} found in {}", project.display()))?;
    Manifest::parse(&text).with_context(|| format!("invalid manifest {}", path.display()))
}

pub fn write_manifest(project: &Path, manifest: &Manifest) -> Result<()> {
    let text = manifest.to_toml_string()?;
    fs::write(project.join(MANIFEST_FILE), text)?;
    Ok(())
}
