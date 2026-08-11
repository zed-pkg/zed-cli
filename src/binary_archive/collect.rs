fn collect_explicit_include(
    project: &Path,
    include: &Path,
    entrypoints: &BTreeSet<String>,
    files: &mut BTreeMap<String, CollectedFile>,
    portable_paths: &mut BTreeMap<String, String>,
) -> Result<()> {
    let source = resolve_project_input(project, include)?;
    let include_relative = source
        .strip_prefix(project)
        .context("resolved include escaped project root")?;
    if source.is_file() {
        let relative = portable_relative(include_relative)?;
        insert_source_file(
            files,
            portable_paths,
            &relative,
            source,
            entrypoints.contains(&relative) || file_is_executable(&project.join(&relative))?,
        )?;
        return Ok(());
    }
    ensure!(source.is_dir(), "include {} does not exist", include.display());
    for entry in WalkDir::new(&source).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "explicit include contains symlink {}; binary artifacts do not allow links",
            entry.path().display()
        );
        if metadata.is_dir() {
            continue;
        }
        ensure!(
            metadata.is_file(),
            "explicit include contains non-regular file {}",
            entry.path().display()
        );
        let relative = portable_relative(entry.path().strip_prefix(project)?)?;
        insert_source_file(
            files,
            portable_paths,
            &relative,
            entry.path().to_path_buf(),
            entrypoints.contains(&relative) || file_is_executable(entry.path())?,
        )?;
    }
    Ok(())
}

fn collect_root_legal_files(
    project: &Path,
    entrypoints: &BTreeSet<String>,
    files: &mut BTreeMap<String, CollectedFile>,
    portable_paths: &mut BTreeMap<String, String>,
) -> Result<()> {
    for entry in fs::read_dir(project)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let upper = name.to_ascii_uppercase();
        if !["LICENSE", "LICENCE", "COPYING", "NOTICE"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            continue;
        }
        insert_source_file(
            files,
            portable_paths,
            &name,
            entry.path(),
            entrypoints.contains(&name),
        )?;
    }
    Ok(())
}

fn resolve_project_input(project: &Path, input: &Path) -> Result<PathBuf> {
    ensure!(
        !input.is_absolute(),
        "binary payload paths must be project-relative: {}",
        input.display()
    );
    let lexical = portable_relative(input)?;
    validate_safe_relative_path("binary payload path", &lexical)
        .map_err(|error| anyhow::anyhow!(error))?;
    let mut cursor = project.to_path_buf();
    for component in Path::new(&lexical).components() {
        let Component::Normal(component) = component else {
            bail!("unsafe binary payload path `{lexical}`");
        };
        cursor.push(component);
        let metadata = fs::symlink_metadata(&cursor)
            .with_context(|| format!("reading payload path {}", cursor.display()))?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "binary payload path {} traverses a symlink",
            cursor.display()
        );
    }
    let canonical = cursor
        .canonicalize()
        .with_context(|| format!("resolving payload path {}", cursor.display()))?;
    ensure!(
        canonical.starts_with(project),
        "binary payload {} escapes project root {}",
        canonical.display(),
        project.display()
    );
    Ok(canonical)
}

fn portable_relative(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .with_context(|| format!("path {} is not UTF-8", path.display()))?;
                parts.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path {} is not a safe project-relative path", path.display())
            }
        }
    }
    ensure!(!parts.is_empty(), "path must identify a file or directory");
    Ok(parts.join("/"))
}
