#!/usr/bin/env python3
"""Playwright checks against a disposable, bundled server with a long issue and a PR diff.

CODEYARD_TEST_SESSION=<session_id> uv run --with playwright python \
  scripts/test-webui-regressions.py http://localhost:3000 /alice/demo/issues/1 /alice/demo/pulls/1/diff
The session must own a comment on the long issue. Default checks do not save forms.
--review-ranges also publishes review comments: use only a disposable fixture with
four added lines in --range-path (default range.txt) in the PR's first diff hunk.
"""
import argparse
import os
import secrets
from pathlib import Path
import tempfile
from urllib.parse import urlencode
from playwright.sync_api import expect, sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('base')
parser.add_argument('issue')
parser.add_argument('diff')
parser.add_argument('--review-ranges', action='store_true')
parser.add_argument('--range-path', default='range.txt')
parser.add_argument('--output-dir', type=Path)
args = parser.parse_args()
base, issue, diff = args.base.rstrip('/'), args.issue, args.diff


def hydrated(page):
    page.locator('[data-hydrated="true"]').wait_for(state='attached', timeout=60000)


with sync_playwright() as pw:
    options = {'headless': True}
    if os.environ.get('CHROMIUM_EXECUTABLE'):
        options['executable_path'] = os.environ['CHROMIUM_EXECUTABLE']
    browser = pw.chromium.launch(**options)
    context = browser.new_context(viewport={"width": 1280, "height": 800})
    context.add_cookies([{"name": "session_id", "value": os.environ["CODEYARD_TEST_SESSION"], "url": base}])
    page = context.new_page()
    browser_errors = []
    page.on('pageerror', lambda error: browser_errors.append(str(error)))

    def open_hydrated(path):
        page.goto(base + path)
        hydrated(page)

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

    phone = browser.new_context(java_script_enabled=False, reduced_motion='reduce', viewport={"width": 375, "height": 400})
    phone.add_cookies(context.cookies())
    native = phone.new_page()
    native.set_default_timeout(60000)
    native.bring_to_front()
    native.goto(base + issue)
    delete = native.locator('details:has(> .repo-confirm-modal)').first
    delete.locator('summary').click()
    delete.locator('.repo-confirm-actions').get_by_role('link', name='Cancel', exact=True).click()
    expect(native.locator('.repo-confirm-modal:visible')).to_have_count(0)
    print('PASS native comment deletion can be canceled')
    native.get_by_label('More repository pages', exact=True).click()
    panel = native.locator('.repo-tabs-more > .native-menu-panel')
    panel.locator('summary').filter(has_text='Watch').first.click()
    bounds = panel.bounding_box()
    assert bounds['y'] >= 0 and bounds['y'] + bounds['height'] <= 400, bounds
    print('PASS phone repository menu fits the viewport')

    if args.review_ranges:
        output = args.output_dir or Path(tempfile.mkdtemp(prefix='webui-regressions-'))
        output.mkdir(parents=True, exist_ok=True)
        target = diff + '?' + urlencode({'path': args.range_path, 'side': 'RIGHT', 'line': 3})
        for width in (1440, 390):
            reviews = browser.new_context(viewport={'width': width, 'height': 950})
            reviews.add_cookies(context.cookies())
            review = reviews.new_page()
            errors = []
            review.on('pageerror', lambda error: errors.append(str(error)))
            assert review.goto(base + target).ok
            hydrated(review)
            review.get_by_label('First line of the comment range', exact=True).fill('1')
            review.locator('select[name="start_side"]').select_option('RIGHT')
            text = f'Hydrated range at {width}px ({secrets.token_hex(8)})'
            review.get_by_label('Comment on this line (Markdown)', exact=True).fill(text)
            review.evaluate('window.scrollTo(0, 0)')
            review.screenshot(path=str(output / f'composer-{width}.png'), full_page=True)
            assert review.evaluate('document.documentElement.scrollWidth <= innerWidth + 1'), 'Range composer overflow'
            review.get_by_role('button', name='Add single comment', exact=True).click()
            expect(review.locator('.collaboration-diff-thread').filter(has_text=text)).to_be_visible()
            review.reload()
            hydrated(review)
            thread = review.locator('.collaboration-diff-thread').filter(has_text=text)
            expect(thread.locator('.collaboration-comment-range')).to_have_text('Lines RIGHT 1–RIGHT 3')
            for split in (False, True):
                if split:
                    review.get_by_role('link', name='Split', exact=True).click()
                    expect(review).to_have_url(base + diff + '?view=split')
                    expect(review.get_by_role('link', name='Split', exact=True)).to_have_attribute('aria-current', 'page')
                    expect(review.locator('.collaboration-split')).to_be_visible()
                    hydrated(review)
                expect(review.locator('.collaboration-diff-thread').filter(has_text=text)).to_be_visible()
                review.evaluate('window.scrollTo(0, 0)')
                review.screenshot(path=str(output / f'range-{width}-{"split" if split else "unified"}.png'), full_page=True)
                assert review.evaluate('document.documentElement.scrollWidth <= innerWidth + 1'), 'Page overflow'
            assert not errors, errors
            reviews.close()
            print(f'PASS hydrated {width}px range publication, reload, unified/split navigation and layout')

        native_text = f'Preserve refused range ({secrets.token_hex(8)})'
        native.goto(base + target)
        native.get_by_label('First line of the comment range', exact=True).fill('5')
        native.get_by_label('Comment on this line (Markdown)', exact=True).fill(native_text)
        native.get_by_role('button', name='Add single comment', exact=True).click()
        expect(native.get_by_label('First line of the comment range', exact=True)).to_have_value('5')
        expect(native.get_by_label('Comment on this line (Markdown)', exact=True)).to_have_value(native_text)
        native.get_by_label('First line of the comment range', exact=True).fill('1')
        native.get_by_role('button', name='Add single comment', exact=True).click()
        thread = native.locator('.collaboration-diff-thread').filter(has_text=native_text)
        expect(thread.locator('.collaboration-comment-range')).to_have_text('Lines RIGHT 1–RIGHT 3')
        native.evaluate('window.scrollTo(0, 0)')
        native.screenshot(path=str(output / 'range-no-javascript.png'), full_page=True)
        print('PASS JavaScript-disabled refusal preservation and real range publication')
        print(f'Screenshots: {output}')
    assert not browser_errors, browser_errors
    browser.close()
