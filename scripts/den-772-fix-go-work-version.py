#!/usr/bin/env python3
"""Apply the DEN-772 Go workspace version fix exactly once."""

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    content = target.read_text(encoding="utf-8")
    count = content.count(old)
    if count != 1:
        raise RuntimeError(f"{path}: expected one replacement target, found {count}")
    target.write_text(content.replace(old, new, 1), encoding="utf-8")


replace_once(
    "src/ops.rs",
    '''const LANGUAGES_BY_ECOSYSTEM: &[(Ecosystem, &str)] = &[
    (Ecosystem::Npm, "nodejs"),
    (Ecosystem::Jvm, "java"),
    (Ecosystem::Jvm, "kotlin"),
    (Ecosystem::Gomod, "golang"),
    (Ecosystem::Pypi, "python"),
    (Ecosystem::Cargo, "rust"),
    (Ecosystem::Pub, "dart"),
    (Ecosystem::Gem, "ruby"),
    (Ecosystem::Composer, "php"),
    (Ecosystem::Nuget, "csharp"),
    (Ecosystem::Swiftpm, "swift"),
    (Ecosystem::Hex, "gleam"),
];

/// Emit the native wiring file for each adapter that needs one.
''',
    '''const LANGUAGES_BY_ECOSYSTEM: &[(Ecosystem, &str)] = &[
    (Ecosystem::Npm, "nodejs"),
    (Ecosystem::Jvm, "java"),
    (Ecosystem::Jvm, "kotlin"),
    (Ecosystem::Gomod, "golang"),
    (Ecosystem::Pypi, "python"),
    (Ecosystem::Cargo, "rust"),
    (Ecosystem::Pub, "dart"),
    (Ecosystem::Gem, "ruby"),
    (Ecosystem::Composer, "php"),
    (Ecosystem::Nuget, "csharp"),
    (Ecosystem::Swiftpm, "swift"),
    (Ecosystem::Hex, "gleam"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct GoDirectiveVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_go_directive(text: &str) -> Option<(GoDirectiveVersion, String)> {
    for line in text.lines() {
        let mut fields = line.trim().split_whitespace();
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

/// Emit the native wiring file for each adapter that needs one.
''',
)

replace_once(
    "src/ops.rs",
    '''                work_paths.sort();
                work_paths.dedup();
                let mut doc = String::from("go 1.21\\n\\nuse (\\n");
                for path in &work_paths {
''',
    '''                work_paths.sort();
                work_paths.dedup();
                let version = required_go_work_version(project, paths);
                let mut doc = format!("go {version}\\n\\nuse (\\n");
                for path in &work_paths {
''',
)

replace_once(
    "src/ops.rs",
    '''    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
    '''    #[test]
    fn go_workspace_uses_the_highest_module_go_directive() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let first = project.join("zed_modules/acme/first");
        let second = project.join("zed_modules/acme/second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(project.join("go.mod"), "module example.com/app\\n\\ngo 1.22\\n").unwrap();
        fs::write(first.join("go.mod"), "module example.com/first\\n\\ngo 1.21\\n").unwrap();
        fs::write(
            second.join("go.mod"),
            "module example.com/second\\n\\ngo 1.24.1 // minimum toolchain\\n",
        )
        .unwrap();
        let roots = BTreeMap::from([(Adapter::Go, vec![first, second])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let document = fs::read_to_string(project.join(".zed/go.work")).unwrap();
        assert!(document.starts_with("go 1.24.1\\n"), "{document}");
    }

    #[test]
    fn malformed_go_directives_do_not_lower_the_safe_default() {
        assert_eq!(required_go_work_version(Path::new("/missing"), &[]), "1.21");
        assert!(parse_go_directive("module example.com/app\\ngo latest\\n").is_none());
        assert!(parse_go_directive("module example.com/app\\ngo 1\\n").is_none());
    }

    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
)

print("DEN-772 Go workspace version fixed")
