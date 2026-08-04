#!/usr/bin/env python3
"""Apply the Windows child-current-directory correction and focused tests."""

from pathlib import Path

source = Path("src/dev.rs")
text = source.read_text(encoding="utf-8")

old_spawn = '''    let status = command
        .envs(environment)
        .current_dir(root)
        .status()
'''
new_spawn = '''    let current_dir = child_process_current_dir(root);
    let status = command
        .envs(environment)
        .current_dir(&current_dir)
        .status()
'''
if text.count(old_spawn) != 1:
    raise SystemExit("expected exactly one development-shell current_dir call")
text = text.replace(old_spawn, new_spawn, 1)

resolve_marker = "fn resolve_shell(explicit: Option<&Path>) -> PathBuf {\n"
helper = r'''fn child_process_current_dir(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
        return PathBuf::from(OsString::from_wide(
            &normalize_windows_child_current_dir(&wide),
        ));
    }

    #[cfg(not(windows))]
    {
        root.to_path_buf()
    }
}

fn normalize_windows_child_current_dir(wide: &[u16]) -> Vec<u16> {
    const SLASH: u16 = b'\\' as u16;
    const VERBATIM: &[u16] = &[SLASH, SLASH, b'?' as u16, SLASH];
    const VERBATIM_UNC: &[u16] = &[
        SLASH,
        SLASH,
        b'?' as u16,
        SLASH,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        SLASH,
    ];

    if wide.starts_with(VERBATIM_UNC) {
        let mut normalized = Vec::with_capacity(wide.len() - VERBATIM_UNC.len() + 2);
        normalized.extend_from_slice(&[SLASH, SLASH]);
        normalized.extend_from_slice(&wide[VERBATIM_UNC.len()..]);
        normalized
    } else if wide.starts_with(VERBATIM) {
        wide[VERBATIM.len()..].to_vec()
    } else {
        wide.to_vec()
    }
}

'''
if text.count(resolve_marker) != 1:
    raise SystemExit("expected exactly one resolve_shell marker")
text = text.replace(resolve_marker, helper + resolve_marker, 1)

test_marker = '''    #[test]
    fn routes_canonical_alias_and_help_spellings() {
'''
tests = r'''    fn utf16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn from_utf16(value: &[u16]) -> String {
        String::from_utf16(value).expect("valid UTF-16 fixture")
    }

    #[test]
    fn windows_child_cwd_strips_verbatim_disk_prefix_without_losing_unicode() {
        let input = utf16(r"\\?\C:\répo\工具");
        let normalized = normalize_windows_child_current_dir(&input);
        assert_eq!(from_utf16(&normalized), r"C:\répo\工具");
    }

    #[test]
    fn windows_child_cwd_converts_verbatim_unc_to_standard_unc() {
        let input = utf16(r"\\?\UNC\server\share\repo");
        let normalized = normalize_windows_child_current_dir(&input);
        assert_eq!(from_utf16(&normalized), r"\\server\share\repo");
    }

    #[test]
    fn windows_child_cwd_preserves_non_verbatim_paths() {
        for value in [r"C:\repo\nested", r"\\server\share\repo", r"\\.\PIPE\zed"] {
            let input = utf16(value);
            assert_eq!(normalize_windows_child_current_dir(&input), input);
        }
    }

'''
if text.count(test_marker) != 1:
    raise SystemExit("expected exactly one unit-test insertion marker")
text = text.replace(test_marker, tests + test_marker, 1)
source.write_text(text, encoding="utf-8")

native = Path("tests/develop_windows_profile_contract.rs")
lines = native.read_text(encoding="utf-8").splitlines(keepends=True)
matches = [
    index
    for index, line in enumerate(lines)
    if "project root does not own package.json" in line
]
if len(matches) != 1:
    raise SystemExit(
        f"expected exactly one Windows project-ownership assertion, found {len(matches)}"
    )
index = matches[0]
continuation = "\\" + "\n"
lines[index : index + 1] = [
    "         $actual = (Get-Item -LiteralPath '.').FullName; " + continuation,
    "         $expected = (Get-Item -LiteralPath $env:ZED_DEV_PROJECT_ROOT).FullName; "
    + continuation,
    "         if (-not [String]::Equals($actual, $expected, "
    "[StringComparison]::OrdinalIgnoreCase)) {{ throw 'project root mismatch' }}; "
    + continuation,
]
native.write_text("".join(lines), encoding="utf-8")

doc = Path("docs/powershell-command-mode.md")
text = doc.read_text(encoding="utf-8")
old_doc = '''Using project ownership rather than textual path equality is intentional on Windows: equivalent paths may be rendered with normal drive-letter syntax or the Win32 verbatim `\\\\?\\` prefix. The security assertion concerns the selected project and profile behavior, not one display spelling of the same directory.
'''
new_doc = '''`ZED_DEV_PROJECT_ROOT` retains the canonical filesystem identity used for project selection and evidence, which may include the Win32 verbatim `\\\\?\\` or `\\\\?\\UNC\\` prefix. Before launching a Windows child process, Zed converts only that prefix to the equivalent drive or UNC spelling accepted reliably as a process current directory. The child therefore starts at the selected project root while the managed environment keeps its canonical identity path. The conversion operates on UTF-16 code units and is lossless for Unicode paths.
'''
if text.count(old_doc) != 1:
    raise SystemExit("expected exactly one Windows path-identity documentation paragraph")
doc.write_text(text.replace(old_doc, new_doc, 1), encoding="utf-8")

Path("scripts/den-1634-finalize.py").unlink()
Path(".github/workflows/den-1634-finalize.yml").unlink()
