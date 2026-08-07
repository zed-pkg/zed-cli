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

## Integrity binding

A retained report can be bound to the exact canonical release plan and verified without publishing or contacting a registry:

```sh
node scripts/release-plan-integrity.mjs bind \
  --plan build/release-plan.json \
  --report build/release-plan.html \
  --integrity build/release-plan.integrity.json

node scripts/release-plan-integrity.mjs verify \
  --plan build/release-plan.json \
  --report build/release-plan.html \
  --integrity build/release-plan.integrity.json
```

The binder validates the plan through the same `ReleasePlan` schema used by the renderer, serializes that validated model with fixed field order, and computes a lowercase SHA-256 digest. Insignificant JSON whitespace and input-object key order therefore do not change the canonical plan digest; artifact-array order remains significant because it is part of the release model.

The plan digest is inserted into a machine-readable `<meta>` element and displayed in the source-provenance list. The final HTML is hashed after that insertion. A versioned `zed-release-plan-report-integrity/v1` manifest records the canonical plan digest, final report digest, and artifact basenames.

The report is atomically replaced first and the integrity manifest is atomically published last. The manifest's presence therefore represents a completed binding sequence. Both output paths are preflighted and rechecked for symbolic links and directories, and private same-directory temporary files are removed on success or failure.

Verification fails closed when:

- the plan, report, or integrity manifest is modified;
- embedded and manifest plan digests disagree;
- the manifest uses an unsupported schema or algorithm;
- digest text is not lowercase 64-character SHA-256 hex;
- artifact filenames do not match the supplied files;
- required or unknown manifest fields are present incorrectly.

The integrity manifest is not a digital signature and does not establish who approved a report. It provides deterministic tamper and mismatch detection for an artifact set; trusted provenance still comes from Git history, protected Actions, attestations, and the ordinary release approval process.

## Accessible review

- A visible-on-focus skip link moves keyboard users directly to the report.
- The filter is associated with instructions and its live result status.
- Every artifact table has a descriptive caption in addition to its visible section heading.
- Heading order and semantic main/section/table landmarks remain stable.
- With JavaScript disabled, filtering is hidden and every release artifact—including its plan digest—remains readable.
- Forced-colors styling retains visible borders and focus indication.

## Printing and PDF review

Use the browser's print command to archive or review the release plan. Print media intentionally hides the filter and skip link, restores every artifact row even when the screen view is filtered, repeats table headers, avoids splitting rows where practical, and retains provenance, the plan digest, and destination counts.

The generated report remains a single local HTML file. Printing does not fetch remote fonts, styles, scripts, images, or analytics.

## Security properties

- All plan-derived values are HTML-escaped before insertion into text or attributes.
- The file contains no remote scripts, styles, fonts, images, analytics, or network calls.
- Inline style and filter code are authorized by exact SHA-256 Content Security Policy hashes; broad `unsafe-inline` execution is not used.
- The renderer validates the expected `ReleasePlan` shape and fails closed on missing or malformed fields and missing command-option values.
- Reports and integrity manifests are staged in same-directory private temporary files and atomically renamed into place.
- Existing symbolic-link outputs are refused rather than followed.
- The report and integrity tools are read-only with respect to package registries. They do not load credentials, contact registries, or publish artifacts.

## Validation

`tests/release_plan_report_test.mjs` checks determinism, schema validation, hostile-value escaping, counts, empty states, CSP construction, strict argument handling, atomic replacement, and symbolic-link refusal. `tests/release_plan_accessibility_test.mjs` locks skip navigation, table captions, progressive enhancement, print restoration, and forced-colors rules. `tests/release_plan_integrity_test.mjs` covers canonical digest stability, binding and verification, plan/report/manifest tampering, strict manifest schema, symlink refusal before report mutation, temporary-file cleanup, and CLI parsing.

The GitHub Actions browser matrix generates a real release plan through the locked Rust CLI, renders it, binds and verifies the three-file artifact set, and exercises the report in Chromium, Firefox, and WebKit. Browser coverage includes displayed and machine-readable digest agreement, text selection, print visibility, no-JavaScript visibility, filtering and Escape reset, keyboard order, accessible names and captions, forced-colors behavior, narrow layouts, console/page errors, and external network requests. The plan JSON, HTML report, and integrity manifest are retained together as a per-engine artifact.
