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
# raw-string openers carry one protective backslash. The CSS literal can use
# r#; the inline script contains selectors such as "#artifact-filter" and must
# therefore use r## so an embedded quote-plus-hash cannot close the literal.
raw_opener = 'output.push_str(r#"\\">'
if text.count(raw_opener) != 2:
    raise SystemExit(f"expected two escaped Rust raw-string openers, found {text.count(raw_opener)}")
text = text.replace(raw_opener, 'output.push_str(r#">', 1)
text = text.replace(raw_opener, 'output.push_str(r##">', 1)

script_close = '})();</script></body></html>"#);'
if text.count(script_close) != 1:
    raise SystemExit(f"expected one inline-script raw-string closer, found {text.count(script_close)}")
text = text.replace(script_close, '})();</script></body></html>"##);', 1)

path.write_text(text)
print("DEN-1301 generated Rust source normalized")
