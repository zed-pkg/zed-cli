from pathlib import Path


path = Path("src/ops.rs")
text = path.read_text(encoding="utf-8")


def replace_once(old: str, new: str, label: str) -> None:
    global text
    if new in text:
        return
    count = text.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected one match, found {count}")
    text = text.replace(old, new, 1)


workspace_helper = r'''fn collect_workspace_links_for_frozen(
    project: &Path,
    manifest: &Manifest,
    workspace: Option<&WorkspaceInfo>,
) -> Result<BTreeMap<String, PathBuf>> {
    let Some(workspace) = workspace else {
        return Ok(BTreeMap::new());
    };

    let mut links = BTreeMap::new();
    let mut pending: VecDeque<(String, String)> = manifest
        .dependencies
        .iter()
        .map(|(key, requirement)| (key.clone(), requirement.clone()))
        .collect();

    while let Some((raw_key, requirement_text)) = pending.pop_front() {
        let (org, name) = split_key(&raw_key)?;
        let key = format!("{org}/{name}");
        let Some(member_dir) = workspace.members.get(&key) else {
            continue;
        };
        let member_manifest = read_manifest(member_dir).with_context(|| {
            format!(
                "reading workspace member `{key}` from {}",
                member_dir.display()
            )
        })?;
        let requirement = Requirement::parse(&requirement_text);
        if !requirement.matches(&member_manifest.package.version) {
            bail!(
                "workspace member {key}@{} does not satisfy `{requirement_text}`",
                member_manifest.package.version
            );
        }
        if member_dir == project || links.contains_key(&key) {
            continue;
        }
        links.insert(key, member_dir.clone());
        pending.extend(member_manifest.dependencies);
    }

    Ok(links)
}

'''
replace_once(
    "// ---------------------------------------------------------------------------\n// install\n",
    workspace_helper + "// ---------------------------------------------------------------------------\n// install\n",
    "insert frozen workspace resolver",
)

replace_once(
    r'''        validate_frozen_manifest_requirements(
            &manifest,
            &lock,
            workspace.as_ref(),
            validate_manifest_requirements,
        )?;
        for locked in &lock.packages {
''',
    r'''        validate_frozen_manifest_requirements(
            &manifest,
            &lock,
            workspace.as_ref(),
            validate_manifest_requirements,
        )?;
        workspace_links =
            collect_workspace_links_for_frozen(project, &manifest, workspace.as_ref())?;
        for locked in &lock.packages {
''',
    "restore workspace members during frozen installs",
)

replace_once(
    r'''            if let Some(ws) = &workspace
                && let Some(member_dir) = ws.members.get(&key)
            {
                if member_dir != project && !workspace_links.contains_key(&key) {
                    workspace_links.insert(key.clone(), member_dir.clone());
                    if let Ok(member_manifest) = read_manifest(member_dir) {
                        for (sub_key, sub_req) in member_manifest.dependencies {
                            let (sub_org, sub_name) = split_key(&sub_key)?;
                            queue.push_back((sub_org, sub_name, sub_req));
                        }
                    }
                }
                continue;
            }
''',
    r'''            if let Some(ws) = &workspace
                && let Some(member_dir) = ws.members.get(&key)
            {
                let member_manifest = read_manifest(member_dir).with_context(|| {
                    format!(
                        "reading workspace member `{key}` from {}",
                        member_dir.display()
                    )
                })?;
                let requirement = Requirement::parse(&req_str);
                if !requirement.matches(&member_manifest.package.version) {
                    bail!(
                        "workspace member {key}@{} does not satisfy `{req_str}`",
                        member_manifest.package.version
                    );
                }
                if member_dir != project && !workspace_links.contains_key(&key) {
                    workspace_links.insert(key.clone(), member_dir.clone());
                    for (sub_key, sub_req) in member_manifest.dependencies {
                        let (sub_org, sub_name) = split_key(&sub_key)?;
                        queue.push_back((sub_org, sub_name, sub_req));
                    }
                }
                continue;
            }
''',
    "validate mutable workspace members",
)

replace_once(
    r'''    for (key, member_dir) in &workspace_links {
        interactive::confirm(cfg.interactive, &format!("link workspace package {key}"))?;
        let (org, name) = split_key(key)?;
        let dest = modules.join(&org).join(&name);
        // Workspace dependencies obey the same ownership decision as registry
        // packages. Copy mode must remain self-contained after the member source
        // directory leaves the Docker/OCI build context.
        link_or_copy(member_dir, &dest, mode)?;
        if let Ok(member_manifest) = read_manifest(member_dir) {
            for (bin_name, rel_target) in &member_manifest.bin {
                bins.insert(bin_name.clone(), dest.join(rel_target));
            }
        }
        installed.push((key.clone(), "workspace".to_string()));
    }
''',
    r'''    for (key, member_dir) in &workspace_links {
        interactive::confirm(cfg.interactive, &format!("link workspace package {key}"))?;
        let (org, name) = split_key(key)?;
        let member_manifest = read_manifest(member_dir).with_context(|| {
            format!(
                "reading workspace member `{key}` from {}",
                member_dir.display()
            )
        })?;
        if !allow_ecosystem_mismatch
            && let Some(problem) = ecosystem_mismatch(
                key,
                &name,
                member_manifest.package.language,
                member_manifest.package.ecosystem(),
                &project_ecos,
            )
        {
            bail!("{problem}");
        }

        let dest = modules.join(&org).join(&name);
        // Workspace dependencies obey the same ownership decision as registry
        // packages. Copy mode must remain self-contained after the member source
        // directory leaves the Docker/OCI build context.
        link_or_copy(member_dir, &dest, mode)?;
        for (bin_name, rel_target) in &member_manifest.bin {
            bins.insert(bin_name.clone(), dest.join(rel_target));
        }

        let dependency_adapter = if use_dependency_adapters {
            member_manifest
                .install
                .adapter
                .as_deref()
                .map(named_adapter)
                .transpose()?
                .unwrap_or(adapter)
        } else {
            adapter
        };
        match dependency_adapter {
            Adapter::Node => {
                used_node_adapter = true;
                let node_dest = project
                    .join("node_modules")
                    .join(format!("@{org}"))
                    .join(&name);
                transaction.backup(&node_dest)?;
                link_or_copy(member_dir, &node_dest, mode)?;
            }
            Adapter::Java => {
                used_java_adapter = true;
                for entry in walkdir::WalkDir::new(&dest)
                    .follow_links(true)
                    .into_iter()
                    .filter_map(|entry| entry.ok())
                {
                    if entry.path().extension().is_some_and(|extension| extension == "jar") {
                        jars.push(entry.path().to_string_lossy().to_string());
                    }
                }
            }
            Adapter::Go => wired_roots
                .entry(Adapter::Go)
                .or_default()
                .push(dest.clone()),
            Adapter::Python => wired_roots
                .entry(Adapter::Python)
                .or_default()
                .push(dest.clone()),
            Adapter::Rust => wired_roots
                .entry(Adapter::Rust)
                .or_default()
                .push(dest.clone()),
            Adapter::Dart => wired_roots
                .entry(Adapter::Dart)
                .or_default()
                .push(dest.clone()),
            Adapter::Auto | Adapter::None => {}
        }
        wired_packages.push(WiredPackage {
            key: key.clone(),
            version: "workspace".to_string(),
            language: member_manifest.package.language,
            ecosystem: member_manifest.package.ecosystem(),
            path: dest,
        });
        installed.push((key.clone(), "workspace".to_string()));
    }
''',
    "wire workspace packages through native adapters",
)

replace_once(
    r'''    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
    r'''    #[test]
    fn frozen_workspace_resolution_expands_transitive_members_and_validates_versions() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let app = root.join("apps/cli");
        let utils = root.join("packages/utils");
        let core = root.join("packages/core");
        for directory in [&app, &utils, &core] {
            fs::create_dir_all(directory).unwrap();
        }

        fs::write(
            root.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "workspace-root"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/workspace-root"

[workspace]
members = ["packages/*", "apps/*"]
"#,
        )
        .unwrap();
        fs::write(
            core.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-core"
version = "1.2.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-core"
"#,
        )
        .unwrap();
        fs::write(
            utils.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-utils"
version = "1.1.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-utils"

[dependencies]
"zedtest/ws-core" = "^1"
"#,
        )
        .unwrap();
        fs::write(
            app.join(MANIFEST_FILE),
            r#"[package]
org = "zedtest"
name = "ws-cli"
version = "1.0.0"

[package.repository]
vcs = "git"
url = "https://example.invalid/ws-cli"

[dependencies]
"zedtest/ws-utils" = "^1"
"#,
        )
        .unwrap();

        let manifest = read_manifest(&app).unwrap();
        let workspace = find_workspace(&app).unwrap();
        let links =
            collect_workspace_links_for_frozen(&app, &manifest, Some(&workspace)).unwrap();
        assert_eq!(
            links.keys().cloned().collect::<Vec<_>>(),
            vec!["zedtest/ws-core".to_string(), "zedtest/ws-utils".to_string()]
        );
        assert_eq!(links["zedtest/ws-core"], core);
        assert_eq!(links["zedtest/ws-utils"], utils);

        let incompatible = manifest
            .to_toml_string()
            .unwrap()
            .replace("\"^1\"", "\"^2\"");
        fs::write(app.join(MANIFEST_FILE), incompatible).unwrap();
        let manifest = read_manifest(&app).unwrap();
        let error = collect_workspace_links_for_frozen(&app, &manifest, Some(&workspace))
            .unwrap_err()
            .to_string();
        assert!(error.contains("ws-utils@1.1.0"), "{error}");
        assert!(error.contains("does not satisfy `^2`"), "{error}");
    }

    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
    "add frozen workspace graph tests",
)

path.write_text(text, encoding="utf-8")
