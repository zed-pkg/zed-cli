#[allow(clippy::too_many_arguments)]
fn validate_record(
    root: &Path,
    record_path: &Path,
    relative: &str,
    record: &Record,
    modules: &BTreeMap<String, ConfiguredSubmodule>,
    gitlinks: &BTreeMap<String, String>,
    seen_names: &mut BTreeMap<String, String>,
    seen_inventory_paths: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let app = &record.metadata.name;

    for (field, actual, expected) in [
        ("$schema", record.schema.as_str(), SCHEMA_REFERENCE),
        ("apiVersion", record.api_version.as_str(), API_VERSION),
        ("kind", record.kind.as_str(), KIND),
    ] {
        if actual != expected {
            diagnostics.push(Diagnostic::error(
                "catalog.header",
                format!("{field} must equal {expected:?}"),
                relative,
                app,
            ));
        }
    }

    if !is_dns_label(app) {
        diagnostics.push(Diagnostic::error(
            "catalog.application-name",
            "metadata.name must be a non-empty DNS label",
            relative,
            app,
        ));
    }
    let file_stem = record_path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if file_stem != app {
        diagnostics.push(Diagnostic::error(
            "catalog.filename",
            format!("file stem must equal metadata.name ({app})"),
            relative,
            app,
        ));
    }
    if let Some(previous) = seen_names.insert(app.clone(), relative.to_string()) {
        diagnostics.push(Diagnostic::error(
            "catalog.duplicate-application",
            format!("application is already declared in {previous}"),
            relative,
            app,
        ));
    }

    if record.spec.owner.trim().is_empty()
        || record.spec.owner.trim() != record.spec.owner
        || record.spec.owner.contains('/')
        || record.spec.owner.contains('\\')
    {
        diagnostics.push(Diagnostic::error(
            "catalog.owner",
            "spec.owner must be one non-empty repository-owner slug",
            relative,
            app,
        ));
    }

    let inventory = &record.spec.inventory;
    if inventory.mode != "git-submodule" {
        diagnostics.push(Diagnostic::error(
            "inventory.mode",
            "spec.inventory.mode must equal 'git-submodule'",
            relative,
            app,
        ));
    }
    if validate_relative_path(&inventory.path).is_err()
        || !inventory.path.starts_with("remote/deployments/")
    {
        diagnostics.push(Diagnostic::error(
            "inventory.path",
            "inventory path must be a safe path under remote/deployments/",
            relative,
            app,
        ));
    }
    if let Some(previous) =
        seen_inventory_paths.insert(inventory.path.clone(), app.clone())
    {
        diagnostics.push(Diagnostic::error(
            "inventory.duplicate-path",
            format!("inventory path is already owned by {previous}"),
            relative,
            app,
        ));
    }
    if !is_explicit_github_repository(&inventory.repository) {
        diagnostics.push(Diagnostic::error(
            "inventory.repository",
            "inventory repository must identify exactly one GitHub owner/repository",
            relative,
            app,
        ));
    }
    if !is_exact_sha1(&inventory.revision) {
        diagnostics.push(Diagnostic::error(
            "inventory.revision",
            "inventory revision must be an exact lowercase 40-hex commit",
            relative,
            app,
        ));
    }

    match modules.get(&inventory.path) {
        None => diagnostics.push(Diagnostic::error(
            "inventory.gitmodules-entry",
            format!("{:?} is not declared in .gitmodules", inventory.path),
            relative,
            app,
        )),
        Some(module)
            if normalize_repository_url(&module.url)
                != normalize_repository_url(&inventory.repository) =>
        {
            diagnostics.push(Diagnostic::error(
                "inventory.repository-drift",
                ".gitmodules URL and catalog inventory repository differ",
                relative,
                app,
            ));
        }
        Some(_) => {}
    }

    match gitlinks.get(&inventory.path) {
        None => diagnostics.push(Diagnostic::error(
            "inventory.gitlink",
            format!("{:?} is not an indexed gitlink", inventory.path),
            relative,
            app,
        )),
        Some(revision) if revision != &inventory.revision => {
            diagnostics.push(Diagnostic::error(
                "inventory.gitlink-drift",
                format!(
                    "catalog revision {} does not match gitlink {}",
                    inventory.revision, revision
                ),
                relative,
                app,
            ));
        }
        Some(_) => {}
    }

    let source = &record.spec.source;
    if source.mode != "direct-repository" {
        diagnostics.push(Diagnostic::error(
            "source.mode",
            "spec.source.mode must equal 'direct-repository'",
            relative,
            app,
        ));
    }
    let normalized_source = normalize_repository_url(&source.repository);
    let normalized_inventory = normalize_repository_url(&inventory.repository);
    if normalized_source == CLUSTER_REPOSITORY {
        diagnostics.push(Diagnostic::error(
            "source.cluster-repository",
            "Argo CD must render the upstream app repository, not a path inside k8s-cluster",
            relative,
            app,
        ));
    }
    if normalized_source != normalized_inventory {
        diagnostics.push(Diagnostic::error(
            "source.repository-drift",
            "Argo source repository must equal the submodule upstream repository",
            relative,
            app,
        ));
    }
    if !is_exact_sha1(&source.target_revision) {
        diagnostics.push(Diagnostic::error(
            "source.target-revision",
            "source.targetRevision must be an exact lowercase 40-hex commit",
            relative,
            app,
        ));
    } else if source.target_revision != inventory.revision {
        diagnostics.push(Diagnostic::error(
            "source.pin-drift",
            "source.targetRevision must equal the inventory gitlink revision",
            relative,
            app,
        ));
    }
    if validate_relative_path(&source.path).is_err() {
        diagnostics.push(Diagnostic::error(
            "source.path",
            "source.path must be a safe non-empty repository-relative path",
            relative,
            app,
        ));
    }
    if !matches!(
        source.renderer.as_str(),
        "kustomize" | "helm" | "jsonnet" | "plain-yaml"
    ) {
        diagnostics.push(Diagnostic::error(
            "source.renderer",
            "source.renderer must be kustomize, helm, jsonnet, or plain-yaml",
            relative,
            app,
        ));
    }

    let repository_slug = normalized_inventory
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let inventory_slug = inventory
        .path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if app.ends_with("-infra")
        || inventory_slug.ends_with("-infra")
        || repository_slug.ends_with("-infra")
    {
        diagnostics.push(Diagnostic::error(
            "policy.infra-is-not-app",
            "*-infra repositories cannot be classified as deployable app submodules",
            relative,
            app,
        ));
    }

    let argo = &record.spec.argo;
    if argo.project.trim().is_empty() || argo.project == "default" {
        diagnostics.push(Diagnostic::error(
            "argo.project",
            "Argo project must be explicit and cannot be 'default'",
            relative,
            app,
        ));
    }
    if argo.namespace.trim().is_empty() || argo.namespace == "default" {
        diagnostics.push(Diagnostic::error(
            "argo.namespace",
            "destination namespace must be explicit and cannot be 'default'",
            relative,
            app,
        ));
    }
    if argo.destination_server.trim().is_empty() {
        diagnostics.push(Diagnostic::error(
            "argo.destination",
            "destinationServer must be a non-empty string",
            relative,
            app,
        ));
    }

    let migration = &record.spec.migration;
    if !matches!(
        migration.phase.as_str(),
        "pilot-inert" | "migration-ready" | "active" | "retired"
    ) {
        diagnostics.push(Diagnostic::error(
            "migration.phase",
            "migration.phase must be pilot-inert, migration-ready, active, or retired",
            relative,
            app,
        ));
    }
    if migration.phase == "pilot-inert"
        && (argo.automated || argo.prune || argo.self_heal)
    {
        diagnostics.push(Diagnostic::error(
            "migration.inert-sync",
            "pilot-inert records must disable automated sync, prune, and self-heal",
            relative,
            app,
        ));
    }
    if validate_relative_path(&migration.static_application).is_err() {
        diagnostics.push(Diagnostic::error(
            "migration.static-application",
            "staticApplication must be a safe repository-relative path",
            relative,
            app,
        ));
    } else if !is_regular_file_within_root(root, &migration.static_application) {
        diagnostics.push(Diagnostic::error(
            "migration.static-application-missing",
            format!(
                "static Application path must be a regular file within the superproject: {}",
                migration.static_application
            ),
            relative,
            app,
        ));
    }
}

