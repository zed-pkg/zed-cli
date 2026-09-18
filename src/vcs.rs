use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use zed_interfaces::vcs::Vcs;

fn run(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("HGPLAIN", "1")
        .env("PAGER", "cat")
        .output()
        .with_context(|| format!("failed to run `{program}` (is it installed?)"))?;
    if !output.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// jj and Sapling repos are used colocated with git, so git commands answer
/// for them; fossil and pijul are not yet verifiable (use
/// `zed publish --skip-vcs-checks` until support lands).
pub fn ensure_clean(vcs: Vcs, dir: &Path) -> Result<()> {
    let dirty = match vcs {
        v if v.uses_git_tags() => !run(dir, "git", &["status", "--porcelain"])?.is_empty(),
        Vcs::Hg => !run(dir, "hg", &["status", "-q"])?.is_empty(),
        Vcs::Fossil => !run(dir, "fossil", &["changes"])?.is_empty(),
        Vcs::Pijul => bail!("pijul worktree checks are not supported yet; use --skip-vcs-checks"),
        other => bail!("worktree checks for {other} are not supported yet; use --skip-vcs-checks"),
    };
    if dirty {
        bail!("worktree has uncommitted changes (use --allow-dirty to override)");
    }
    Ok(())
}

fn hg_tag_commit(dir: &Path, tag: &str) -> Result<Option<String>> {
    if tag.is_empty() || tag.chars().any(char::is_control) {
        bail!("Mercurial tag must be non-empty and contain no control characters");
    }
    let tags = run(dir, "hg", &["tags", "-T", "{tag}\t{node}\n"])?;
    Ok(tags.lines().find_map(|line| {
        let (name, node) = line.split_once('\t')?;
        (name == tag && !node.is_empty()).then(|| node.to_string())
    }))
}

/// Commit id the tag points at, if the tag exists.
pub fn tag_commit(vcs: Vcs, dir: &Path, tag: &str) -> Result<Option<String>> {
    match vcs {
        v if v.uses_git_tags() => {
            let spec = format!("refs/tags/{tag}^{{commit}}");
            match run(dir, "git", &["rev-parse", "-q", "--verify", &spec]) {
                Ok(commit) if !commit.is_empty() => Ok(Some(commit)),
                _ => Ok(None),
            }
        }
        Vcs::Hg => hg_tag_commit(dir, tag),
        Vcs::Fossil | Vcs::Pijul => {
            bail!("tag verification for {vcs} is not supported yet; use --skip-vcs-checks")
        }
        other => bail!("tag verification for {other} is not supported yet; use --skip-vcs-checks"),
    }
}

pub fn head_commit(vcs: Vcs, dir: &Path) -> Result<String> {
    match vcs {
        v if v.uses_git_tags() => run(dir, "git", &["rev-parse", "HEAD"]),
        Vcs::Hg => run(dir, "hg", &["log", "-r", ".", "-T", "{node}"]),
        Vcs::Fossil | Vcs::Pijul => {
            bail!("head lookup for {vcs} is not supported yet; use --skip-vcs-checks")
        }
        other => bail!("head lookup for {other} is not supported yet; use --skip-vcs-checks"),
    }
}

/// Full provenance gate for `zed publish`: clean tree, tag exists, tag
/// points at HEAD. Returns the commit the tag points at.
pub fn verify_publish_provenance(
    vcs: Vcs,
    dir: &Path,
    tag: &str,
    allow_dirty: bool,
) -> Result<String> {
    if !allow_dirty {
        ensure_clean(vcs, dir)?;
    }
    let Some(tag_commit) = tag_commit(vcs, dir, tag)? else {
        bail!(
            "required {vcs} tag `{tag}` not found; create it first (authors must \
             tag the backing repo before publishing)"
        );
    };
    let head = head_commit(vcs, dir)?;
    if tag_commit != head {
        bail!(
            "tag `{tag}` points at {tag_commit}, but HEAD is {head}; \
             publish from the tagged commit"
        );
    }
    Ok(tag_commit)
}
 

#[cfg(test)]
mod tests {
    use super::hg_tag_commit;

    #[test]
    fn mercurial_tag_lookup_rejects_control_characters_before_spawning() {
        let error = hg_tag_commit(Path::new("."), "bad\ntag");
        assert!(error.is_err());
    }

    use std::path::Path;
}
