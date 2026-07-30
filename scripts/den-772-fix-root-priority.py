#!/usr/bin/env python3
"""Apply the DEN-772 project-root priority fix exactly once.

The connected GitHub API exposes whole-file writes rather than patch writes.
This temporary helper performs fail-closed textual edits in GitHub Actions;
the validated Rust changes are committed separately and this helper is removed.
"""

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
    '''/// Infer the ecosystem from the files a project keeps at its root. Ordered so
/// the most specific marker wins when a repo carries several (a Next.js app
/// with a Dockerfile is still `node`).
pub(crate) fn detect_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
        ("tsconfig.json", "node"),
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("pubspec.yaml", "dart"),
        ("mix.exs", "elixir"),
        ("rebar.config", "erlang"),
        ("gleam.toml", "gleam"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("Package.swift", "swift"),
        ("shard.yml", "crystal"),
        ("dune-project", "ocaml"),
        ("build.zig.zon", "zig"),
        ("DESCRIPTION", "r"),
        // Julia's Project.toml is checked after the more specific markers
        // above so a repo carrying both is not mistaken for Julia.
        ("Project.toml", "julia"),
        ("CMakeLists.txt", "cpp"),
    ];
    if let Some((_, target)) = MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
    {
        return Some((*target).to_string());
    }

    // Some consumer folders are intentionally pre-manifest (for example,
    // generated app skeletons). Keep this bounded and shallow so Zed never
    // recursively classifies an unrelated large checkout.
    const STRUCTURE_MARKERS: &[(&str, &str)] = &[
        ("src/main.rs", "rust"),
        ("src/lib.rs", "rust"),
        ("src/index.ts", "node"),
        ("src/main.ts", "node"),
        ("src/index.js", "node"),
        ("src/main.js", "node"),
        ("main.go", "go"),
        ("cmd/main.go", "go"),
        ("main.py", "python"),
        ("app.py", "python"),
        ("src/main.py", "python"),
        ("lib/main.dart", "dart"),
        ("src/main.gleam", "gleam"),
        ("src/main/java", "java"),
        ("src/main/kotlin", "java"),
    ];
    STRUCTURE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}
''',
    '''/// Infer the ecosystem from the files a project keeps at its root. A native
/// package-manager manifest is authoritative; source-layout inference is only a
/// fallback for intentionally pre-manifest project skeletons.
pub(crate) fn detect_target(project: &Path) -> Option<String> {
    detect_native_manifest_target(project).or_else(|| detect_structure_target(project))
}

/// Detect only authoritative native manifests. Manifestless root selection uses
/// this separately so a nearer `main.go` or `src/main.rs` cannot outrank the
/// actual `go.mod` or `Cargo.toml` that owns the project.
pub(crate) fn detect_native_manifest_target(project: &Path) -> Option<String> {
    const MARKERS: &[(&str, &str)] = &[
        ("package.json", "node"),
        ("tsconfig.json", "node"),
        ("Cargo.toml", "rust"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("pubspec.yaml", "dart"),
        ("mix.exs", "elixir"),
        ("rebar.config", "erlang"),
        ("gleam.toml", "gleam"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "java"),
        ("Gemfile", "ruby"),
        ("composer.json", "php"),
        ("Package.swift", "swift"),
        ("shard.yml", "crystal"),
        ("dune-project", "ocaml"),
        ("build.zig.zon", "zig"),
        ("DESCRIPTION", "r"),
        // Julia's Project.toml is checked after the more specific markers
        // above so a repo carrying both is not mistaken for Julia.
        ("Project.toml", "julia"),
        ("CMakeLists.txt", "cpp"),
    ];
    MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}

/// Detect bounded source layouts for projects that intentionally do not yet
/// have their ecosystem manifest. This is weaker evidence than a native
/// manifest and must never move an install below an authoritative ancestor.
pub(crate) fn detect_structure_target(project: &Path) -> Option<String> {
    const STRUCTURE_MARKERS: &[(&str, &str)] = &[
        ("src/main.rs", "rust"),
        ("src/lib.rs", "rust"),
        ("src/index.ts", "node"),
        ("src/main.ts", "node"),
        ("src/index.js", "node"),
        ("src/main.js", "node"),
        ("main.go", "go"),
        ("cmd/main.go", "go"),
        ("main.py", "python"),
        ("app.py", "python"),
        ("src/main.py", "python"),
        ("lib/main.dart", "dart"),
        ("src/main.gleam", "gleam"),
        ("src/main/java", "java"),
        ("src/main/kotlin", "java"),
    ];
    STRUCTURE_MARKERS
        .iter()
        .find(|(marker, _)| project.join(marker).exists())
        .map(|(_, target)| (*target).to_string())
}
''',
)

replace_once(
    "src/manifestless.rs",
    '''    if let Some(root) = native_or_lock_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    ProjectSelection {
''',
    '''    if let Some(root) = native_or_lock_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    if let Some(root) = structure_ancestor(requested) {
        return ProjectSelection {
            root,
            has_manifest: false,
        };
    }
    ProjectSelection {
''',
)

replace_once(
    "src/manifestless.rs",
    '''fn native_or_lock_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(LOCKFILE_FILE).is_file() || ops::detect_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Select a nested project only when there is exactly one plausible native
/// root. Multiple candidates are a monorepo; staying at the requested root is
/// safer than silently choosing between sibling applications.
fn unique_nested_project(start: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir())
        .filter(|entry| {
            entry.path().join(LOCKFILE_FILE).is_file() || ops::detect_target(entry.path()).is_some()
        })
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort();
    candidates.dedup();
    (candidates.len() == 1).then(|| candidates.remove(0))
}
''',
    '''fn native_or_lock_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(LOCKFILE_FILE).is_file()
            || ops::detect_native_manifest_target(dir).is_some()
        {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn structure_ancestor(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if ops::detect_structure_target(dir).is_some() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

fn nested_candidates(
    start: &Path,
    matches: impl Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = WalkDir::new(start)
        .min_depth(1)
        .max_depth(4)
        .into_iter()
        .filter_entry(should_descend)
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_dir() && matches(entry.path()))
        .map(|entry| entry.into_path())
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
}

/// Select a nested project only when there is exactly one plausible native
/// root. Authoritative native manifests absorb weaker source-layout candidates
/// below them; unrelated sibling candidates still make the repository
/// ambiguous, so Zed safely stays at the requested root.
fn unique_nested_project(start: &Path) -> Option<PathBuf> {
    let mut authoritative = nested_candidates(start, |path| {
        path.join(LOCKFILE_FILE).is_file()
            || ops::detect_native_manifest_target(path).is_some()
    });
    let mut heuristic = nested_candidates(start, |path| {
        ops::detect_structure_target(path).is_some()
    });
    heuristic.retain(|candidate| {
        !authoritative
            .iter()
            .any(|root| candidate.starts_with(root))
    });
    authoritative.extend(heuristic);
    authoritative.sort();
    authoritative.dedup();
    (authoritative.len() == 1).then(|| authoritative.remove(0))
}
''',
)

replace_once(
    "src/manifestless.rs",
    '''    #[test]
    fn nearest_native_ancestor_wins_from_a_source_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("apps/web/src/components");
        fs::create_dir_all(&source).unwrap();
        fs::write(temp.path().join("apps/web/package.json"), "{}").unwrap();
        assert_eq!(select_project(&source).root, temp.path().join("apps/web"));
    }

    #[test]
    fn no_specs_require_an_explicit_frozen_lock_and_frozen_rejects_specs() {
''',
    '''    #[test]
    fn nearest_native_ancestor_wins_from_a_source_subdirectory() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("apps/web/src/components");
        fs::create_dir_all(&source).unwrap();
        fs::write(temp.path().join("apps/web/package.json"), "{}").unwrap();
        assert_eq!(select_project(&source).root, temp.path().join("apps/web"));
    }

    #[test]
    fn native_manifest_ancestor_beats_a_nearer_structure_marker() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("services/api");
        let invocation = project.join("cmd/app/deep");
        fs::create_dir_all(&invocation).unwrap();
        fs::write(project.join("go.mod"), "module example.com/api\n").unwrap();
        fs::write(project.join("cmd/app/main.go"), "package main\n").unwrap();

        assert_eq!(select_project(&invocation).root, project);
    }

    #[test]
    fn one_nested_manifest_absorbs_its_structure_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("services/api");
        fs::create_dir_all(project.join("cmd/app")).unwrap();
        fs::write(project.join("go.mod"), "module example.com/api\n").unwrap();
        fs::write(project.join("cmd/app/main.go"), "package main\n").unwrap();

        assert_eq!(select_project(temp.path()).root, project);
    }

    #[test]
    fn unrelated_structure_sibling_keeps_nested_selection_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let api = temp.path().join("services/api");
        let web = temp.path().join("apps/web");
        fs::create_dir_all(api.join("cmd/app")).unwrap();
        fs::create_dir_all(web.join("src")).unwrap();
        fs::write(api.join("go.mod"), "module example.com/api\n").unwrap();
        fs::write(api.join("cmd/app/main.go"), "package main\n").unwrap();
        fs::write(web.join("src/main.ts"), "console.log('web')\n").unwrap();

        assert_eq!(select_project(temp.path()).root, temp.path());
    }

    #[test]
    fn no_specs_require_an_explicit_frozen_lock_and_frozen_rejects_specs() {
''',
)

print("DEN-772 project-root priority fix applied")
