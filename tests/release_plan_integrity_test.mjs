import assert from "node:assert/strict";
import { mkdtemp, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { renderReleasePlan } from "../scripts/render-release-plan-html.mjs";
import {
  bindReleasePlanIntegrity,
  canonicalReleasePlanJson,
  extractEmbeddedPlanDigest,
  parseIntegrityArguments,
  releasePlanDigest,
  verifyReleasePlanIntegrity,
} from "../scripts/release-plan-integrity.mjs";

const fixture = {
  release_set: "acme/clients@1.2.3#v1.2.3",
  source: {
    package: "acme/clients",
    version: "1.2.3",
    vcs_tag: "v1.2.3",
    repository: "https://github.com/acme/clients",
  },
  zed: [
    { target: "nodejs", package: "acme/node", version: "1.2.3", dir: "node" },
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

async function setup() {
  const root = await mkdtemp(join(tmpdir(), "zed-integrity-"));
  const planPath = join(root, "plan.json");
  const reportPath = join(root, "report.html");
  const integrityPath = join(root, "integrity.json");
  await writeFile(planPath, JSON.stringify(fixture, null, 2));
  await writeFile(reportPath, renderReleasePlan(fixture));
  return { root, planPath, reportPath, integrityPath };
}

test("canonical plan digest ignores insignificant key order and whitespace", () => {
  const reordered = {
    forge: fixture.forge,
    native: fixture.native,
    zed: fixture.zed,
    source: {
      repository: fixture.source.repository,
      vcs_tag: fixture.source.vcs_tag,
      version: fixture.source.version,
      package: fixture.source.package,
    },
    release_set: fixture.release_set,
  };
  assert.equal(canonicalReleasePlanJson(fixture), canonicalReleasePlanJson(reordered));
  assert.equal(releasePlanDigest(fixture), releasePlanDigest(reordered));
});

test("bind and verify publish a machine-readable and visible digest", async () => {
  const paths = await setup();
  try {
    const bound = await bindReleasePlanIntegrity(paths);
    const verified = await verifyReleasePlanIntegrity(paths);
    assert.deepEqual(verified, {
      planDigest: bound.planDigest,
      reportDigest: bound.reportDigest,
    });
    const report = await readFile(paths.reportPath, "utf8");
    assert.equal(extractEmbeddedPlanDigest(report), bound.planDigest);
    const manifest = JSON.parse(await readFile(paths.integrityPath, "utf8"));
    assert.equal(manifest.plan.canonical_sha256, bound.planDigest);
    assert.equal(manifest.report.sha256, bound.reportDigest);
  } finally {
    await rm(paths.root, { recursive: true, force: true });
  }
});

test("plan report and manifest tampering all fail verification", async () => {
  const paths = await setup();
  try {
    await bindReleasePlanIntegrity(paths);
    const originalPlan = await readFile(paths.planPath, "utf8");
    const originalReport = await readFile(paths.reportPath, "utf8");
    const originalManifest = await readFile(paths.integrityPath, "utf8");

    await writeFile(paths.planPath, originalPlan.replace("1.2.3", "1.2.4"));
    await assert.rejects(verifyReleasePlanIntegrity(paths), /plan digest/);
    await writeFile(paths.planPath, originalPlan);

    await writeFile(paths.reportPath, `${originalReport}\n<!-- tampered -->`);
    await assert.rejects(verifyReleasePlanIntegrity(paths), /report digest/);
    await writeFile(paths.reportPath, originalReport);

    const manifest = JSON.parse(originalManifest);
    manifest.report.sha256 = "x".repeat(64);
    await writeFile(paths.integrityPath, JSON.stringify(manifest));
    await assert.rejects(verifyReleasePlanIntegrity(paths), /lowercase SHA-256/);
  } finally {
    await rm(paths.root, { recursive: true, force: true });
  }
});

test("plan report and integrity paths must be distinct", async () => {
  const paths = await setup();
  try {
    const originalReport = await readFile(paths.reportPath, "utf8");
    await assert.rejects(
      bindReleasePlanIntegrity({
        planPath: paths.planPath,
        reportPath: paths.reportPath,
        integrityPath: paths.reportPath,
      }),
      /must be distinct/,
    );
    assert.equal(await readFile(paths.reportPath, "utf8"), originalReport);
  } finally {
    await rm(paths.root, { recursive: true, force: true });
  }
});

test(
  "integrity output symlinks are refused before report mutation",
  { skip: process.platform === "win32" },
  async () => {
    const paths = await setup();
    try {
      const protectedPath = join(paths.root, "protected.json");
      await writeFile(protectedPath, "protected");
      await symlink(protectedPath, paths.integrityPath);
      const originalReport = await readFile(paths.reportPath, "utf8");
      await assert.rejects(bindReleasePlanIntegrity(paths), /symbolic link/);
      assert.equal(await readFile(paths.reportPath, "utf8"), originalReport);
      assert.equal(await readFile(protectedPath, "utf8"), "protected");
      assert.equal(
        (await readdir(paths.root)).some((name) => name.endsWith(".tmp")),
        false,
      );
    } finally {
      await rm(paths.root, { recursive: true, force: true });
    }
  },
);

test("CLI parsing is strict", () => {
  assert.deepEqual(
    parseIntegrityArguments([
      "verify",
      "--plan",
      "p",
      "--report",
      "r",
      "--integrity",
      "i",
    ]),
    { mode: "verify", planPath: "p", reportPath: "r", integrityPath: "i" },
  );
  assert.throws(() => parseIntegrityArguments(["wat"]), /bind or verify/);
  assert.throws(() => parseIntegrityArguments(["bind", "--plan"]), /requires a value/);
  assert.throws(
    () => parseIntegrityArguments(["bind", "--wat", "x"]),
    /unknown argument/,
  );
  assert.throws(
    () => parseIntegrityArguments(["bind", "--plan", "p"]),
    /--report is required/,
  );
});
