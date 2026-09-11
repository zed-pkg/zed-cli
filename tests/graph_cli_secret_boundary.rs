use toml::Value;

fn collect_flag_envs(value: &Value, envs: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    if let Some(flags) = table.get("flags").and_then(Value::as_table) {
        for flag in flags.values().filter_map(Value::as_table) {
            if let Some(env) = flag.get("env").and_then(Value::as_str) {
                envs.push(env.to_owned());
            }
        }
    }
    for (name, child) in table {
        if name != "flags" {
            collect_flag_envs(child, envs);
        }
    }
}

#[test]
fn graph_registry_token_is_environment_only() {
    let source = std::fs::read_to_string(".graph-cli-flags.toml")
        .expect("read .graph-cli-flags.toml");
    let document: Value = toml::from_str(&source).expect("parse graph flags contract");

    let ignored = document
        .get("env")
        .and_then(Value::as_table)
        .and_then(|env| env.get("ignore"))
        .and_then(Value::as_array)
        .expect("graph contract must declare [env].ignore")
        .iter()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        ignored.contains(&"ZED_PKG_TOKEN"),
        "registry bearer token must stay environment-only in the graph contract"
    );

    let mut public_envs = Vec::new();
    collect_flag_envs(&document, &mut public_envs);
    assert!(
        !public_envs.iter().any(|env| env == "ZED_PKG_TOKEN"),
        "graph flags contract must never expose ZED_PKG_TOKEN through argv"
    );

    assert!(
        !source.contains("aliases = [\"token\"]"),
        "graph contract must not advertise --token"
    );
}
