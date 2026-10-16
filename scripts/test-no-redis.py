#!/usr/bin/env python3
"""No external cache dependency may quietly return to source or deployment."""
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


class SelectedDatabaseSessions(unittest.TestCase):
    def test_lockfile_has_no_cache_client_or_pool(self):
        packages = tomllib.loads((ROOT / 'Cargo.lock').read_text())['package']
        self.assertFalse({'redis', 'deadpool-redis'} & {p['name'] for p in packages})

    def test_native_code_has_no_cache_client_or_placeholder_pool(self):
        forbidden = re.compile(r'\b(?:redis|deadpool_redis)::|\bRedisPool\b|backend::cache|cache::session|\.redis\b|init_redis')
        for directory in ('src', 'examples'):
            for path in (ROOT / directory).rglob('*.rs'):
                with self.subTest(path=str(path.relative_to(ROOT))):
                    self.assertIsNone(forbidden.search(path.read_text()))
        self.assertFalse((ROOT / 'src/backend/cache').exists())

    def test_packaging_keeps_state_dir_and_never_injects_an_empty_master_key(self):
        text = (ROOT / 'compose.yaml').read_text()
        self.assertIn('GITSERV_STATE_DIR: ${GITSERV_STATE_DIR:-/data/state}', text)
        self.assertIn('SECRETS_MASTER_KEY: ${SECRETS_MASTER_KEY:?', text)
        self.assertNotIn('SECRETS_MASTER_KEY: ${SECRETS_MASTER_KEY:-}', text)
        self.assertIn('GITSERV_STATE_DIR=/data/state', (ROOT / 'Containerfile').read_text())

    def test_compose_needs_only_postgres_git_and_object_storage(self):
        text = (ROOT / 'compose.yaml').read_text()
        self.assertNotRegex(text, r'(?im)\b(?:redis|garnet|REDIS_URL)\b')
        self.assertIn('PGDATA: /var/lib/postgresql/data', text)
        self.assertIn('/postgres:/var/lib/postgresql/data:Z', text)


if __name__ == '__main__':
    unittest.main()
