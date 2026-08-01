from pathlib import Path

path = Path(__file__).resolve().parents[1] / "src/release.rs"
text = path.read_text()

unused = '''fn push_optional_cell(output: &mut String, value: Option<&str>, fallback: &str) {
    push_cell(output, value.unwrap_or(fallback));
}

'''
if text.count(unused) != 1:
    raise SystemExit("expected one unused helper block")
text = text.replace(unused, "", 1)

# The transformation script is stored as Python source, so its embedded Rust
# raw-string openers carry one protective backslash. Remove that source-level
# escape after materialization; the HTML itself must begin with a literal `>`.
raw_opener = 'output.push_str(r#"\\">'
if text.count(raw_opener) != 2:
    raise SystemExit(f"expected two escaped Rust raw-string openers, found {text.count(raw_opener)}")
text = text.replace(raw_opener, 'output.push_str(r#">')

path.write_text(text)
print("DEN-1301 generated Rust source normalized")
