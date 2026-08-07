#!/usr/bin/env python3
from __future__ import annotations

import os
from pathlib import Path

from playwright.sync_api import Browser, BrowserContext, Page, sync_playwright

ROOT = Path(__file__).resolve().parents[2]
REPORT = Path(os.environ.get("ZED_RELEASE_REPORT", ROOT / "build/release-plan.html")).resolve()
RESULTS = ROOT / "build" / "release-plan-browser-results"


def attach_guards(page: Page, external_requests: list[str], errors: list[str]) -> None:
    page.on(
        "console",
        lambda message: errors.append(f"console: {message.text}")
        if message.type == "error"
        else None,
    )
    page.on("pageerror", lambda error: errors.append(f"pageerror: {error}"))
    page.on(
        "request",
        lambda request: external_requests.append(request.url)
        if request.url.startswith(("http://", "https://"))
        else None,
    )


def exercise_interactive(context: BrowserContext, engine: str) -> None:
    context.tracing.start(screenshots=True, snapshots=True, sources=True)
    page = context.new_page()
    errors: list[str] = []
    external_requests: list[str] = []
    attach_guards(page, external_requests, errors)

    try:
        page.goto(REPORT.as_uri(), wait_until="load")
        page.get_by_role("heading", name="acme/browser-report@1.2.3#v1.2.3").wait_for()
        assert not page.locator("html").evaluate("element => element.classList.contains('no-js')")

        page.keyboard.press("Tab")
        assert page.evaluate("document.activeElement.classList.contains('skip-link')")
        assert page.locator(".skip-link").is_visible()
        page.keyboard.press("Tab")
        assert page.evaluate("document.activeElement.id") == "artifact-filter"

        assert page.locator("[data-count-kind='zed'] span").inner_text() == "2"
        assert page.locator("[data-count-kind='native'] span").inner_text() == "2"
        assert page.locator("[data-count-kind='forge'] span").inner_text() == "3"
        assert page.locator("[data-total-count]").inner_text() == "7"
        assert page.locator("tbody tr[data-search]").count() == 7
        assert page.locator("table caption").count() == 3
        captions = page.locator("table caption").all_inner_texts()
        assert captions[0].startswith("Zed artifacts.")
        assert captions[1].startswith("Native registry artifacts.")
        assert captions[2].startswith("Forge package mirrors.")
        assert page.locator("#artifact-filter").get_attribute("aria-describedby") == (
            "filter-help filter-status"
        )
        assert page.locator("h1,h2").evaluate_all(
            "elements => elements.map(element => element.tagName)"
        ) == ["H1", "H2", "H2", "H2", "H2"]

        filter_input = page.get_by_label("Filter artifacts")
        filter_input.fill("npm")
        assert page.locator("tbody tr[data-search]:visible").count() == 4
        assert "4 of 7 artifacts" in page.get_by_role("status").inner_text()

        page.emulate_media(media="print")
        assert page.locator(".filter-controls").evaluate(
            "element => getComputedStyle(element).display"
        ) == "none"
        assert page.locator("thead").first.evaluate(
            "element => getComputedStyle(element).display"
        ) == "table-header-group"
        assert set(
            page.locator("tbody tr[data-search]").evaluate_all(
                "elements => elements.map(element => getComputedStyle(element).display)"
            )
        ) == {"table-row"}
        page.emulate_media(media="screen")

        filter_input.press("Escape")
        assert filter_input.input_value() == ""
        assert page.locator("tbody tr[data-search]:visible").count() == 7
        assert page.get_by_role("status").inner_text() == "Showing all 7 artifacts."

        page.emulate_media(forced_colors="active")
        assert page.locator("header").evaluate(
            "element => getComputedStyle(element).borderTopStyle"
        ) != "none"
        page.emulate_media(forced_colors="none")

        page.set_viewport_size({"width": 390, "height": 844})
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        assert page.get_by_role("main").is_visible()
        assert page.get_by_role("heading", name="Forge package mirrors").is_visible()

        assert not external_requests, external_requests
        assert not errors, errors
        context.tracing.stop()
    except BaseException:
        RESULTS.mkdir(parents=True, exist_ok=True)
        page.screenshot(path=RESULTS / f"{engine}-interactive-failure.png", full_page=True)
        context.tracing.stop(path=RESULTS / f"{engine}-interactive-trace.zip")
        raise


def exercise_without_javascript(browser: Browser, engine: str) -> None:
    context = browser.new_context(
        java_script_enabled=False,
        viewport={"width": 390, "height": 844},
    )
    page = context.new_page()
    errors: list[str] = []
    external_requests: list[str] = []
    attach_guards(page, external_requests, errors)
    try:
        page.goto(REPORT.as_uri(), wait_until="load")
        assert page.locator("html").evaluate("element => element.classList.contains('no-js')")
        assert page.locator(".filter-controls").evaluate(
            "element => getComputedStyle(element).display"
        ) == "none"
        assert page.locator("noscript").is_visible()
        assert "all release artifacts remain visible" in page.locator("noscript").inner_text()
        assert page.locator("tbody tr[data-search]").count() == 7
        assert page.locator("tbody tr[data-search]:visible").count() == 7
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        assert not external_requests, external_requests
        assert not errors, errors
    except BaseException:
        RESULTS.mkdir(parents=True, exist_ok=True)
        page.screenshot(path=RESULTS / f"{engine}-no-js-failure.png", full_page=True)
        raise
    finally:
        context.close()


def main() -> None:
    if not REPORT.is_file():
        raise SystemExit(f"release report does not exist: {REPORT}")
    engine = os.environ.get("PLAYWRIGHT_BROWSER", "chromium")
    if engine not in {"chromium", "firefox", "webkit"}:
        raise SystemExit(f"unsupported PLAYWRIGHT_BROWSER: {engine}")

    RESULTS.mkdir(parents=True, exist_ok=True)
    with sync_playwright() as playwright:
        browser = getattr(playwright, engine).launch()
        try:
            context = browser.new_context(viewport={"width": 1280, "height": 900})
            try:
                exercise_interactive(context, engine)
            finally:
                context.close()
            exercise_without_javascript(browser, engine)
        finally:
            browser.close()
    print(f"zed release-plan {engine} accessibility and print contract passed")


if __name__ == "__main__":
    main()
