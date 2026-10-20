#!/usr/bin/env python3
"""Disposable regressions for bundle publishing, CI networking and E2E setup."""
import ast
import os
from pathlib import Path
import shutil
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


class Scripts(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.env = dict(os.environ)

    def shell(self, source, *, cwd=None, env=None, success=True):
        result = subprocess.run(["bash", "-euo", "pipefail", "-c", source],
                                cwd=cwd or self.work, env=env or self.env,
                                text=True, capture_output=True, timeout=120)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def executable(self, path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        path.chmod(0o755)
        return path

    def test_bundle_output_equality_aliases_and_ancestors(self):
        for name in ["scripts", "src/webui/assets"]:
            (self.work / name).mkdir(parents=True)
        for name in ["build-webui.sh", "check-web-dependencies.py"]:
            shutil.copy(ROOT / "scripts" / name, self.work / "scripts" / name)
        (self.work / "src/webui/assets/tailwind.css").write_text("css\n")
        self.env["TAILWIND"] = str(self.executable(self.work / "bin/tailwind", '''#!/usr/bin/env bash
if [[ $1 == --help ]]; then echo v4.1.18; exit; fi
while (($#)); do
    if [[ $1 == -o ]]; then cp src/webui/assets/tailwind.css "$2"; exit; fi
    shift
done
'''))
        self.env["DX"] = str(self.executable(self.work / "bin/dx", '''#!/usr/bin/env bash
set -eu
if [[ $1 == --version ]]; then echo 0.7.10; exit; fi
out=$CARGO_TARGET_DIR/dx/codeyard/release/web/public
mkdir -p "$out"
printf bundle >"$out/index.html"
'''))
        self.executable(self.work / "bin/cargo", "#!/bin/sh\nprintf 'codeyard v1.0\ndioxus-web v0.7.10\n'\n")
        self.env["PATH"] = str(self.work / "bin") + ":" + self.env["PATH"]
        self.env["CARGO_TARGET_DIR"] = str(self.work / "target")
        public = self.work / "target/dx/codeyard/release/web/public"
        public.parent.mkdir(parents=True)
        # Canonical equality must also work through an existing parent symlink.
        (self.work / "alias").symlink_to(self.work / "target", target_is_directory=True)
        for out in [public, self.work / "alias/dx/codeyard/release/web/public",
                    self.work / "dist/webui"]:
            self.env["WEBUI_DIST"] = str(out)
            self.shell("bash scripts/build-webui.sh release")
            self.assertEqual((out / "index.html").read_text(), "bundle")
        # A published bundle can be rebuilt and replaced.
        self.shell("bash scripts/build-webui.sh release")
        for out in [public.parent, public / "nested", self.work / "target", self.work / "alias"]:
            self.env["WEBUI_DIST"] = str(out)
            self.shell("bash scripts/build-webui.sh release", success=False)
            self.assertEqual((public / "index.html").read_text(), "bundle")

        # Moving cargo output outside the checkout must not make the checkout deletable.
        self.env["CARGO_TARGET_DIR"] = str(self.work.parent / (self.work.name + "-target"))
        sentinel = self.work / "source-sentinel"
        sentinel.write_text("keep source")
        for out in [self.work, self.work.parent]:
            self.env["WEBUI_DIST"] = str(out)
            result = self.shell("bash scripts/build-webui.sh release", success=False)
            self.assertIn("checkout or its ancestor", result.stderr)
            self.assertEqual(sentinel.read_text(), "keep source")
        unrelated = self.work / "unrelated"
        unrelated.mkdir()
        (unrelated / "sentinel").write_text("keep data")
        self.env["WEBUI_DIST"] = str(unrelated)
        result = self.shell("bash scripts/build-webui.sh release", success=False)
        self.assertIn("owned bundle marker", result.stderr)
        self.assertEqual((unrelated / "sentinel").read_text(), "keep data")

    def test_documented_sqlite_backup_is_private_and_refuses_unsafe_destinations(self):
        docs = (ROOT / "docs/STORAGE_ARCHITECTURE.md").read_text()
        command = docs.split("```bash\n", 1)[1].split("```", 1)[0]
        source = self.work / "source.db"
        with sqlite3.connect(source) as database:
            database.execute("CREATE TABLE sessions(token TEXT)")
            database.execute("INSERT INTO sessions VALUES ('session-secret')")
        backup = self.work / "backup"
        destination = backup / "gitserv.db"
        command = command.replace("/path/to/gitserv.db", str(source)).replace("/backup/codeyard", str(backup))
        self.executable(self.work / "bin/sqlite3", f'''#!{sys.executable}
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as source:
    with sqlite3.connect(sys.argv[2].split("'")[1]) as destination:
        source.backup(destination)
''')
        self.env["PATH"] = str(self.work / "bin") + ":" + self.env["PATH"]
        self.shell("umask 022\n" + command)
        self.assertEqual(stat.S_IMODE(backup.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(destination.stat().st_mode), 0o600)
        with sqlite3.connect(destination) as database:
            self.assertEqual(database.execute("SELECT token FROM sessions").fetchone(), ("session-secret",))
        self.shell(command)  # An already private destination remains usable.
        destination.chmod(0o644)
        self.shell(command, success=False)
        destination.unlink()
        destination.symlink_to(source)
        self.shell(command, success=False)

    def test_workflow_uses_container_dns_and_dynamic_host_ports(self):
        workflow = (ROOT / ".github/workflows/ci.yml").read_text()
        self.assertIn("          - 5432\n", workflow)
        self.assertNotIn("5432:5432", workflow)
        step = workflow.split("      - name: Configure PostgreSQL\n", 1)[1].split("      - name:", 1)[0]
        source = "\n".join(line[10:] for line in step.splitlines()[1:])
        source = source.replace("${{ job.services.postgres.ports[5432] }}", "55432")
        for container, address in [(True, "postgres:5432"), (False, "127.0.0.1:55432")]:
            output = self.work / "github-env"
            output.write_text("")
            env = dict(self.env, GITHUB_ENV=str(output))
            # Substitute the two runtime marker paths with disposable fixtures.
            marker = self.work / "container-marker"
            if container:
                marker.touch()
            else:
                marker.unlink(missing_ok=True)
            script = source.replace("/.dockerenv", str(marker)).replace("/run/.containerenv", str(marker))
            self.shell(script, env=env)
            self.assertIn("@" + address + "/gitserv_test", output.read_text())

    def test_e2e_relative_target_is_absolute_before_changing_directory(self):
        script = (ROOT / "scripts/e2e.sh").read_text()
        settings = script.split("TARGET_DIR=", 1)[1].split("# ssh and ssh-keygen", 1)[0]
        self.executable(self.work / "target/release/codeyard", "#!/bin/sh\necho started\n")
        (self.work / "node").mkdir()
        self.shell('ROOT=$PWD; E2E_PROFILE=release; CARGO_TARGET_DIR=target\nTARGET_DIR=' + settings +
                   '\n[[ $BIN == /* ]]; cd node; [[ $("$BIN") == started ]]')

    def test_e2e_nodes_stop_dotenv_search_at_the_fixture(self):
        script = (ROOT / "scripts/e2e.sh").read_text()
        functions = script.split("server_dir()", 1)[1].split("cluster_cli()", 1)[0]
        binary = Path(self.env["CARGO_TARGET_DIR"]) / "debug/codeyard"
        self.assertTrue(binary.is_file(), str(binary))
        unrelated = self.work / "unrelated.sqlite"
        (self.work / ".env").write_text("DATABASE_URL=sqlite://" + str(unrelated) + "?mode=rwc\n")
        self.env["TEST_BINARY"] = str(binary)
        self.shell('''RUN=$PWD/fixture; mkdir -p "$RUN"
DATABASE_URL=; BASE=http://localhost; WEBUI_DIST=unused; ACT=unused
declare -A HTTP_PORTS=() SSH_PORTS=() IROH_PORTS=([ci1]=12345)
get() { echo unused; }
server_dir()''' + functions + '''
for node in server ci1; do
    configure_node "$node"
    [[ -f $RUN/.env && ! -s $RUN/.env ]]
    (cd "$NODE_DIR"; env -i "${NODE_ENV[@]}" "$TEST_BINARY" init)
done
''')
        self.assertFalse(unrelated.exists())

    def test_e2e_libpq_passwords_are_environment_only(self):
        script = (ROOT / "scripts/e2e.sh").read_text()
        function = "pg_command()" + script.split("pg_command()", 1)[1].split("# ---------------------------------------------------------------- build", 1)[0]
        tool = self.executable(self.work / "libpq", f'''#!{sys.executable}
import os, sys
assert os.environ['PGPASSWORD'] == 'p%40ss', os.environ.get('PGPASSWORD')
assert all('p%2540ss' not in arg and 'password=' not in arg for arg in sys.argv)
assert 'postgres://user@localhost/db?sslmode=disable' in sys.argv[1]
''')
        python = self.executable(self.work / "python", f'''#!/bin/sh
for arg do
    case "$arg" in *p%2540ss*) echo 'password leaked to Python argv' >&2; exit 1;; esac
done
exec {sys.executable} "$@"
''')
        self.env.update(PYTHON=str(python), TOOL=str(tool))
        for url in ["postgres://user:p%2540ss@localhost/db?sslmode=disable",
                    "postgres://user@localhost/db?sslmode=disable&password=p%2540ss"]:
            self.env["URL"] = url
            for flag in ["--maintenance-db", "--dbname"]:
                self.shell(function + '\npg_command "$URL" "$TOOL" ' + flag)

    def test_e2e_relative_act_remains_executable_in_a_workspace(self):
        source = (ROOT / "scripts/e2e.sh").read_text()
        settings = source.split("ACT=", 1)[1].split("TARGET_DIR=", 1)[0]
        self.executable(self.work / "tools/act", "#!/bin/sh\necho act\n")
        (self.work / "workspace").mkdir()
        self.shell('E2E_ACT_BINARY=./tools/act\nACT=' + settings +
                   '\n[[ $ACT == /* ]]; cd workspace; [[ $("$ACT") == act ]]')

    def test_database_cleanup_retains_failed_name_and_exit_status(self):
        source = (ROOT / "scripts/e2e.sh").read_text()
        functions = "drop_pg()" + source.split("drop_pg()", 1)[1].split("trap cleanup EXIT", 1)[0]
        result = self.shell(functions + '''
PG_DB=fixture
pg_tool() { echo dropdb; }
pg_command() { echo 'dropdb diagnostic' >&2; return 1; }
GITSERV_TEST_DATABASE_URL=unused
if drop_pg; then exit 1; fi
[[ $PG_DB == fixture ]]
pg_command() { return 0; }
drop_pg
[[ -z $PG_DB ]]
PG_DB=fixture
pg_command() { echo 'retry failed' >&2; return 1; }
terminate_servers() { :; }
WORK=$PWD/disposable; mkdir "$WORK"; E2E_KEEP=0
trap cleanup EXIT
''', success=False)
        self.assertIn("dropdb diagnostic", result.stderr)
        self.assertIn("retry failed", result.stderr)
        # Both ordinary rounds record drop_pg as an inline result before the summary.
        self.assertEqual(source.count("step -i 'disposable database removed' drop_pg"), 2)
        self.assertIn("prepare_database() {\n    drop_pg || return 1", source)

    def test_refusals_require_diagnostics_and_reject_timeouts_and_signals(self):
        source = (ROOT / "scripts/e2e.sh").read_text()
        function = "must_fail()" + source.split("must_fail()", 1)[1].split("# --------------------------------------------------------------- tooling", 1)[0]
        self.shell(function + '''
RUN=$PWD
refuse() { echo 'Permission denied (publickey).' >&2; return 128; }
must_fail key 'Permission denied \\(publickey\\)' refuse
for exit_code in 0 124 125 126 127 130 137 143; do
    bad_exit() { echo 'Permission denied (publickey).' >&2; return "$exit_code"; }
    if must_fail key 'Permission denied' bad_exit; then exit 1; fi
done
wrong_error() { echo 'Connection refused' >&2; return 128; }
if must_fail key 'Permission denied' wrong_error; then exit 1; fi
''')

    def test_readiness_log_consumers_drain_verbose_output(self):
        source = (ROOT / "scripts/e2e.sh").read_text()
        for message in ["starting codeyard", "database-free actions-worker ready", "CI worker could not claim a run"]:
            line = next(line.strip() for line in source.splitlines() if "tail -n" in line and message in line)
            consumer = line.rsplit("|", 1)[1].split(" &&", 1)[0].removesuffix("; do")
            log = self.work / "server.log"
            log.write_text(message + "\n" + "verbose output\n" * 100000)
            self.env["LOG"] = str(log)
            self.shell('tail -n +1 "$LOG" | ' + consumer)

    def test_openssh_helper_build_and_tests_share_each_vps_invocation(self):
        (self.work / "scripts").mkdir()
        shutil.copy(ROOT / "scripts/ci-local.sh", self.work / "scripts/ci-local.sh")
        self.executable(self.work / "scripts/vps", '''#!/usr/bin/env bash
set -euo pipefail
[[ $1 != --pg ]] || shift
rm -f "$CARGO_TARGET_DIR/debug/codeyard"
"$@"
''')
        self.executable(self.work / "bin/cargo", '''#!/usr/bin/env bash
set -euo pipefail
case $1 in
    build) mkdir -p "$CARGO_TARGET_DIR/debug"; touch "$CARGO_TARGET_DIR/debug/codeyard" ;;
    test) [[ -f $GITSERV_TEST_OPENSSH_HELPER ]] ;;
esac
''')
        for script in ["s3-tests.sh", "e2e.sh"]:
            (self.work / "scripts" / script).write_text("exit 0\n")
        self.env.update(CARGO_TARGET_DIR=str(self.work / "target"),
                        PATH=str(self.work / "bin") + ":" + self.env["PATH"])
        self.shell("bash scripts/ci-local.sh")

    def test_builder_fixture_failure_stops_owned_process_groups_before_removing_work(self):
        source = (ROOT / "scripts/test-builder-sync.sh").read_text()
        functions = source.split("# Each background fixture", 1)[1].split("export FAKE_BIN", 1)[0]
        functions = "# Each background fixture" + functions
        pid_file = self.work / "pids"
        self.env["PID_FILE"] = str(pid_file)
        self.shell('WORK=$PWD/fixture; mkdir "$WORK"\n' + functions + '''
bash -c 'sleep 100 & child=$!; echo "$$ $child" >"$PID_FILE"; wait' &
pid=$!; BACKGROUND[$pid]=1
for _ in {1..100}; do [[ -s $PID_FILE ]] && break; sleep .01; done
[[ -s $PID_FILE ]]
exit 1
''', success=False)
        self.assertFalse((self.work / "fixture").exists())
        for pid in map(int, pid_file.read_text().split()):
            stat = Path(f"/proc/{pid}/stat")
            if stat.exists():
                self.assertEqual(stat.read_text().split()[2], "Z", f"fixture process {pid} is running")

    def test_live_actions_success_selectors_only_match_the_status_chip(self):
        tree = ast.parse((ROOT / "scripts/test-actions-live.py").read_text())
        success_selectors = [node.args[0].value for node in ast.walk(tree)
                             if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute)
                             and node.func.attr == "locator" and node.args
                             and isinstance(node.args[0], ast.Constant)
                             and "ci-status-success" in str(node.args[0].value)]
        # The chip and its child SVG both carry ci-status-success; only the chip is unique.
        self.assertEqual(success_selectors,
                         [".actions-run-heading .actions-status-chip.ci-status-success"] * 2)

    def test_actions_showcase_uses_current_job_navigation_and_live_log_search(self):
        source = (ROOT / "scripts/test-actions-ui.py").read_text()
        self.assertIn('page.locator(\'.actions-sidebar a.actions-nav-item[href*="workflow_job="]\').first.click()', source)
        self.assertIn('page.get_by_placeholder("Search logs").fill("fixture text that is absent")', source)
        self.assertNotIn('name="All jobs"', source)
        self.assertNotIn('name="Filter logs"', source)
        self.assertIn('page.get_by_text("No log lines match your search.", exact=True)', source)

    def test_compose_forwards_file_cluster_configuration(self):
        # Check interpolation even when a Compose implementation is not installed on the builder.
        source = (ROOT / "compose.yaml").read_text()
        for name in ["NODE_KEY_FILE", "CLUSTER_NODES_FILE"]:
            self.assertIn(f"      {name}: ${{{name}:-}}", source)

    def test_vps_postgres_requires_the_random_password(self):
        script = (ROOT / "scripts/vps").read_text()
        assignment = "inner=" + script.split("\ninner=", 1)[1].split("\nremote()", 1)[0]
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        self.env.update(PG_BIN=str(Path(shutil.which("initdb")).parent), PG_PORT=str(port))
        self.shell(assignment + '''
bash -c "$inner" inner bash -euc '
    [[ $GITSERV_TEST_DATABASE_URL == postgres://cargo-builder:?*@* ]]
    [[ ${#PGPASSWORD} == 64 ]]
    psql -h 127.0.0.1 -p "$PG_PORT" -U cargo-builder -d gitserv_test -c "SELECT 1" >/dev/null
    if env -u PGPASSWORD psql -w -h 127.0.0.1 -p "$PG_PORT" -U cargo-builder -d gitserv_test -c "SELECT 1"; then
        echo "passwordless superuser connection succeeded" >&2; exit 1
    fi
'
''')


if __name__ == "__main__":
    unittest.main()
