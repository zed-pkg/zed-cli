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

Open `build/release-plan.html` directly from disk. The report includes source provenance, exact Zed/native/forge counts, artifact tables, explicit empty states, and progressive client-side filtering. Press `Escape` while the filter is focused to clear it and restore the complete plan.

## Accessible review

- A visible-on-focus skip link moves keyboard users directly to the report.
- The filter is associated with instructions and its live result status.
- Every artifact table has a descriptive caption in addition to its visible section heading.
- Heading order and semantic main/section/table landmarks remain stable.
- With JavaScript disabled, filtering is hidden and every release artifact remains readable.
- Forced-colors styling retains visible borders and focus indication.

## Printing and PDF review

Use the browser's print command to archive or review the release plan. Print media intentionally hides the filter and skip link, restores every artifact row even when the screen view is filtered, repeats table headers, avoids splitting rows where practical, and retains provenance plus destination counts.

The generated report remains a single local HTML file. Printing does not fetch remote fonts, styles, scripts, images, or analytics.

## Security properties

- All plan-derived values are HTML-escaped before insertion into text or attributes.
- The file contains no remote scripts, styles, fonts, images, analytics, or network calls.
- Inline style and filter code are authorized by exact SHA-256 Content Security Policy hashes; broad `unsafe-inline` execution is not used.
- The renderer validates the expected `ReleasePlan` shape and fails closed on missing or malformed fields and missing command-option values.
- Reports are staged in a same-directory private temporary file and atomically renamed into place.
- An existing symbolic-link output is refused rather than followed, so the renderer cannot overwrite the link target.
- The report is read-only. It does not load credentials, contact registries, or publish artifacts.

## Validation

`tests/release_plan_report_test.mjs` checks determinism, schema validation, hostile-value escaping, counts, empty states, CSP construction, strict argument handling, atomic replacement, and symbolic-link refusal. `tests/release_plan_accessibility_test.mjs` locks skip navigation, table captions, progressive enhancement, print restoration, and forced-colors rules.

The GitHub Actions browser matrix generates a real release plan through the locked Rust CLI and verifies the same offline file in Chromium, Firefox, and WebKit. It covers filtering and Escape reset, keyboard order, accessible names and captions, print media, no-JavaScript readability, forced-colors behavior, narrow layouts, console/page errors, and external network requests.
