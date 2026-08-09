fn print_human(report: &Report) {
    println!(
        "GitOps composition contract: {} ({} records, {} errors, {} warnings, offline={})",
        if report.valid { "valid" } else { "invalid" },
        report.records,
        report.errors,
        report.warnings,
        report.offline
    );
    for item in &report.diagnostics {
        let application = if item.application.is_empty() {
            String::new()
        } else {
            format!(" [{}]", item.application)
        };
        println!(
            "{}: {}{}: {}: {}",
            item.severity, item.path, application, item.rule_id, item.message
        );
    }
}

fn print_sarif(report: &Report) -> Result<()> {
    let rule_ids = report
        .diagnostics
        .iter()
        .map(|item| item.rule_id.clone())
        .collect::<BTreeSet<_>>();
    let rules = rule_ids
        .into_iter()
        .map(|rule_id| {
            json!({
                "id": rule_id,
                "shortDescription": { "text": "Zed GitOps composition policy" }
            })
        })
        .collect::<Vec<_>>();
    let results = report
        .diagnostics
        .iter()
        .map(|item| {
            json!({
                "ruleId": item.rule_id.clone(),
                "level": item.severity.clone(),
                "message": { "text": item.message.clone() },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": item.path.clone() },
                        "region": { "startLine": 1 }
                    }
                }],
                "properties": {
                    "application": item.application.clone()
                }
            })
        })
        .collect::<Vec<_>>();
    let document = json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "zed-gitops",
                    "informationUri": "https://github.com/zed-pkg/zed-cli",
                    "rules": rules
                }
            },
            "results": results
        }]
    });
    println!("{}", serde_json::to_string_pretty(&document)?);
    Ok(())
}

