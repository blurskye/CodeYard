#!/usr/bin/env python3
"""Showcase the Actions UI against a repository with pre-seeded fixture runs.

This test does not mock requests, seed data, dispatch workflows, or alter repository
or CI state. The repository must already contain queued, running, successful, and failed
runs; at least one run should have logs.

  uv run --with playwright python scripts/test-actions-ui.py \
    --base-url http://127.0.0.1:8080 --owner alice --repo actions-fixture \
    --username alice --password-file /path/to/private/password-file
"""
import argparse
import os
import re
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from playwright.sync_api import expect, sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--base-url", default="http://localhost:8080")
parser.add_argument("--owner", required=True)
parser.add_argument("--repo", required=True)
parser.add_argument("--username")
parser.add_argument("--password-file", type=Path)
parser.add_argument("--output-dir", type=Path, default=Path("/tmp/codeyard-actions"))
args = parser.parse_args()
if bool(args.username) != bool(args.password_file):
    parser.error("--username and --password-file must be supplied together")
args.output_dir.mkdir(parents=True, exist_ok=True)
base = args.base_url.rstrip("/")
path = f"/{args.owner}/{args.repo}/actions"
url = base + path
required_statuses = {"queued", "running", "success", "failed"}


def launch_options():
    options = {"headless": True}
    if os.environ.get("CHROMIUM_EXECUTABLE"):
        options["executable_path"] = os.environ["CHROMIUM_EXECUTABLE"]
    return options


def login(context):
    if not args.username:
        return
    response = context.request.post(
        base + "/api/auth/login",
        data={
            "username": args.username,
            "password": args.password_file.read_text().rstrip("\r\n"),
        },
    )
    assert response.ok, f"Login failed: {response.status} {response.text()}"


def status_from_label(label):
    match = re.search(r", (queued|running|success|failed|cancelled|unknown),", label.lower())
    return match.group(1) if match else None


with sync_playwright() as playwright:
    browser = playwright.chromium.launch(**launch_options())
    try:
        context = browser.new_context(viewport={"width": 1440, "height": 1000})
        login(context)
        page = context.new_page()
        browser_errors = []
        page.on("pageerror", lambda error: browser_errors.append(str(error)))
        response = page.goto(url)
        assert response and response.ok, f"Actions page failed: {response.status if response else 'no response'}"
        assert urlparse(page.url).path == path, "Authentication is required for the seeded fixture"

        rows = page.locator(".actions-run-row")
        row_count = rows.count()
        assert row_count, (
            "Seeded SSR prerequisite missing: the repository has no workflow runs. "
            "Seed fixture runs before using this showcase test."
        )
        statuses = {
            status
            for label in rows.evaluate_all("els => els.map(el => el.getAttribute('aria-label') || '')")
            if (status := status_from_label(label))
        }
        missing = required_statuses - statuses
        assert not missing, f"Seeded SSR prerequisite missing run statuses: {sorted(missing)}"
        assert page.locator(".actions-sidebar .actions-nav-item").count() > 1, (
            "Seeded SSR prerequisite missing: no workflow definitions or historical workflows"
        )
        assert page.evaluate("document.fonts.check('14px \\\"Mona Sans\\\"')"), "Mona Sans failed to load"
        expect(page.locator('.repo-tab[href$="/actions"]')).to_have_class(re.compile(r"repo-tab-active"))
        expect(page.locator("#actions-run-filters")).to_have_attribute("method", "get")

        # The native dispatch control is inspected but never submitted in this fixture-only test.
        dispatch = page.locator(".actions-dispatch")
        assert dispatch.count(), "Fixture user must have write access so the native dispatch form is visible"
        dispatch.locator("summary").click()
        dispatch_form = dispatch.locator("form.actions-dispatch-menu")
        expect(dispatch_form).to_have_attribute("method", "post")
        expect(dispatch_form.locator('input[name="csrf_token"]')).to_have_count(1)
        expect(dispatch_form.locator('select[name="workflow_path"]')).to_have_count(1)
        dispatch.locator("summary").click()

        page.screenshot(path=str(args.output_dir / "runs-dark.png"), full_page=True)

        # Filters are GET forms now; each submit must survive a complete SSR/hydrated read.
        filters = page.locator("#actions-run-filters")
        filters.locator('select[name="status"]').select_option("failed")
        filters.get_by_role("button", name="Filter runs").click()
        expect(page).to_have_url(re.compile(r"[?&]status=failed(?:&|$)"))
        expect(rows.first).to_be_visible()
        expect(page.locator('.actions-run-row:not([aria-label*=" failed,"])')).to_have_count(0)
        assert all(
            status_from_label(label) == "failed"
            for label in rows.evaluate_all("els => els.map(el => el.getAttribute('aria-label') || '')")
        )

        search = page.locator('input[name="q"]')
        search.fill("fixture-run-that-does-not-exist")
        page.locator("#actions-run-filters").get_by_role("button", name="Filter runs").click()
        expect(page.get_by_text("No matching workflow runs", exact=True)).to_be_visible()
        page.goto(url)
        expect(rows).to_have_count(row_count)

        # Open a genuine seeded run and exercise list/detail/log/summary navigation.
        failed_row = rows.filter(has=page.locator(".ci-status-failed")).first
        run_href = failed_row.get_attribute("href")
        assert run_href and parse_qs(urlparse(run_href).query).get("job"), "Run link has no deep-link id"
        failed_row.click()
        expect(page.locator("#job-logs")).to_be_visible()
        expect(page).to_have_url(re.compile(r"[?&]job=[0-9a-f-]+(?:&|$)"))
        assert page.locator(".actions-log-line").count(), (
            "Seeded SSR prerequisite missing: the failed fixture run has no logs"
        )
        page.get_by_role("link", name="Summary", exact=True).click()
        expect(page.locator("#run-summary")).to_be_visible()
        page.screenshot(path=str(args.output_dir / "summary-dark.png"), full_page=True)
        page.locator('.actions-sidebar a.actions-nav-item[href*="workflow_job="]').first.click()
        expect(page.locator("#job-logs")).to_be_visible()
        page.get_by_placeholder("Search logs").fill("fixture text that is absent")
        expect(page.get_by_text("No log lines match your search.", exact=True)).to_be_visible()
        page.goto(base + run_href)
        expect(page.locator("#job-logs")).to_be_visible()
        page.screenshot(path=str(args.output_dir / "logs-dark.png"), full_page=True)

        page.set_viewport_size({"width": 390, "height": 844})
        page.screenshot(path=str(args.output_dir / "logs-mobile.png"), full_page=True)
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Logs overflow on mobile"
        page.get_by_role("link", name="← Workflow runs", exact=True).click()
        expect(rows).to_have_count(row_count)
        page.screenshot(path=str(args.output_dir / "runs-mobile.png"), full_page=True)
        assert page.evaluate("document.documentElement.scrollWidth <= innerWidth"), "Runs overflow on mobile"

        page.set_viewport_size({"width": 1440, "height": 1000})
        page.evaluate(
            "document.documentElement.classList.remove('dark', 'system');"
            "document.documentElement.classList.add('light')"
        )
        page.screenshot(path=str(args.output_dir / "runs-light.png"), full_page=True)

        # The server error page is a real response, not an intercepted fixture.
        error_response = page.goto(url + "?job=not-a-uuid")
        assert error_response and error_response.status == 400
        expect(page.get_by_text("Can't complete this request", exact=True)).to_be_visible()
        assert not browser_errors, browser_errors

        # Repeat the contract at the raw HTML boundary with JavaScript disabled.
        no_js = browser.new_context(java_script_enabled=False, viewport={"width": 390, "height": 844})
        no_js.add_cookies(context.cookies())
        fallback = no_js.new_page()
        fallback_response = fallback.goto(url)
        assert fallback_response and fallback_response.ok
        expect(fallback.locator("#actions-run-filters")).to_have_attribute("method", "get")
        expect(fallback.locator(".actions-run-row")).to_have_count(row_count)
        fallback.goto(base + run_href)
        expect(fallback.locator("#job-logs")).to_be_visible()
        no_js.close()

        print(
            "PASS: seeded SSR runs and statuses, native GET filters and POST dispatch contract, "
            "hydrated and JavaScript-disabled deep links, logs, summary, error, mobile, light and dark."
        )
        print("Fixture screenshots:", args.output_dir)
    finally:
        browser.close()
