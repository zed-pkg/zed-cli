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

    let mut builders = BTreeMap::<String, SubmoduleBuilder>::new();
    for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() {
            continue;
        }
        let record =
            std::str::from_utf8(raw).context(".gitmodules contains non-UTF-8 data")?;
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

