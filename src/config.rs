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
        let configured_home = match &globals.home {
            Some(h) => h.clone(),
            None => dirs::home_dir()
                .context("could not determine home directory; set ZED_PKG_HOME")?
                .join(ZED_HOME_DIR_NAME),
        };
        // Store paths become symlink targets during installation. Keeping a
        // relative --home here would make that target relative to the nested
        // zed_modules/<org>/ (or node_modules/@<org>/) link directory rather
        // than to the project where the user invoked zed.
        let home = if configured_home.is_absolute() {
            configured_home
        } else {
            std::env::current_dir()
                .context("could not determine current directory")?
                .join(configured_home)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_home_is_resolved_from_the_invocation_directory() {
        let globals = Globals {
            registry: "https://reg.example.com/".to_string(),
            home: Some(PathBuf::from(".zed-home")),
            token: None,
        };
        let cfg = Config::from_globals(&globals).unwrap();

        assert!(cfg.home.is_absolute());
        assert_eq!(cfg.home, std::env::current_dir().unwrap().join(".zed-home"));
        assert_eq!(cfg.registry, "https://reg.example.com");
    }

    #[test]
    fn credentials_roundtrip_normalizes_registry_slashes() {
        let home = tempfile::tempdir().unwrap();
        let mut creds = Credentials::default();
        // Stored with a trailing slash, looked up without — and vice versa —
        // must hit the same entry.
        creds.set_token("https://reg.example.com/", "tok-a".to_string());
        creds.save(home.path()).unwrap();

        let loaded = Credentials::load(home.path()).unwrap();
        assert_eq!(
            loaded.token_for("https://reg.example.com").as_deref(),
            Some("tok-a")
        );
        assert_eq!(
            loaded.token_for("https://reg.example.com/").as_deref(),
            Some("tok-a")
        );
        assert_eq!(loaded.token_for("https://other.example.com"), None);
    }

    #[test]
    fn credentials_load_without_file_is_empty_not_an_error() {
        let home = tempfile::tempdir().unwrap();
        let creds = Credentials::load(home.path()).unwrap();
        assert!(creds.registries.is_empty());
    }

    #[test]
    fn credentials_load_rejects_malformed_toml() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("credentials.toml"), "not = [valid").unwrap();
        assert!(Credentials::load(home.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credentials_file_is_0600_even_over_a_lax_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("credentials.toml");

        let mut creds = Credentials::default();
        creds.set_token("https://reg.example.com", "s3cret".to_string());
        creds.save(home.path()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "fresh credentials file must be 0600");

        // An existing file keeps its inode (and would keep its old mode);
        // save() must clamp it back down rather than trust it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        creds.save(home.path()).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "re-saving must re-enforce 0600");
    }

    #[test]
    fn resolve_token_prefers_explicit_over_saved_credentials() {
        let home = tempfile::tempdir().unwrap();
        let mut creds = Credentials::default();
        creds.set_token("https://reg.example.com", "from-file".to_string());
        creds.save(home.path()).unwrap();

        let explicit = Config {
            registry: "https://reg.example.com".to_string(),
            home: home.path().to_path_buf(),
            token: Some("from-flag".to_string()),
        };
        assert_eq!(explicit.resolve_token().as_deref(), Some("from-flag"));

        let fallback = Config {
            token: None,
            ..explicit.clone()
        };
        assert_eq!(fallback.resolve_token().as_deref(), Some("from-file"));

        let unknown_registry = Config {
            registry: "https://elsewhere.example.com".to_string(),
            home: home.path().to_path_buf(),
            token: None,
        };
        assert_eq!(unknown_registry.resolve_token(), None);
    }

    #[test]
    fn resolve_token_survives_a_corrupt_credentials_file() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("credentials.toml"), "not = [valid").unwrap();
        let cfg = Config {
            registry: "https://reg.example.com".to_string(),
            home: home.path().to_path_buf(),
            token: None,
        };
        // A corrupt file must degrade to "no saved token", not a panic/err.
        assert_eq!(cfg.resolve_token(), None);
    }
}
