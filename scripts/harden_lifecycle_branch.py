#!/usr/bin/env python3
"""Apply the review hardening patch to the lifecycle branch exactly once."""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LIFECYCLE = ROOT / "src/lifecycle.rs"
DOCS = ROOT / "docs/lifecycle-hooks.md"


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


source = LIFECYCLE.read_text(encoding="utf-8")
source = replace_once(
    source,
    'const CONVENTION_SUFFIXES: [&str; 6] = ["", ".sh", ".bash", ".ps1", ".cmd", ".bat"];\n',
    'const CONVENTION_SUFFIXES: [&str; 6] = ["", ".sh", ".bash", ".ps1", ".cmd", ".bat"];\n'
    'const LIFECYCLE_PHASE_NAMES: [&str; 12] = [\n'
    '    "pre-install",\n'
    '    "post-install",\n'
    '    "pre-build",\n'
    '    "post-build",\n'
    '    "pre-test",\n'
    '    "post-test",\n'
    '    "pre-pack",\n'
    '    "post-pack",\n'
    '    "pre-publish",\n'
    '    "post-publish",\n'
    '    "pre-uninstall",\n'
    '    "post-uninstall",\n'
    '];\n',
    "phase vocabulary",
)
source = replace_once(
    source,
    '    let document: ManifestLifecycle = toml::from_str(&contents)\n'
    '        .with_context(|| format!("parsing lifecycle configuration in {}", path.display()))?;\n'
    '    Ok(document\n'
    '        .lifecycle\n'
    '        .get(phase.as_str())\n'
    '        .cloned()\n'
    '        .map(HookValue::into_config))\n',
    '    let document: ManifestLifecycle = toml::from_str(&contents)\n'
    '        .with_context(|| format!("parsing lifecycle configuration in {}", path.display()))?;\n'
    '    for configured_phase in document.lifecycle.keys() {\n'
    '        ensure!(\n'
    '            LIFECYCLE_PHASE_NAMES.contains(&configured_phase.as_str()),\n'
    '            "unknown lifecycle phase `{configured_phase}` in {}; expected one of {}",\n'
    '            path.display(),\n'
    '            LIFECYCLE_PHASE_NAMES.join(", ")\n'
    '        );\n'
    '    }\n'
    '    Ok(document\n'
    '        .lifecycle\n'
    '        .get(phase.as_str())\n'
    '        .cloned()\n'
    '        .map(HookValue::into_config))\n',
    "phase validation",
)
source = replace_once(
    source,
    '            let candidate = project.join(&relative);\n'
    '            if !candidate.exists() {\n'
    '                continue;\n'
    '            }\n'
    '            let metadata = fs::metadata(&candidate)\n'
    '                .with_context(|| format!("reading lifecycle hook {}", candidate.display()))?;\n'
    '            ensure!(\n'
    '                metadata.is_file(),\n'
    '                "lifecycle hook {} is not a regular file",\n'
    '                candidate.display()\n'
    '            );\n',
    '            let candidate = project.join(&relative);\n'
    '            let metadata = match fs::symlink_metadata(&candidate) {\n'
    '                Ok(metadata) => metadata,\n'
    '                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,\n'
    '                Err(error) => {\n'
    '                    return Err(error).with_context(|| {\n'
    '                        format!("reading lifecycle hook {}", candidate.display())\n'
    '                    });\n'
    '                }\n'
    '            };\n'
    '            ensure!(\n'
    '                !metadata.file_type().is_symlink(),\n'
    '                "lifecycle hook {} must not be a symbolic link",\n'
    '                candidate.display()\n'
    '            );\n'
    '            ensure!(\n'
    '                metadata.is_file(),\n'
    '                "lifecycle hook {} is not a regular file",\n'
    '                candidate.display()\n'
    '            );\n',
    "symlink rejection",
)
source = replace_once(
    source,
    '        .env("ZED_PROJECT_ROOT", root)\n'
    '        .env("ZED_PACKAGE_MANIFEST", root.join(MANIFEST_FILE))\n',
    '        .env("ZED_PROJECT_ROOT", root)\n'
    '        .env("ZED_PKG_ROOT", root)\n'
    '        .env("ZED_PACKAGE_MANIFEST", root.join(MANIFEST_FILE))\n',
    "package root environment",
)
source = replace_once(
    source,
    '        assert_eq!(fs::read_to_string(output).unwrap(), "post-pack");\n'
    '    }\n'
    '}\n',
    '        assert_eq!(fs::read_to_string(output).unwrap(), "post-pack");\n'
    '    }\n'
    '\n'
    '    #[test]\n'
    '    fn misspelled_lifecycle_phase_is_rejected() {\n'
    '        let project = tempfile::tempdir().unwrap();\n'
    '        write(\n'
    '            &project.path().join(MANIFEST_FILE),\n'
    '            "[lifecycle.pre-buid]\\ncommand = \\\"must-not-be-ignored\\\"\\n",\n'
    '        );\n'
    '        let error = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap_err();\n'
    '        assert!(error.to_string().contains("unknown lifecycle phase `pre-buid`"));\n'
    '    }\n'
    '\n'
    '    #[cfg(unix)]\n'
    '    #[test]\n'
    '    fn symbolic_link_hook_is_rejected_even_when_target_is_inside_project() {\n'
    '        use std::os::unix::fs::symlink;\n'
    '\n'
    '        let project = tempfile::tempdir().unwrap();\n'
    '        let target = project.path().join("real-pre-build");\n'
    '        write(&target, "exit 0\\n");\n'
    '        fs::create_dir_all(project.path().join(".zpkg")).unwrap();\n'
    '        symlink(&target, project.path().join(".zpkg/pre-build")).unwrap();\n'
    '\n'
    '        let error = resolve_hooks(project.path(), LifecyclePhase::PreBuild).unwrap_err();\n'
    '        assert!(error.to_string().contains("must not be a symbolic link"));\n'
    '    }\n'
    '}\n',
    "lifecycle tests",
)
LIFECYCLE.write_text(source, encoding="utf-8")

docs = DOCS.read_text(encoding="utf-8")
docs = replace_once(
    docs,
    "On Unix, executable hooks run directly; non-executable shell files run with `sh`. PowerShell and command files use their native interpreters. Zed rejects convention files that resolve outside the project root.\n",
    "On Unix, executable hooks run directly; non-executable shell files run with `sh`. PowerShell and command files use their native interpreters. Zed rejects symbolic-link hooks and convention files that resolve outside the project root.\n",
    "hook security documentation",
)
docs = replace_once(
    docs,
    "- `ZED_PROJECT_ROOT`\n",
    "- `ZED_PROJECT_ROOT` and its package-oriented alias `ZED_PKG_ROOT`\n",
    "environment documentation",
)
DOCS.write_text(docs, encoding="utf-8")
