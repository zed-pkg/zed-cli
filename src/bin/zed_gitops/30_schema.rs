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
        diagnostics,
    );
    check_object_keys(
        value.get("metadata").unwrap_or(&Value::Null),
        &["name"],
        "metadata",
        path,
        app,
        diagnostics,
    );
    let spec = value.get("spec").unwrap_or(&Value::Null);
    check_object_keys(
        spec,
        &["owner", "inventory", "source", "argo", "migration"],
        "spec",
        path,
        app,
        diagnostics,
    );
    check_object_keys(
        spec.get("inventory").unwrap_or(&Value::Null),
        &["mode", "path", "repository", "revision"],
        "spec.inventory",
        path,
        app,
        diagnostics,
    );
    check_object_keys(
        spec.get("source").unwrap_or(&Value::Null),
        &["mode", "repository", "targetRevision", "path", "renderer"],
        "spec.source",
        path,
        app,
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
        diagnostics,
    );
    check_object_keys(
        spec.get("migration").unwrap_or(&Value::Null),
        &["phase", "staticApplication"],
        "spec.migration",
        path,
        app,
        diagnostics,
    );
}

fn check_object_keys(
    value: &Value,
    allowed: &[&str],
    identity: &str,
    path: &str,
    app: &str,
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
            "catalog.unknown-field",
            format!("{identity} contains unsupported field {key:?}"),
            path,
            app,
        ));
    }
}

