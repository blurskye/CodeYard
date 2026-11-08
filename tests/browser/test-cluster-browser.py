#!/usr/bin/env python3
"""Check cluster administration on an owned e2e fixture, with and without hydration."""
import json
import os
import re
import sys
import subprocess
import time
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

base, jar, node, other, peer_jar = sys.argv[1:]
with open(jar, encoding="utf-8") as source:
    session = json.load(source)["session_id"]

with sync_playwright() as playwright:
    browser = playwright.chromium.launch(executable_path=os.environ["CODEYARD_BROWSER_CHROMIUM"])
    for javascript in (True, False):
        context = browser.new_context(java_script_enabled=javascript)
        context.add_cookies([{"name": "session_id", "value": session, "url": base}])
        page = context.new_page()
        errors = []
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.goto(base + "/admin")
        if javascript:
            page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
        page.get_by_role("link", name="Nodes", exact=True).click()
        page.wait_for_url(base + "/admin/nodes")
        if javascript:
            page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
        row = page.locator("tbody tr").filter(has=page.get_by_text(node, exact=True))
        expect(row).to_have_count(1)
        expect(row.locator("td").nth(3)).to_have_text("active")
        with page.expect_navigation(wait_until="domcontentloaded"):
            row.get_by_role("button", name="Drain", exact=True).click()
        row.get_by_role("button", name="Cancel drain", exact=True).wait_for()
        expect(row.locator("td").nth(3)).to_contain_text(re.compile("draining|drained"))
        expect(row.locator("td").nth(3)).to_contain_text("repositories and files")
        with page.expect_navigation(wait_until="domcontentloaded"):
            row.get_by_role("button", name="Cancel drain", exact=True).click()
        row.get_by_role("button", name="Drain", exact=True).wait_for()
        expect(row.locator("td").nth(3)).to_have_text("active")
        page.get_by_role("link", name="Site settings", exact=True).click()
        page.wait_for_url(base + "/admin/settings")
        if javascript:
            page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
        policy = page.locator('form').filter(has=page.locator('input[name="scaling_role"][value="runner"]'))
        expect(policy.get_by_role("checkbox", name="Paused", exact=True)).to_be_checked()
        expect(policy.get_by_role("checkbox", name="Dry run", exact=True)).to_be_checked()
        maximum = policy.get_by_label("Maximum nodes", exact=True)
        previous = maximum.input_value()
        changed = str(int(previous) + 1)
        maximum.fill(changed)
        with page.expect_navigation(wait_until="domcontentloaded"):
            policy.get_by_role("button", name="Save scaling policy", exact=True).click()
        expect(maximum).to_have_value(changed)
        page.reload()
        if javascript:
            page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
        expect(maximum).to_have_value(changed)
        maximum.fill(previous)
        with page.expect_navigation(wait_until="domcontentloaded"):
            policy.get_by_role("button", name="Save scaling policy", exact=True).click()
        page.reload()
        expect(maximum).to_have_value(previous)
        assert not errors, errors
        print(f"PASS cluster administration {'hydrated navigation' if javascript else 'without JavaScript'}: drain, progress, cancel and paused dry-run policy save", flush=True)
        context.close()
    # Alice comments through web1 while Bob has the issue open on web2. The
    # existing notification hint must refresh that page, including with clock skew.
    helper = Path(__file__).resolve().parents[1] / "e2e/e2e-web.py"
    def post(path, payload, field):
        result = subprocess.run([sys.executable, "-I", str(helper), "req", "--base", base,
                                 "--jar", jar, "--who", "alice", "POST", path,
                                 "--accept", "json", "--status", "201", "--json", json.dumps(payload),
                                 "--print", "json:" + field], text=True, capture_output=True,
                                timeout=30, check=True)
        return result.stdout.strip()
    number = post("/api/repos/alice/proj/issues", {"title": "Cross-node browser update", "body": "Live hint fixture"}, "number")
    assert number.isdigit()
    with open(peer_jar, encoding="utf-8") as source:
        peer_session = json.load(source)["session_id"]
    context = browser.new_context()
    context.add_cookies([{"name": "session_id", "value": peer_session, "url": other}])
    page = context.new_page()
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    with page.expect_websocket(timeout=60000):
        page.goto(f"{other}/alice/proj/issues/{number}")
    page.locator('[data-hydrated="true"]').wait_for(state="attached", timeout=60000)
    marker = "Cross-node comment arrives within two seconds"
    post(f"/api/repos/alice/proj/issues/{number}/comments", {"body": "@bob " + marker}, "id")
    started = time.monotonic()
    expect(page.get_by_text(marker, exact=False).first).to_be_visible(timeout=2000)
    assert time.monotonic() - started <= 2, "cross-node refresh exceeded two seconds"
    assert not errors, errors
    print("PASS cluster live issue: comment on web1 updates Bob's open page on web2 within two seconds", flush=True)
    context.close()
    browser.close()
