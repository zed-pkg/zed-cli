use std::collections::BTreeSet;
use std::process::{Command, Output};

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn help_long_options(help: &str) -> BTreeSet<String> {
    help.lines()
        .flat_map(str::split_whitespace)
        .filter_map(|token| {
            let token = token.trim_end_matches([',', ':']);
            token
                .strip_prefix("--")
                .filter(|name| !name.is_empty() && *name != "help")
                .map(|name| name.split(['=', '<']).next().unwrap_or(name).to_owned())
        })
        .collect()
}

#[test]
fn composed_root_gitops_model_matches_the_sibling_executable_surface() {
    let root = zed_cli::completion::root_command();
    let gitops = root
        .find_subcommand("gitops")
        .expect("root model must advertise the external gitops helper");
    let validate = gitops
        .find_subcommand("validate")
        .expect("root model must advertise gitops validate");
    let modeled = validate
        .get_arguments()
        .filter_map(|argument| argument.get_long())
        .filter(|name| !matches!(*name, "help" | "version"))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

    let expected = [
        "catalog",
        "changed-from",
        "format",
        "offline",
        "root",
        "schema",
        "strict",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();
    assert_eq!(modeled, expected, "synthetic root helper surface drifted");

    let output = Command::new(env!("CARGO_BIN_EXE_zed-gitops"))
        .args(["validate", "--help"])
        .output()
        .expect("run standalone zed-gitops help");
    assert!(output.status.success(), "{}", text(&output));
    assert!(output.stderr.is_empty(), "{}", text(&output));
    let direct_help = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        help_long_options(&direct_help),
        modeled,
        "root completion/help model and the standalone helper must expose the same long options"
    );
}

#[test]
fn secret_argv_is_rejected_before_external_gitops_dispatch() {
    let secret = "SYNTHETIC_GITOPS_SECRET_MUST_NOT_REACH_CHILD";
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["--token", secret, "gitops", "validate", "--help"])
        .output()
        .expect("run root zed with forbidden secret argv");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--token is not accepted"), "{stderr}");
    assert!(
        !stderr.contains(secret),
        "secret value escaped rejection: {stderr}"
    );
    assert!(
        !stderr.contains("Usage: zed-gitops"),
        "external helper must not start after secret argv rejection: {stderr}"
    );
}
