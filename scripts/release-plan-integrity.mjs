#!/usr/bin/env node
import { createHash, randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  open,
  readFile,
  realpath,
  rename,
  rm,
  stat,
} from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import process from "node:process";
import { pathToFileURL } from "node:url";

import { validateReleasePlan } from "./render-release-plan-html.mjs";

export const INTEGRITY_SCHEMA = "zed-release-plan-report-integrity/v1";
const DIGEST = /^[0-9a-f]{64}$/;

function assertString(value, label) {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${label} must be a non-empty string`);
  }
  return value;
}

function assertDigest(value, label) {
  if (typeof value !== "string" || !DIGEST.test(value)) {
    throw new TypeError(`${label} must be a lowercase SHA-256 hex digest`);
  }
  return value;
}

export function sha256Hex(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function canonicalReleasePlanJson(input) {
  return `${JSON.stringify(validateReleasePlan(input))}\n`;
}

export function releasePlanDigest(input) {
  return sha256Hex(canonicalReleasePlanJson(input));
}

export function injectPlanDigest(html, digest) {
  assertString(html, "release report HTML");
  assertDigest(digest, "plan digest");
  if (
    html.includes('name="zed-release-plan-sha256"') ||
    html.includes("data-plan-sha256")
  ) {
    throw new Error("release report already contains plan integrity metadata");
  }

  const titleAnchor = "<title>";
  const provenanceAnchor = "</ul>\n</header>";
  if (!html.includes(titleAnchor) || !html.includes(provenanceAnchor)) {
    throw new Error("release report is missing required integrity anchors");
  }

  const withMeta = html.replace(
    titleAnchor,
    `<meta name="zed-release-plan-sha256" content="${digest}">\n${titleAnchor}`,
  );
  return withMeta.replace(
    provenanceAnchor,
    `<li data-plan-sha256><strong>Plan SHA-256</strong><code>${digest}</code></li>\n${provenanceAnchor}`,
  );
}

export function extractEmbeddedPlanDigest(html) {
  assertString(html, "release report HTML");
  const meta = html.match(
    /<meta name="zed-release-plan-sha256" content="([0-9a-f]{64})">/,
  )?.[1];
  const visible = html.match(
    /<li data-plan-sha256><strong>Plan SHA-256<\/strong><code>([0-9a-f]{64})<\/code><\/li>/,
  )?.[1];
  if (!meta || !visible || meta !== visible) {
    throw new Error("release report plan digest metadata is missing or inconsistent");
  }
  return meta;
}

export function assertDistinctArtifactPaths({ planPath, reportPath, integrityPath }) {
  const resolved = [
    resolve(assertString(planPath, "plan path")),
    resolve(assertString(reportPath, "report path")),
    resolve(assertString(integrityPath, "integrity path")),
  ];
  if (new Set(resolved).size !== resolved.length) {
    throw new Error("release plan, report, and integrity paths must be distinct");
  }
  return {
    planPath: resolved[0],
    reportPath: resolved[1],
    integrityPath: resolved[2],
  };
}

async function existingArtifactIdentity(path) {
  try {
    const [canonical, metadata] = await Promise.all([realpath(path), stat(path)]);
    return {
      canonical: process.platform === "win32" ? canonical.toLowerCase() : canonical,
      inode:
        Number.isSafeInteger(metadata.dev) &&
        Number.isSafeInteger(metadata.ino) &&
        metadata.ino !== 0
          ? `${metadata.dev}:${metadata.ino}`
          : null,
    };
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}

export async function assertDistinctArtifactFiles({
  planPath,
  reportPath,
  integrityPath,
}) {
  const artifacts = [
    ["release plan", planPath],
    ["release report", reportPath],
    ["release report integrity manifest", integrityPath],
  ];
  const canonicalPaths = new Map();
  const inodes = new Map();

  for (const [label, path] of artifacts) {
    const identity = await existingArtifactIdentity(path);
    if (!identity) continue;

    const canonicalAlias = canonicalPaths.get(identity.canonical);
    if (canonicalAlias) {
      throw new Error(`${canonicalAlias} and ${label} must be distinct files`);
    }
    canonicalPaths.set(identity.canonical, label);

    if (identity.inode) {
      const inodeAlias = inodes.get(identity.inode);
      if (inodeAlias) {
        throw new Error(`${inodeAlias} and ${label} must be distinct files`);
      }
      inodes.set(identity.inode, label);
    }
  }

  return { planPath, reportPath, integrityPath };
}

async function metadataOrNull(path) {
  try {
    return await lstat(path);
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
}

export async function assertSafeIntegrityOutputPath(outputPath, label) {
  assertString(outputPath, `${label} path`);
  const destination = resolve(outputPath);
  const existing = await metadataOrNull(destination);
  if (existing?.isSymbolicLink()) {
    throw new Error(`refusing to write ${label} through symbolic link: ${destination}`);
  }
  if (existing?.isDirectory()) {
    throw new Error(`${label} output is a directory: ${destination}`);
  }
  return destination;
}

export async function writeAtomicIntegrityOutput(outputPath, content, label) {
  assertString(content, label);
  const destination = await assertSafeIntegrityOutputPath(outputPath, label);
  const parent = dirname(destination);
  await mkdir(parent, { recursive: true });
  const temporary = resolve(
    parent,
    `.${basename(destination)}.${process.pid}.${randomUUID()}.tmp`,
  );
  let handle;
  try {
    handle = await open(temporary, "wx", 0o600);
    await handle.writeFile(content, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;

    const beforePublish = await metadataOrNull(destination);
    if (beforePublish?.isSymbolicLink()) {
      throw new Error(`refusing to replace symbolic-link ${label}: ${destination}`);
    }
    await rename(temporary, destination);
  } finally {
    if (handle) await handle.close().catch(() => {});
    await rm(temporary, { force: true }).catch(() => {});
  }
  return destination;
}

export function buildIntegrityManifest({ planPath, reportPath, planDigest, reportDigest }) {
  assertDigest(planDigest, "manifest plan digest");
  assertDigest(reportDigest, "manifest report digest");
  return `${JSON.stringify(
    {
      schema: INTEGRITY_SCHEMA,
      algorithm: "sha256",
      plan: {
        file: basename(assertString(planPath, "plan path")),
        canonical_sha256: planDigest,
      },
      report: {
        file: basename(assertString(reportPath, "report path")),
        sha256: reportDigest,
      },
    },
    null,
    2,
  )}\n`;
}

function parsePlan(source, label) {
  try {
    return JSON.parse(source);
  } catch (error) {
    throw new SyntaxError(`${label} is not valid JSON: ${error.message}`);
  }
}

export function validateIntegrityManifest(input, planPath, reportPath) {
  if (!input || typeof input !== "object" || Array.isArray(input)) {
    throw new TypeError("integrity manifest must be an object");
  }
  if (input.schema !== INTEGRITY_SCHEMA || input.algorithm !== "sha256") {
    throw new Error("unsupported release report integrity manifest schema");
  }
  const expectedTop = ["algorithm", "plan", "report", "schema"];
  if (Object.keys(input).sort().join("\0") !== expectedTop.join("\0")) {
    throw new Error("integrity manifest contains unknown or missing top-level fields");
  }
  if (
    !input.plan ||
    typeof input.plan !== "object" ||
    Array.isArray(input.plan) ||
    !input.report ||
    typeof input.report !== "object" ||
    Array.isArray(input.report)
  ) {
    throw new TypeError("integrity manifest plan and report entries must be objects");
  }
  if (
    Object.keys(input.plan).sort().join("\0") !==
      ["canonical_sha256", "file"].join("\0") ||
    Object.keys(input.report).sort().join("\0") !== ["file", "sha256"].join("\0")
  ) {
    throw new Error("integrity manifest contains unknown or missing artifact fields");
  }
  if (input.plan.file !== basename(planPath) || input.report.file !== basename(reportPath)) {
    throw new Error("integrity manifest artifact filenames do not match the supplied files");
  }
  assertDigest(input.plan.canonical_sha256, "integrity manifest plan digest");
  assertDigest(input.report.sha256, "integrity manifest report digest");
  return input;
}

export async function bindReleasePlanIntegrity(paths) {
  const resolvedPaths = assertDistinctArtifactPaths(paths);
  const { planPath, reportPath, integrityPath } =
    await assertDistinctArtifactFiles(resolvedPaths);
  await Promise.all([
    assertSafeIntegrityOutputPath(reportPath, "release report"),
    assertSafeIntegrityOutputPath(integrityPath, "release report integrity manifest"),
  ]);
  const [planSource, reportSource] = await Promise.all([
    readFile(planPath, "utf8"),
    readFile(reportPath, "utf8"),
  ]);
  const plan = parsePlan(planSource, "release plan");
  const planDigest = releasePlanDigest(plan);
  const boundReport = injectPlanDigest(reportSource, planDigest);
  const reportDigest = sha256Hex(boundReport);
  const manifest = buildIntegrityManifest({
    planPath,
    reportPath,
    planDigest,
    reportDigest,
  });

  await writeAtomicIntegrityOutput(reportPath, boundReport, "release report");
  await writeAtomicIntegrityOutput(
    integrityPath,
    manifest,
    "release report integrity manifest",
  );
  return { planDigest, reportDigest, manifestPath: integrityPath };
}

export async function verifyReleasePlanIntegrity(paths) {
  const resolvedPaths = assertDistinctArtifactPaths(paths);
  const { planPath, reportPath, integrityPath } =
    await assertDistinctArtifactFiles(resolvedPaths);
  const [planSource, reportSource, integritySource] = await Promise.all([
    readFile(planPath, "utf8"),
    readFile(reportPath, "utf8"),
    readFile(integrityPath, "utf8"),
  ]);
  const plan = parsePlan(planSource, "release plan");
  const manifest = validateIntegrityManifest(
    parsePlan(integritySource, "integrity manifest"),
    planPath,
    reportPath,
  );
  const planDigest = releasePlanDigest(plan);
  const embeddedDigest = extractEmbeddedPlanDigest(reportSource);
  const reportDigest = sha256Hex(reportSource);

  if (manifest.plan.canonical_sha256 !== planDigest || embeddedDigest !== planDigest) {
    throw new Error("release plan digest does not match the report integrity metadata");
  }
  if (manifest.report.sha256 !== reportDigest) {
    throw new Error("release report digest does not match the integrity manifest");
  }
  return { planDigest, reportDigest };
}

function optionValue(argv, index, option) {
  if (index + 1 >= argv.length) throw new Error(`${option} requires a value`);
  return argv[index + 1];
}

export function parseIntegrityArguments(argv) {
  if (argv.length === 0 || argv[0] === "--help" || argv[0] === "-h") {
    return { help: true };
  }
  const mode = argv[0];
  if (mode !== "bind" && mode !== "verify") {
    throw new Error("first argument must be bind or verify");
  }
  const result = { mode, planPath: null, reportPath: null, integrityPath: null };
  for (let index = 1; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === "--plan") {
      result.planPath = optionValue(argv, index, argument);
      index += 1;
    } else if (argument === "--report") {
      result.reportPath = optionValue(argv, index, argument);
      index += 1;
    } else if (argument === "--integrity") {
      result.integrityPath = optionValue(argv, index, argument);
      index += 1;
    } else {
      throw new Error(`unknown argument: ${argument}`);
    }
  }
  for (const [field, option] of [
    ["planPath", "--plan"],
    ["reportPath", "--report"],
    ["integrityPath", "--integrity"],
  ]) {
    if (!result[field]) throw new Error(`${option} is required`);
  }
  return result;
}

async function main() {
  const args = parseIntegrityArguments(process.argv.slice(2));
  if (args.help) {
    process.stdout.write(
      "usage: node scripts/release-plan-integrity.mjs <bind|verify> --plan PLAN.json --report REPORT.html --integrity INTEGRITY.json\n",
    );
    return;
  }
  const operation = args.mode === "bind" ? bindReleasePlanIntegrity : verifyReleasePlanIntegrity;
  const result = await operation(args);
  process.stdout.write(
    `${args.mode === "bind" ? "bound" : "verified"} release report plan=${result.planDigest} report=${result.reportDigest}\n`,
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    process.stderr.write(`release-plan integrity failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
