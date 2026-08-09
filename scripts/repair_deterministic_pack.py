#!/usr/bin/env python3

from pathlib import Path

path = Path("src/pack.rs")
text = path.read_text()

old = '''    let mut included: Vec<PathBuf> = Vec::new();
    let mut excluded_count = 0usize;
    for entry in WalkDir::new(project)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(project).map(Path::to_path_buf) else {
            continue;
        };
        if always.is_match(&rel) || !excludes.is_match(&rel) {
            included.push(rel);
        } else {
            excluded_count += 1;
        }
    }
    included.sort();

    let out_dir = match out_dir {
        Some(d) => d.to_path_buf(),
        None => project.join(PACK_OUT_DIR),
    };'''

new = '''    let out_dir = match out_dir {
        Some(d) => d.to_path_buf(),
        None => project.join(PACK_OUT_DIR),
    };
    let output_relative = out_dir.strip_prefix(project).ok().map(Path::to_path_buf);

    let mut included: Vec<PathBuf> = Vec::new();
    let mut excluded_count = 0usize;
    for entry in WalkDir::new(project)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(project).map(Path::to_path_buf) else {
            continue;
        };
        if output_relative
            .as_ref()
            .is_some_and(|output| rel.starts_with(output))
        {
            excluded_count += 1;
            continue;
        }
        if always.is_match(&rel) || !excludes.is_match(&rel) {
            included.push(rel);
        } else {
            excluded_count += 1;
        }
    }
    included.sort();'''

if text.count(old) != 1:
    raise SystemExit(f"pack collection block drifted: found {text.count(old)} matches")
text = text.replace(old, new, 1)

regression = r'''

    #[test]
    fn consecutive_default_packs_are_identical_and_exclude_prior_outputs() {
        let project = tempfile::tempdir().unwrap();
        let source_manifest = r#"
[package]
org = "acme"
name = "deterministic"
version = "1.0.0"

[package.repository]
url = "https://github.com/acme/deterministic"
"#;
        fs::write(
            project.path().join(zed_interfaces::paths::MANIFEST_FILE),
            source_manifest,
        )
        .unwrap();
        fs::write(project.path().join("payload.txt"), "stable payload\n").unwrap();
        let manifest = Manifest::parse(source_manifest).unwrap();

        let first = pack(project.path(), &manifest, None).unwrap();
        let first_sha = first.sha256.clone();
        let first_size = first.size;
        let first_count = first.file_count;
        let first_files = archive_files(&first.path);
        assert!(first_files.contains("pkg/.zpkg.toml"));
        assert!(first_files.contains("pkg/payload.txt"));
        assert!(
            !first_files
                .iter()
                .any(|entry| entry.starts_with("pkg/.zed/pack/"))
        );

        let second = pack(project.path(), &manifest, None).unwrap();
        let second_files = archive_files(&second.path);
        assert_eq!(second.sha256, first_sha);
        assert_eq!(second.size, first_size);
        assert_eq!(second.file_count, first_count);
        assert!(
            !second_files
                .iter()
                .any(|entry| entry.starts_with("pkg/.zed/pack/"))
        );
    }
'''

if "consecutive_default_packs_are_identical_and_exclude_prior_outputs" in text:
    raise SystemExit("deterministic pack regression unexpectedly exists")
head, separator, tail = text.rpartition("\n}")
if not separator or tail:
    raise SystemExit("tests module closing brace drifted")
path.write_text(head + regression + "\n}")
