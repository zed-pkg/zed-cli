fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        TopLevelCommand::Validate(args) => run_validate(args),
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run_validate(args: ValidateArgs) -> Result<i32> {
    if !args.offline {
        bail!("online validation is not implemented; pass --offline");
    }
    let report = validate_gitops(&args)?;
    match args.format {
        OutputFormat::Human => print_human(&report),
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
        OutputFormat::Sarif => print_sarif(&report)?,
    }
    Ok(if report.valid { 0 } else { 2 })
}

fn validate_gitops(args: &ValidateArgs) -> Result<Report> {
    let root = fs::canonicalize(&args.root).with_context(|| {
        format!(
            "canonicalizing GitOps superproject root {}",
            args.root.display()
        )
    })?;

    if let Some(rev) = args.changed_from.as_deref() {
        resolve_local_rev(&root, rev)?;
    }

    let schema_file = resolve_schema_path(&root, args.schema.as_deref())?;
    let catalog_present = catalog_directory_exists(&root, &args.catalog)?;
    if schema_file.is_none() && !catalog_present {
        bail!(
            "GitOps validation requires `{DEFAULT_SCHEMA}` or `{}` under {}",
            path_text(&args.catalog)?,
            root.display()
        );
    }

    let modules = configured_submodules(&root)?
        .into_iter()
        .map(|module| (module.path.clone(), module))
        .collect::<BTreeMap<_, _>>();
    let gitlinks = indexed_gitlinks(&root)?;
    let mut diagnostics = Vec::new();
    let mut schema_rel = None;

    if let Some(schema_file) = schema_file.as_ref() {
        schema_rel = Some(relative_display(&root, schema_file));
        if let Some(contract) =
            load_gitlink_contract(&root, schema_file, args.strict, &mut diagnostics)?
        {
            let untracked = untracked_git_directories(
                &root,
                &contract.spec.approved_app_path_prefixes,
                &modules,
                &gitlinks,
            )?;
            validate_gitlink_contract(&contract, &modules, &gitlinks, &untracked, &mut diagnostics);
        }
    }

    let mut records = 0usize;
    if catalog_present {
        let catalog_report = validate_catalog(&args.root, &args.catalog, args.strict, true)?;
        records = catalog_report.records;
        diagnostics.extend(catalog_report.diagnostics);
    }

    let changed_gitlinks = if let Some(rev) = args.changed_from.as_deref() {
        changed_gitlink_paths(&root, rev, &modules, &gitlinks)?
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    diagnostics.sort_by(|left, right| {
        (
            &left.severity,
            &left.path,
            &left.application,
            &left.rule_id,
            &left.message,
        )
            .cmp(&(
                &right.severity,
                &right.path,
                &right.application,
                &right.rule_id,
                &right.message,
            ))
    });
    let errors = diagnostics
        .iter()
        .filter(|item| item.severity == "error")
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|item| item.severity == "warning")
        .count();
    Ok(Report {
        valid: errors == 0,
        records,
        gitlinks: gitlinks.len(),
        errors,
        warnings,
        offline: true,
        schema: schema_rel,
        changed_from: args.changed_from.clone(),
        changed_gitlinks,
        diagnostics,
    })
}

fn catalog_directory_exists(root: &Path, requested_catalog: &Path) -> Result<bool> {
    let catalog_text = path_text(requested_catalog)?;
    validate_relative_path(&catalog_text)
        .with_context(|| format!("invalid --catalog path `{catalog_text}`"))?;
    let catalog = root.join(&catalog_text);
    match fs::symlink_metadata(&catalog) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!("inspecting catalog directory {}", catalog.display())
        }),
        Ok(metadata) => Ok(!metadata.file_type().is_symlink() && metadata.is_dir()),
    }
}

fn validate_catalog(
    requested_root: &Path,
    requested_catalog: &Path,
    strict: bool,
    offline: bool,
) -> Result<Report> {
    let root = fs::canonicalize(requested_root).with_context(|| {
        format!(
            "canonicalizing GitOps superproject root {}",
            requested_root.display()
        )
    })?;
    let catalog_text = path_text(requested_catalog)?;
    validate_relative_path(&catalog_text)
        .with_context(|| format!("invalid --catalog path `{catalog_text}`"))?;
    let catalog = root.join(&catalog_text);
    let catalog_metadata = fs::symlink_metadata(&catalog)
        .with_context(|| format!("inspecting catalog directory {}", catalog.display()))?;
    if catalog_metadata.file_type().is_symlink() || !catalog_metadata.is_dir() {
        bail!(
            "catalog {} must be a real directory inside the superproject",
            catalog.display()
        );
    }
    let catalog = fs::canonicalize(&catalog)
        .with_context(|| format!("canonicalizing catalog directory {}", catalog.display()))?;
    if !catalog.starts_with(&root) {
        bail!(
            "catalog {} escapes the superproject root {}",
            catalog.display(),
            root.display()
        );
    }

    let modules = configured_submodules(&root)?
        .into_iter()
        .map(|module| (module.path.clone(), module))
        .collect::<BTreeMap<_, _>>();
    let gitlinks = indexed_gitlinks(&root)?;

    let mut entries = fs::read_dir(&catalog)
        .with_context(|| format!("reading catalog directory {}", catalog.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing catalog directory {}", catalog.display()))?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut diagnostics = Vec::new();
    let mut records = 0usize;
    let mut seen_names = BTreeMap::<String, String>::new();
    let mut seen_inventory_paths = BTreeMap::<String, String>::new();

    for entry in entries {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        records += 1;
        let relative = relative_display(&root, &path);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspecting catalog record {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            diagnostics.push(Diagnostic::error(
                "catalog.regular-file",
                "catalog records must be regular JSON files, not symlinks or directories",
                &relative,
                "",
            ));
            continue;
        }

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                diagnostics.push(Diagnostic::error(
                    "catalog.read",
                    format!("cannot read catalog record: {error}"),
                    &relative,
                    "",
                ));
                continue;
            }
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                diagnostics.push(Diagnostic::error(
                    "catalog.invalid-json",
                    format!("cannot parse JSON: {error}"),
                    &relative,
                    "",
                ));
                continue;
            }
        };
        let application = value
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if strict {
            validate_known_fields(&value, &relative, &application, &mut diagnostics);
        }
        let record: Record = match serde_json::from_value(value) {
            Ok(record) => record,
            Err(error) => {
                diagnostics.push(Diagnostic::error(
                    "catalog.shape",
                    format!("record does not match the v1alpha1 shape: {error}"),
                    &relative,
                    &application,
                ));
                continue;
            }
        };

        validate_record(
            &root,
            &path,
            &relative,
            &record,
            &modules,
            &gitlinks,
            &mut seen_names,
            &mut seen_inventory_paths,
            &mut diagnostics,
        );
    }

    if records == 0 {
        diagnostics.push(Diagnostic::error(
            "catalog.empty",
            format!("no JSON records found under {catalog_text}"),
            catalog_text,
            "",
        ));
    }

    diagnostics.sort_by(|left, right| {
        (
            &left.severity,
            &left.path,
            &left.application,
            &left.rule_id,
            &left.message,
        )
            .cmp(&(
                &right.severity,
                &right.path,
                &right.application,
                &right.rule_id,
                &right.message,
            ))
    });
    let errors = diagnostics
        .iter()
        .filter(|item| item.severity == "error")
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|item| item.severity == "warning")
        .count();
    Ok(Report {
        valid: errors == 0,
        records,
        gitlinks: gitlinks.len(),
        errors,
        warnings,
        offline,
        schema: None,
        changed_from: None,
        changed_gitlinks: Vec::new(),
        diagnostics,
    })
}

