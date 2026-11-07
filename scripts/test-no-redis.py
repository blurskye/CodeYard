#!/usr/bin/env python3
"""The optional cache has one native owner and is absent from authority and WASM."""
from pathlib import Path
import re
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


class OptionalContentCache(unittest.TestCase):
    def test_only_the_cache_crate_owns_the_client(self):
        packages = tomllib.loads((ROOT / 'Cargo.lock').read_text())['package']
        owners = {p['name'] for p in packages if any(d.split()[0] == 'redis' for d in p.get('dependencies', []))}
        self.assertEqual(owners, {'codeyard-cache'})
        self.assertNotIn('deadpool-redis', {p['name'] for p in packages})
        forbidden = re.compile(r'\b(?:redis|deadpool_redis)::|\bRedisPool\b|cache::session|init_redis')
        for directory in ('src', 'backend', 'webui', 'examples'):
            for path in (ROOT / directory).rglob('*.rs'):
                if path.is_relative_to(ROOT / 'backend/cache'):
                    continue
                with self.subTest(path=str(path.relative_to(ROOT))):
                    self.assertIsNone(forbidden.search(path.read_text()))
        self.assertFalse((ROOT / 'backend/app/src/cache').exists())
        self.assertNotIn('codeyard-cache', (ROOT / 'webui/Cargo.toml').read_text())

    def test_data_commands_are_disposable_content_only(self):
        text = '\n'.join(p.read_text() for p in (ROOT / 'backend/cache/src').glob('*.rs'))
        commands = set(re.findall(r'redis::cmd\("([A-Z]+)"\)', text))
        self.assertEqual(commands, {'GET', 'MGET', 'SET', 'DEL'})
        self.assertNotIn('"EVAL"', text)
        self.assertIn('"PX"', text)
        self.assertIn('Mode::Local', text)

    def test_packaging_keeps_state_dir_and_never_injects_an_empty_master_key(self):
        text = (ROOT / 'compose.yaml').read_text()
        self.assertIn('GITSERV_STATE_DIR: ${GITSERV_STATE_DIR:-/data/state}', text)
        self.assertIn('SECRETS_MASTER_KEY: ${SECRETS_MASTER_KEY:?', text)
        self.assertNotIn('SECRETS_MASTER_KEY: ${SECRETS_MASTER_KEY:-}', text)
        self.assertIn('GITSERV_STATE_DIR=/data/state', (ROOT / 'Containerfile').read_text())

    def test_compose_cache_is_opt_in(self):
        text = (ROOT / 'compose.yaml').read_text()
        self.assertIn('profiles: ["cache"]', text)
        self.assertIn('CACHE_MODE: ${CACHE_MODE:-local}', text)
        self.assertIn('allkeys-lru', text)
        self.assertIn('PGDATA: /var/lib/postgresql/data', text)
        self.assertIn('/postgres:/var/lib/postgresql/data:Z', text)


if __name__ == '__main__':
    unittest.main()
