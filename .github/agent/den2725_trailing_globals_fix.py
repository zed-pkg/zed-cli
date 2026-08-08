#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement target, found {count}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/external_subcommands.rs",
    '''            let mut arguments = args[index + 2..].to_vec();
            if !arguments
''',
    '''            let (mut arguments, trailing_environment) =
                extract_root_options(&args[index + 2..])?;
            environment.extend(trailing_environment);
            if !arguments
''',
)

replace_once(
    "src/external_subcommands.rs",
    '''        return Some(ExternalRoute {
            name: token.to_owned(),
            arguments: args[index + 1..].to_vec(),
            environment,
        });
''',
    '''        let (arguments, trailing_environment) = extract_root_options(&args[index + 1..])?;
        environment.extend(trailing_environment);
        return Some(ExternalRoute {
            name: token.to_owned(),
            arguments,
            environment,
        });
''',
)

replace_once(
    "src/external_subcommands.rs",
    '''fn root_value_option(token: &str) -> Option<(&'static str, Option<&str>)> {
''',
    '''fn extract_root_options(
    args: &[OsString],
) -> Option<(Vec<OsString>, Vec<(OsString, OsString)>)> {
    let mut arguments = Vec::new();
    let mut environment = Vec::new();
    let mut index = 0;

    while index < args.len() {
        let Some(token) = args[index].to_str() else {
            arguments.push(args[index].clone());
            index += 1;
            continue;
        };
        if token == "--" {
            arguments.extend_from_slice(&args[index..]);
            break;
        }

        if let Some((key, inline)) = root_value_option(token) {
            let (value, consumed) = match inline {
                Some(value) if !value.is_empty() => (OsString::from(value), 1),
                Some(_) => return None,
                None => {
                    let value = args.get(index + 1)?.clone();
                    if value.is_empty() {
                        return None;
                    }
                    (value, 2)
                }
            };
            environment.push((OsString::from(key), value));
            index += consumed;
            continue;
        }

        if let Some((key, value)) = root_boolean_option(token) {
            environment.push((OsString::from(key), OsString::from(value)));
            index += 1;
            continue;
        }
        if is_root_boolean_spelling(token) {
            return None;
        }

        arguments.push(args[index].clone());
        index += 1;
    }

    Some((arguments, environment))
}

fn is_root_boolean_spelling(token: &str) -> bool {
    ROOT_BOOLEAN_OPTIONS.iter().any(|(option, _)| {
        token == *option
            || token == format!("--no-{}", option.trim_start_matches('-'))
            || token
                .strip_prefix(option)
                .is_some_and(|tail| tail.starts_with('='))
    })
}

fn root_value_option(token: &str) -> Option<(&'static str, Option<&str>)> {
''',
)

replace_once(
    "src/external_subcommands.rs",
    '''    #[test]
    fn help_spelling_routes_to_external_help() {
''',
    '''    #[test]
    fn root_options_after_plugin_are_lifted_until_double_dash() {
        let route = external_route(&os_args(&[
            "zed",
            "gitops",
            "validate",
            "--token",
            "fixture-value",
            "--offline",
            "--",
            "--home",
            "child-owned-value",
        ]))
        .expect("external route");
        assert_eq!(
            route.arguments,
            os_args(&[
                "validate",
                "--offline",
                "--",
                "--home",
                "child-owned-value"
            ])
        );
        assert_eq!(
            route.environment,
            vec![(
                OsString::from("ZED_PKG_TOKEN"),
                OsString::from("fixture-value")
            )]
        );
    }

    #[test]
    fn malformed_trailing_root_boolean_fails_closed() {
        assert!(external_route(&os_args(&[
            "zed",
            "gitops",
            "validate",
            "--git-submodules=maybe"
        ]))
        .is_none());
    }

    #[test]
    fn help_spelling_routes_to_external_help() {
''',
)

replace_once(
    "docs/gitops-validator.md",
    '''Root options placed before `gitops` are passed as their canonical
`ZED_PKG_*` environment variables rather than being exposed on the child
command line.
''',
    '''Root options placed before or after `gitops` are passed as their canonical
`ZED_PKG_*` environment variables rather than being exposed on the child
command line. A literal `--` ends global-option extraction and passes every
remaining argument to the external command unchanged.
''',
)

replace_once(
    "tests/external_gitops_dispatch.rs",
    '''    assert!(text.contains("Usage: zed-gitops validate"), "{text}");
    assert!(text.contains("--offline"), "{text}");
''',
    '''    assert!(text.contains("Usage: zed-gitops"), "{text}");
    assert!(text.contains("validate [OPTIONS]"), "{text}");
    assert!(text.contains("--offline"), "{text}");
''',
)
