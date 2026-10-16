#!/usr/bin/env python3
"""Tooling-only dependency graph guard tests; does not invoke Cargo."""
import subprocess
import sys
import unittest
from pathlib import Path


class WebDependencyTests(unittest.TestCase):
    def check(self, graph):
        return subprocess.run([sys.executable, str(Path(__file__).with_name('check-web-dependencies.py'))],
                              input=graph, text=True, capture_output=True)

    def test_isolated_browser_graph(self):
        result = self.check('codeyard v0.1.0\ndioxus-web v0.7.10\nweb-sys v0.3.81\n')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_native_authority_in_browser_fails(self):
        result = self.check('codeyard v0.1.0\ndioxus-web v0.7.10\nsqlx-postgres v0.8.6\nredis v0.27.6\n')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('sqlx-postgres', result.stderr)
        self.assertIn('redis', result.stderr)

    def test_empty_or_wrong_target_is_not_success(self):
        self.assertNotEqual(self.check('').returncode, 0)
        self.assertNotEqual(self.check('codeyard v0.1.0\naxum v0.7.9\n').returncode, 0)


if __name__ == '__main__':
    unittest.main()
