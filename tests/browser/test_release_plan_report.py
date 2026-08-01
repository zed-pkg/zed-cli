#!/usr/bin/env python3
from __future__ import annotations

import os
from pathlib import Path

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[2]
REPORT = Path(os.environ.get("ZED_RELEASE_REPORT", ROOT / "build/release-plan.html")).resolve()
RESULTS = ROOT / "build" / "release-plan-browser-results"


def main() -> None:
    if not REPORT.is_file():
        raise SystemExit(f"release report does not exist: {REPORT}")

    RESULTS.mkdir(parents=True, exist_ok=True)
    console_errors: list[str] = []
    page_errors: list[str] = []
    external_requests: list[str] = []

    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        context = browser.new_context(viewport={"width": 1280, "height": 900})
        context.tracing.start(screenshots=True, snapshots=True, sources=True)
        page = context.new_page()
        page.on(
            "console",
            lambda message: console_errors.append(message.text)
            if message.type == "error"
            else None,
        )
        page.on("pageerror", lambda error: page_errors.append(str(error)))
        page.on(
            "request",
            lambda request: external_requests.append(request.url)
            if request.url.startswith(("http://", "https://"))
            else None,
        )

        try:
            page.goto(REPORT.as_uri(), wait_until="load")
            page.get_by_role("heading", name="acme/browser-report@1.2.3#v1.2.3").wait_for()

            assert page.locator("[data-count-kind='zed'] span").inner_text() == "2"
            assert page.locator("[data-count-kind='native'] span").inner_text() == "2"
            assert page.locator("[data-count-kind='forge'] span").inner_text() == "3"
            assert page.locator("[data-total-count]").inner_text() == "7"
            assert page.locator("tbody tr[data-search]").count() == 7

            filter_input = page.get_by_label("Filter artifacts")
            filter_input.fill("npm")
            assert page.locator("tbody tr[data-search]:visible").count() == 4
            assert "4 of 7 artifacts" in page.get_by_role("status").inner_text()

            filter_input.press("Control+A")
            filter_input.fill("crates-io")
            assert page.locator("tbody tr[data-search]:visible").count() == 1
            assert (
                page.locator("tbody tr[data-search]:visible").get_attribute("data-kind")
                == "native"
            )

            filter_input.fill("")
            filter_input.focus()
            assert page.evaluate("document.activeElement.id") == "artifact-filter"

            page.set_viewport_size({"width": 390, "height": 844})
            assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
            assert page.get_by_role("main").is_visible()
            assert page.get_by_role("heading", name="Forge package mirrors").is_visible()

            assert not external_requests, external_requests
            assert not console_errors, console_errors
            assert not page_errors, page_errors
            context.tracing.stop()
        except BaseException:
            page.screenshot(path=RESULTS / "failure.png", full_page=True)
            context.tracing.stop(path=RESULTS / "trace.zip")
            raise
        finally:
            context.close()
            browser.close()

    print("zed release-plan browser contract passed")


if __name__ == "__main__":
    main()
