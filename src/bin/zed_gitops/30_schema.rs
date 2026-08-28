fn validate_known_fields(
    value: &Value,
    path: &str,
    app: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    check_object_keys(
        value,
        &["$schema", "apiVersion", "kind", "metadata", "spec"],
        "record",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    check_object_keys(
        value.get("metadata").unwrap_or(&Value::Null),
        &["name"],
        "metadata",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    let spec = value.get("spec").unwrap_or(&Value::Null);
    check_object_keys(
        spec,
        &["owner", "inventory", "source", "argo", "migration"],
        "spec",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    check_object_keys(
        spec.get("inventory").unwrap_or(&Value::Null),
        &["mode", "path", "repository", "revision"],
        "spec.inventory",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    check_object_keys(
        spec.get("source").unwrap_or(&Value::Null),
        &["mode", "repository", "targetRevision", "path", "renderer"],
        "spec.source",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    check_object_keys(
        spec.get("argo").unwrap_or(&Value::Null),
        &[
            "project",
            "namespace",
            "destinationServer",
            "automated",
            "prune",
            "selfHeal",
        ],
        "spec.argo",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
    check_object_keys(
        spec.get("migration").unwrap_or(&Value::Null),
        &["phase", "staticApplication"],
        "spec.migration",
        path,
        app,
        "catalog.unknown-field",
        diagnostics,
    );
}

fn check_object_keys(
    value: &Value,
    allowed: &[&str],
    identity: &str,
    path: &str,
    app: &str,
    rule_id: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    let allowed = allowed.iter().copied().collect::<BTreeSet<_>>();
    for key in object
        .keys()
        .filter(|key| !allowed.iter().any(|allowed| *allowed == key.as_str()))
    {
        diagnostics.push(Diagnostic::error(
            rule_id,
            format!("{identity} contains unsupported field {key:?}"),
            path,
            app,
        ));
    }
}

fn validate_gitlink_contract_known_fields(
    value: &Value,
    path: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    check_object_keys(
        value,
        &["$schema", "apiVersion", "kind", "spec"],
        "gitlink contract",
        path,
        "",
        "schema.unknown-field",
        diagnostics,
    );
    let spec = value.get("spec").unwrap_or(&Value::Null);
    check_object_keys(
        spec,
        &[
            "approvedAppPathPrefixes",
            "forbiddenPathSuffixes",
            "allowedGitlinks",
        ],
        "gitlink contract spec",
        path,
        "",
        "schema.unknown-field",
        diagnostics,
    );
    if let Some(allowed) = spec.get("allowedGitlinks").and_then(Value::as_array) {
        for (index, item) in allowed.iter().enumerate() {
            check_object_keys(
                item,
                &["path", "repository"],
                &format!("gitlink contract spec.allowedGitlinks[{index}]"),
                path,
                "",
                "schema.unknown-field",
                diagnostics,
            );
        }
    }
}

