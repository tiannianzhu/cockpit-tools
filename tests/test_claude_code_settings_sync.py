import hashlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).parents[1] / "src-tauri/src/modules/claude_code_remote_settings.py"
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("claude_code_remote_settings", SCRIPT)
remote = importlib.util.module_from_spec(spec)
spec.loader.exec_module(remote)


class InstallerTests(unittest.TestCase):
    def payload(self, content):
        return {
            "content": content,
            "revision": hashlib.sha256(content.encode()).hexdigest(),
            "codex_home": "/ignored",
            "path": "/ignored",
        }

    def test_whole_file_overwrite_preserves_unknown_fields_without_extra_files(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root)
            directory = home / ".claude"
            directory.mkdir(mode=0o750)
            target = directory / "settings.json"
            old = b'{ malformed old bytes\x00'
            target.write_bytes(old)
            new = '{"permissions":{"allow":["x"]},"custom":42}\n'
            result = remote.install_settings(self.payload(new), home)
            self.assertEqual(target.read_bytes(), new.encode())
            self.assertEqual(set(directory.iterdir()), {target})
            self.assertEqual(result["revision"], hashlib.sha256(new.encode()).hexdigest())
            self.assertTrue(result["verified"])
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            self.assertEqual(directory.stat().st_mode & 0o777, 0o750)

    def test_missing_directory_is_created_private(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root) / "new-home"
            content = '{"enabled":true}'
            remote.install_settings(self.payload(content), home)
            directory = home / ".claude"
            self.assertEqual((directory / "settings.json").read_bytes(), content.encode())
            self.assertEqual(directory.stat().st_mode & 0o777, 0o700)

    def test_unchanged_content_creates_no_extra_files_and_preserves_directory_mode(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root)
            directory = home / ".claude"
            directory.mkdir(mode=0o750)
            target = directory / "settings.json"
            content = b'{"same":true}'
            target.write_bytes(content)
            target.chmod(0o644)
            result = remote.install_settings(self.payload(content.decode()), home)
            self.assertTrue(result["verified"])
            self.assertEqual(set(directory.iterdir()), {target})
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            self.assertEqual(directory.stat().st_mode & 0o777, 0o750)

    def test_failed_replace_preserves_target_and_removes_temporary_file(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root)
            target = home / ".claude/settings.json"
            target.parent.mkdir()
            target.write_bytes(b"original")
            with mock.patch.object(remote.os, "replace", side_effect=OSError("fixture failure")):
                with self.assertRaises(OSError):
                    remote.install_settings(self.payload('{"updated":true}'), home)
            self.assertEqual(target.read_bytes(), b"original")
            self.assertEqual(set(target.parent.iterdir()), {target})

    def test_invalid_or_mismatched_input_does_not_change_target(self):
        with tempfile.TemporaryDirectory() as root:
            home = Path(root)
            target = home / ".claude/settings.json"
            target.parent.mkdir()
            target.write_bytes(b"original")
            invalid = [
                self.payload("[]"),
                {"content": "{}", "revision": "bad"},
                {"content": "{", "revision": "bad"},
                self.payload('{"env":[]}'),
            ]
            for payload in invalid:
                with self.assertRaises(ValueError):
                    remote.install_settings(payload, home)
                self.assertEqual(target.read_bytes(), b"original")


if __name__ == "__main__":
    unittest.main()
