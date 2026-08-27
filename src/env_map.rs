//! Immutable application environment snapshot.
//!
//! `std::env` and process argv are copied at the process boundary. CLI
//! overrides from flags-2-env are merged into an ordinary map. This module
//! never writes the process environment.

use std::collections::BTreeMap;

pub type EnvMap = BTreeMap<String, String>;

/// Deterministic merge: later override entries win over the initial map.
pub fn get_env_map(
    initial: EnvMap,
    overrides: impl IntoIterator<Item = (String, String)>,
) -> EnvMap {
    overrides
        .into_iter()
        .fold(initial, |mut env, (key, value)| {
            env.insert(key, value);
            env
        })
}

/// Return a trimmed non-empty value from an environment snapshot.
pub fn env_value<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Copy the process environment. This is an impure boundary helper.
pub fn process_env_map() -> EnvMap {
    std::env::vars().collect()
}

/// Copy process arguments. This is an impure boundary helper.
pub fn process_argv() -> Vec<String> {
    std::env::args().collect()
}

/// Snapshot the current process environment for tests and fallbacks.
pub fn current_env_map() -> EnvMap {
    process_env_map()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_values_override_environment_values() {
        let initial = EnvMap::from([
            ("ZED_PKG_INTERACTIVE".into(), "false".into()),
            ("ZED_PKG_FROZEN".into(), "false".into()),
        ]);
        let overrides = EnvMap::from([("ZED_PKG_INTERACTIVE".into(), "true".into())]);
        let env = get_env_map(initial, overrides);

        assert_eq!(
            env.get("ZED_PKG_INTERACTIVE").map(String::as_str),
            Some("true")
        );
        assert_eq!(env.get("ZED_PKG_FROZEN").map(String::as_str), Some("false"));
    }

    #[test]
    fn merge_does_not_mutate_process_environment() {
        let before = std::env::var_os("ZED_PKG_INTERACTIVE");
        let env = get_env_map(
            EnvMap::from([("ZED_PKG_INTERACTIVE".into(), "false".into())]),
            [("ZED_PKG_INTERACTIVE".into(), "true".into())],
        );
        assert_eq!(
            env.get("ZED_PKG_INTERACTIVE").map(String::as_str),
            Some("true")
        );
        assert_eq!(std::env::var_os("ZED_PKG_INTERACTIVE"), before);
    }

    #[test]
    fn source_does_not_write_process_environment() {
        const SRC: &str = include_str!("env_map.rs");
        let production = SRC.split("#[cfg(test)]").next().unwrap_or(SRC);
        assert!(!production.contains("set_var"));
    }
}
