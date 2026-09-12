import { expect, test } from "@playwright/test";
import { cp, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { promisify } from "node:util";
import { execFile } from "node:child_process";

const execFileAsync = promisify(execFile);
const here = dirname(fileURLToPath(import.meta.url));
const repository = resolve(here, "../../..");
const fixture = join(repository, "tests/fixtures/release-plan-browser");
const zed = process.env.ZED_BIN;

if (!zed) {
  throw new Error("ZED_BIN must point to the compiled zed executable");
}

async function generateReport() {
  const root = await mkdtemp(join(tmpdir(), "zed-release-plan-browser-"));
  const project = join(root, "project");
  const report = join(root, "release-plan.html");
  await cp(fixture, project, { recursive: true });
  const { stdout, stderr } = await execFileAsync(zed, ["release", "plan", "--html", report], {
    cwd: project,
    env: { ...process.env, ZED_PKG_RELEASE_JSON: "false" },
  });
  expect(stderr).toBe("");
  expect(stdout).toContain(report);
  return { root, project, report };
}

async function openReport(page) {
  const generated = await generateReport();
  const consoleErrors = [];
  const pageErrors = [];
  const externalRequests = [];
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  page.on("pageerror", (error) => pageErrors.push(error.message));
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.protocol !== "file:") externalRequests.push(request.url());
  });
  await page.goto(pathToFileURL(generated.report).href);
  return { ...generated, consoleErrors, pageErrors, externalRequests };
}

test("renders the existing release plan as an accessible offline report", async ({ page }) => {
  const generated = await openReport(page);
  try {
    await expect(page).toHaveTitle("acme/browser-release 2.4.0 release plan");
    await expect(page.getByRole("heading", { name: "Release plan" })).toBeVisible();
    await expect(page.getByText("acme/browser-release@2.4.0#v2.4.0", { exact: true })).toBeVisible();
    await expect(page.getByRole("link", { name: "https://github.com/acme/browser-release" })).toHaveAttribute(
      "href",
      "https://github.com/acme/browser-release",
    );
    await expect(page.locator('[data-count="zed"]')).toHaveText("2");
    await expect(page.locator('[data-count="native"]')).toHaveText("2");
    await expect(page.locator('[data-count="forge"]')).toHaveText("1");
    await expect(page.getByRole("table", { name: "Zed artifacts" })).toBeVisible();
    await expect(page.getByRole("table", { name: "Native registry artifacts" })).toBeVisible();
    await expect(page.getByRole("table", { name: "Forge package mirrors" })).toBeVisible();
    expect(generated.consoleErrors).toEqual([]);
    expect(generated.pageErrors).toEqual([]);
    expect(generated.externalRequests).toEqual([]);
  } finally {
    await rm(generated.root, { recursive: true, force: true });
  }
});

test("filters all artifact tables and Escape restores the complete plan", async ({ page }) => {
  const generated = await openReport(page);
  try {
    const filter = page.getByRole("searchbox", { name: "Filter release artifacts" });
    await filter.fill("npm");
    await expect(page.locator('[data-artifact-row]:visible')).toHaveCount(2);
    await expect(page.locator("#filter-status")).toContainText("2 of 5 artifacts");
    await page.keyboard.press("Escape");
    await expect(filter).toHaveValue("");
    await expect(page.locator('[data-artifact-row]:visible')).toHaveCount(5);
    await expect(page.locator("#filter-status")).toContainText("5 of 5 artifacts");
    expect(generated.consoleErrors).toEqual([]);
    expect(generated.pageErrors).toEqual([]);
    expect(generated.externalRequests).toEqual([]);
  } finally {
    await rm(generated.root, { recursive: true, force: true });
  }
});

test("supports keyboard review and a narrow mobile viewport without overflow", async ({ page }) => {
  await page.setViewportSize({ width: 360, height: 740 });
  const generated = await openReport(page);
  try {
    await page.getByRole("searchbox", { name: "Filter release artifacts" }).focus();
    await page.keyboard.type("rust");
    await expect(page.locator('[data-artifact-row]:visible')).toHaveCount(2);
    const widths = await page.evaluate(() => ({
      document: document.documentElement.scrollWidth,
      viewport: document.documentElement.clientWidth,
    }));
    expect(widths.document).toBeLessThanOrEqual(widths.viewport);
    await expect(page.getByRole("main")).toBeVisible();
    await expect(page.getByRole("contentinfo")).toBeVisible();
    expect(generated.consoleErrors).toEqual([]);
    expect(generated.pageErrors).toEqual([]);
    expect(generated.externalRequests).toEqual([]);
  } finally {
    await rm(generated.root, { recursive: true, force: true });
  }
});

test("the generated report is self-contained and carries a restrictive policy", async () => {
  const generated = await generateReport();
  try {
    const html = await readFile(generated.report, "utf8");
    expect(html).toContain("default-src 'none'");
    expect(html).not.toMatch(/<(?:script|link|img)[^>]+(?:src|href)=["']https?:/i);
    expect(html).not.toContain("eval(");
    expect(html).not.toContain("new Function");
    expect(basename(generated.report)).toBe("release-plan.html");
  } finally {
    await rm(generated.root, { recursive: true, force: true });
  }
});
