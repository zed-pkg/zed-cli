from pathlib import Path


path = Path("src/ops.rs")
text = path.read_text(encoding="utf-8")
start = text.index("pub fn uninstall(project: &Path, cfg: &Config, specs: &[String]) -> Result<()> {")
end = text.index("\n// ---------------------------------------------------------------------------\n// build hooks", start)

replacement = r'''pub fn uninstall(project: &Path, cfg: &Config, specs: &[String]) -> Result<()> {
    let lock_path = project.join(LOCKFILE_FILE);
    let text = fs::read_to_string(&lock_path)
        .with_context(|| format!("zed uninstall requires {LOCKFILE_FILE}"))?;
    let lock = Lockfile::parse(&text).with_context(|| format!("invalid {LOCKFILE_FILE}"))?;

    // Workspace packages deliberately do not appear in the artifact lock: the
    // lock records immutable registry hashes, while workspace members are live
    // source projections. Reconstruct the active workspace graph from the same
    // manifest boundary used by frozen install so a workspace-only project can
    // still uninstall and later restore its exact materialized graph.
    let manifest = read_manifest(project).ok();
    let workspace = find_workspace(project);
    let workspace_links = match manifest.as_ref() {
        Some(manifest) => {
            collect_workspace_links_for_frozen(project, manifest, workspace.as_ref())?
        }
        None => BTreeMap::new(),
    };
    let total_materialized = lock.packages.len() + workspace_links.len();
    if total_materialized == 0 {
        println!("nothing to uninstall");
        return Ok(());
    }

    let mut targets = BTreeSet::new();
    if specs.is_empty() {
        targets.extend(lock.packages.iter().map(LockedPackage::full_name));
        targets.extend(workspace_links.keys().cloned());
    } else {
        for spec in specs {
            if spec.contains('@') {
                bail!("uninstall accepts package identities without versions (expected org/name)");
            }
            let (org, name) = split_key(spec)?;
            let key = format!("{org}/{name}");
            if workspace_links.contains_key(&key) {
                bail!(
                    "selective uninstall of workspace package `{key}` is not supported; run `zed uninstall` without package arguments to remove the complete materialized graph while retaining {LOCKFILE_FILE}"
                );
            }
            if lock.find(&org, &name).is_none() {
                bail!("{key} is neither pinned by {LOCKFILE_FILE} nor an active workspace package");
            }
            targets.insert(key);
        }
    }

    interactive::confirm(
        cfg.interactive,
        &format!(
            "uninstall {} package(s) from {} while retaining {LOCKFILE_FILE}",
            targets.len(),
            project.display()
        ),
    )?;

    let store = Store::new(&cfg.home);
    let _install_lock = store.install_lock()?;
    let modules_dir = manifest
        .as_ref()
        .map(|manifest| manifest.modules_dir().to_string())
        .unwrap_or_else(|| MODULES_DIR.to_string());
    let modules = project.join(&modules_dir);
    let had_node_adapter = project.join(".zed").join("node_path").is_file();
    let had_java_adapter = project.join(".zed").join("classpath").is_file();
    let uninstall_all = targets.len() == total_materialized;

    let mut transaction = ProjectTransaction::begin(project)?;
    eprintln!(
        "transaction {}: staging uninstall rollback data",
        transaction.id()
    );
    if uninstall_all {
        transaction.backup(&modules)?;
        // These files are all generated projections of the materialized graph.
        // Remove them transactionally on a full uninstall so no toolchain sees
        // stale package paths while the retained lock waits for frozen restore.
        for generated in [
            "paths.json",
            "node_path",
            "classpath",
            "go.work",
            "pythonpath",
            "cargo-paths.toml",
            "pub-deps.yaml",
        ] {
            transaction.backup(&project.join(".zed").join(generated))?;
        }
    } else {
        transaction.backup(&modules.join(BIN_DIR))?;
        if had_java_adapter {
            transaction.backup(&project.join(".zed").join("classpath"))?;
        }
    }

    for key in &targets {
        interactive::confirm(cfg.interactive, &format!("unmaterialize {key}"))?;
        let (org, name) = split_key(key)?;
        if !uninstall_all {
            transaction.backup(&modules.join(&org).join(&name))?;
        }
        if had_node_adapter {
            transaction.backup(
                &project
                    .join("node_modules")
                    .join(format!("@{org}"))
                    .join(&name),
            )?;
        }
    }

    let remaining: Vec<&LockedPackage> = lock
        .packages
        .iter()
        .filter(|package| !targets.contains(&package.full_name()))
        .collect();
    let remaining_workspace = workspace_links
        .keys()
        .filter(|key| !targets.contains(*key))
        .count();
    if !uninstall_all {
        let mut bins: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut mode = InstallMode::Copy;
        let mut jars = Vec::new();
        for package in &remaining {
            let installed = modules.join(&package.org).join(&package.name);
            if fs::symlink_metadata(&installed)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                mode = InstallMode::Symlink;
            }
            if let Ok(package_manifest) = read_manifest(&installed) {
                for (name, target) in package_manifest.bin {
                    bins.insert(name, installed.join(target));
                }
            }
            if had_java_adapter && installed.exists() {
                for entry in walkdir::WalkDir::new(&installed)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jar")
                    {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        // Selective uninstall currently targets registry packages only. Keep
        // workspace bins and Java entries in the rebuilt aggregate projections.
        for key in workspace_links.keys().filter(|key| !targets.contains(*key)) {
            let (org, name) = split_key(key)?;
            let installed = modules.join(&org).join(&name);
            if fs::symlink_metadata(&installed)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                mode = InstallMode::Symlink;
            }
            if let Ok(package_manifest) = read_manifest(&installed) {
                for (bin_name, target) in package_manifest.bin {
                    bins.insert(bin_name, installed.join(target));
                }
            }
            if had_java_adapter && installed.exists() {
                for entry in walkdir::WalkDir::new(&installed)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "jar")
                    {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
        }
        hoist_bins(&modules, &bins, mode)?;
        if had_java_adapter && !jars.is_empty() {
            jars.sort();
            jars.dedup();
            let classpath = project.join(".zed").join("classpath");
            fs::create_dir_all(classpath.parent().context("classpath parent")?)?;
            fs::write(classpath, jars.join(":") + "\n")?;
        }
    }

    let remaining_total = remaining.len() + remaining_workspace;
    interactive::confirm(
        cfg.interactive,
        &format!(
            "record {remaining_total} remaining installed package(s) and commit transaction {}",
            transaction.id()
        ),
    )?;
    store.record_project(
        project,
        remaining
            .iter()
            .map(|package| package.sha256.clone())
            .collect(),
    )?;
    transaction.commit()?;

    for key in &targets {
        println!("uninstalled {key}");
    }
    println!(
        "{remaining_total} package(s) remain materialized; {LOCKFILE_FILE} retained for frozen reinstall"
    );
    Ok(())
}
'''

text = text[:start] + replacement + text[end:]
path.write_text(text, encoding="utf-8")
