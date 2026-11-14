#!/usr/bin/env python3
"""Check image-builder acceleration and failed-install cleanup without Windows media."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[2] / 'backend/runner/scripts/windows-base-image/build.sh'
HAS_KVM = os.access('/dev/kvm', os.R_OK | os.W_OK)


class WindowsImageTests(unittest.TestCase):
    def run_builder(self, allow_tcg, exit_code=23, ready=False):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / 'bin'
            tools.mkdir()
            fake = tools / 'qemu-test-helper'
            fake.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
if name == 'qemu-img':
    pathlib.Path(sys.argv[-2]).touch()
elif name == 'qemu-system-x86_64':
    pathlib.Path(os.environ['TEST_QEMU_ARGS']).write_text(json.dumps(sys.argv[1:]))
    if os.environ['TEST_QEMU_READY'] == '1':
        serial = sys.argv[sys.argv.index('-serial') + 1]
        pathlib.Path(serial.removeprefix('file:')).write_text('CODEYARD_IMAGE_READY\\n')
    sys.exit(int(os.environ['TEST_QEMU_EXIT']))
''')
            fake.chmod(0o700)
            for name in ('qemu-img', 'qemu-system-x86_64', 'xorriso'):
                (tools / name).symlink_to(fake.name)
            iso = root / 'test.iso'
            iso.write_bytes(b'Test fixture, not Windows media')
            output = root / 'base.qcow2'
            args_file = root / 'args.json'
            env = dict(os.environ, PATH=str(tools) + os.pathsep + os.environ['PATH'],
                       TEST_QEMU_ARGS=str(args_file), WINDOWS_ALLOW_TCG=allow_tcg,
                       TEST_QEMU_EXIT=str(exit_code), TEST_QEMU_READY='1' if ready else '0')
            result = subprocess.run(['bash', str(SCRIPT), '--accept-license', str(iso), str(output)],
                                    env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    timeout=30)
            args = json.loads(args_file.read_text()) if args_file.exists() else None
            published = ready and exit_code == 0 and (HAS_KVM or allow_tcg == '1')
            self.assertEqual(output.exists(), published)
            if published:
                self.assertEqual(output.stat().st_mode & 0o777, 0o400)
                self.assertEqual(output.stat().st_nlink, 1)
            self.assertEqual(list(root.glob('.codeyard-windows-build.*')), [])
            return result, args

    def test_clean_exit_without_readiness_does_not_publish(self):
        result, _ = self.run_builder('1', exit_code=0)
        self.assertEqual(result.returncode, 1, result.stderr.decode())
        self.assertIn(b'Provisioning did not complete', result.stderr)

    def test_ready_image_is_read_only_and_stage_is_removed(self):
        result, _ = self.run_builder('1', exit_code=0, ready=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertIn(b'workflow integration is not supported', result.stdout)

    def test_kvm_is_required_by_default(self):
        result, args = self.run_builder('0')
        if HAS_KVM:
            self.assertEqual(result.returncode, 23, result.stderr.decode())
            self.assertEqual(args[args.index('-machine') + 1], 'q35,accel=kvm')
            self.assertEqual(args[args.index('-cpu') + 1], 'host')
        else:
            self.assertEqual(result.returncode, 1, result.stderr.decode())
            self.assertIsNone(args)
            self.assertIn(b'WINDOWS_ALLOW_TCG=1', result.stderr)

    def test_explicit_fallback_still_prefers_kvm(self):
        result, args = self.run_builder('1')
        self.assertEqual(result.returncode, 23, result.stderr.decode())
        self.assertEqual(args[args.index('-machine') + 1], 'q35,accel=' + ('kvm' if HAS_KVM else 'tcg'))
        self.assertEqual(args[args.index('-cpu') + 1], 'host' if HAS_KVM else 'max')


if __name__ == '__main__':
    unittest.main()
