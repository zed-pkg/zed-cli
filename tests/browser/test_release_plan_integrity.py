#!/usr/bin/env python3
from __future__ import annotations

import json
import os
from pathlib import Path

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[2]
REPORT = Path(os.environ.get("ZED_RELEASE_REPORT", ROOT / "build/release-plan.html")).resolve()
INTEGRITY = Path(
    os.environ.get("ZED_RELEASE_INTEGRITY", ROOT / "build/release-plan.integrity.json")
).resolve()
RESULTS = ROOT / "build" / "release-plan-browser-results"


def main() -> None:
    engine = os.environ.get("PLAYWRIGHT_BROWSER", "chromium")
    if engine not in {"chromium", "firefox", "webkit"}:
        raise SystemExit(f"unsupported PLAYWRIGHT_BROWSER: {engine}")
    if not REPORT.is_file() or not INTEGRITY.is_file():
        raise SystemExit("release report or integrity manifest is missing")

    manifest = json.loads(INTEGRITY.read_text(encoding="utf8"))
    expected = manifest["plan"]["canonical_sha256"]
    assert len(expected) == 64

    RESULTS.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = getattr(playwright, engine).launch()
        context = browser.new_context(viewport={"width": 1280, "height": 900})
        context.tracing.start(screenshots=True, snapshots=True, sources=True)
        page = context.new_page()
        errors: list[str] = []
        external_requests: list[str] = []
        page.on(
            "console",
            lambda message: errors.append(message.text)
            if message.type == "error"
            else None,
        )
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.on(
            "request",
            lambda request: external_requests.append(request.url)
            if request.url.startswith(("http://", "https://"))
            else None,
        )

        try:
            page.goto(REPORT.as_uri(), wait_until="load")
            meta = page.locator('meta[name="zed-release-plan-sha256"]')
            assert meta.get_attribute("content") == expected
            visible = page.locator("[data-plan-sha256] code")
            assert visible.inner_text() == expected
            visible.select_text()
            assert expected in page.evaluate("window.getSelection().toString()")

            page.emulate_media(media="print")
            assert visible.is_visible()
            page.emulate_media(media="screen")

            no_script_context = browser.new_context(java_script_enabled=False)
            no_script_page = no_script_context.new_page()
            no_script_page.goto(REPORT.as_uri(), wait_until="load")
            assert no_script_page.locator("[data-plan-sha256] code").inner_text() == expected
            assert no_script_page.locator("tbody tr[data-search]").count() == 7
            no_script_context.close()

            assert not external_requests, external_requests
            assert not errors, errors
            context.tracing.stop()
        except BaseException:
            page.screenshot(
                path=RESULTS / f"{engine}-integrity-failure.png",
                full_page=True,
            )
            context.tracing.stop(
                path=RESULTS / f"{engine}-integrity-trace.zip"
            )
            raise
        finally:
            context.close()
            browser.close()

    print(f"zed release-plan integrity browser contract passed in {engine}")


if __name__ == "__main__":
    main()
