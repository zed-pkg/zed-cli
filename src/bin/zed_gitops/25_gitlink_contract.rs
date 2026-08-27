fn resolve_schema_path(root: &Path, requested: Option<&Path>) -> Result<Option<PathBuf>> {
    let relative = path_text(requested.unwrap_or_else(|| Path::new(DEFAULT_SCHEMA)))?;
    validate_relative_path(&relative)
        .with_context(|| format!("invalid --schema path `{relative}`"))?;
    let candidate = root.join(&relative);
    match fs::symlink_metadata(&candidate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if requested.is_some() {
                bail!("gitlink contract {} does not exist", candidate.display());
            }
            Ok(None)
        }
        Err(error) => Err(error).with_context(|| {
            format!("inspecting gitlink contract {}", candidate.display())
        }),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "gitlink contract {} must be a regular file inside the superproject",
                    candidate.display()
                );
            }
            let canonical = fs::canonicalize(&candidate).with_context(|| {
                format!("canonicalizing gitlink contract {}", candidate.display())
            })?;
            if !canonical.starts_with(root) {
                bail!(
                    "gitlink contract {} escapes the superproject root {}",
                    canonical.display(),
                    root.display()
                );
            }
            Ok(Some(canonical))
        }
    }
}

fn load_gitlink_contract(
    root: &Path,
    schema_file: &Path,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<GitlinkContract>> {
    let relative = relative_display(root, schema_file);
    let text = fs::read_to_string(schema_file)
        .with_context(|| format!("reading gitlink contract {}", schema_file.display()))?;
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "schema.invalid-json",
                format!("cannot parse gitlink contract JSON: {error}"),
                &relative,
                "",
            ));
            return Ok(None);
        }
    };
    if strict {
        validate_gitlink_contract_known_fields(&value, &relative, diagnostics);
    }
    let contract: GitlinkContract = match serde_json::from_value(value) {
        Ok(contract) => contract,
        Err(error) => {
            diagnostics.push(Diagnostic::error(
                "schema.shape",
                format!("gitlink contract does not match v1alpha1: {error}"),
                &relative,
                "",
            ));
            return Ok(None);
        }
    };

    if contract.api_version != GITLINK_CONTRACT_API_VERSION {
        diagnostics.push(Diagnostic::error(
            "schema.header",
            format!("apiVersion must equal {GITLINK_CONTRACT_API_VERSION:?}"),
            &relative,
            "",
        ));
    }
    if contract.kind != GITLINK_CONTRACT_KIND {
        diagnostics.push(Diagnostic::error(
            "schema.header",
            format!("kind must equal {GITLINK_CONTRACT_KIND:?}"),
            &relative,
            "",
        ));
    }
    if contract.spec.approved_app_path_prefixes.is_empty() {
        diagnostics.push(Diagnostic::error(
            "schema.approved-paths",
            "spec.approvedAppPathPrefixes must contain at least one repository-relative prefix",
            &relative,
            "",
        ));
    }
    for prefix in &contract.spec.approved_app_path_prefixes {
        let normalized = prefix.trim_end_matches('/');
        if validate_relative_path(normalized).is_err() {
            diagnostics.push(Diagnostic::error(
                "schema.approved-paths",
                format!("approved app path prefix `{prefix}` is not a safe repository-relative path"),
                &relative,
                "",
            ));
        }
    }
    for allowed in &contract.spec.allowed_gitlinks {
        if validate_relative_path(&allowed.path).is_err() {
            diagnostics.push(Diagnostic::error(
                "schema.allowed-gitlink",
                format!("allowed gitlink path `{}` is not a safe repository-relative path", allowed.path),
                &relative,
                "",
            ));
        } else if !contract.spec.approved_app_path_prefixes.is_empty()
            && !path_under_any_prefix(&allowed.path, &contract.spec.approved_app_path_prefixes)
        {
            diagnostics.push(Diagnostic::error(
                "schema.allowed-path-prefix",
                format!(
                    "allowed gitlink `{}` is not under spec.approvedAppPathPrefixes",
                    allowed.path
                ),
                &relative,
                "",
            ));
        }
    }
    Ok(Some(contract))
}

fn validate_gitlink_contract(
    contract: &GitlinkContract,
    modules: &BTreeMap<String, ConfiguredSubmodule>,
    gitlinks: &BTreeMap<String, String>,
    untracked: &[String],
    diagnostics: &mut Vec<Diagnostic>,
) {
    if contract.spec.approved_app_path_prefixes.is_empty() {
        return;
    }
    let prefixes = &contract.spec.approved_app_path_prefixes;
    let allowed: BTreeMap<&str, Option<&str>> = contract
        .spec
        .allowed_gitlinks
        .iter()
        .map(|item| (item.path.as_str(), item.repository.as_deref()))
        .collect();
    let allow_list_active = !allowed.is_empty();
    let mut app_paths = BTreeSet::new();
    app_paths.extend(
        modules
            .keys()
            .filter(|path| path_under_any_prefix(path, prefixes))
            .map(String::as_str),
    );
    app_paths.extend(
        gitlinks
            .keys()
            .filter(|path| path_under_any_prefix(path, prefixes))
            .map(String::as_str),
    );

    for path in app_paths {
        if path_has_forbidden_suffix(path, &contract.spec.forbidden_path_suffixes) {
            diagnostics.push(Diagnostic::error(
                "gitlink.forbidden-suffix",
                format!("app gitlink `{path}` matches a forbidden path suffix"),
                path,
                "",
            ));
        }
        if allow_list_active && !allowed.contains_key(path) {
            diagnostics.push(Diagnostic::error(
                "gitlink.unexpected",
                format!("gitlink `{path}` is not listed in the gitlink contract"),
                path,
                "",
            ));
        }
        if modules.contains_key(path) && !gitlinks.contains_key(path) {
            diagnostics.push(Diagnostic::error(
                "gitlink.uninitialized",
                format!("`.gitmodules` path `{path}` is not an indexed mode-160000 gitlink"),
                path,
                "",
            ));
        }
        if gitlinks.contains_key(path) && !modules.contains_key(path) {
            diagnostics.push(Diagnostic::error(
                "gitlink.missing-gitmodules",
                format!("indexed gitlink `{path}` has no `.gitmodules` entry"),
                path,
                "",
            ));
        }
        if let (Some(module), Some(Some(expected))) = (modules.get(path), allowed.get(path))
            && normalize_repository_url(expected) != normalize_repository_url(&module.url)
        {
            diagnostics.push(Diagnostic::error(
                "gitlink.repository-drift",
                format!("`.gitmodules` URL for `{path}` does not match the gitlink contract"),
                path,
                "",
            ));
        }
    }

    if allow_list_active {
        for allowed_path in allowed.keys() {
            if !modules.contains_key(*allowed_path) && !gitlinks.contains_key(*allowed_path) {
                diagnostics.push(Diagnostic::error(
                    "gitlink.missing",
                    format!("contract gitlink `{allowed_path}` is absent from `.gitmodules` and the index"),
                    *allowed_path,
                    "",
                ));
            }
        }
    }

    for path in untracked {
        diagnostics.push(Diagnostic::error(
            "gitlink.untracked",
            format!("untracked Git directory `{path}` impersonates an app submodule"),
            path,
            "",
        ));
    }
}

fn path_under_any_prefix(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        let prefix = prefix.trim_end_matches('/');
        path == prefix || path.starts_with(&format!("{prefix}/"))
    })
}

fn path_has_forbidden_suffix(path: &str, suffixes: &[String]) -> bool {
    let segment = path.rsplit('/').next().unwrap_or(path);
    suffixes.iter().any(|suffix| {
        let suffix = suffix.trim();
        !suffix.is_empty() && segment.ends_with(suffix)
    })
}
