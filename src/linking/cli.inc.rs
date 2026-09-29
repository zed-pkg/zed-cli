fn route(args: &[OsString]) -> Route {
    let Some((command_index, command)) = first_command(args) else {
        return Route::Existing;
    };
    match command.as_str() {
        "link" | "ln" | "unlink" | "links" => Route::Link,
        "help" => match next_positional(args, command_index + 1) {
            Some((target_index, target))
                if matches!(target.as_str(), "link" | "ln" | "unlink" | "links") =>
            {
                Route::Help {
                    help_index: command_index,
                    target_index,
                }
            }
            _ => Route::Existing,
        },
        _ => Route::Existing,
    }
}

fn first_command(args: &[OsString]) -> Option<(usize, String)> {
    let mut index = 1;
    while index < args.len() {
        let token = args.get(index)?.to_string_lossy();
        if token == "--" {
            return next_positional(args, index + 1);
        }
        if global_option_takes_value(&token) {
            index += if token.contains('=') { 1 } else { 2 };
            continue;
        }
        if token.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, token.into_owned()));
    }
    None
}

fn next_positional(args: &[OsString], mut index: usize) -> Option<(usize, String)> {
    while index < args.len() {
        let token = args.get(index)?.to_string_lossy();
        if !token.starts_with('-') {
            return Some((index, token.into_owned()));
        }
        index += 1;
    }
    None
}

fn global_option_takes_value(token: &str) -> bool {
    const OPTIONS: &[&str] = &[
        "--registry",
        "--home",
        "--auth-url",
        "--supabase-url",
        "--supabase-key",
    ];
    OPTIONS.iter().any(|option| {
        token == *option
            || token
                .strip_prefix(option)
                .is_some_and(|remainder| remainder.starts_with('='))
    })
}

fn utf8_args(args: &[OsString]) -> Result<Vec<String>> {
    args.iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .context("flags-2-env requires UTF-8 command-line arguments")
        })
        .collect()
}

fn validate_link_flags(argv: &[String]) -> Result<()> {
    let parser_argv = argv
        .iter()
        .filter(|token| !matches!(token.as_str(), "--help" | "-h" | "--version" | "-V"))
        .cloned()
        .collect::<Vec<_>>();
    let contract_dir = tempfile::tempdir().context("creating local-link flags2env directory")?;
    let contract_path = contract_dir.path().join(".cli-flags.toml");
    fs::write(&contract_path, LINK_CONTRACT).context("writing embedded local-link contract")?;
    let contract_path = contract_path
        .to_str()
        .context("embedded local-link contract path is not UTF-8")?;
    let parser = BundledFlags2Env::new();
    parser
        .audit_config(Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed link flags2env audit failed: {error}"))?;
    let parsed = parser
        .parse_structured(&parser_argv, Some(contract_path))
        .map_err(|error| anyhow::anyhow!("zed link flags2env parse failed: {error}"))?;
    if !parsed.unknown_options.is_empty() {
        bail!(
            "flags2env rejected unknown zed link option(s): {}",
            parsed.unknown_options.join(", ")
        );
    }
    if !parsed.errors.is_empty() {
        bail!(
            "flags2env rejected invalid zed link value(s): {}",
            parsed.errors.join("; ")
        );
    }
    Ok(())
}

