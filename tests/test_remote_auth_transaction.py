"""Exercise the SSH auth transaction against isolated local fixtures."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / 'src-tauri/src/modules/ssh_server.rs').read_text()
WRAPPER = re.search(r'const REMOTE_SYNC_SCRIPT: &str = r#"(.*?)"#;', SOURCE, re.S)[1]
RESOLVE_HOME = re.search(r'const REMOTE_RESOLVE_HOME_SCRIPT: &str = r#"(.*?)"#;', SOURCE, re.S)[1]
AUTH = b'{"auth_mode":"apikey","OPENAI_API_KEY":"fixture-secret"}'


class TransactionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.home = self.root / '.codex'
        self.home.mkdir()
        self.original_auth = b'{"old": true}'
        self.original_config = b'model = "prior"\n'
        (self.home / 'auth.json').write_bytes(self.original_auth)
        (self.home / 'auth.json').chmod(0o640)
        (self.home / 'config.toml').write_bytes(self.original_config)

    def run_sync(self, injection):
        request = {
            'home': str(self.home),
            'auth': base64.b64encode(AUTH).decode(),
            'sha256': hashlib.sha256(AUTH).hexdigest(),
        }
        anchor = '\ntry:\n    main()'
        self.assertEqual(WRAPPER.count(anchor), 1)
        wrapper = WRAPPER.replace(anchor, '\n' + injection + anchor, 1)
        process = subprocess.Popen(
            ['python3', '-c', wrapper],
            env=dict(os.environ, HOME=str(self.root)),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        process.stdin.write(json.dumps(request) + '\n')
        process.stdin.flush()
        try:
            # The SSH lease must remain open until the transaction exits.
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
            raise
        out, err = process.communicate(timeout=2)
        self.assertNotIn('fixture-secret', out + err)
        return process.returncode, out

    def test_resolves_equivalent_homes_without_creating_directories(self):
        (self.root / 'profile-link').symlink_to(self.home, target_is_directory=True)
        for home in ['~/.codex', '~/.codex/', '~/.codex/.', str(self.home), 'profile-link']:
            with self.subTest(home=home):
                result = subprocess.run(
                    ['python3', '-c', RESOLVE_HOME],
                    input=json.dumps({'codex_home': home}) + '\n',
                    env=dict(os.environ, HOME=str(self.root)), cwd=self.root,
                    capture_output=True, text=True, timeout=5, check=True,
                )
                self.assertEqual(json.loads(result.stdout), str(self.home.resolve()))
        missing = self.root / 'new profile'
        result = subprocess.run(
            ['python3', '-c', RESOLVE_HOME],
            input=json.dumps({'codex_home': '~/new profile'}) + '\n',
            env=dict(os.environ, HOME=str(self.root)), cwd=self.root,
            capture_output=True, text=True, timeout=5, check=True,
        )
        self.assertEqual(json.loads(result.stdout), str(missing.resolve()))
        self.assertFalse(missing.exists())
        self.assertEqual((self.home / 'auth.json').read_bytes(), self.original_auth)
        self.assertEqual((self.home / 'config.toml').read_bytes(), self.original_config)

    def test_reload_failure_keeps_verified_auth(self):
        code, out = self.run_sync(
            "def stop_desktop_server(home, alive): raise RuntimeError('App-server did not exit')"
        )
        self.assertNotEqual(code, 0)
        self.assertIn('credentials_synced', out)
        self.assertIn('reload_error:', out)
        self.assertIn('App-server did not exit', out)
        self.assertNotIn('applied', out)
        self.assertNotIn('rolled_back', out)
        self.assertEqual((self.home / 'auth.json').read_bytes(), AUTH)
        self.assertEqual(stat.S_IMODE((self.home / 'auth.json').stat().st_mode), 0o600)
        self.assertEqual((self.home / 'config.toml').read_bytes(), self.original_config)

    def test_fsync_failure_after_auth_replace_restores_original_bytes_and_mode(self):
        injection = '''
real_replace = os.replace
real_fsync = os.fsync
arm_fsync_failure = False
injected_fsync_failure = False

def replace_with_fault(src, dst, *args, **kwargs):
    global arm_fsync_failure
    result = real_replace(src, dst, *args, **kwargs)
    if os.path.basename(dst) == 'auth.json' and not injected_fsync_failure:
        arm_fsync_failure = True
    return result

def fsync_with_fault(fd):
    global arm_fsync_failure, injected_fsync_failure
    if arm_fsync_failure:
        arm_fsync_failure = False
        injected_fsync_failure = True
        raise OSError(5, 'injected directory sync failure')
    return real_fsync(fd)

os.replace = replace_with_fault
os.fsync = fsync_with_fault
'''
        code, out = self.run_sync(injection)
        self.assertNotEqual(code, 0)
        self.assertIn('rolled_back', out)
        self.assertNotIn('credentials_synced', out)
        self.assertEqual((self.home / 'auth.json').read_bytes(), self.original_auth)
        self.assertEqual(stat.S_IMODE((self.home / 'auth.json').stat().st_mode), 0o640)
        self.assertEqual((self.home / 'config.toml').read_bytes(), self.original_config)


if __name__ == '__main__':
    unittest.main()
