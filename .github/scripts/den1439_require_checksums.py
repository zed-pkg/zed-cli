from pathlib import Path

path = Path("src/environment.rs")
text = path.read_text()
old = '''        for (platform, artifact) in &tool.artifacts {
            if artifact.checksum.is_none() && artifact.url.is_none() {
                bail!(
                    "mise lock `{lock_path}` has no checksum or URL for `{name}` on `{platform}`"
                );
            }
        }
'''
new = '''        for (platform, artifact) in &tool.artifacts {
            if artifact.checksum.is_none() {
                bail!(
                    "mise lock `{lock_path}` has no cryptographic checksum for `{name}` on `{platform}`; a URL alone is provenance, not immutable artifact identity"
                );
            }
        }
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"expected one frozen-artifact check, found {count}")
path.write_text(text.replace(old, new, 1))
