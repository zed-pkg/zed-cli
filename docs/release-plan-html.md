# Offline release-plan reports

`zed release plan --html <PATH>` writes a self-contained browser report from the same `ReleasePlan` model used by the human and JSON outputs.

```bash
zed release plan --html ./artifacts/release-plan.html
```

The equivalent environment fallback is:

```bash
ZED_PKG_RELEASE_HTML=./artifacts/release-plan.html zed release plan
```

`--html` and `--json` are mutually exclusive. Human output remains the default.

## Review workflow

Open the resulting file directly in a browser. No web server is required.

The report includes:

- source package, version, repository, and VCS tag;
- coordinated Zed package artifacts;
- native registry artifacts;
- forge package mirrors;
- artifact counts and a keyboard-accessible filter across every table.

Press `Escape` while the filter is focused to clear it.

## Security properties

- The report contains no remote scripts, styles, fonts, images, analytics, or network requests.
- A restrictive Content Security Policy defaults every resource type to `none` and permits only nonce-bound embedded CSS and JavaScript.
- Every manifest-derived value is HTML-escaped before rendering.
- Repository links are emitted only for credential-free HTTP or HTTPS URLs. Other repository identifiers are displayed as inert text.
- The report is written through a same-directory temporary file and atomically persisted.
- An existing symbolic-link output path is refused rather than followed.
- Browser interactivity is limited to local table filtering; no credentials or environment values are embedded.

## Automation

The Playwright contract builds the real `zed` binary, generates a report from a coordinated npm/crates fixture, and opens it through `file://`. It verifies semantic tables and counts, filtering and Escape reset, keyboard use, responsive containment, console/page errors, the CSP, and the absence of external requests.

```bash
cd tests/browser/release-plan
npm ci
npx playwright install chromium
ZED_BIN=../../../target/debug/zed npm test
```
