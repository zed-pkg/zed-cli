import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  escapeHtml,
  parseArguments,
  renderReleasePlan,
  validateReleasePlan,
  writeReleasePlanReport,
} from "../scripts/render-release-plan-html.mjs";

const fixture = {
  release_set: "acme/clients@1.2.3#v1.2.3",
  source: {
    package: "acme/clients",
    version: "1.2.3",
    vcs_tag: "v1.2.3",
    repository: "https://github.com/acme/clients",
  },
  zed: [
    { target: "nodejs", package: "acme/clients-node", version: "1.2.3", dir: "node" },
    { target: "rust", package: "acme/clients-rust", version: "1.2.3", dir: "rust" },
  ],
  native: [
    {
      target: "nodejs",
      registry: "npm",
      package: "@acme/client",
      version: "1.2.3",
      vcs_tag: "v1.2.3",
      dir: "node",
    },
  ],
  forge: [
    {
      target: "nodejs",
      registry: "github-packages",
      format: "npm",
      package: "@acme/client",
      version: "1.2.3",
      vcs_tag: "v1.2.3",
      dir: "node",
    },
  ],
};

test("rendering is deterministic and includes exact counts", () => {
  const first = renderReleasePlan(fixture);
  const second = renderReleasePlan(structuredClone(fixture));
  assert.equal(first, second);
  assert.match(first, /data-count-kind="zed"[^>]*><strong>Zed artifacts<\/strong><span>2<\/span>/);
  assert.match(first, /data-count-kind="native"[^>]*><strong>Native registries<\/strong><span>1<\/span>/);
  assert.match(first, /data-count-kind="forge"[^>]*><strong>Forge mirrors<\/strong><span>1<\/span>/);
  assert.match(first, /<strong data-total-count>4<\/strong>/);
  assert.match(first, /script-src &#39;sha256-[A-Za-z0-9+/=]+&#39;/);
  assert.doesNotMatch(first, /unsafe-inline|https:\/\/fonts|<script src=/);
});

test("all plan-derived strings are escaped before insertion", () => {
  const hostile = structuredClone(fixture);
  hostile.release_set = '<img src=x onerror="globalThis.pwned=1">';
  hostile.source.repository = "javascript:alert(1)&<script>bad()</script>";
  hostile.native[0].package = "<svg/onload=alert(1)>";
  const html = renderReleasePlan(hostile);
  assert.doesNotMatch(html, /<img src=x|<svg\/onload|<script>bad/);
  assert.match(html, /&lt;img src=x onerror=&quot;globalThis\.pwned=1&quot;&gt;/);
  assert.match(html, /javascript:alert\(1\)&amp;&lt;script&gt;bad\(\)&lt;\/script&gt;/);
  assert.equal(escapeHtml("<&\"'"), "&lt;&amp;&quot;&#39;");
});

test("empty native and forge routes render explicit empty states", () => {
  const empty = structuredClone(fixture);
  empty.native = [];
  empty.forge = [];
  const html = renderReleasePlan(empty);
  assert.match(html, /No native registry routes are declared\./);
  assert.match(html, /No forge package mirrors are declared\./);
  assert.match(html, /<strong data-total-count>2<\/strong>/);
});

test("schema validation fails closed", () => {
  assert.throws(() => validateReleasePlan(null), /release plan must be an object/);
  assert.throws(
    () => validateReleasePlan({ ...fixture, native: {} }),
    /native must be an array/,
  );
  const missing = structuredClone(fixture);
  delete missing.source.repository;
  assert.throws(() => validateReleasePlan(missing), /source\.repository/);
});

test("option parsing rejects missing values before filesystem access", () => {
  assert.throws(() => parseArguments(["--input"]), /--input requires a value/);
  assert.throws(() => parseArguments(["--output"]), /--output requires a value/);
  assert.throws(() => parseArguments(["--wat"]), /unknown argument: --wat/);
  assert.deepEqual(parseArguments(["--input", "-", "--output", "report.html"]), {
    input: "-",
    output: "report.html",
  });
});

test("report publication creates parents and atomically replaces a regular file", async () => {
  const root = await mkdtemp(join(tmpdir(), "zed-release-report-"));
  try {
    const output = join(root, "nested", "release-plan.html");
    await writeReleasePlanReport(output, "first");
    assert.equal(await readFile(output, "utf8"), "first");
    await writeReleasePlanReport(output, "second");
    assert.equal(await readFile(output, "utf8"), "second");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test(
  "report publication refuses an existing symbolic-link output",
  { skip: process.platform === "win32" },
  async () => {
    const root = await mkdtemp(join(tmpdir(), "zed-release-report-link-"));
    try {
      const protectedFile = join(root, "protected.txt");
      const output = join(root, "release-plan.html");
      await writeFile(protectedFile, "do not replace", "utf8");
      await symlink(protectedFile, output);
      await assert.rejects(
        writeReleasePlanReport(output, "hostile overwrite"),
        /symbolic link/,
      );
      assert.equal(await readFile(protectedFile, "utf8"), "do not replace");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
);
