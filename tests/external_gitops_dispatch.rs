use std::process::{Command, Output};

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn gitops_usage(suffix: &str) -> String {
    format!("Usage: zed-gitops{}{suffix}", std::env::consts::EXE_SUFFIX)
}

#[test]
fn root_help_advertises_gitops_validate() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .arg("--help")
        .output()
        .expect("run zed help");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains("gitops"), "{text}");
    assert!(text.contains("Validate GitOps composition"), "{text}");
}

#[test]
fn root_dispatches_to_the_sibling_gitops_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["gitops", "validate", "--help"])
        .output()
        .expect("run zed gitops help");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains(&gitops_usage(" validate")), "{text}");
    assert!(text.contains("--offline"), "{text}");
}

#[test]
fn root_help_alias_reaches_the_external_binary() {
    let output = Command::new(env!("CARGO_BIN_EXE_zed"))
        .args(["help", "gitops"])
        .output()
        .expect("run zed help gitops");
    assert!(output.status.success(), "{}", text(&output));
    let text = text(&output);
    assert!(text.contains(&gitops_usage("")), "{text}");
}
