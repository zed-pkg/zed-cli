use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use zed_interfaces::manifest::Manifest;
use zed_interfaces::paths::{MANIFEST_FILE, ZED_HOME_DIR_NAME};

use crate::cli::Globals;

#[derive(Debug, Clone)]
pub struct Config {
    pub registry: String,
    pub home: PathBuf,
    pub token: Option<String>,
    pub auth_url: String,
    pub supabase_url: Option<String>,
    pub supabase_key: Option<String>,
    pub interactive: bool,
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
        let registry = globals.registry.trim_end_matches('/').to_string();
        let auth_url = globals
            .auth_url
            .clone()
            .unwrap_or_else(|| format!("{registry}/shared-auth"))
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            registry,
            home,
            token: globals.token.clone(),
            auth_url,
            supabase_url: globals
                .supabase_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| value.trim_end_matches('/').to_string()),
            supabase_key: globals
                .supabase_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
            interactive: globals.interactive,
        })
    }

    /// Explicit token flag/env wins, followed by the refreshable human auth
    /// session and finally a legacy saved registry token.
    pub fn resolve_token(&self) -> Result<Option<String>> {
        if self.token.is_some() {
            return Ok(self.token.clone());
        }
        if let Some(token) = crate::auth::resolve_bearer(self)? {
            return Ok(Some(token));
        }
        Ok(Credentials::load(&self.home)
            .ok()
            .and_then(|c| c.token_for(&self.registry)))
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

#[derive(Debug)]
struct ManifestOverride {
    project: PathBuf,
    text: String,
}

#[derive(Debug)]
struct ResolvedRequirements {
    project: PathBuf,
    requirements: BTreeMap<String, String>,
}

thread_local! {
    static MANIFEST_OVERRIDE: RefCell<Option<ManifestOverride>> = const { RefCell::new(None) };
    static INSTALL_PREFETCH_CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
    static RESOLVED_REQUIREMENTS: RefCell<Option<ResolvedRequirements>> = const { RefCell::new(None) };
}

struct ManifestOverrideGuard;

impl Drop for ManifestOverrideGuard {
    fn drop(&mut self) {
        MANIFEST_OVERRIDE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

struct InstallPrefetchGuard;

impl Drop for InstallPrefetchGuard {
    fn drop(&mut self) {
        INSTALL_PREFETCH_CONFIG.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

struct ResolvedRequirementsGuard;

impl Drop for ResolvedRequirementsGuard {
    fn drop(&mut self) {
        RESOLVED_REQUIREMENTS.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

fn normalized_project(project: &Path) -> PathBuf {
    fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf())
}

/// Mark one facade operation so an in-memory manifest proposed by `zed add`
/// or `zed remove` is completely solved before the implementation-local
/// reinstall begins. The context is thread-local and panic-safe; ordinary
/// manifest overrides remain side-effect free.
pub(crate) fn with_install_prefetch<T>(
    cfg: &Config,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    INSTALL_PREFETCH_CONFIG.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(anyhow!(
                "an install prefetch context is already active on this thread"
            ));
        }
        *slot = Some(cfg.clone());
        Ok(())
    })?;
    let guard = InstallPrefetchGuard;
    let result = operation();
    drop(guard);
    result
}

/// Apply the solver's exact registry selections only to the root consumer
/// manifest. Package manifests read from the immutable store remain untouched.
/// This lets the existing transactional installer materialize one already
/// solved graph instead of independently making greedy version choices.
pub(crate) fn with_resolved_requirements<T>(
    project: &Path,
    requirements: BTreeMap<String, String>,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let project = normalized_project(project);
    RESOLVED_REQUIREMENTS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(anyhow!(
                "a resolved dependency graph is already active on this thread"
            ));
        }
        *slot = Some(ResolvedRequirements {
            project,
            requirements,
        });
        Ok(())
    })?;
    let guard = ResolvedRequirementsGuard;
    let result = operation();
    drop(guard);
    result
}

/// Run one installer operation with an in-memory root manifest. Package
/// manifests read from the store still come from disk; only the exact consumer
/// directory is overridden. The guard is thread-local and panic-safe, so a
/// manifestless install never creates or leaves a temporary `.zpkg.toml`.
pub(crate) fn with_manifest_override<T>(
    project: &Path,
    text: String,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let project = normalized_project(project);
    MANIFEST_OVERRIDE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            return Err(anyhow!(
                "a manifest override is already active on this thread"
            ));
        }
        *slot = Some(ManifestOverride {
            project: project.clone(),
            text,
        });
        Ok(())
    })?;
    let guard = ManifestOverrideGuard;
    let prefetch_cfg = INSTALL_PREFETCH_CONFIG.with(|slot| slot.borrow().clone());
    let result = match prefetch_cfg {
        Some(cfg) => {
            let prepared = crate::install_graph::prepare(&project, &cfg)?;
            with_resolved_requirements(&project, prepared.exact_requirements(), operation)
        }
        None => operation(),
    };
    drop(guard);
    result
}

fn apply_resolved_requirements(project: &Path, manifest: &mut Manifest) {
    RESOLVED_REQUIREMENTS.with(|slot| {
        let slot = slot.borrow();
        let Some(resolved) = slot.as_ref().filter(|resolved| resolved.project == project) else {
            return;
        };
        for (key, requirement) in &resolved.requirements {
            manifest
                .dependencies
                .insert(key.clone(), requirement.clone());
        }
    });
}

pub fn read_manifest(project: &Path) -> Result<Manifest> {
    let normalized = normalized_project(project);
    let override_text = MANIFEST_OVERRIDE.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|manifest| manifest.project == normalized)
            .map(|manifest| manifest.text.clone())
    });

    let mut manifest = if let Some(text) = override_text {
        Manifest::parse(&text)
            .with_context(|| format!("invalid in-memory manifest for {}", project.display()))?
    } else {
        let path = project.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("no {MANIFEST_FILE} found in {}", project.display()))?;
        Manifest::parse(&text).with_context(|| format!("invalid manifest {}", path.display()))?
    };
    apply_resolved_requirements(&normalized, &mut manifest);
    Ok(manifest)
}

pub fn write_manifest(project: &Path, manifest: &Manifest) -> Result<()> {
    let mut text = manifest.to_toml_string()?;
    if let Some(consumes_gitmodules) =
        crate::git_submodules::manifest_gitmodules_consumption(project)?
    {
        text = crate::git_submodules::set_manifest_consumes_gitmodules(&text, consumes_gitmodules)?;
    }
    fs::write(project.join(MANIFEST_FILE), text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASIC_MANIFEST: &str = r#"
[package]
org = "manifestless"
name = "consumer"
version = "0.0.0"

[package.repository]
vcs = "git"
url = "https://localhost/manifestless/consumer"

[dependencies]
"acme/http-kit" = "^1"
"#;

    #[test]
    fn manifest_override_is_scoped_and_never_written_to_disk() {
        let project = tempfile::tempdir().unwrap();

        with_manifest_override(project.path(), BASIC_MANIFEST.to_string(), || {
            let manifest = read_manifest(project.path())?;
            assert_eq!(
                manifest
                    .dependencies
                    .get("acme/http-kit")
                    .map(String::as_str),
                Some("^1")
            );
            assert!(!project.path().join(MANIFEST_FILE).exists());
            Ok(())
        })
        .unwrap();

        assert!(read_manifest(project.path()).is_err());
        assert!(!project.path().join(MANIFEST_FILE).exists());
    }

    #[test]
    fn manifest_rewrite_preserves_gitmodules_consumption_declaration() {
        let project = tempfile::tempdir().unwrap();
        fs::write(
            project.path().join(MANIFEST_FILE),
            format!("{BASIC_MANIFEST}\n[interop.git]\nconsume_gitmodules = true\n"),
        )
        .unwrap();
        let manifest = read_manifest(project.path()).unwrap();

        write_manifest(project.path(), &manifest).unwrap();

        assert_eq!(
            crate::git_submodules::manifest_gitmodules_consumption(project.path()).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn nested_manifest_overrides_fail_closed() {
        let project = tempfile::tempdir().unwrap();
        let error = with_manifest_override(project.path(), BASIC_MANIFEST.to_string(), || {
            with_manifest_override(project.path(), BASIC_MANIFEST.to_string(), || Ok(()))
        })
        .unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[test]
    fn solved_requirements_are_consumer_scoped_and_restored() {
        let project = tempfile::tempdir().unwrap();
        let dependency = tempfile::tempdir().unwrap();
        fs::write(project.path().join(MANIFEST_FILE), BASIC_MANIFEST).unwrap();
        fs::write(dependency.path().join(MANIFEST_FILE), BASIC_MANIFEST).unwrap();

        with_resolved_requirements(
            project.path(),
            BTreeMap::from([
                ("acme/http-kit".to_string(), "=1.5.0".to_string()),
                ("acme/transitive".to_string(), "=2.0.0".to_string()),
            ]),
            || {
                let root = read_manifest(project.path())?;
                assert_eq!(
                    root.dependencies.get("acme/http-kit").map(String::as_str),
                    Some("=1.5.0")
                );
                assert_eq!(
                    root.dependencies.get("acme/transitive").map(String::as_str),
                    Some("=2.0.0")
                );
                let package = read_manifest(dependency.path())?;
                assert_eq!(
                    package
                        .dependencies
                        .get("acme/http-kit")
                        .map(String::as_str),
                    Some("^1")
                );
                assert!(!package.dependencies.contains_key("acme/transitive"));
                Ok(())
            },
        )
        .unwrap();

        let restored = read_manifest(project.path()).unwrap();
        assert_eq!(
            restored
                .dependencies
                .get("acme/http-kit")
                .map(String::as_str),
            Some("^1")
        );
        assert!(!restored.dependencies.contains_key("acme/transitive"));
    }

    #[test]
    fn nested_solved_graphs_fail_closed() {
        let project = tempfile::tempdir().unwrap();
        let error = with_resolved_requirements(project.path(), BTreeMap::new(), || {
            with_resolved_requirements(project.path(), BTreeMap::new(), || Ok(()))
        })
        .unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[test]
    fn install_prefetch_context_is_scoped_and_nested_contexts_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let cfg = Config {
            registry: "file:///unused".to_string(),
            home: home.path().to_path_buf(),
            token: None,
            auth_url: "https://localhost/unused".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        };

        with_install_prefetch(&cfg, || {
            let active = INSTALL_PREFETCH_CONFIG.with(|slot| slot.borrow().is_some());
            assert!(active);
            let nested = with_install_prefetch(&cfg, || Ok(())).unwrap_err();
            assert!(nested.to_string().contains("already active"));
            Ok(())
        })
        .unwrap();

        let active = INSTALL_PREFETCH_CONFIG.with(|slot| slot.borrow().is_some());
        assert!(!active);
    }

    #[test]
    fn relative_home_is_resolved_from_the_invocation_directory() {
        let globals = Globals {
            registry: "https://reg.example.com/".to_string(),
            home: Some(PathBuf::from(".zed-home")),
            token: None,
            auth_url: None,
            supabase_url: None,
            supabase_key: None,
            interactive: false,
            git_submodules: false,
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
            auth_url: "https://reg.example.com/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        };
        assert_eq!(
            explicit.resolve_token().unwrap().as_deref(),
            Some("from-flag")
        );

        let fallback = Config {
            token: None,
            ..explicit.clone()
        };
        assert_eq!(
            fallback.resolve_token().unwrap().as_deref(),
            Some("from-file")
        );

        let unknown_registry = Config {
            registry: "https://elsewhere.example.com".to_string(),
            home: home.path().to_path_buf(),
            token: None,
            auth_url: "https://elsewhere.example.com/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        };
        assert_eq!(unknown_registry.resolve_token().unwrap(), None);
    }

    #[test]
    fn resolve_token_survives_a_corrupt_credentials_file() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("credentials.toml"), "not = [valid").unwrap();
        let cfg = Config {
            registry: "https://reg.example.com".to_string(),
            home: home.path().to_path_buf(),
            token: None,
            auth_url: "https://reg.example.com/shared-auth".to_string(),
            supabase_url: None,
            supabase_key: None,
            interactive: false,
        };
        // A corrupt file must degrade to "no saved token", not a panic/err.
        assert_eq!(cfg.resolve_token().unwrap(), None);
    }
}
