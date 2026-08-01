# Offline release-plan report

`zed release plan --json` is the authoritative, credential-free release model. The repository-owned renderer turns that exact JSON into a self-contained HTML review artifact without changing release planning or publishing behavior.

```sh
zed release plan --json > build/release-plan.json
node scripts/render-release-plan-html.mjs \
  --input build/release-plan.json \
  --output build/release-plan.html
```

The renderer also accepts JSON on standard input:

```sh
zed release plan --json | \
  node scripts/render-release-plan-html.mjs --output build/release-plan.html
```

Open `build/release-plan.html` directly from disk. The report includes source provenance, exact Zed/native/forge counts, artifact tables, empty states, and client-side filtering.

## Security properties

- All plan-derived values are HTML-escaped before insertion into text or attributes.
- The file contains no remote scripts, styles, fonts, images, analytics, or network calls.
- Inline style and filter code are authorized by exact SHA-256 Content Security Policy hashes; broad `unsafe-inline` execution is not used.
- The renderer validates the expected `ReleasePlan` shape and fails closed on missing or malformed fields.
- The report is read-only. It does not load credentials, contact registries, or publish artifacts.

## Validation

`tests/release_plan_report_test.mjs` checks determinism, schema validation, hostile-value escaping, counts, empty states, and CSP construction. `tests/browser/test_release_plan_report.py` generates a real plan through the Rust CLI and verifies the rendered artifact in Chromium, including filtering, keyboard focus, narrow layout, console/page errors, and external network requests.
