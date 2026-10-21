#!/usr/bin/env python3
"""Build fixture releases and inspect every shipped image layer, not just the rootfs."""
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parent.parent
PODMAN = shutil.which("podman") or "/run/current-system/sw/bin/podman"


class ReleaseImageTests(unittest.TestCase):
    def test_only_installed_runtime_files_are_shipped(self):
        for with_closure in (False, True):
            with self.subTest(with_closure=with_closure), tempfile.TemporaryDirectory() as work:
                root = Path(work)
                release = root / "dist/release"
                (release / "webui/assets").mkdir(parents=True)
                (release / "codeyard").write_text("#!/bin/sh\nprintf 'fixture binary\\n'\n")
                (release / "webui/index.html").write_text(
                    '<script type="module" src="/assets/fixture.js"></script>'
                )
                (release / "webui/assets/fixture.js").write_text("/* fixture bundle */")
                closure = "nix/store/fixture-runtime/lib/fixture.so"
                with tarfile.open(release / "runtime-nix.tar.gz", "w:gz") as archive:
                    if with_closure:
                        data = b"fixture runtime library"
                        member = tarfile.TarInfo(closure)
                        member.size = len(data)
                        archive.addfile(member, io.BytesIO(data))
                files = ["codeyard", "runtime-nix.tar.gz", "webui/index.html", "webui/assets/fixture.js"]
                checksums = subprocess.check_output(["sha256sum", *files], cwd=release)
                (release / "SHA256SUMS").write_bytes(checksums)
                shutil.copy(ROOT / "Containerfile", root)
                image = f"localhost/codeyard-release-test:{uuid.uuid4().hex}"
                try:
                    subprocess.run([
                        PODMAN, "build", "-f", "Containerfile", "-t", image, "."
                    ], cwd=root, check=True)
                    config = json.loads(subprocess.check_output([PODMAN, "image", "inspect", image]))[0]["Config"]
                    self.assertEqual(config["Entrypoint"], ["/usr/bin/tini", "--"])
                    subprocess.run([PODMAN, "run", "--rm", image], check=True)
                    subprocess.run([
                        PODMAN, "run", "--rm", image, "/bin/sh", "-c",
                        "(sleep .2 & echo $! > /tmp/orphan) & wait; "
                        "pid=$(cat /tmp/orphan); "
                        "for i in 1 2 3 4 5 6 7 8 9 10; do "
                        "kill -0 $pid 2>/dev/null || exit 0; sleep .1; done; exit 1",
                    ], check=True)
                    saved = root / "image.tar"
                    subprocess.run([
                        PODMAN, "save", "--format", "docker-archive", "-o", str(saved), image
                    ], check=True)
                    expected = {
                        "usr/local/bin/codeyard": (release / "codeyard").read_bytes(),
                        "app/webui/dist/index.html": (release / "webui/index.html").read_bytes(),
                        "app/webui/dist/assets/fixture.js": (release / "webui/assets/fixture.js").read_bytes(),
                    }
                    if with_closure:
                        expected[closure] = b"fixture runtime library"
                    counts = dict.fromkeys(expected, 0)
                    with tarfile.open(saved) as archive:
                        manifest = json.load(archive.extractfile("manifest.json"))
                        for layer in manifest[0]["Layers"]:
                            with tarfile.open(fileobj=archive.extractfile(layer)) as contents:
                                for member in contents:
                                    path = member.name.removeprefix("./").lstrip("/")
                                    self.assertFalse(path.startswith("opt/codeyard-release"), path)
                                    self.assertNotEqual(Path(path).name, "runtime-nix.tar.gz")
                                    if path in expected and member.isfile():
                                        counts[path] += 1
                                        self.assertEqual(contents.extractfile(member).read(), expected[path])
                                        if path == "usr/local/bin/codeyard":
                                            self.assertEqual(member.mode & 0o777, 0o755)
                    self.assertEqual(counts, dict.fromkeys(expected, 1))
                    # Verification still rejects a damaged release before installing it.
                    (release / "codeyard").write_text("damaged binary")
                    refused = subprocess.run([
                        PODMAN, "build", "-f", "Containerfile", "."
                    ], cwd=root, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
                    self.assertNotEqual(refused.returncode, 0, refused.stdout.decode())
                    self.assertIn(b"codeyard: FAILED", refused.stdout)
                finally:
                    subprocess.run([PODMAN, "rmi", "-f", image], check=False)


if __name__ == "__main__":
    unittest.main()
