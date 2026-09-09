//! Process bridge for the Cargo-style `zed install --git` surface.
//!
//! The primary `zed` process owns the public `.cli-flags.toml` argv boundary.
//! Once flags-2-env has audited and resolved that contract and clap has projected
//! typed values, this bridge invokes the separately shipped `zed-git-install`
//! executable using environment variables. The helper has its own embedded
//! flags-2-env contract and performs source, manifest, target CLI contract,
//! build, atomic activation, and shared receipt validation.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInstallRequest {
    pub git: String,
    pub revision: String,
    pub binary: String,
    pub force: bool,
    pub global_bin_dir: Option<PathBuf>,
    pub home: PathBuf,
}

pub fn install(request: &GitInstallRequest) -> Result<()> {
    let helper = sibling_helper()?;
    let mut command = Command::new(&helper);
    command
        .env("ZED_PKG_GIT_INSTALL_URL", &request.git)
        .env("ZED_PKG_GIT_INSTALL_REV", &request.revision)
        .env("ZED_PKG_GIT_INSTALL_BIN", &request.binary)
        .env(
            "ZED_PKG_GIT_INSTALL_FORCE",
            if request.force { "true" } else { "false" },
        )
        .env("ZED_PKG_HOME", &request.home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(directory) = &request.global_bin_dir {
        command.env("ZED_PKG_GLOBAL_BIN_DIR", directory);
    }

    let status = command
        .status()
        .with_context(|| format!("starting Git CLI installer {}", helper.display()))?;
    ensure!(
        status.success(),
        "Git CLI installer {} exited with {status}",
        helper.display()
    );
    Ok(())
}

fn sibling_helper() -> Result<PathBuf> {
    let current = std::env::current_exe().context("resolving current zed executable")?;
    let directory = current
        .parent()
        .context("current zed executable has no parent directory")?;
    let helper = directory.join(platform_helper_name());
    ensure!(
        is_regular_file(&helper),
        "Cargo-style Git install requires `{}` beside the zed executable; reinstall zed with its complete binary set",
        helper.display()
    );
    Ok(helper)
}

#[cfg(windows)]
fn platform_helper_name() -> &'static str {
    "zed-git-install.exe"
}

#[cfg(not(windows))]
fn platform_helper_name() -> &'static str {
    "zed-git-install"
}

fn is_regular_file(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|metadata| metadata.file_type().is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::platform_helper_name;

    #[test]
    fn helper_name_is_platform_qualified() {
        #[cfg(windows)]
        assert_eq!(platform_helper_name(), "zed-git-install.exe");
        #[cfg(not(windows))]
        assert_eq!(platform_helper_name(), "zed-git-install");
    }
}
