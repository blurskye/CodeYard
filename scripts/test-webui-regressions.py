#!/usr/bin/env python3
"""Playwright checks against a disposable, bundled server with a long issue and a PR diff.

CODEYARD_TEST_SESSION=<session_id> uv run --with playwright python \
  scripts/test-webui-regressions.py http://localhost:3000 /alice/demo/issues/1 /alice/demo/pulls/1/diff
The session must own a comment on the issue. No forms are saved or deleted.
"""
import os
import sys
from playwright.sync_api import expect, sync_playwright

base, issue, diff = sys.argv[1:]
with sync_playwright() as pw:
    browser = pw.chromium.launch()
    context = browser.new_context(viewport={"width": 1280, "height": 800})
    context.add_cookies([{"name": "session_id", "value": os.environ["CODEYARD_TEST_SESSION"], "url": base}])
    page = context.new_page()

    def open_hydrated(path):
        page.goto(base + path)
        page.locator('[data-hydrated="true"]').wait_for()

    open_hydrated(issue)
    area = page.locator('#comment-body')
    area.fill('')
    area.locator('xpath=ancestor::form').get_by_role('button', name='Bold', exact=True).click()
    expect(area).to_be_focused()
    page.keyboard.type('replacement')
    expect(area).to_have_value('**replacement**')
    print('PASS toolbar returns focus and selects the placeholder')

    open_hydrated('/explore')
    picker = page.get_by_role('combobox', name='Type')
    picker.focus()
    before = page.locator('select[name="type"]').input_value()
    for key in ['Control+f', 'Meta+f', 'Alt+f']:
        picker.press(key)
        expect(page.locator('select[name="type"]')).to_have_value(before)
    print('PASS dropdown preserves modified browser shortcuts')

    open_hydrated(diff)
    targets = page.locator('a.patch-add')
    assert targets.count() >= 2
    for target in targets.all()[:20]:
        inset = target.evaluate("e => getComputedStyle(e, '::after').inset")
        assert inset in ('0px -12px', '0px -12px 0px -12px'), inset
    print('PASS diff comment targets stay within their line')

    open_hydrated(issue)
    page.evaluate('window.scrollTo(0, 1000)')
    previous = page.evaluate('window.scrollY')
    assert previous > 400, 'fixture issue must be long enough to scroll'
    page.locator('a[href="/help"]').first.evaluate('e => e.click()')
    page.wait_for_url(base + '/help')
    page.get_by_role('heading', name='Keyboard shortcuts', exact=True).wait_for()
    # Delay the actual model request, not a replacement response.
    page.route('**' + issue, lambda route: (page.wait_for_timeout(400), route.continue_()))
    page.go_back()
    page.wait_for_url(base + issue)
    expect(page.locator('#comment-body')).to_be_attached()
    page.wait_for_function('y => Math.abs(window.scrollY - y) < 5', arg=previous)
    print('PASS Back restores scrolling after delayed content mounts')

    phone = browser.new_context(java_script_enabled=False, viewport={"width": 375, "height": 400})
    phone.add_cookies(context.cookies())
    native = phone.new_page()
    native.goto(base + issue)
    delete = native.locator('details:has(> .repo-confirm-modal)').first
    delete.locator('summary').click()
    delete.get_by_role('link', name='Cancel', exact=True).click()
    expect(native.locator('.repo-confirm-modal:visible')).to_have_count(0)
    print('PASS native comment deletion can be canceled')
    native.get_by_label('More repository pages', exact=True).click()
    panel = native.locator('.repo-tabs-more > .native-menu-panel')
    panel.locator('summary').filter(has_text='Watch').first.click()
    bounds = panel.bounding_box()
    assert bounds['y'] >= 0 and bounds['y'] + bounds['height'] <= 400, bounds
    print('PASS phone repository menu fits the viewport')
    browser.close()
