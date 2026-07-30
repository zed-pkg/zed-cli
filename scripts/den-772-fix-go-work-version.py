#!/usr/bin/env python3
"""Apply the DEN-772 Go workspace version fix exactly once."""

from pathlib import Path

OPS = Path(__file__).resolve().parents[1] / "src" / "ops.rs"
content = OPS.read_text(encoding="utf-8")

anchor = "/// Emit the native wiring file for each adapter that needs one."
if content.count(anchor) != 1:
    raise RuntimeError("expected one toolchain-wiring anchor")
if "struct GoDirectiveVersion" in content:
    raise RuntimeError("Go directive version support is already present")

version_support = '''#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GoDirectiveVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_go_directive(text: &str) -> Option<(GoDirectiveVersion, String)> {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("go") {
            continue;
        }
        let token = fields.next()?;
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
        if parts.next().is_some() {
            return None;
        }
        return Some((
            GoDirectiveVersion {
                major,
                minor,
                patch,
            },
            token.to_string(),
        ));
    }
    None
}

/// A go.work file must declare a Go version at least as new as every module it
/// includes. Start from Zed's compatibility floor and select the highest
/// numeric `go` directive from the consumer and installed package modules.
fn required_go_work_version(project: &Path, paths: &[PathBuf]) -> String {
    let mut selected = (
        GoDirectiveVersion {
            major: 1,
            minor: 21,
            patch: 0,
        },
        "1.21".to_string(),
    );
    for root in std::iter::once(project).chain(paths.iter().map(PathBuf::as_path)) {
        let Ok(document) = fs::read_to_string(root.join("go.mod")) else {
            continue;
        };
        let Some(candidate) = parse_go_directive(&document) else {
            continue;
        };
        if candidate.0 > selected.0 {
            selected = candidate;
        }
    }
    selected.1
}

'''
content = content.replace(anchor, version_support + anchor, 1)

backslash = chr(92)
old_doc = (
    '                let mut doc = String::from("go 1.21'
    f'{backslash}n{backslash}nuse ({backslash}n");'
)
new_doc = (
    '                let version = required_go_work_version(project, paths);\n'
    '                let mut doc = format!("go {version}'
    f'{backslash}n{backslash}nuse ({backslash}n");'
)
if content.count(old_doc) != 1:
    raise RuntimeError(f"expected one hardcoded go.work version, found {content.count(old_doc)}")
content = content.replace(old_doc, new_doc, 1)

test_anchor = '''    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
'''
if content.count(test_anchor) != 1:
    raise RuntimeError("expected one frozen-lock test anchor")
version_tests = '''    #[test]
    fn go_workspace_uses_the_highest_module_go_directive() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let first = project.join("zed_modules/acme/first");
        let second = project.join("zed_modules/acme/second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(
            project.join("go.mod"),
            r#"module example.com/app

go 1.22
"#,
        )
        .unwrap();
        fs::write(
            first.join("go.mod"),
            r#"module example.com/first

go 1.21
"#,
        )
        .unwrap();
        fs::write(
            second.join("go.mod"),
            r#"module example.com/second

go 1.24.1 // minimum toolchain
"#,
        )
        .unwrap();
        let roots = BTreeMap::from([(Adapter::Go, vec![first, second])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let document = fs::read_to_string(project.join(".zed/go.work")).unwrap();
        assert_eq!(document.lines().next(), Some("go 1.24.1"), "{document}");
    }

    #[test]
    fn malformed_go_directives_do_not_lower_the_safe_default() {
        assert_eq!(required_go_work_version(Path::new("/missing"), &[]), "1.21");
        assert!(parse_go_directive("module example.com/app go latest").is_none());
        assert!(parse_go_directive("go 1").is_none());
    }

'''
content = content.replace(test_anchor, version_tests + test_anchor, 1)
OPS.write_text(content, encoding="utf-8")
print("DEN-772 Go workspace version fixed")
