import assert from "node:assert/strict";
import test from "node:test";

import { renderReleasePlan } from "../scripts/render-release-plan-html.mjs";

const fixture = {
  release_set: "acme/browser-report@1.2.3#v1.2.3",
  source: {
    package: "acme/browser-report",
    version: "1.2.3",
    vcs_tag: "v1.2.3",
    repository: "https://github.com/acme/browser-report",
  },
  zed: [
    { target: "nodejs", package: "acme/browser-report-node", version: "1.2.3", dir: "node" },
  ],
  native: [
    {
      target: "nodejs",
      registry: "npm",
      package: "@acme/browser-report",
      version: "1.2.3",
      vcs_tag: "v1.2.3",
      dir: "node",
    },
  ],
  forge: [],
};

test("report includes progressive-enhancement accessibility structure", () => {
  const html = renderReleasePlan(fixture);
  assert.match(html, /<html lang="en" class="no-js">/);
  assert.match(html, /<a class="skip-link" href="#release-report">Skip to release report<\/a>/);
  assert.match(html, /<main id="release-report" tabindex="-1">/);
  assert.match(html, /aria-describedby="filter-help filter-status"/);
  assert.match(html, /<noscript><p class="panel">JavaScript is disabled; all release artifacts remain visible\.<\/p><\/noscript>/);
  assert.match(html, /document\.documentElement\.classList\.remove\("no-js"\)/);
  assert.match(html, /input\.disabled = false/);
});

test("each artifact table has a descriptive caption", () => {
  const html = renderReleasePlan(fixture);
  const captions = [...html.matchAll(/<caption class="visually-hidden">([^<]+)<\/caption>/g)].map(
    (match) => match[1],
  );
  assert.deepEqual(captions, [
    "Zed artifacts. Universal artifacts published to the Zed registry.",
    "Native registry artifacts. Canonical ecosystem packages derived from the same source release.",
    "Forge package mirrors. Additional forge-hosted package routes using the native package format.",
  ]);
});

test("print rules restore filtered rows and remove interactive controls", () => {
  const html = renderReleasePlan(fixture);
  assert.match(html, /@page\{margin:12mm\}/);
  assert.match(html, /@media print\{/);
  assert.match(html, /\.skip-link,\.filter-controls,noscript\{display:none!important\}/);
  assert.match(html, /\[hidden\]\{display:table-row!important\}/);
  assert.match(html, /thead\{display:table-header-group\}/);
  assert.match(html, /tr,\.provenance li,\.metric\{break-inside:avoid\}/);
});

test("forced-colors styling preserves visible boundaries", () => {
  const html = renderReleasePlan(fixture);
  assert.match(html, /@media\(forced-colors:active\)/);
  assert.match(html, /border-color:CanvasText/);
});
