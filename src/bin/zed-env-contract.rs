use std::{collections::{BTreeMap, BTreeSet}, env, fs, path::{Path, PathBuf}};

use anyhow::{bail, Context, Result};
use toml::Value;

#[derive(Debug, Clone)]
struct EnvVar {
    name: String,
    kind: String,
    required: bool,
    required_when: Option<String>,
    default: Option<String>,
    secret: bool,
    description: Option<String>,
    allowed: Vec<String>,
    deprecated: bool,
    replacement: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "check".to_string());
    let manifest = manifest_path(args.next().map(PathBuf::from))?;
    let vars = load_contract(&manifest)?;
    validate_contract(&vars)?;

    match command.as_str() {
        "check" => check_values(&vars),
        "list" => {
            for var in vars.values() {
                println!("{}\t{}\t{}\t{}", var.name, var.kind, requirement(var), if var.secret { "secret" } else { "plain" });
            }
            Ok(())
        }
        "template" => {
            for var in vars.values() {
                if let Some(description) = &var.description {
                    println!("# {description}");
                }
                println!("# {} ({})", requirement(var), var.kind);
                let value = if var.secret { "" } else { var.default.as_deref().unwrap_or("") };
                println!("{}={}\n", var.name, value);
            }
            Ok(())
        }
        "json" => {
            let rows = vars.values().map(|v| serde_json::json!({
                "name": v.name,
                "type": v.kind,
                "required": v.required,
                "required_when": v.required_when,
                "default": if v.secret { None::<String> } else { v.default.clone() },
                "secret": v.secret,
                "description": v.description,
                "enum": v.allowed,
                "deprecated": v.deprecated,
                "replacement": v.replacement,
            })).collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&rows)?);
            Ok(())
        }
        other => bail!("unknown env-contract command `{other}`; expected check, list, template, or json"),
    }
}

fn manifest_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit { return Ok(path); }
    let mut current = env::current_dir()?;
    loop {
        let candidate = current.join(".zpkg.toml");
        if candidate.is_file() { return Ok(candidate); }
        if !current.pop() { break; }
    }
    bail!("could not find .zpkg.toml in this directory or any parent")
}

fn load_contract(path: &Path) -> Result<BTreeMap<String, EnvVar>> {
    let text = fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let root: Value = text.parse().with_context(|| format!("invalid TOML in {}", path.display()))?;
    let Some(env_table) = root.get("env").and_then(Value::as_table) else { return Ok(BTreeMap::new()); };
    let Some(rows) = env_table.get("vars").and_then(Value::as_array) else { return Ok(BTreeMap::new()); };
    let mut out = BTreeMap::new();
    for row in rows {
        let table = row.as_table().context("each [[env.vars]] entry must be a table")?;
        let name = string(table, "name")?.to_string();
        if out.contains_key(&name) { bail!("duplicate env contract variable `{name}`"); }
        let allowed = table.get("enum").and_then(Value::as_array).map(|values| {
            values.iter().map(|v| v.as_str().context("env var enum values must be strings").map(str::to_owned)).collect::<Result<Vec<_>>>()
        }).transpose()?.unwrap_or_default();
        out.insert(name.clone(), EnvVar {
            name,
            kind: table.get("type").and_then(Value::as_str).unwrap_or("string").to_string(),
            required: table.get("required").and_then(Value::as_bool).unwrap_or(false),
            required_when: opt_string(table, "required_when")?,
            default: opt_scalar_string(table, "default")?,
            secret: table.get("secret").and_then(Value::as_bool).unwrap_or(false),
            description: opt_string(table, "description")?,
            allowed,
            deprecated: table.get("deprecated").and_then(Value::as_bool).unwrap_or(false),
            replacement: opt_string(table, "replacement")?,
        });
    }
    Ok(out)
}

fn validate_contract(vars: &BTreeMap<String, EnvVar>) -> Result<()> {
    const TYPES: &[&str] = &["string", "integer", "boolean", "url", "json"];
    for var in vars.values() {
        if !valid_env_name(&var.name) { bail!("invalid environment variable name `{}`", var.name); }
        if !TYPES.contains(&var.kind.as_str()) { bail!("unsupported type `{}` for `{}`", var.kind, var.name); }
        if var.secret && var.default.is_some() { bail!("secret `{}` must not declare a default value", var.name); }
        if let Some(default) = &var.default {
            validate_value(var, default).with_context(|| format!("invalid default for `{}`", var.name))?;
        }
        if let Some(replacement) = &var.replacement {
            if !var.deprecated { bail!("`{}` declares replacement without deprecated=true", var.name); }
            if !vars.contains_key(replacement) { bail!("replacement `{replacement}` for `{}` is not declared", var.name); }
        }
        if let Some(condition) = &var.required_when {
            let (dependency, _, _) = parse_condition(condition)?;
            if !vars.contains_key(dependency) { bail!("required_when for `{}` references undeclared `{dependency}`", var.name); }
        }
    }
    Ok(())
}

fn check_values(vars: &BTreeMap<String, EnvVar>) -> Result<()> {
    let snapshot = env::vars().collect::<BTreeMap<_, _>>();
    let mut missing = BTreeSet::new();
    for var in vars.values() {
        let needed = var.required || var.required_when.as_deref().map(|c| condition_matches(c, &snapshot)).transpose()?.unwrap_or(false);
        match snapshot.get(&var.name) {
            Some(value) => validate_value(var, value).with_context(|| format!("invalid value for `{}`", var.name))?,
            None if needed && var.default.is_none() => { missing.insert(var.name.clone()); },
            None => {}
        }
    }
    if !missing.is_empty() { bail!("missing required environment variables: {}", missing.into_iter().collect::<Vec<_>>().join(", ")); }
    println!("environment contract ok ({} variables)", vars.len());
    Ok(())
}

fn validate_value(var: &EnvVar, value: &str) -> Result<()> {
    match var.kind.as_str() {
        "integer" => { value.parse::<i64>().context("expected integer")?; }
        "boolean" => { if !matches!(value.to_ascii_lowercase().as_str(), "true"|"false"|"1"|"0"|"yes"|"no"|"on"|"off") { bail!("expected boolean"); } }
        "json" => { serde_json::from_str::<serde_json::Value>(value).context("expected JSON")?; }
        "url" => { if !value.contains("://") { bail!("expected absolute URL"); } }
        "string" => {}
        _ => unreachable!(),
    }
    if !var.allowed.is_empty() && !var.allowed.iter().any(|candidate| candidate == value) {
        bail!("expected one of: {}", var.allowed.join(", "));
    }
    Ok(())
}

fn condition_matches(condition: &str, values: &BTreeMap<String, String>) -> Result<bool> {
    let (name, op, expected) = parse_condition(condition)?;
    let actual = values.get(name).map(String::as_str).unwrap_or("");
    Ok(if op == "==" { actual == expected } else { actual != expected })
}

fn parse_condition(input: &str) -> Result<(&str, &str, &str)> {
    let (name, op, rhs) = if let Some((a,b)) = input.split_once(" == ") { (a.trim(), "==", b.trim()) }
        else if let Some((a,b)) = input.split_once(" != ") { (a.trim(), "!=", b.trim()) }
        else { bail!("required_when supports only `VAR == 'value'` and `VAR != 'value'`"); };
    if !valid_env_name(name) { bail!("invalid variable `{name}` in required_when"); }
    let expected = rhs.strip_prefix('\'').and_then(|v| v.strip_suffix('\''))
        .or_else(|| rhs.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
        .context("required_when value must be quoted")?;
    Ok((name, op, expected))
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_')) && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

fn requirement(var: &EnvVar) -> String {
    if var.required { "required".into() }
    else if let Some(c) = &var.required_when { format!("required when {c}") }
    else { "optional".into() }
}

fn string<'a>(table: &'a toml::map::Map<String, Value>, key: &str) -> Result<&'a str> {
    table.get(key).and_then(Value::as_str).with_context(|| format!("missing string `{key}` in [[env.vars]]"))
}
fn opt_string(table: &toml::map::Map<String, Value>, key: &str) -> Result<Option<String>> {
    table.get(key).map(|v| v.as_str().map(str::to_owned).with_context(|| format!("`{key}` must be a string"))).transpose()
}
fn opt_scalar_string(table: &toml::map::Map<String, Value>, key: &str) -> Result<Option<String>> {
    table.get(key).map(|v| match v { Value::String(s) => Ok(s.clone()), Value::Integer(v) => Ok(v.to_string()), Value::Boolean(v) => Ok(v.to_string()), _ => bail!("`{key}` must be a string, integer, or boolean") }).transpose()
}
