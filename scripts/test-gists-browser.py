#!/usr/bin/env python3
"""Exercise the real gist application with and without hydration, in an owned SQLite install.

Requires a built server, the real WEBUI_DIST bundle, Playwright, and Chromium. Never uses an
existing installation. Screenshots contain only these disposable test accounts and gists.
"""
import argparse
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request

from playwright.sync_api import sync_playwright


def hydrated(page):
    page.locator('[data-hydrated="true"]').wait_for(timeout=60000)
    assert page.locator("main.gists-page").count() == 1


def run(base, chromium, output, headed):
    password = secrets.token_urlsafe(24) + "Az!7"
    source = "\n# Without JavaScript\n<script>window.__gist_xss=true</script>\n🌻\n"
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(executable_path=chromium, headless=not headed)
        plain = browser.new_context(java_script_enabled=False, viewport={"width": 1280, "height": 900})
        page = plain.new_page()
        page.goto(base + "/setup")
        form = page.locator('form[action="/setup"]')
        for name, value in [("username", "browser_alice"), ("password", password), ("confirm_password", password)]:
            form.locator(f'[name="{name}"]').fill(value)
        form.locator('[name="node_mode"]').select_option("public")
        form.locator('button[type="submit"]').click()
        page.wait_for_url(base + "/")
        page.goto(base + "/gists/new")
        page.locator('[name="description"]').fill("Browser secret")
        page.locator('[name="name_0"]').fill("README.md")
        page.locator('[name="content_0"]').fill(source)
        page.get_by_role("button", name="Add another file", exact=True).click()
        assert page.locator('[name="content_0"]').input_value() == source
        page.locator('[name="name_1"]').fill("hello.txt")
        page.locator('[name="content_1"]').fill("Original second file\n")
        page.get_by_role("button", name="Create gist", exact=True).click()
        page.wait_for_url(re.compile(re.escape(base) + r"/gists/[0-9a-f]{32}$"))
        detail = page.url.removeprefix(base)
        response = page.goto(base + detail)
        assert "noindex" in response.headers.get("x-robots-tag", "")
        assert page.locator(".gist-markdown h1").inner_text().strip() == "Without JavaScript"
        comment = page.locator(f'form[action^="{detail}/comments?"]')
        comment.locator('[name="body"]').fill("Native **comment**")
        comment.get_by_role("button", name="Comment", exact=True).click()
        assert page.locator(".gist-comment strong").inner_text() == "comment"
        page.get_by_role("link", name="Edit", exact=True).click()
        page.locator('[name="content_0"]').fill(source + "Edited without JavaScript.\n")
        page.get_by_role("button", name="Save gist", exact=True).click()
        star=page.locator(f'form[action="{detail}/star"]')
        assert star.locator('[name="add"]').input_value() == "yes"
        with page.expect_navigation(wait_until="load"):
            star.locator('button').click()
        assert page.get_by_role("button", name="Unstar", exact=True).count() == 1, page.locator("main").inner_text()
        state = plain.storage_state()
        plain.close()

        context = browser.new_context(storage_state=state, viewport={"width": 1280, "height": 900})
        page = context.new_page()
        errors = []
        documents = []
        models = []
        def page_error(error):
            errors.append(str(error))
            print(f"Browser page error: {error}", flush=True)
        page.on("pageerror", page_error)
        page.on("console", lambda message: print(f"Browser console {message.type}: {message.text}", flush=True) if message.type in ["error", "warning"] else None)
        page.on("requestfailed", lambda request: print(f"Browser request failed: {request.url}: {request.failure}", flush=True))
        page.on("response", lambda response: print(f"Browser HTTP {response.status}: {response.url}", flush=True) if response.status >= 400 else None)
        page.on("request", lambda request: documents.append(request.url) if request.is_navigation_request() and request.resource_type == "document" else None)
        page.on("response", lambda response: models.append(response.url) if response.headers.get("content-type", "").startswith("application/vnd.codeyard.page+json") else None)
        page.goto(base + "/gists")
        hydrated(page)
        count = len(documents)
        page.get_by_role("link", name="Your gists", exact=True).click()
        page.wait_for_url(base + "/gists?tab=mine")
        page.get_by_role("link", name="README.md", exact=True).click()
        page.wait_for_url(base + detail)
        page.get_by_role("link", name="Edit", exact=True).click()
        page.wait_for_url(base + detail + "/edit")
        assert len(documents) == count, "Links reloaded the document instead of using hydrated navigation"
        assert len(models) >= 3, "Hydrated navigation did not fetch authorized page models"
        page.locator('[name="name_1"]').fill("renamed.txt")
        page.locator('[name="content_1"]').fill("Hydrated edit\n")
        page.get_by_role("button", name="Add another file", exact=True).click()
        hydrated(page)
        assert page.locator('[name="name_1"]').input_value() == "renamed.txt"
        long_name = "long-" + "x" * 190 + ".rs"
        page.locator('[name="name_2"]').fill(long_name)
        page.locator('[name="content_2"]').fill("fn main() {}\n")
        page.get_by_role("button", name="Save gist", exact=True).click()
        page.wait_for_url(base + detail)
        hydrated(page)
        assert page.get_by_role("heading", name="renamed.txt", exact=True).count() == 1
        assert page.get_by_role("heading", name=long_name, exact=True).count() == 1
        assert page.evaluate("typeof window.__gist_xss") == "undefined"
        assert not errors, errors
        for width in [375, 1280]:
            page.set_viewport_size({"width": width, "height": 900})
            for label, path in [("list", "/gists?tab=mine"), ("detail", detail), ("edit", detail + "/edit"), ("new", "/gists/new")]:
                page.goto(base + path)
                hydrated(page)
                assert page.evaluate("document.documentElement.scrollWidth <= innerWidth + 1"), (width, label)
                page.screenshot(path=str(output / f"{label}-{width}.png"), full_page=True)
        assert not errors, errors
        context.close()
        browser.close()
    print("Browser gist checks: no-JS forms, hydrated navigation/forms, and 375px/1280px layouts passed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/codeyard", type=Path)
    parser.add_argument("--bundle", default="dist/webui", type=Path)
    parser.add_argument("--chromium", default=shutil.which("chromium"))
    parser.add_argument("--output", default="dist/gist-browser", type=Path)
    parser.add_argument("--headed", action="store_true", help="Use a real display (or xvfb-run) for visual acceptance")
    args = parser.parse_args()
    assert args.chromium, "Chromium is required"
    binary, bundle = args.binary.resolve(), args.bundle.resolve()
    assert binary.is_file() and (bundle / "index.html").is_file(), "Build the server and real browser bundle first"
    wasm = list(bundle.rglob("*.wasm"))
    assert wasm and all(path.stat().st_size <= 64 * 1024 * 1024 for path in wasm), "Use a release browser bundle; unstripped debug WASM exceeds the runtime's 64 MiB asset limit"
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="codeyard-gist-browser-") as directory:
        root = Path(directory)
        # An empty .env stops dotenvy from loading a deployment's .env from an ancestor directory.
        (root / ".env").write_text("")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        env = {key: value for key, value in os.environ.items() if key in ["PATH", "LD_LIBRARY_PATH", "NIX_LD_LIBRARY_PATH"]}
        env.update(HOME=str(root), DATABASE_URL=f"sqlite://{root}/application.sqlite", GITSERV_STATE_DIR=str(root / "state"), REPOS_PATH=str(root / "repos"), WEBUI_DIST=str(bundle), PUBLIC_URL=base, COOKIE_SECURE="false", RUST_LOG="warn")
        with (root / "server.log").open("w+") as log:
            server = subprocess.Popen([str(binary), "serve", "--roles", "server", "--ssh-mode", "disabled", "--http-addr", f"127.0.0.1:{port}"], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
            try:
                for _ in range(300):
                    assert server.poll() is None, "Disposable server exited"
                    try:
                        with urllib.request.urlopen(base + "/health", timeout=1):
                            break
                    except OSError:
                        time.sleep(0.1)
                else:
                    raise AssertionError("Disposable server did not start")
                run(base, args.chromium, args.output, args.headed)
            except BaseException:
                log.seek(0)
                print(log.read())
                raise
            finally:
                server.terminate()
                try:
                    server.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
