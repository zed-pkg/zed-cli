#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import process from "node:process";
import { pathToFileURL } from "node:url";

const STYLE = `
:root{color-scheme:light dark;font-family:ui-sans-serif,system-ui,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif;line-height:1.5}
*{box-sizing:border-box}
body{margin:0;background:Canvas;color:CanvasText}
main{width:min(88rem,100%);margin:0 auto;padding:clamp(1rem,4vw,3rem)}
header,.panel{border:1px solid color-mix(in srgb,CanvasText 22%,transparent);border-radius:.9rem;padding:clamp(1rem,3vw,1.5rem);margin-block:1rem}
.eyebrow{margin:0;font-size:.78rem;font-weight:750;letter-spacing:.09em;text-transform:uppercase}
h1,h2{line-height:1.2;overflow-wrap:anywhere}
.provenance{display:grid;grid-template-columns:repeat(auto-fit,minmax(14rem,1fr));gap:.7rem;margin:1rem 0 0;padding:0;list-style:none}
.provenance li,.metric{border:1px solid color-mix(in srgb,CanvasText 18%,transparent);border-radius:.65rem;padding:.75rem;background:color-mix(in srgb,Canvas 96%,CanvasText 4%)}
.provenance strong,.metric strong{display:block;font-size:.78rem;letter-spacing:.06em;text-transform:uppercase}
.metrics{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:.8rem}
.metric span{font-size:clamp(1.6rem,5vw,2.5rem);font-weight:800}
label{display:block;font-weight:700;margin-bottom:.4rem}
input{width:100%;min-height:2.75rem;padding:.65rem .75rem;border:1px solid color-mix(in srgb,CanvasText 28%,transparent);border-radius:.55rem;background:Canvas;color:CanvasText;font:inherit}
input:focus-visible,a:focus-visible{outline:.2rem solid Highlight;outline-offset:.15rem}
.table-wrap{overflow-x:auto;border:1px solid color-mix(in srgb,CanvasText 20%,transparent);border-radius:.65rem}
table{width:100%;border-collapse:collapse;min-width:44rem}
th,td{text-align:left;vertical-align:top;padding:.7rem;border-bottom:1px solid color-mix(in srgb,CanvasText 14%,transparent)}
th{font-size:.78rem;letter-spacing:.05em;text-transform:uppercase;background:color-mix(in srgb,Canvas 92%,CanvasText 8%)}
tbody tr:last-child td{border-bottom:0}
code{overflow-wrap:anywhere;word-break:break-word}
[hidden]{display:none!important}
.empty{text-align:center;font-style:italic}
footer{padding:1rem 0;font-size:.9rem}
@media(max-width:42rem){.metrics{grid-template-columns:1fr}.panel,header{border-radius:.65rem}table{min-width:36rem}}
`;

const SCRIPT = `
(() => {
  "use strict";
  const input = document.querySelector("#artifact-filter");
  const status = document.querySelector("#filter-status");
  const rows = [...document.querySelectorAll("tbody tr[data-search]")];
  const apply = () => {
    const query = input.value.trim().toLocaleLowerCase();
    let visible = 0;
    for (const row of rows) {
      const matches = !query || row.dataset.search.includes(query);
      row.hidden = !matches;
      if (matches) visible += 1;
    }
    status.textContent = query
      ? visible + " of " + rows.length + " artifacts match “" + input.value.trim() + "”."
      : "Showing all " + rows.length + " artifacts.";
  };
  input.addEventListener("input", apply);
  apply();
})();
`;

function sha256(value) {
  return createHash("sha256").update(value, "utf8").digest("base64");
}

export function escapeHtml(value) {
  return String(value)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function assertString(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${label} must be a non-empty string`);
  }
  return value;
}

function assertOptionalString(value, label) {
  if (value !== null && value !== undefined && typeof value !== "string") {
    throw new TypeError(`${label} must be a string or null`);
  }
  return value ?? null;
}

function assertArray(value, label) {
  if (!Array.isArray(value)) {
    throw new TypeError(`${label} must be an array`);
  }
  return value;
}

export function validateReleasePlan(plan) {
  if (!plan || typeof plan !== "object" || Array.isArray(plan)) {
    throw new TypeError("release plan must be an object");
  }
  assertString(plan.release_set, "release_set");
  if (!plan.source || typeof plan.source !== "object" || Array.isArray(plan.source)) {
    throw new TypeError("source must be an object");
  }
  for (const field of ["package", "version", "vcs_tag", "repository"]) {
    assertString(plan.source[field], `source.${field}`);
  }

  const zed = assertArray(plan.zed, "zed").map((item, index) => ({
    target: assertOptionalString(item?.target, `zed[${index}].target`),
    package: assertString(item?.package, `zed[${index}].package`),
    version: assertString(item?.version, `zed[${index}].version`),
    dir: assertString(item?.dir, `zed[${index}].dir`),
  }));
  const native = assertArray(plan.native, "native").map((item, index) => ({
    target: assertString(item?.target, `native[${index}].target`),
    registry: assertString(item?.registry, `native[${index}].registry`),
    package: assertString(item?.package, `native[${index}].package`),
    version: assertString(item?.version, `native[${index}].version`),
    vcs_tag: assertString(item?.vcs_tag, `native[${index}].vcs_tag`),
    dir: assertString(item?.dir, `native[${index}].dir`),
  }));
  const forge = assertArray(plan.forge, "forge").map((item, index) => ({
    target: assertString(item?.target, `forge[${index}].target`),
    registry: assertString(item?.registry, `forge[${index}].registry`),
    format: assertString(item?.format, `forge[${index}].format`),
    package: assertString(item?.package, `forge[${index}].package`),
    version: assertString(item?.version, `forge[${index}].version`),
    vcs_tag: assertString(item?.vcs_tag, `forge[${index}].vcs_tag`),
    dir: assertString(item?.dir, `forge[${index}].dir`),
  }));

  return {
    release_set: plan.release_set,
    source: {
      package: plan.source.package,
      version: plan.source.version,
      vcs_tag: plan.source.vcs_tag,
      repository: plan.source.repository,
    },
    zed,
    native,
    forge,
  };
}

function searchValue(values) {
  return escapeHtml(values.join(" ").toLocaleLowerCase());
}

function row(cells, kind) {
  return `<tr data-kind="${escapeHtml(kind)}" data-search="${searchValue(cells)}">${cells
    .map((cell) => `<td><code>${escapeHtml(cell)}</code></td>`)
    .join("")}</tr>`;
}

function emptyRow(columns, label) {
  return `<tr class="empty"><td colspan="${columns}">${escapeHtml(label)}</td></tr>`;
}

function tableSection({ id, title, description, headers, rows, empty }) {
  const body = rows.length ? rows.join("") : emptyRow(headers.length, empty);
  return `<section class="panel" aria-labelledby="${id}-heading">
<h2 id="${id}-heading">${escapeHtml(title)}</h2>
<p>${escapeHtml(description)}</p>
<div class="table-wrap"><table data-kind-table="${escapeHtml(id)}">
<thead><tr>${headers.map((header) => `<th scope="col">${escapeHtml(header)}</th>`).join("")}</tr></thead>
<tbody>${body}</tbody>
</table></div>
</section>`;
}

export function renderReleasePlan(input) {
  const plan = validateReleasePlan(input);
  const counts = {
    zed: plan.zed.length,
    native: plan.native.length,
    forge: plan.forge.length,
  };
  const total = counts.zed + counts.native + counts.forge;

  const zedRows = plan.zed.map((artifact) =>
    row(
      [artifact.target ?? "repository", artifact.package, artifact.version, artifact.dir],
      "zed",
    ),
  );
  const nativeRows = plan.native.map((artifact) =>
    row(
      [
        artifact.target,
        artifact.registry,
        artifact.package,
        artifact.version,
        artifact.vcs_tag,
        artifact.dir,
      ],
      "native",
    ),
  );
  const forgeRows = plan.forge.map((artifact) =>
    row(
      [
        artifact.target,
        artifact.registry,
        artifact.format,
        artifact.package,
        artifact.version,
        artifact.vcs_tag,
        artifact.dir,
      ],
      "forge",
    ),
  );

  const styleHash = sha256(STYLE);
  const scriptHash = sha256(SCRIPT);
  const csp = [
    "default-src 'none'",
    `style-src 'sha256-${styleHash}'`,
    `script-src 'sha256-${scriptHash}'`,
    "img-src data:",
    "base-uri 'none'",
    "form-action 'none'",
  ].join("; ");

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="${escapeHtml(csp)}">
<title>${escapeHtml(plan.release_set)} — Zed release plan</title>
<style>${STYLE}</style>
</head>
<body>
<main>
<header>
<p class="eyebrow">Credential-free release review</p>
<h1>${escapeHtml(plan.release_set)}</h1>
<p>This offline report is rendered from <code>zed release plan --json</code>. It does not upload, publish, or read credentials.</p>
<ul class="provenance" aria-label="Source provenance">
<li><strong>Package</strong><code>${escapeHtml(plan.source.package)}</code></li>
<li><strong>Version</strong><code>${escapeHtml(plan.source.version)}</code></li>
<li><strong>VCS tag</strong><code>${escapeHtml(plan.source.vcs_tag)}</code></li>
<li><strong>Repository</strong><code>${escapeHtml(plan.source.repository)}</code></li>
</ul>
</header>
<section class="panel" aria-labelledby="summary-heading">
<h2 id="summary-heading">Release destinations</h2>
<div class="metrics">
<div class="metric" data-count-kind="zed"><strong>Zed artifacts</strong><span>${counts.zed}</span></div>
<div class="metric" data-count-kind="native"><strong>Native registries</strong><span>${counts.native}</span></div>
<div class="metric" data-count-kind="forge"><strong>Forge mirrors</strong><span>${counts.forge}</span></div>
</div>
<p><strong data-total-count>${total}</strong> total planned artifacts and mirrors.</p>
<label for="artifact-filter">Filter artifacts</label>
<input id="artifact-filter" type="search" autocomplete="off" placeholder="Search target, registry, package, tag, or directory">
<p id="filter-status" role="status" aria-live="polite">Showing all ${total} artifacts.</p>
</section>
${tableSection({
  id: "zed",
  title: "Zed artifacts",
  description: "Universal artifacts published to the Zed registry.",
  headers: ["Target", "Package", "Version", "Directory"],
  rows: zedRows,
  empty: "No Zed artifacts are planned.",
})}
${tableSection({
  id: "native",
  title: "Native registry artifacts",
  description: "Canonical ecosystem packages derived from the same source release.",
  headers: ["Target", "Registry", "Package", "Version", "VCS tag", "Directory"],
  rows: nativeRows,
  empty: "No native registry routes are declared.",
})}
${tableSection({
  id: "forge",
  title: "Forge package mirrors",
  description: "Additional forge-hosted package routes using the native package format.",
  headers: ["Target", "Forge", "Format", "Package", "Version", "VCS tag", "Directory"],
  rows: forgeRows,
  empty: "No forge package mirrors are declared.",
})}
<footer>Generated locally by <code>scripts/render-release-plan-html.mjs</code>.</footer>
</main>
<script>${SCRIPT}</script>
</body>
</html>`;
}

function parseArguments(argv) {
  const result = { input: "-", output: null };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--input") {
      result.input = argv[++index];
    } else if (argument === "--output") {
      result.output = argv[++index];
    } else if (argument === "--help" || argument === "-h") {
      return { help: true };
    } else {
      throw new Error(`unknown argument: ${argument}`);
    }
  }
  if (!result.output) {
    throw new Error("--output is required");
  }
  return result;
}

async function readInput(path) {
  if (path === "-") {
    const chunks = [];
    for await (const chunk of process.stdin) chunks.push(chunk);
    return Buffer.concat(chunks).toString("utf8");
  }
  return readFile(path, "utf8");
}

async function main() {
  const args = parseArguments(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(
      "usage: node scripts/render-release-plan-html.mjs [--input PLAN.json|-] --output REPORT.html\n",
    );
    return;
  }
  const source = await readInput(args.input);
  let plan;
  try {
    plan = JSON.parse(source);
  } catch (error) {
    throw new SyntaxError(`release plan input is not valid JSON: ${error.message}`);
  }
  await writeFile(args.output, renderReleasePlan(plan), { encoding: "utf8", flag: "w" });
  process.stdout.write(`wrote offline release report to ${args.output}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    process.stderr.write(`release-plan report failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
