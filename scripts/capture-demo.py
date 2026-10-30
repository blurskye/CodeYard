#!/usr/bin/env python3
"""Record GIFs of real flows through the forge: the README demo and a set for visual review.

Run after building on the VPS and copying its binary/bundle locally, with codeyard-actions
beside the binary and the trusted runner from scripts/start-ci-runner.sh:
  uv run --no-project --with playwright python scripts/capture-demo.py \
    --binary dist/demo/codeyard --bundle dist/demo/webui --out-dir /tmp/gifs [--flows demo,issue]

The "demo" and "admin" flows are the README demos: --flows demo,admin --out-dir branding writes branding/demo.gif and branding/admin.gif. Server state is disposable; only
the GIFs are kept. A step that cannot find its element is reported and the flow goes on.
"""
import argparse
import base64
import json
import re
import os
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parent.parent
HELPER = ROOT / "scripts/e2e-web.py"
DESKTOP = {"width": 1600, "height": 900}
PHONE = {"width": 390, "height": 844}

WORKFLOW = """name: CI
on:
  push:
  workflow_dispatch:
jobs:
  test:
    name: Build and test
    runs-on: ubuntu-latest
    steps:
      - name: Build
        run: |
          for part in parser server web; do echo "Compiling $part"; sleep 1; done
      - name: Test
        run: |
          for n in 1 2 3 4 5 6; do echo "test suite $n ... ok"; sleep 1; done
      - name: Done
        run: echo "All checks passed"
"""

# A visible arrow pointer, since headless recordings have none. Its tip is the mouse position,
# and it keeps its place across pages.
CURSOR = """
addEventListener('DOMContentLoaded', () => {
  const arrow = document.createElement('div');
  arrow.innerHTML = '<svg width="20" height="28" viewBox="0 0 20 28"><path d="M1.5 1.5v21.2l5.1-5 3.3 7.6 3.4-1.5-3.3-7.4h7.1z" '
    + 'fill="#000" stroke="#fff" stroke-width="1.5" stroke-linejoin="round"/></svg>';
  arrow.style.cssText = 'position:fixed;z-index:2147483647;width:20px;height:28px;margin:-1px 0 0 -1px;'
    + 'pointer-events:none;filter:drop-shadow(0 1px 2px rgba(0,0,0,.35));transform-origin:1px 1px;'
    + 'transition:transform .1s;left:50%;top:50%';
  const saved = (sessionStorage.getItem('cursor') || '').split(',');
  if (saved.length === 2) { arrow.style.left = saved[0] + 'px'; arrow.style.top = saved[1] + 'px'; }
  document.body.appendChild(arrow);
  addEventListener('mousemove', e => { arrow.style.left = e.clientX + 'px'; arrow.style.top = e.clientY + 'px';
    sessionStorage.setItem('cursor', e.clientX + ',' + e.clientY); });
  addEventListener('mousedown', () => arrow.style.transform = 'scale(.85)');
  addEventListener('mouseup', () => arrow.style.transform = '');
});
"""

# Scrolls the page by a distance with an eased animation, so a recording never jumps.
SMOOTH_SCROLL = """
([dy, ms]) => new Promise(done => {
  const start = scrollY, begin = performance.now();
  const step = now => {
    const t = Math.min(1, (now - begin) / ms), ease = t < .5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2;
    scrollTo(0, start + dy * ease);
    t < 1 ? requestAnimationFrame(step) : done();
  };
  requestAnimationFrame(step);
})
"""


def ports():
    with socket.socket() as http, socket.socket() as ssh:
        http.bind(("127.0.0.1", 0))
        ssh.bind(("127.0.0.1", 0))
        return http.getsockname()[1], ssh.getsockname()[1]


class Forge:
    """The disposable server and the API calls that seed it."""

    def __init__(self, base, fixture):
        self.base, self.fixture = base, fixture

    def request(self, command, *options):
        result = subprocess.run([sys.executable, "-I", str(HELPER), command, "--base", self.base,
                                 "--jar", str(self.fixture / "jar.json"), "--who", "alice", *options],
                                cwd=ROOT, text=True, capture_output=True, timeout=80)
        if result.returncode:
            sys.exit(f"{command} {' '.join(options[:3])}: {result.stderr[-1500:]}")
        return result.stdout.strip()

    def api(self, method, path, data, status="200", printed=None):
        opts = ["req", method, path, "--accept", "json", "--status", status, "--json", json.dumps(data)]
        return self.request(*opts, *(["--print", "json:" + printed] if printed else []))

    def commit(self, repo, files, message, branch="main", expected="unborn", update=False):
        method, status, sha = ("PUT", "200", "commit.sha") if update else ("POST", "201", "sha")
        for path, content in files:
            expected = self.api(method, f"/api/repos/alice/{repo}/contents/{path}",
                                {"content": base64.b64encode(content.encode()).decode(), "message": message,
                                 "branch": branch, "expected_revision": expected}, status, sha)
        return expected

    def seed(self):
        password = "Demo-" + secrets.token_urlsafe(15)
        self.request("form", "/setup", "--action", "/setup", "--set", "username=alice",
                     "--set", "email=alice@example.test", "--set", f"password={password}",
                     "--set", f"confirm_password={password}", "--set", "node_mode=public")
        for name, description in (("docs-site", "Handbook and guides for the team"),
                                  ("infra", "Deployment scripts and server configuration"),
                                  ("hello-codeyard", "A small Rust service with CI on every push")):
            self.api("POST", "/api/repos", {"name": name, "description": description, "is_private": False}, "201")
        self.commit("docs-site", [("README.md", "# Docs site\n\nThe team handbook.\n")], "Start the handbook")
        self.commit("infra", [("README.md", "# Infra\n\nHow we deploy.\n")], "Add deployment notes")
        head = self.commit("hello-codeyard", [
            ("README.md", "# hello-codeyard\n\nA tiny Rust service, built and tested by Codeyard Actions.\n\n"
                          "```sh\ncargo run\n```\n"),
            ("src/main.rs", 'fn main() {\n    println!("Hello from Codeyard!");\n}\n'),
            ("Cargo.toml", '[package]\nname = "hello-codeyard"\nversion = "0.1.0"\nedition = "2021"\n'),
            (".github/workflows/ci.yml", WORKFLOW),
        ], "Add the service and its CI workflow")
        repo = "/api/repos/alice/hello-codeyard"
        self.api("POST", repo + "/tags", {"name": "v0.1.0", "sha": head, "message": "First release"})
        self.api("POST", repo + "/branches", {"name": "greeting", "sha": head})
        self.commit("hello-codeyard", [("src/main.rs", 'fn main() {\n    println!("Hello, friendly Codeyard!");\n}\n')],
                    "Make the greeting friendlier", "greeting", head, update=True)
        self.api("POST", repo + "/issues", {"title": "Document the configuration",
                                            "body": "List every environment variable the service reads."}, "201")
        # Teammates for the users page. Only an approval-based node creates accounts for people.
        self.request("form", "/admin/settings", "--action", "/admin/settings", "--has", "node_mode",
                     "--set", "node_mode=private")
        for name in ("bob", "carol"):
            self.request("form", "/admin/users/new", "--action", "/admin/users",
                         "--set", f"username={name}", "--set", f"email={name}@example.test",
                         "--set", "password=Demo-" + secrets.token_urlsafe(15))


class Recorder:
    """One flow: a fresh browser context whose video becomes one GIF."""

    def __init__(self, browser, base, cookies, viewport, theme=None):
        self.base, self.viewport, self.problems = base, viewport, []
        self.video_dir = Path(tempfile.mkdtemp(prefix="demo-video-"))
        self.context = browser.new_context(viewport=viewport, device_scale_factor=1,
                                           record_video_dir=str(self.video_dir), record_video_size=viewport,
                                           is_mobile=viewport is PHONE, has_touch=viewport is PHONE)
        jar = [{"name": k, "value": v, "url": base} for k, v in cookies.items() if not k.startswith("_")]
        if theme:
            jar.append({"name": "theme", "value": theme, "url": base})
        self.context.add_cookies(jar)
        self.context.add_init_script(CURSOR)
        self.started = time.monotonic()
        self.page = self.context.new_page()
        self.trim = 0.0

    def open(self, path, pause=1500):
        self.page.goto(self.base + path, wait_until="load")
        self.page.evaluate("() => document.fonts.ready")
        if not self.trim:
            # Drop the blank frames from before the first page painted.
            self.trim = time.monotonic() - self.started
        self.page.wait_for_timeout(pause)

    def step(self, what, action):
        try:
            action()
        except Exception as error:  # noqa: BLE001 - report the step and keep recording
            self.problems.append(f"{what}: {str(error).splitlines()[0][:160]}")

    def click(self, locator, pause=1200):
        # Menus keep hidden copies of many links; use the one on screen.
        target = locator.filter(visible=True).first
        target.wait_for(state="visible", timeout=5000)
        self.reveal(target)
        box = target.bounding_box()
        self.page.mouse.move(box["x"] + box["width"] / 2, box["y"] + box["height"] / 2, steps=20)
        self.page.wait_for_timeout(200)
        target.click()
        self.page.wait_for_load_state("load")
        self.page.wait_for_timeout(pause)

    def link(self, name, pause=1200):
        self.click(self.page.get_by_role("link", name=name), pause)

    def type(self, locator, text):
        target = locator.first
        self.click(target, 200)
        target.press_sequentially(text, delay=25)
        self.page.wait_for_timeout(400)

    def glide(self, pixels):
        if pixels:
            self.page.evaluate(SMOOTH_SCROLL, [pixels, min(1200, 350 + abs(pixels))])

    def reveal(self, target):
        """Scrolls smoothly until the target sits in the middle of the screen, if it is off screen."""
        box = target.bounding_box()
        height = self.viewport["height"]
        if box and (box["y"] < 60 or box["y"] + box["height"] > height - 20):
            self.glide(round(box["y"] + box["height"] / 2 - height / 2))
        target.scroll_into_view_if_needed()

    def scroll(self, pixels, pause=900):
        self.glide(pixels)
        self.page.wait_for_timeout(pause)

    def finish(self, output, hold=2500):
        try:
            self.page.wait_for_timeout(hold)
            video = Path(self.page.video.path())
            self.context.close()
            to_gif(video, output, self.trim, self.viewport["width"])
        finally:
            self.context.close()
            shutil.rmtree(self.video_dir, ignore_errors=True)
        return self.problems


def flow_actions(r):
    r.open("/", 1800)
    r.step("open repository", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard"]'), 1800))
    r.step("scroll readme", lambda: (r.scroll(300), r.scroll(-300, 500)))
    r.step("actions tab", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/actions"]'), 1500))
    dispatch = r.page.locator(".actions-dispatch")
    r.step("open run menu", lambda: r.click(dispatch.locator("summary"), 900))
    r.step("pick workflow", lambda: dispatch.locator('select[name="workflow_path"]').select_option(label="CI"))
    r.step("run workflow", lambda: r.click(dispatch.get_by_role("button", name="Run workflow", exact=True), 0))
    done = r.page.locator(".actions-run-heading .actions-status-chip.ci-status-success")
    deadline = time.monotonic() + 240
    while not done.count() and time.monotonic() < deadline:
        r.page.wait_for_timeout(1000)
    if not done.count():
        r.problems.append("the run did not show success within four minutes")


def flow_new_repository(r):
    r.open("/")
    r.step("new repository", lambda: r.click(r.page.locator('a[href="/new"]'), 1500))
    r.step("name", lambda: r.type(r.page.locator("#repo-name"), "weekend-project"))
    r.step("description", lambda: r.type(r.page.locator("#repo-description"), "Something fun to build"))
    r.step("create", lambda: r.click(r.page.get_by_role("button", name="Create repository"), 2500))
    r.step("scroll setup", lambda: (r.scroll(400), r.scroll(-400)))


def flow_browse(r):
    r.open("/alice/hello-codeyard")
    r.step("src folder", lambda: r.link("src"))
    r.step("main.rs", lambda: r.link("main.rs", 1800))
    r.step("back to code", lambda: r.link("hello-codeyard"))
    r.step("history", lambda: r.click(r.page.get_by_role("link", name="History"), 1800))
    r.step("a commit", lambda: r.click(r.page.locator('a[href*="/commit/"]'), 2000))
    r.step("back to code", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard"]'), 1200))
    r.step("branches", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/branches"]'), 1800))
    r.step("back to code", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard"]'), 1200))
    r.step("tags", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/tags"]'), 1800))
    r.step("releases", lambda: r.open("/alice/hello-codeyard/releases", 1800))


def flow_edit_file(r):
    r.open("/alice/hello-codeyard/blob/main/README.md")
    r.step("edit", lambda: r.click(r.page.locator('a[href*="/edit/"]'), 1500))
    editor = r.page.locator("#repository-file-content")
    r.step("type", lambda: (r.click(editor, 200), editor.press("Control+End"),
                            editor.press_sequentially("\n## Configuration\n\nSet PORT to choose the port.\n", delay=25)))
    r.step("message", lambda: r.type(r.page.locator('input[name="message"]'), " (configuration)"))
    r.step("commit", lambda: r.click(r.page.get_by_role("button", name="Commit changes"), 2000))
    r.step("scroll", lambda: (r.scroll(300), r.scroll(-300)))


def flow_issue(r):
    r.open("/alice/hello-codeyard")
    r.step("issues tab", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/issues"]'), 1500))
    r.step("new issue", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/issues/new"]'), 1500))
    r.step("title", lambda: r.type(r.page.locator("#new-title"), "The greeting should mention the user"))
    r.step("body", lambda: r.type(r.page.locator('textarea[name="body"]'), "Say hello to whoever runs it, by name."))
    r.step("create", lambda: r.click(r.page.locator('button[name="intent"][value="create"]:not(.sr-only)'), 2000))
    r.step("comment", lambda: r.type(r.page.locator('textarea[name="body"]').last, "I can take this one."))
    r.step("send comment", lambda: r.click(r.page.get_by_role("button", name="Comment", exact=True), 2000))
    r.step("close", lambda: r.click(r.page.get_by_role("button", name="Close issue"), 2000))


def flow_pull_request(r):
    r.open("/alice/hello-codeyard")
    r.step("pulls tab", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/pulls"]'), 1500))
    r.step("new pull", lambda: r.click(r.page.locator('a[href^="/alice/hello-codeyard/pulls/new"], a[href^="/alice/hello-codeyard/compare"]'), 1500))
    r.step("compare branch", lambda: r.open("/alice/hello-codeyard/pulls/new?base=main&head=greeting", 1500))
    r.step("title", lambda: r.type(r.page.locator("#new-title"), "Make the greeting friendlier"))
    r.step("create", lambda: r.click(r.page.locator('button[name="intent"][value="create"]:not(.sr-only)'), 2000))
    r.step("files changed", lambda: r.click(r.page.locator('a[href$="/diff"]'), 2000))
    r.step("conversation", lambda: r.click(r.page.get_by_role("link", name=re.compile("^Conversation")), 1500))
    r.step("scroll to merge", lambda: r.reveal(r.page.get_by_role("button", name="Merge pull request")))
    r.step("merge", lambda: r.click(r.page.get_by_role("button", name="Merge pull request"), 2500))


def flow_admin(r):
    r.open("/admin")
    link = lambda name: r.page.locator("nav.identity-settings-nav").get_by_role("link", name=name, exact=True)
    r.step("admin Users", lambda: r.click(link("Users"), 1500))
    r.step("row menu", lambda: r.click(r.page.locator(".identity-row-menu summary").first, 1500))
    r.step("close menu", lambda: r.click(r.page.locator(".identity-row-menu summary").first, 600))
    r.step("administrators", lambda: r.click(r.page.locator(".identity-admin-filter").nth(1), 1200))
    r.step("all accounts", lambda: r.click(r.page.locator(".identity-admin-filter").first, 1000))
    r.step("new user", lambda: r.click(r.page.get_by_role("link", name="New user", exact=True), 1500))
    for name in ("Site settings", "Email", "Audit log", "Nodes", "Storage", "Overview"):
        r.step(f"admin {name}", lambda name=name: r.click(link(name), 1500))


def flow_settings(r):
    r.open("/settings")
    for pixels in (500, 500, 500, -1500):
        r.step("scroll settings", lambda pixels=pixels: r.scroll(pixels, 1000))
    r.step("profile", lambda: r.open("/alice", 1800))
    r.step("notifications", lambda: r.open("/notifications", 1500))
    r.step("explore", lambda: r.open("/explore", 1500))


def flow_dark(r):
    r.open("/")
    r.step("repository", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard"]'), 1500))
    r.step("actions", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/actions"]'), 1500))
    r.step("a run", lambda: r.click(r.page.locator(".actions-run-row"), 2000))
    r.step("issues", lambda: r.open("/alice/hello-codeyard/issues", 1500))
    r.step("pulls", lambda: r.open("/alice/hello-codeyard/pulls", 1500))


def flow_phone(r):
    r.open("/")
    r.step("repository", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard"]'), 1500))
    r.step("scroll", lambda: (r.scroll(500), r.scroll(-500)))
    r.step("more menu", lambda: r.click(r.page.locator(".repo-tabs-more summary"), 1200))
    r.step("actions", lambda: r.click(r.page.locator('a[href="/alice/hello-codeyard/actions"]'), 1500))
    r.step("a run", lambda: r.click(r.page.locator(".actions-run-row"), 2000))
    r.step("site menu", lambda: r.click(r.page.locator("header summary, header button").first, 1500))
    r.step("admin", lambda: r.open("/admin", 1500))
    r.step("admin sections", lambda: r.click(r.page.locator(".identity-settings-picker summary"), 1500))


FLOWS = {
    "demo": (flow_actions, DESKTOP, None),
    "new-repository": (flow_new_repository, DESKTOP, None),
    "browse": (flow_browse, DESKTOP, None),
    "edit-file": (flow_edit_file, DESKTOP, None),
    "issue": (flow_issue, DESKTOP, None),
    "pull-request": (flow_pull_request, DESKTOP, None),
    "admin": (flow_admin, DESKTOP, None),
    "settings": (flow_settings, DESKTOP, None),
    "dark": (flow_dark, DESKTOP, "dark"),
    "phone": (flow_phone, PHONE, None),
}


def to_gif(video, output, trim, width):
    ffmpeg = shutil.which("ffmpeg") or sys.exit("ffmpeg is required")
    palette = video.with_suffix(".png")
    scale = f"fps=10,scale={width}:-1:flags=lanczos"
    start = ["-ss", f"{max(trim - 0.1, 0):.2f}"]
    subprocess.run([ffmpeg, "-y", "-loglevel", "error", *start, "-i", str(video), "-vf",
                    scale + ",palettegen=stats_mode=diff", str(palette)], check=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([ffmpeg, "-y", "-loglevel", "error", *start, "-i", str(video), "-i", str(palette), "-lavfi",
                    scale + "[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
                    "-loop", "0", str(output)], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--bundle", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--flows", default=",".join(FLOWS))
    parser.add_argument("--socket", default=os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/run/user/1000"), "codeyard-ci/podman.sock"))
    parser.add_argument("--chromium", default=os.environ.get("CHROMIUM_EXECUTABLE"))
    args = parser.parse_args()
    flows = [name.strip() for name in args.flows.split(",") if name.strip()]
    if unknown := [name for name in flows if name not in FLOWS]:
        parser.error(f"unknown flows {unknown}; choose from {', '.join(FLOWS)}")
    binary, bundle = args.binary.resolve(), args.bundle.resolve()
    if not binary.is_file() or not (bundle / "index.html").is_file():
        parser.error("pass a VPS-built server binary and browser bundle")
    if not shutil.which("ffmpeg"):
        parser.error("ffmpeg is required before recording")
    args.socket = str(Path(args.socket).resolve())
    if not Path(args.socket).exists():
        parser.error(f"no runner socket at {args.socket}; start scripts/start-ci-runner.sh")
    # New repositories run their jobs on the in-house runner, which must sit beside the server.
    actions = binary.with_name("codeyard-actions")
    if not actions.is_file():
        parser.error(f"no codeyard-actions beside {binary}; build it with the server")

    with tempfile.TemporaryDirectory(prefix="demo-fixture-") as scratch:
        fixture = Path(scratch)
        (fixture / "home").mkdir()
        (fixture / "repos").mkdir()
        (fixture / ".env").touch()
        http_port, ssh_port = ports()
        base = f"http://127.0.0.1:{http_port}"
        env = {
            "PATH": os.environ.get("PATH", ""), "LANG": "C.UTF-8", "HOME": str(fixture / "home"),
            "DATABASE_URL": f"sqlite://{fixture / 'state' / 'gitserv.db'}?mode=rwc",
            "GITSERV_STATE_DIR": str(fixture / "state"), "REPOS_PATH": str(fixture / "repos"),
            "HOST_KEY_PATH": str(fixture / "host_key"), "HTTP_ADDR": f"127.0.0.1:{http_port}",
            "SSH_ADDR": f"127.0.0.1:{ssh_port}", "SSH_MODE": "builtin", "PUBLIC_URL": base,
            "PUBLIC_HTTP_CLONE_BASE": base, "COOKIE_SECURE": "false", "WEBUI_DIST": str(bundle),
            "RUST_LOG": "warn", "CI_EXECUTION_MODE": "podman", "CI_PODMAN_SOCKET": args.socket,
            "CI_ACTIONS_BINARY": str(actions),
        }
        with (fixture / "server.log").open("wb") as log:
            proc = subprocess.Popen([str(binary), "serve"], cwd=fixture, env=env, stdin=subprocess.DEVNULL,
                                    stdout=log, stderr=subprocess.STDOUT)
            try:
                for _ in range(120):
                    if proc.poll() is not None:
                        sys.exit(f"server exited: {(fixture / 'server.log').read_text()[-3000:]}")
                    try:
                        with urllib.request.build_opener(urllib.request.ProxyHandler({})).open(base + "/health", timeout=1) as response:
                            if response.status == 200:
                                break
                    except OSError:
                        time.sleep(.5)
                else:
                    sys.exit("server never became ready")
                forge = Forge(base, fixture)
                forge.seed()
                # Let the push runs finish so the recorded run page is the only active one.
                time.sleep(25)
                cookies = json.loads((fixture / "jar.json").read_text())
                with sync_playwright() as pw:
                    options = {"headless": True}
                    if args.chromium:
                        options["executable_path"] = args.chromium
                    browser = pw.chromium.launch(**options)
                    for name in flows:
                        flow, viewport, theme = FLOWS[name]
                        recorder = Recorder(browser, base, cookies, viewport, theme)
                        flow(recorder)
                        output = args.out_dir.resolve() / f"{name}.gif"
                        problems = recorder.finish(output)
                        print(f"{output} {output.stat().st_size // 1024} KiB", flush=True)
                        for problem in problems:
                            print(f"  step failed: {problem}", flush=True)
                    browser.close()
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=10)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    main()
