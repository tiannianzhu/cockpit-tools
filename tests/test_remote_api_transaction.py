"""Exercise the exact SSH wrapper with isolated files and deterministic bundle functions."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / 'src-tauri/src/modules/ssh_server.rs').read_text()
WRAPPER = re.search(r'const REMOTE_SYNC_SCRIPT: &str = r#"(.*?)"#;', SOURCE, re.S)[1]
RESOLVE_HOME = re.search(r'const REMOTE_RESOLVE_HOME_SCRIPT: &str = r#"(.*?)"#;', SOURCE, re.S)[1]
STUB = '''
def prepare_api_bundle(home, api):
    if api.get('prepare_fail'): raise ValueError('invalid fixture definition')
    return {'config.toml': b'model = "custom"\\n', 'cockpit-model-catalog.json': b'{"models": []}'}
def validate_api_bundle(home, api, prepared):
    assert set(prepared) == {'config.toml', 'cockpit-model-catalog.json'}
    if api.get('validation_fail'): raise ValueError('fixture catalog mismatch')
'''


class TransactionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / '.codex'
        self.home.mkdir()
        helper = self.root / '.local/bin/codex'
        helper.parent.mkdir(parents=True)
        helper.write_text('#!/bin/sh\necho unexpected CLI call >&2\nexit 99\n')
        helper.chmod(0o700)
        self.original = {'auth.json': b'{"old": true}', 'config.toml': b'model = "prior"\n'}
        for name, content in self.original.items():
            (self.home / name).write_bytes(content)
        self.generation = 1

    def run_sync(self, **options):
        auth = b'{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-secret"}'
        request = {'home': str(self.home), 'auth': base64.b64encode(auth).decode(),
                   'sha256': hashlib.sha256(auth).hexdigest(), 'generation': str(self.generation),
                   'api_bundle': {'model_catalog_definition': {'base_model': 'template', 'models': []}, **options}}
        self.generation += 1
        wrapper = WRAPPER.replace('\ntry:\n    main()', '\n'+options.get('stop_stub', '')+'\ntry:\n    main()')
        process = subprocess.Popen(['python3', '-c', STUB + wrapper],
            env=dict(os.environ, HOME=str(self.root), PATH=str(self.root / '.local/bin') + os.pathsep + os.environ['PATH']),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        process.stdin.write(json.dumps(request) + '\n')
        process.stdin.flush()
        # Keep the SSH lease open until the bounded fixture process exits.
        process.wait(timeout=5)
        out, err = process.communicate(timeout=2)
        self.assertNotIn('fixture-secret', out + err)
        return process.returncode, out

    def assert_originals(self):
        for name, content in self.original.items():
            self.assertEqual((self.home / name).read_bytes(), content)
        for name in ('cockpit-model-catalog.json', 'cockpit-model-definition.json', '.cockpit-api-rollback.json'):
            self.assertFalse((self.home / name).exists(), name)

    def test_success_leaves_only_runtime_files(self):
        code, out = self.run_sync()
        self.assertEqual(code, 0)
        self.assertIn('applied', out)
        self.assertEqual((self.home / 'config.toml').read_bytes(), b'model = "custom"\n')
        self.assertEqual(self.run_sync()[0], 0)
        self.assertEqual({path.name for path in self.home.iterdir()}, {
            'auth.json', 'config.toml', 'cockpit-model-catalog.json',
        })

    def test_sync_initializes_another_home_without_changing_existing_account(self):
        existing_home = self.home
        self.home = self.root / 'another profile'
        self.assertFalse(self.home.exists())
        code, out = self.run_sync()
        self.assertEqual(code, 0)
        self.assertIn('applied', out)
        self.assertEqual(self.home.stat().st_mode & 0o777, 0o700)
        self.assertEqual({path.name for path in self.home.iterdir()}, {
            'auth.json', 'config.toml', 'cockpit-model-catalog.json',
        })
        for name, content in self.original.items():
            self.assertEqual((existing_home / name).read_bytes(), content)

    def test_resolves_equivalent_homes_without_creating_directories(self):
        (self.root / 'profile-link').symlink_to(self.home, target_is_directory=True)
        for home in ['~/.codex', '~/.codex/', '~/.codex/.', str(self.home), 'profile-link']:
            with self.subTest(home=home):
                result = subprocess.run(['python3', '-c', RESOLVE_HOME],
                    input=json.dumps({'codex_home': home}) + '\n',
                    env=dict(os.environ, HOME=str(self.root)), cwd=self.root,
                    capture_output=True, text=True, timeout=5, check=True)
                self.assertEqual(json.loads(result.stdout), str(self.home.resolve()))
        missing = self.root / 'new profile'
        result = subprocess.run(['python3', '-c', RESOLVE_HOME],
            input=json.dumps({'codex_home': '~/new profile'}) + '\n',
            env=dict(os.environ, HOME=str(self.root)), cwd=self.root,
            capture_output=True, text=True, timeout=5, check=True)
        self.assertEqual(json.loads(result.stdout), str(missing.resolve()))
        self.assertFalse(missing.exists())
        self.assert_originals()

    def test_legacy_sidecars_retired_only_after_success(self):
        obsolete = ['cockpit-model-definition.json', '.cockpit-api-previous.json', '.cockpit-api-rollback.json', '.cockpit-auth-sync-generation']
        for name in obsolete:
            (self.home / name).write_text('legacy')
        self.assertNotEqual(self.run_sync(validation_fail=True)[0], 0)
        for name in obsolete:
            self.assertEqual((self.home / name).read_text(), 'legacy')
        self.assertEqual(self.run_sync()[0], 0)
        for name in obsolete:
            self.assertFalse((self.home / name).exists())

    def test_validation_failure_restores_all_files(self):
        code, out = self.run_sync(validation_fail=True)
        self.assertNotEqual(code, 0)
        self.assertIn('rolled_back', out)
        self.assertNotIn('credentials_synced', out)
        self.assert_originals()

    def test_stop_failure_keeps_verified_files_without_fallback(self):
        code, out = self.run_sync(stop_stub="def stop_desktop_server(home, alive): raise RuntimeError('App-server did not exit')")
        self.assertNotEqual(code, 0)
        self.assertIn('App-server did not exit', out)
        self.assertIn('reload_error:', out)
        self.assertNotIn('applied', out)
        self.assertNotIn('rolled_back', out)
        self.assertEqual((self.home / 'config.toml').read_bytes(), b'model = "custom"\n')

    def test_disconnected_waiter_cannot_overwrite_new_credentials(self):
        import fcntl
        lock = os.open(self.home, os.O_RDONLY)
        self.addCleanup(os.close, lock)
        fcntl.flock(lock, fcntl.LOCK_EX)
        auth = b'{"fixture": "old"}'
        request = {'home': str(self.home), 'auth': base64.b64encode(auth).decode(),
                   'sha256': hashlib.sha256(auth).hexdigest()}
        process = subprocess.Popen(['python3', '-c', STUB + WRAPPER],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        out, err = process.communicate(json.dumps(request)+'\n', timeout=5)
        self.assertNotEqual(process.returncode, 0)
        self.assertNotIn('credentials_synced', out)
        self.assert_originals()
        fcntl.flock(lock, fcntl.LOCK_UN)
        self.assertEqual(self.run_sync()[0], 0)

    def test_prepare_failure_writes_no_account_files(self):
        code, _ = self.run_sync(prepare_fail=True)
        self.assertNotEqual(code, 0)
        self.assert_originals()

if __name__ == '__main__':
    unittest.main()
