#!/usr/bin/env python3
"""The UI gallery server must not read a checkout .env or touch a real database."""
from pathlib import Path
import ast
from unittest.mock import Mock
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / "scripts/capture-ui-gallery.py"

# Stands in for the server binary. It loads .env the way dotenvy does: the nearest file
# from the working directory upward, without overriding variables already set.
FAKE_SERVER = textwrap.dedent("""\
    #!/usr/bin/env python3
    import json, os, sys
    from pathlib import Path

    for directory in [Path.cwd(), *Path.cwd().parents]:
        env_file = directory / ".env"
        if env_file.is_file():
            for line in env_file.read_text().splitlines():
                key, sep, value = line.partition("=")
                if sep and key not in os.environ:
                    os.environ[key] = value
            break
    record = {"cwd": str(Path.cwd()), "env_file_in_cwd": Path(".env").is_file(),
              "database_url": os.environ.get("DATABASE_URL")}
    Path(__file__).resolve().with_name("record.json").write_text(json.dumps(record))
    sys.exit(1)
""")


class GalleryServerEnvironment(unittest.TestCase):
    def test_ban_capture_uses_account_menu_fragment_and_cancel_link(self):
        tree = ast.parse(SCRIPT.read_text())
        function = next(node for node in tree.body if isinstance(node, ast.FunctionDef)
                        and node.name == "open_ban_confirmation")
        namespace = {}
        exec(compile(ast.Module(body=[function], type_ignores=[]), str(SCRIPT), "exec"), namespace)
        page = Mock()
        base = "http://fixture"
        page.goto.side_effect = lambda url, **kwargs: setattr(page, "url", url)
        page.get_by_role.return_value.click.side_effect = lambda: setattr(page, "url", base + "/admin/users#ban-account-2")
        dialog = page.get_by_role.return_value
        dialog.locator.return_value.get_by_role.return_value.click.side_effect = lambda: setattr(page, "url", base + "/admin/users")
        namespace["open_ban_confirmation"](page, base)
        page.goto.assert_called_once_with(base + "/admin/users", wait_until="domcontentloaded")
        self.assertEqual(page.get_by_label.call_count, 2)
        page.get_by_label.assert_called_with("Actions for bob", exact=True)
        self.assertEqual([call.args for call in page.get_by_role.call_args_list],
                         [("link",), ("dialog",), ("link",), ("dialog",)])
        self.assertEqual([call.kwargs["name"] for call in page.get_by_role.call_args_list],
                         ["Ban account", "Ban bob", "Ban account", "Ban bob"])
        dialog.locator.return_value.get_by_role.assert_called_once_with("link", name="Cancel", exact=True)
        page.keyboard.press.assert_not_called()

    def test_server_uses_fixture_env_and_database_not_the_checkout(self):
        with tempfile.TemporaryDirectory() as scratch:
            work = Path(scratch).resolve()
            (work / "bin").mkdir()
            (work / "bundle").mkdir()
            (work / "bundle" / "index.html").write_text("<!doctype html>\n")
            stubs = work / "stubs" / "playwright"
            stubs.mkdir(parents=True)
            (stubs / "__init__.py").write_text("")
            (stubs / "sync_api.py").write_text("def sync_playwright():\n    raise RuntimeError('not used')\n")
            # A checkout .env above the binary's directory names a real installation.
            (work / ".env").write_text("DATABASE_URL=sqlite:///real-installation/gitserv.db\n")
            binary = work / "bin" / "codeyard"
            binary.write_text(FAKE_SERVER)
            binary.chmod(0o755)

            subprocess.run(
                [sys.executable, str(SCRIPT), "--binary", str(binary), "--bundle", str(work / "bundle"),
                 "--output", str(work / "out"), "--chromium", sys.executable],
                env={**os.environ, "PYTHONPATH": str(stubs.parent)},
                capture_output=True, text=True, timeout=120,
            )
            record_path = binary.with_name("record.json")
            self.assertTrue(record_path.is_file(), "the fake server never started")
            record = json.loads(record_path.read_text())

            self.assertTrue(record["cwd"].startswith(str(work / "bin" / "gallery-fixture-")), record["cwd"])
            self.assertTrue(record["env_file_in_cwd"], "the fixture needs an empty .env so dotenvy stops there")
            self.assertEqual(record["database_url"], f"sqlite://{record['cwd']}/state/gitserv.db?mode=rwc")


if __name__ == "__main__":
    unittest.main()
