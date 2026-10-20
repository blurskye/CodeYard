#!/usr/bin/env python3
"""Dispatch the real live smoke workflow through native Actions forms.

Prerequisites: a trusted runner, an account, and a writable test repository with
examples/workflows/live-ci.yml installed as .github/workflows/live-ci.yml on main.
No requests are mocked and this command starts a real workflow run.

  uv run --with playwright python scripts/test-actions-live.py \
    --base-url http://127.0.0.1:8081 --owner ci-demo --repo live-actions \
    --username ci-demo --password-file /path/to/private/password-file
"""
import argparse
import os
import re
import time
from pathlib import Path
from urllib.parse import parse_qs, urlparse

from playwright.sync_api import expect, sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--base-url", default="http://localhost:8080")
parser.add_argument("--owner", required=True)
parser.add_argument("--repo", required=True)
parser.add_argument("--username", required=True)
parser.add_argument("--password-file", type=Path, required=True)
parser.add_argument("--workflow", default="Live CI smoke test")
parser.add_argument("--branch", default="main")
parser.add_argument("--output-dir", type=Path, default=Path("/tmp/codeyard-actions-live"))
args = parser.parse_args()
args.output_dir.mkdir(parents=True, exist_ok=True)
base = args.base_url.rstrip("/")
url = f"{base}/{args.owner}/{args.repo}/actions"
run_id = None


def wait_for_job(request, job_url, predicate, timeout=180):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        response = request.get(job_url)
        assert response.ok, f"Run read failed: {response.status} {response.text()}"
        last = response.json()
        if predicate(last):
            return last
        time.sleep(1)
    raise AssertionError(f"Timed out waiting for run state; last response: {last}")


def read_logs(request, logs_url):
    response = request.get(logs_url)
    assert response.ok, f"Log read failed: {response.status} {response.text()}"
    return response.json()["logs"]


def wait_for_log(request, logs_url, text, timeout=180):
    deadline = time.monotonic() + timeout
    logs = []
    while time.monotonic() < deadline:
        logs = read_logs(request, logs_url)
        if any(text in line["line"] for line in logs):
            return logs
        time.sleep(1)
    raise AssertionError(f"Timed out waiting for {text!r}; last {len(logs)} persisted lines")


with sync_playwright() as playwright:
    options = {"headless": True}
    if os.environ.get("CHROMIUM_EXECUTABLE"):
        options["executable_path"] = os.environ["CHROMIUM_EXECUTABLE"]
    browser = playwright.chromium.launch(**options)
    try:
        context = browser.new_context(viewport={"width": 1440, "height": 1100})
        login = context.request.post(
            base + "/api/auth/login",
            data={
                "username": args.username,
                "password": args.password_file.read_text().strip(),
            },
        )
        assert login.ok, f"Login failed: {login.status} {login.text()}"
        page = context.new_page()
        browser_errors = []
        page.on("pageerror", lambda error: browser_errors.append(str(error)))
        response = page.goto(url)
        assert response and response.ok, f"Actions page failed: {response.status if response else 'no response'}"

        dispatch = page.locator(".actions-dispatch")
        assert dispatch.count(), "The signed-in fixture user cannot dispatch this repository"
        dispatch.locator("summary").click()
        workflow = dispatch.locator('select[name="workflow_path"]')
        available = workflow.locator("option").all_text_contents()
        assert args.workflow in available, (
            f"Workflow {args.workflow!r} is not dispatchable on {args.branch!r}; available: {available}"
        )
        workflow.select_option(label=args.workflow)
        dispatch.get_by_label("Branch or tag", exact=True).fill(args.branch)
        with page.expect_response(
            lambda candidate: candidate.request.method == "POST"
            and candidate.url.endswith("/actions/dispatch")
        ) as submitted:
            dispatch.get_by_role("button", name="Run workflow", exact=True).click()
        assert submitted.value.status in (302, 303), (
            f"Dispatch failed: {submitted.value.status} {submitted.value.text()}"
        )
        expect(page).to_have_url(re.compile(r"[?&]job=[0-9a-f-]+(?:&|$)"), timeout=30_000)
        run_id = parse_qs(urlparse(page.url).query)["job"][0]
        deep_url = page.url
        job_url = base + "/api/ci/jobs/" + run_id
        logs_url = job_url + "/logs"
        print("Started real run:", run_id, flush=True)

        # The dispatch redirect is already a genuine deep link. Capture it before
        # doing slower list navigation; a fast runner may advance to success.
        expect(page.locator("#job-logs")).to_be_visible(timeout=30_000)
        active = wait_for_job(
            context.request,
            job_url,
            lambda job: job["status"] in {"running", "success", "failed", "cancelled"},
            timeout=120,
        )
        assert active["status"] in {"running", "success"}, active
        page.reload()
        expect(page.locator("#job-logs")).to_be_visible(timeout=30_000)
        if page.locator(".actions-run-heading .ci-status-running").count():
            screenshot = "running.png"
        else:
            expect(page.locator(".actions-run-heading .actions-status-chip.ci-status-success")).to_be_visible(timeout=30_000)
            screenshot = "completed-early.png"
        page.screenshot(path=str(args.output_dir / screenshot), full_page=True)

        page.get_by_role("link", name="← Workflow runs", exact=True).click()
        row = page.locator(f'.actions-run-row[href*="job={run_id}"]')
        expect(row).to_be_visible(timeout=30_000)
        row.click()
        expect(page.locator("#job-logs")).to_be_visible(timeout=30_000)
        page.goto(deep_url)
        expect(page.locator("#job-logs")).to_be_visible(timeout=30_000)

        # These lines are emitted over time by the workflow itself. Seeing them in
        # the database and in the refreshed page proves real runner progress.
        wait_for_log(context.request, logs_url, "LIVE_TICK 03/15")
        expect(page.locator("#job-logs")).to_contain_text("LIVE_TICK 03/15", timeout=30_000)
        page.reload()
        expect(page.locator("#job-logs")).to_contain_text("LIVE_TICK 03/15", timeout=30_000)
        wait_for_log(context.request, logs_url, "LIVE_TICK 10/15")

        final_job = wait_for_job(
            context.request,
            job_url,
            lambda job: job["status"] in {"success", "failed", "cancelled"},
            timeout=120,
        )
        assert final_job["status"] == "success", final_job
        persisted = wait_for_log(context.request, logs_url, "LIVE_CI_PASSED", timeout=30)
        ticks = []
        for line in persisted:
            match = re.search(r"LIVE_TICK (\d+)/15", line["line"])
            if match:
                ticks.append((int(match.group(1)), line.get("id"), line.get("timestamp")))
        tick_numbers = {number for number, _, _ in ticks}
        assert {3, 10}.issubset(tick_numbers) and len(tick_numbers) >= 10, ticks
        tick_ids = [line_id for _, line_id, _ in ticks if line_id is not None]
        assert not tick_ids or len(tick_ids) == len(set(tick_ids)), "Tick log ids are not unique"

        # A fresh document must retain the terminal state and the real output.
        page.goto(deep_url)
        expect(page.locator(".actions-run-heading .actions-status-chip.ci-status-success")).to_be_visible(timeout=30_000)
        expect(page.locator("#job-logs")).to_contain_text("LIVE_CI_PASSED")
        finish = page.locator("#job-logs summary").filter(has_text="Finish")
        expect(finish).to_be_visible()
        if finish.get_attribute("aria-expanded") != "true":
            finish.click()
        expect(page.locator(".actions-log-text", has_text="LIVE_CI_PASSED").last).to_be_visible()
        page.screenshot(path=str(args.output_dir / "completed.png"), full_page=True)
        after_reload = read_logs(context.request, logs_url)
        assert any("LIVE_CI_PASSED" in line["line"] for line in after_reload)
        assert len(after_reload) >= len(persisted), "Persisted log history shrank after completion"
        assert not browser_errors, browser_errors

        print(
            "PASS: real native dispatch, prompt running/deep-link capture, persisted live ticks, "
            "terminal success, and final logs after a fresh page load."
        )
        print("Run URL:", deep_url)
        print("Screenshots:", args.output_dir)
    except Exception:
        if "page" in locals():
            page.screenshot(path=str(args.output_dir / "failure.png"), full_page=True)
        if run_id:
            print("Run at failure:", context.request.get(base + "/api/ci/jobs/" + run_id).text())
            print("Logs at failure:", context.request.get(base + "/api/ci/jobs/" + run_id + "/logs").text())
        raise
    finally:
        browser.close()
