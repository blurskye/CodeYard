#!/usr/bin/env python3
"""Exercise the disposable runner-settings fixture with and without hydration."""
import json
import os
import re
import sys

from playwright.sync_api import expect, sync_playwright

base, org = sys.argv[1:]
settings = f"/settings/actions/runners?org={org}"


def hydrated(page):
    try:
        page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
    except Exception:
        print("Hydration failed:", page.url, page.title(), page.locator("script[src]").evaluate_all("nodes => nodes.map(n => n.src)"), flush=True)
        raise

def group_heading(page, name):
    return page.get_by_role("heading", name=re.compile(rf"^{re.escape(name)} \(#[0-9]+\)$"))


with sync_playwright() as pw:
    browser = pw.chromium.launch(executable_path=os.environ["CODEYARD_BROWSER_CHROMIUM"], args=["--no-sandbox"])
    for javascript in (True, False):
        context = browser.new_context(java_script_enabled=javascript, viewport={"width": 390, "height": 844})
        context.add_cookies([{"name": "session_id", "value": os.environ["CODEYARD_TEST_SESSION"], "url": base}])
        page = context.new_page()
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.on("console", lambda message: print("Browser console:", message.text, flush=True) if message.type == "error" else None)
        page.on("requestfailed", lambda request: print("Request failed:", request.url, request.failure, flush=True))
        page.on("response", lambda response: print("HTTP error:", response.status, response.url, flush=True) if response.status >= 400 else None)
        page.goto(f"{base}/settings/organizations/{org}")
        if javascript:
            hydrated(page)
        page.get_by_role("link", name="Manage Actions runners and groups", exact=True).click()
        page.wait_for_url(base + settings)
        page.get_by_role("heading", name="Runner groups", exact=True).wait_for()
        if javascript:
            hydrated(page)
        def select_access(form, value, label):
            if javascript:
                form.locator('button[role="combobox"]').click()
                page.get_by_role("option", name=label, exact=True).click()
            else:
                form.locator('select[name="visibility"]').select_option(value)

        name = "Hydrated runners" if javascript else "Native runners"
        create = page.locator('form:has(button:text-is("Create group"))')
        create.get_by_label("Name", exact=True).fill(name)
        select_access(create, "selected", "Selected repositories")
        create.get_by_role("button", name="Create group", exact=True).click()
        article = page.locator("article").filter(has=group_heading(page, name))
        group_heading(page, name).wait_for()
        if javascript:
            hydrated(page)
        edit = article.locator('form:has(button:text-is("Save group"))')
        edit.get_by_label("Name", exact=True).fill(name + " renamed")
        select_access(edit, "private", "Private repositories")
        edit.get_by_label("Restrict to selected workflows", exact=True).check()
        edit.get_by_label("Allowed workflows").fill("browser-org/repo/.github/workflows/ci.yml@main")
        edit.get_by_role("button", name="Save group", exact=True).click()
        article = page.locator("article").filter(has=group_heading(page, name + " renamed"))
        group_heading(page, name + " renamed").wait_for()
        expect(article.locator('select[name="visibility"]')).to_have_value("private")
        expect(article.get_by_label("Restrict to selected workflows", exact=True)).to_be_checked()
        expect(article.get_by_label("Allowed workflows")).to_have_value("browser-org/repo/.github/workflows/ci.yml@main")
        if javascript:
            hydrated(page)
            assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        with page.expect_download() as download:
            page.get_by_role("button", name="Download removal token", exact=True).click()
        with open(download.value.path(), encoding="utf-8") as downloaded:
            credential = json.load(downloaded)
        assert credential["token"].startswith("codeyard-remove-v1.")
        assert credential["token"] not in page.content()
        article.get_by_label("Confirm deletion (runners return to Default)", exact=True).check()
        article.get_by_role("button", name="Delete group", exact=True).click()
        expect(group_heading(page, name + " renamed")).to_have_count(0)
        assert not errors, errors
        print(f"PASS runner settings {'hydrated navigation' if javascript else 'without JavaScript'}: create, edit, private workflow selection, credential download, delete", flush=True)
        context.close()
    browser.close()
