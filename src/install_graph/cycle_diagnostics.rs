//! Structured, terminal-visible diagnostics for dependency-cycle back-edges.
//!
//! The resolver calls this module only after it has recognized a back-edge in
//! the active provenance path. The diagnostic goes through the Rust SDK from
//! `ores-otel/ores.otel.log`, while the transport deliberately writes to stderr
//! so stdout remains safe for CLI automation.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};

use next_loggers::{
    JsonObject, LogLevel, LogRecord, Logger, LoggerError, Options, Transport, json,
};

const EVENT_NAME: &str = "dependency_cycle_detected";
const STRATEGY: &str = "canonical-store-symlink";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CycleDiagnostic {
    cycle: Vec<String>,
    closing_from: String,
    closing_to: String,
    requirement: String,
}

impl CycleDiagnostic {
    fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.cycle.join("->"),
            self.closing_from,
            self.closing_to,
            self.requirement
        )
    }

    fn cycle_path(&self) -> String {
        self.cycle.join(" -> ")
    }

    fn terminal_line(&self) -> String {
        format!(
            "zed-pkg: dependency cycle detected: {}; closing edge {} -> {} requires `{}`; \
             strategy={STRATEGY}; recursive-copy=stopped",
            self.cycle_path(),
            self.closing_from,
            self.closing_to,
            self.requirement
        )
    }

    fn fields(&self) -> JsonObject {
        JsonObject::from_iter([
            ("event".into(), json!(EVENT_NAME)),
            ("cycle.path".into(), json!(self.cycle_path())),
            ("cycle.nodes".into(), json!(&self.cycle)),
            ("cycle.closing_edge.from".into(), json!(&self.closing_from)),
            ("cycle.closing_edge.to".into(), json!(&self.closing_to)),
            ("cycle.requirement".into(), json!(&self.requirement)),
            ("cycle.identity".into(), json!("registry::org/name@version")),
            ("cycle.strategy".into(), json!(STRATEGY)),
            ("cycle.recursive_copy".into(), json!(false)),
        ])
    }
}

#[derive(Debug)]
struct CycleStderrTransport;

impl Transport for CycleStderrTransport {
    fn write(&self, record: &LogRecord) -> Result<(), LoggerError> {
        let fields = &record.fields;
        let cycle_path = string_field(fields, "cycle.path");
        let closing_from = string_field(fields, "cycle.closing_edge.from");
        let closing_to = string_field(fields, "cycle.closing_edge.to");
        let requirement = string_field(fields, "cycle.requirement");
        eprintln!(
            "zed-pkg: dependency cycle detected: {cycle_path}; closing edge {closing_from} -> \
             {closing_to} requires `{requirement}`; strategy={STRATEGY}; recursive-copy=stopped"
        );
        Ok(())
    }
}

fn string_field<'a>(fields: &'a JsonObject, key: &str) -> &'a str {
    fields
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
}

fn logger() -> &'static Logger {
    static LOGGER: OnceLock<Logger> = OnceLock::new();
    LOGGER.get_or_init(|| {
        let options = Options {
            app_name: "zed-cli".into(),
            name: Some("dependency-resolver".into()),
            runtime: "rust".into(),
            max_level: LogLevel::Warn,
            console: false,
            ..Options::default()
        }
        .with_transport(Arc::new(CycleStderrTransport));
        Logger::new(options)
    })
}

fn emitted_cycles() -> &'static Mutex<BTreeSet<String>> {
    static EMITTED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    EMITTED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn coordinate(segment: &str) -> &str {
    segment
        .rsplit_once('@')
        .map_or(segment, |(coordinate, _version)| coordinate)
}

fn diagnostic_for_back_edge(path: &[String], requirement: &str) -> Option<CycleDiagnostic> {
    let unresolved_target = path.last()?;
    let target_coordinate = coordinate(unresolved_target);
    let cycle_start = path[..path.len().saturating_sub(1)]
        .iter()
        .position(|segment| coordinate(segment) == target_coordinate)?;
    let mut cycle = path[cycle_start..path.len() - 1].to_vec();
    let exact_target = cycle.first()?.clone();
    let closing_from = cycle.last()?.clone();
    cycle.push(exact_target.clone());
    Some(CycleDiagnostic {
        cycle,
        closing_from,
        closing_to: exact_target,
        requirement: requirement.to_owned(),
    })
}

/// Emit one structured warning for a recognized resolver back-edge.
///
/// Diagnostics are deduplicated per process because constraint propagation can
/// revisit the same exact edge through more than one equivalent root path.
/// Logging is best-effort and can never make resolution fail.
pub(super) fn emit_cycle_back_edge(path: &[String], requirement: &str) {
    let Some(diagnostic) = diagnostic_for_back_edge(path, requirement) else {
        return;
    };
    let fingerprint = diagnostic.fingerprint();
    let first_observation = emitted_cycles()
        .lock()
        .map(|mut emitted| emitted.insert(fingerprint))
        .unwrap_or(true);
    if !first_observation {
        return;
    }

    let _ = logger()
        .warn(vec![json!("dependency cycle detected")])
        .add_fields(diagnostic.fields())
        .add_tags(["zed-pkg", "dependency-cycle", "resolver", "ores-otel"])
        .send();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_exact_versioned_cycle_from_a_longer_root_path() {
        let path = vec![
            "consumer/app@0.1.0".to_owned(),
            "acme/a@1.0.0".to_owned(),
            "acme/b@1.0.0".to_owned(),
            "acme/a".to_owned(),
        ];
        let diagnostic = diagnostic_for_back_edge(&path, "^1").unwrap();

        assert_eq!(
            diagnostic.cycle,
            ["acme/a@1.0.0", "acme/b@1.0.0", "acme/a@1.0.0"]
        );
        assert_eq!(diagnostic.closing_from, "acme/b@1.0.0");
        assert_eq!(diagnostic.closing_to, "acme/a@1.0.0");
        assert_eq!(diagnostic.requirement, "^1");
    }

    #[test]
    fn recognizes_a_versioned_self_loop() {
        let path = vec![
            "consumer/app@0.1.0".to_owned(),
            "acme/a@2.0.0".to_owned(),
            "acme/a".to_owned(),
        ];
        let diagnostic = diagnostic_for_back_edge(&path, "=2.0.0").unwrap();

        assert_eq!(diagnostic.cycle, ["acme/a@2.0.0", "acme/a@2.0.0"]);
        assert_eq!(
            diagnostic.terminal_line(),
            "zed-pkg: dependency cycle detected: acme/a@2.0.0 -> acme/a@2.0.0; closing edge acme/a@2.0.0 -> acme/a@2.0.0 requires `=2.0.0`; strategy=canonical-store-symlink; recursive-copy=stopped"
        );
    }

    #[test]
    fn does_not_misclassify_a_non_cycle_path() {
        let path = vec![
            "consumer/app@0.1.0".to_owned(),
            "acme/a@1.0.0".to_owned(),
            "acme/b".to_owned(),
        ];
        assert!(diagnostic_for_back_edge(&path, "^1").is_none());
    }
}
