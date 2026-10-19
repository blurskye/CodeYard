#!/usr/bin/env python3
"""Disposable regressions for bundle publishing, CI networking and E2E setup."""
import os
from pathlib import Path
import shutil
import socket
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
                                text=True, capture_output=True)
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
        public.mkdir(parents=True)
        # Canonical equality must also work through an existing parent symlink.
        (self.work / "alias").symlink_to(self.work / "target", target_is_directory=True)
        for out in [public, self.work / "alias/dx/codeyard/release/web/public",
                    self.work / "dist/webui"]:
            self.env["WEBUI_DIST"] = str(out)
            self.shell("bash scripts/build-webui.sh release")
            self.assertEqual((out / "index.html").read_text(), "bundle")
        for out in [public.parent, public / "nested", self.work / "target", self.work / "alias"]:
            self.env["WEBUI_DIST"] = str(out)
            self.shell("bash scripts/build-webui.sh release", success=False)
            self.assertEqual((public / "index.html").read_text(), "bundle")

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
