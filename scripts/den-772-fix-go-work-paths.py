#!/usr/bin/env python3
"""Apply the DEN-772 Go workspace path fix exactly once."""

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
    '''            Adapter::Go => {
                // A go.work `use` block is the only non-invasive way to add
                // modules to a Go build; editing go.mod `replace` lines would
                // mean rewriting a file the user owns.
                let mut doc = String::from("go 1.21\\n\\nuse (\\n\\t./\\n");
                for p in &rel {
                    doc.push_str(&format!("\\t./{p}\\n"));
                }
                doc.push_str(")\\n");
                let path = zed_dir.join("go.work");
''',
    '''            Adapter::Go => {
                // A go.work `use` block is the only non-invasive way to add
                // modules to a Go build; editing go.mod `replace` lines would
                // mean rewriting a file the user owns. Go resolves every path
                // relative to the go.work file itself, which lives in `.zed/`,
                // not relative to the process working directory.
                let mut work_paths: Vec<String> = std::iter::once(project)
                    .chain(paths.iter().map(PathBuf::as_path))
                    .map(|path| {
                        pathdiff_relative(&zed_dir, path)
                            .to_string_lossy()
                            .replace('\\\\', "/")
                    })
                    .collect();
                work_paths.sort();
                work_paths.dedup();
                let mut doc = String::from("go 1.21\\n\\nuse (\\n");
                for path in &work_paths {
                    doc.push_str(&format!("\\t{path}\\n"));
                }
                doc.push_str(")\\n");
                let path = zed_dir.join("go.work");
''',
)

replace_once(
    "src/ops.rs",
    '''    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
    '''    #[test]
    fn go_workspace_paths_are_relative_to_the_generated_file() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("consumer");
        let package = project.join("zed_modules/acme/tool");
        fs::create_dir_all(&package).unwrap();
        let roots = BTreeMap::from([(Adapter::Go, vec![package])]);

        write_toolchain_wiring(&project, &roots).unwrap();

        let document = fs::read_to_string(project.join(".zed/go.work")).unwrap();
        assert!(document.contains("\\t..\\n"), "{document}");
        assert!(
            document.contains("\\t../zed_modules/acme/tool\\n"),
            "{document}"
        );
        assert!(!document.contains("\\t./\\n"), "{document}");
        assert!(!document.contains("\\t./zed_modules"), "{document}");
    }

    #[test]
    fn lock_only_frozen_restore_skips_only_the_missing_manifest_comparison() {
''',
)

print("DEN-772 Go workspace paths fixed")
