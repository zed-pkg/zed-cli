//! Apply the repository's flags-2-env contract before clap reads configuration.
//!
//! The native parser is optional at runtime because clap remains the actual
//! typed command parser and directly supports every declared env fallback.
//! Release bundles can ship libflags2env for config auditing/normalization;
//! minimal standalone binaries retain identical behavior without it.

pub fn apply_cli_flags() {
    let parser = match unsafe { flags2env::Flags2Env::load(None) } {
        Ok(parser) => parser,
        Err(_) => return,
    };
    let argv: Vec<String> = std::env::args().collect();
    let config = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cli-flags.toml");
    if !config.exists() {
        return;
    }
    let Some(config) = config.to_str() else {
        return;
    };
    let Ok(overrides) = parser.parse(&argv, Some(config)) else {
        return;
    };
    for (key, value) in overrides {
        // SAFETY: this runs once at process startup before any threads are
        // created or clap/config reads begin.
        unsafe { std::env::set_var(key, value) };
    }
}
