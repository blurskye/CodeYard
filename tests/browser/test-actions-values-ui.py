#!/usr/bin/env python3
"""Real UI checks; invoked by examples/actions_values_ui.rs, never a mock bundle."""
import os
import sys
from pathlib import Path
from urllib.parse import quote

from playwright.sync_api import expect, sync_playwright

base, repository, organization = sys.argv[1:]
markers = (
    "repository-browser-private",
    "environment-browser-private",
    "organization-browser-private",
    "phone-browser-private",
)
errors = []
artifacts = Path("dist/actions-values-ui")
artifacts.mkdir(parents=True, exist_ok=True)


def observe(page):
    page.on("pageerror", lambda error: errors.append(str(error)))
    page.on("console", lambda message: errors.append(message.text)
            if message.type in ("error", "warning") and "hydrat" in message.text.lower() else None)


def private_values_absent(page):
    body = page.content()
    assert all(marker not in body for marker in markers), "secret value appeared in page data"


def fits(page):
    widths = page.evaluate("""() => ({
        viewport: document.documentElement.clientWidth,
        document: document.documentElement.scrollWidth,
        body: document.body.scrollWidth
    })""")
    if max(widths["document"], widths["body"]) > widths["viewport"] + 1:
        widths["url"] = page.url
        widths["overflow"] = page.evaluate("""() => Array.from(document.body.querySelectorAll('*'))
            .filter(element => element.getBoundingClientRect().right > innerWidth + 1)
            .slice(0, 24).map(element => {
                const rect = element.getBoundingClientRect(), style = getComputedStyle(element);
                return {tag: element.tagName, class: element.className,
                    width: rect.width, right: rect.right, minWidth: style.minWidth,
                    display: style.display, columns: style.gridTemplateColumns};
            })""")
        raise AssertionError(widths)


def visit_settings(page, path):
    response = page.goto(base + path)
    assert response and response.ok, f"Settings navigation failed: {path}"
    if path == organization:
        expect(page.locator('form:has(input[name="previous"][value="ORGANIZATION_REGION"]) textarea[name="value"]')).to_have_value("org-visible")
        expect(page.get_by_text("ORGANIZATION_TOKEN", exact=True)).to_be_visible()
    else:
        expect(page.locator('form[action$="/environment-secret-save"]')).to_be_visible()


def scope(page, value, *, hydrated=False):
    picker = page.get_by_role("combobox", name="Repository or environment", exact=True)
    if hydrated:
        # A navigation briefly restores the native select before its enhancement effect runs.
        # Wait for the actual visible enhanced control instead of racing tag detection.
        picker = picker.and_(page.locator("button"))
        expect(picker).to_be_visible()
        picker.click()
        label = "Environment: " + value if value else "Repository"
        page.get_by_role("option", name=label, exact=True).click()
    else:
        picker.select_option(value)
    page.get_by_role("button", name="Show values", exact=True).click()


with sync_playwright() as pw:
    browser = pw.chromium.launch(
        executable_path=os.environ["CODEYARD_BROWSER_CHROMIUM"], chromium_sandbox=True,
    )
    cookie = {"name": "session_id", "value": os.environ["CODEYARD_TEST_SESSION"], "url": base}
    phone = browser.new_context(java_script_enabled=False, viewport={"width": 375, "height": 812})
    phone.add_cookies([cookie])
    native = phone.new_page()
    observe(native)
    visit_settings(native, repository + "/settings?tab=secrets&environment=Production")
    secret = native.locator('form[action$="/environment-secret-save"]')
    expect(secret).to_be_visible()
    native.screenshot(path=str(artifacts / "environment-375.png"), full_page=True)
    fits(native)
    private_values_absent(native)
    secret.get_by_label("Name", exact=True).fill("PHONE_SECRET")
    secret.get_by_label("Secret", exact=True).fill("phone-browser-private")
    secret.get_by_role("button", name="Save secret", exact=True).click()
    expect(native.get_by_text("PHONE_SECRET", exact=True)).to_be_visible()
    private_values_absent(native)
    expect(native.locator('form[action$="/environment-secret-save"] textarea[name="value"]')).to_have_value("")
    variable = native.locator('form[action$="/environment-variable-save"]').filter(
        has=native.get_by_role("heading", name="New environment variable", exact=True))
    variable.get_by_label("Name", exact=True).fill("PHONE_VARIABLE")
    variable.get_by_label("Value", exact=True).fill("phone-visible")
    variable.get_by_role("button", name="Add variable", exact=True).click()
    expect(native.locator('form:has(input[name="previous"][value="PHONE_VARIABLE"]) textarea[name="value"]')).to_have_value("phone-visible")
    scope(native, "")
    expect(native.locator('form[action$="/secret-save"]')).to_be_visible()
    expect(native.locator('form[action$="/environment-secret-save"]')).to_have_count(0)
    fits(native)
    print("PASS native phone forms save environment values and switch scopes without JavaScript")

    visit_settings(native, repository + "/settings?tab=secrets&environment=" + quote("L" * 255))
    fits(native)
    private_values_absent(native)
    visit_settings(native, organization)
    native.screenshot(path=str(artifacts / "organization-375.png"), full_page=True)
    fits(native)
    private_values_absent(native)
    print("PASS environment and organization settings fit 375 px, including a 255-character name")

    desktop = browser.new_context(viewport={"width": 1280, "height": 800})
    desktop.add_cookies([cookie])
    page = desktop.new_page()
    observe(page)
    visit_settings(page, repository + "/settings?tab=secrets&environment=Production")
    page.locator('[data-hydrated="true"]').wait_for(timeout=60000)
    private_values_absent(page)
    page.screenshot(path=str(artifacts / "environment-desktop.png"), full_page=True)
    fits(page)
    draft = page.locator('form[action$="/environment-secret-save"] textarea[name="value"]')
    draft.fill("environment-draft-must-not-cross-scopes")
    scope(page, "", hydrated=True)
    repository_secret = page.locator('form[action$="/secret-save"] textarea[name="value"]')
    expect(repository_secret).to_have_value("")
    repository_secret.fill("repository-draft-must-not-cross-scopes")
    scope(page, "Production", hydrated=True)
    expect(page.locator('form[action$="/environment-secret-save"] textarea[name="value"]')).to_have_value("")
    expect(page.locator('form:has(input[name="previous"][value="REGION"]) textarea[name="value"]')).to_have_value("env-visible")
    private_values_absent(page)
    print("PASS hydration initializes values and scope changes discard secret drafts")

    visit_settings(page, organization)
    page.locator('[data-hydrated="true"]').wait_for(timeout=60000)
    private_values_absent(page)
    page.screenshot(path=str(artifacts / "organization-desktop.png"), full_page=True)
    fits(page)
    assert not errors, errors
    print("PASS desktop organization settings hydrate without errors or secret values")
    page.set_viewport_size({"width": 375, "height": 812})
    for path, name in ((organization, "organization"),
                       (repository + "/settings?tab=secrets&environment=" + quote("L" * 255), "environment")):
        visit_settings(page, path)
        page.locator('[data-hydrated="true"]').wait_for(timeout=60000)
        private_values_absent(page)
        fits(page)
        page.screenshot(path=str(artifacts / (name + "-375-hydrated.png")), full_page=True)
    assert not errors, errors
    print("PASS hydrated organization and long-name environment settings fit 375 px")
    browser.close()
