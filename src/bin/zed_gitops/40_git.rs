fn configured_submodules(root: &Path) -> Result<Vec<ConfiguredSubmodule>> {
    let path = root.join(".gitmodules");
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{} must be a regular file", path.display());
    }

    let output = git_output(
        root,
        &[
            "config",
            "--null",
            "--file",
            ".gitmodules",
            "--get-regexp",
            r"^submodule\..*\.(path|url)$",
        ],
    )?;
    if !output.status.success() {
        if output.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return git_failure(
            root,
            &[
                "config",
                "--null",
                "--file",
                ".gitmodules",
                "--get-regexp",
                r"^submodule\..*\.(path|url)$",
            ],
            output,
        );
    }
    submodules_from_config_stdout(&output.stdout)
}

fn submodules_from_config_stdout(stdout: &[u8]) -> Result<Vec<ConfiguredSubmodule>> {
    let mut builders = BTreeMap::<String, SubmoduleBuilder>::new();
    for raw in stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(raw).context(".gitmodules contains non-UTF-8 data")?;
        let (key, value) = record
            .split_once('\n')
            .or_else(|| record.split_once(' '))
            .with_context(|| format!("unrecognized git config record `{record}`"))?;
        let Some(rest) = key.strip_prefix("submodule.") else {
            continue;
        };
        let (name, field) = ["path", "url"]
            .into_iter()
            .find_map(|field| {
                rest.strip_suffix(&format!(".{field}"))
                    .map(|name| (name, field))
            })
            .with_context(|| format!("unsupported .gitmodules key `{key}`"))?;
        let builder = builders.entry(name.to_string()).or_default();
        match field {
            "path" => builder.path = Some(value.to_string()),
            "url" => builder.url = Some(value.to_string()),
            _ => unreachable!(),
        }
    }

    let mut modules = Vec::with_capacity(builders.len());
    let mut paths = BTreeSet::new();
    for (name, builder) in builders {
        let path = builder
            .path
            .with_context(|| format!("submodule `{name}` is missing path"))?;
        validate_relative_path(&path)?;
        if !paths.insert(path.clone()) {
            bail!("duplicate submodule path `{path}` in .gitmodules");
        }
        let url = builder
            .url
            .filter(|url| !url.trim().is_empty())
            .with_context(|| format!("submodule `{name}` is missing url"))?;
        modules.push(ConfiguredSubmodule { path, url });
    }
    modules.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(modules)
}

fn resolve_local_rev(root: &Path, rev: &str) -> Result<String> {
    if rev.is_empty() || rev.starts_with('-') || rev.contains('\0') || rev.contains('\n') {
        bail!("invalid --changed-from ref `{rev}`");
    }
    let spec = format!("{rev}^{{commit}}");
    let output = git_output(root, &["rev-parse", "--verify", "--quiet", &spec])?;
    if !output.status.success() {
        bail!(
            "--changed-from `{rev}` is not a local commit; fetch it first or omit --changed-from for a full offline tree validation"
        );
    }
    let sha = String::from_utf8(output.stdout)
        .context("--changed-from resolved to non-UTF-8 data")?
        .trim()
        .to_string();
    if !is_exact_sha1(&sha) {
        bail!("--changed-from `{rev}` did not resolve to a commit");
    }
    Ok(sha)
}

fn configured_submodules_at(root: &Path, rev: &str) -> Result<Vec<ConfiguredSubmodule>> {
    let blob = format!("{rev}:.gitmodules");
    let output = git_output(
        root,
        &[
            "config",
            "--null",
            "--blob",
            &blob,
            "--get-regexp",
            r"^submodule\..*\.(path|url)$",
        ],
    )?;
    if !output.status.success() {
        if output.status.code() == Some(1) || output.status.code() == Some(128) {
            return Ok(Vec::new());
        }
        return git_failure(
            root,
            &[
                "config",
                "--null",
                "--blob",
                &blob,
                "--get-regexp",
                r"^submodule\..*\.(path|url)$",
            ],
            output,
        );
    }
    submodules_from_config_stdout(&output.stdout)
}

fn tree_gitlinks(root: &Path, rev: &str) -> Result<BTreeMap<String, String>> {
    let output = checked_git(root, &["ls-tree", "-r", "-z", "--full-tree", rev])?;
    let mut gitlinks = BTreeMap::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let line = std::str::from_utf8(raw).context("git ls-tree output is not UTF-8")?;
        let Some((metadata, path)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        if mode != "160000" || kind != "commit" {
            continue;
        }
        if !is_exact_sha1(object) {
            bail!("gitlink `{path}` at `{rev}` has unsupported object identity `{object}`");
        }
        if gitlinks
            .insert(path.to_string(), object.to_string())
            .is_some()
        {
            bail!("duplicate gitlink path `{path}` at `{rev}`");
        }
    }
    Ok(gitlinks)
}

fn changed_gitlink_paths(
    root: &Path,
    rev: &str,
    current_modules: &BTreeMap<String, ConfiguredSubmodule>,
    current_gitlinks: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let sha = resolve_local_rev(root, rev)?;
    let previous_gitlinks = tree_gitlinks(root, &sha)?;
    let previous_modules = configured_submodules_at(root, &sha)?
        .into_iter()
        .map(|module| (module.path.clone(), module))
        .collect::<BTreeMap<_, _>>();
    let mut changed = BTreeSet::new();
    for (path, object) in current_gitlinks {
        match previous_gitlinks.get(path) {
            Some(previous) if previous == object => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }
    for path in previous_gitlinks.keys() {
        if !current_gitlinks.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    for (path, module) in current_modules {
        match previous_modules.get(path) {
            Some(previous)
                if normalize_repository_url(&previous.url)
                    == normalize_repository_url(&module.url) => {}
            _ => {
                changed.insert(path.clone());
            }
        }
    }
    for path in previous_modules.keys() {
        if !current_modules.contains_key(path) {
            changed.insert(path.clone());
        }
    }
    Ok(changed)
}

fn untracked_git_directories(
    root: &Path,
    prefixes: &[String],
    modules: &BTreeMap<String, ConfiguredSubmodule>,
    gitlinks: &BTreeMap<String, String>,
) -> Result<Vec<String>> {
    let mut found = BTreeSet::new();
    for prefix in prefixes {
        let normalized = prefix.trim_end_matches('/');
        if validate_relative_path(normalized).is_err() {
            continue;
        }
        let base = root.join(normalized);
        if !base.exists() {
            continue;
        }
        let walker = WalkDir::new(&base)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                if entry.file_name() == ".git" {
                    return false;
                }
                let Ok(relative) = entry.path().strip_prefix(root) else {
                    return false;
                };
                let relative = relative
                    .to_str()
                    .map(|value| value.replace('\\', "/"))
                    .unwrap_or_default();
                relative.is_empty()
                    || (!modules.contains_key(&relative) && !gitlinks.contains_key(&relative))
            });
        for entry in walker {
            let entry = entry.with_context(|| {
                format!("walking untracked Git directories under {}", base.display())
            })?;
            if !entry.file_type().is_dir() {
                continue;
            }
            let git_marker = entry.path().join(".git");
            if fs::symlink_metadata(&git_marker).is_err() {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                continue;
            };
            let relative = relative
                .to_str()
                .map(|value| value.replace('\\', "/"))
                .unwrap_or_default();
            if !relative.is_empty() {
                found.insert(relative);
            }
        }
    }
    Ok(found.into_iter().collect())
}

fn indexed_gitlinks(root: &Path) -> Result<BTreeMap<String, String>> {
    let output = checked_git(root, &["ls-files", "--stage"])?;
    let text = String::from_utf8(output.stdout).context("Git index output is not UTF-8")?;
    let mut gitlinks = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Some((metadata, path)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = metadata.split_whitespace();
        let mode = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        let stage = fields.next().unwrap_or_default();
        if mode != "160000" || stage != "0" {
            continue;
        }
        if !is_exact_sha1(object) {
            bail!("gitlink `{path}` has unsupported object identity `{object}`");
        }
        if gitlinks.insert(path.to_string(), object.to_string()).is_some() {
            bail!("duplicate indexed gitlink path `{path}`");
        }
    }
    Ok(gitlinks)
}

fn normalize_repository_url(value: &str) -> String {
    let mut normalized = value.trim().replace('\\', "/").to_ascii_lowercase();
    if let Some(rest) = normalized.strip_prefix("git+") {
        normalized = rest.to_string();
    }
    for prefix in [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
    ] {
        if let Some(rest) = normalized.strip_prefix(prefix) {
            normalized = format!("github.com/{rest}");
            break;
        }
    }
    while normalized.ends_with('/') {
        normalized.pop();
    }
    if normalized.ends_with(".git") {
        normalized.truncate(normalized.len() - 4);
    }
    normalized
}

fn is_explicit_github_repository(value: &str) -> bool {
    let normalized = normalize_repository_url(value);
    let Some(identity) = normalized.strip_prefix("github.com/") else {
        return false;
    };
    let mut parts = identity.split('/');
    matches!(parts.next(), Some(part) if !part.is_empty())
        && matches!(parts.next(), Some(part) if !part.is_empty())
        && parts.next().is_none()
}

fn is_regular_file_within_root(root: &Path, relative: &str) -> bool {
    if validate_relative_path(relative).is_err() {
        return false;
    }
    let candidate = root.join(relative);
    let Ok(metadata) = fs::symlink_metadata(&candidate) else {
        return false;
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    fs::canonicalize(candidate).is_ok_and(|resolved| resolved.starts_with(root))
}

fn validate_relative_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.trim() != value
        || value.contains('\\')
        || value.contains('\0')
        || value.split('/').any(|part| part.is_empty())
    {
        bail!("unsafe repository-relative path `{value}`");
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
        || value
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".git"))
    {
        bail!("unsafe repository-relative path `{value}`");
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(|value| value.replace('\\', "/"))
        .context("paths must be UTF-8")
}

fn is_exact_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_dns_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .and_then(|path| path.to_str())
        .map(|path| path.replace('\\', "/"))
        .unwrap_or_else(|| path.display().to_string())
}

fn git_output(root: &Path, args: &[&str]) -> Result<Output> {
    ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("running git in {}", root.display()))
}

fn checked_git(root: &Path, args: &[&str]) -> Result<Output> {
    let output = git_output(root, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        git_failure(root, args, output)
    }
}

fn git_failure<T>(root: &Path, args: &[&str], output: Output) -> Result<T> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    bail!(
        "git -C {} {} failed with {}{}",
        root.display(),
        args.join(" "),
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

