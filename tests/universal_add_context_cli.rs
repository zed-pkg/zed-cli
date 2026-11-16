//! Universal add context is CLI-visible: parse project and target without
//! contacting zpkg.net, invoking native managers or mutating source manifests.
use clap::Parser;
use std::path::PathBuf;
use zed_cli::cli::{Cli, Cmd};

#[test]
fn explicit_project_and_target_are_accepted_for_add() {
    let cli = Cli::try_parse_from([
        "zed",
        "add",
        "acme/answer@^1",
        "--project",
        "apps/api",
        "--target",
        "attolang",
    ])
    .expect("parse universal add selectors");
    match cli.cmd {
        Cmd::Add {
            spec,
            project,
            target,
        } => {
            assert_eq!(spec, "acme/answer@^1");
            assert_eq!(project, Some(PathBuf::from("apps/api")));
            assert_eq!(target.as_deref(), Some("attolang"));
        }
        _ => panic!("expected add command"),
    }
}

#[test]
fn normal_add_remains_backwards_compatible() {
    let cli =
        Cli::try_parse_from(["zed", "add", "acme/answer@^1"]).expect("parse legacy add syntax");
    match cli.cmd {
        Cmd::Add { spec, .. } => assert_eq!(spec, "acme/answer@^1"),
        _ => panic!("expected add command"),
    }
}

#[test]
fn global_options_work_with_explicit_add_selectors() {
    let cli = Cli::try_parse_from([
        "zed",
        "--registry",
        "file:///tmp/fixture",
        "add",
        "acme/math",
        "--target",
        "rust",
        "--project",
        "workspace/crate",
    ])
    .expect("parse global+scoped add");
    match cli.cmd {
        Cmd::Add {
            project, target, ..
        } => {
            assert_eq!(project, Some(PathBuf::from("workspace/crate")));
            assert_eq!(target.as_deref(), Some("rust"));
        }
        _ => panic!("expected add command"),
    }
}

#[test]
fn project_and_target_support_equals_form_and_reverse_order() {
    for args in [
        vec![
            "zed", "add", "acme/answer@^1", "--project=apps/api",
            "--target=attolang",
        ],
        vec![
            "zed", "--target=attolang", "add", "acme/answer@^1",
            "--project=apps/api",
        ],
    ] {
        let cli = Cli::try_parse_from(args).expect("parse equivalent add selector spellings");
        match cli.cmd {
            Cmd::Add { project, target, .. } => {
                assert_eq!(project, Some(PathBuf::from("apps/api")));
                assert_eq!(target.as_deref(), Some("attolang"));
            }
            _ => panic!("expected scoped add command"),
        }
    }
}

