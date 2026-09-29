"""Exercise the Linux socket/process boundary without signalling real processes."""
import io
import re
import signal
import struct
import threading
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

SOURCE = (Path(__file__).resolve().parents[1] / 'src-tauri/src/modules/ssh_server.rs').read_text()
SCRIPT = re.search(r'const REMOTE_SYNC_SCRIPT: &str = r#"(.*?)"#;', SOURCE, re.S)[1]


class RestartTests(unittest.TestCase):
    def setUp(self):
        self.ns = {}
        # Load the production functions, without main or process signal handlers.
        script = SCRIPT.split('\ntry:\n    main()')[0]
        script = script.replace('    signal.signal(sig, stop)', '    pass')
        exec(script, self.ns)
        self.alive = threading.Event()
        self.alive.set()
        self.sock = MagicMock()
        self.sock.getsockopt.return_value = struct.pack('3i', 4321, 1234, 1234)
        self.socket = MagicMock()
        self.socket.AF_UNIX = 1
        self.socket.SOCK_STREAM = 1
        self.socket.SOL_SOCKET = 1
        self.socket.SO_PEERCRED = 17
        self.socket.socket.return_value.__enter__.return_value = self.sock
        self.ns['socket'] = self.socket
        self.os = MagicMock()
        self.os.path.basename.side_effect = __import__('os').path.basename
        self.os.path.join.side_effect = __import__('os').path.join
        self.os.path.realpath.side_effect = lambda path: path
        self.os.getuid.return_value = 1234
        self.os.readlink.return_value = '/package/bin/codex'
        self.ns['os'] = self.os
        self.identity = MagicMock(side_effect=['start-A', 'start-A', None])
        self.ns['process_identity'] = self.identity

    def stop(self):
        with patch('builtins.open', return_value=io.BytesIO(b'codex\0-c\0feature=true\0app-server\0--listen\0unix://\0')):
            self.ns['stop_desktop_server']('/selected/home', self.alive)

    def test_stops_only_selected_socket_owner_with_sigterm(self):
        self.stop()
        self.sock.connect.assert_called_once_with('/selected/home/app-server-control/app-server-control.sock')
        self.os.kill.assert_called_once_with(4321, signal.SIGTERM)
        self.assertNotIn('subprocess', self.ns)

    def test_no_listener_needs_no_restart(self):
        for error in (FileNotFoundError(), ConnectionRefusedError()):
            self.sock.connect.side_effect = error
            self.stop()
        self.os.kill.assert_not_called()

    def test_other_user_is_never_signalled(self):
        self.sock.getsockopt.return_value = struct.pack('3i', 4321, 9999, 9999)
        with self.assertRaisesRegex(RuntimeError, 'current user'):
            self.stop()
        self.os.kill.assert_not_called()

    def test_unrelated_executable_is_never_signalled(self):
        self.os.readlink.return_value = '/usr/bin/python3'
        with self.assertRaisesRegex(RuntimeError, 'not a listening'):
            self.stop()
        self.os.kill.assert_not_called()

    def test_pid_reuse_before_signal_is_not_signalled(self):
        self.identity.side_effect = ['start-A', 'start-B']
        self.stop()
        self.os.kill.assert_not_called()

    def test_cancellation_before_signal(self):
        self.alive.clear()
        with self.assertRaisesRegex(RuntimeError, 'cancelled'):
            self.stop()
        self.os.kill.assert_not_called()

    def test_timeout_never_escalates_to_sigkill(self):
        self.identity.side_effect = None
        self.identity.return_value = 'start-A'
        self.ns['time'] = MagicMock()
        self.ns['time'].monotonic.side_effect = [0, 16]
        with self.assertRaisesRegex(RuntimeError, 'no force kill'):
            self.stop()
        self.os.kill.assert_called_once_with(4321, signal.SIGTERM)

    def test_signal_permission_failure_is_reported(self):
        self.os.kill.side_effect = PermissionError(1, 'denied')
        with self.assertRaises(PermissionError):
            self.stop()


if __name__ == '__main__':
    unittest.main()
