"""Remote installer for the user's complete Claude Code settings file.

This module is sent to the SSH user's Python interpreter. The target is always
that login user's ~/.claude/settings.json; request path/auth fields are ignored.
"""

import hashlib
import json
import os
from pathlib import Path
import secrets
import sys


def install_settings(payload, home=None):
    if not isinstance(payload, dict):
        raise ValueError("invalid request")
    content = payload.get("content")
    revision = payload.get("revision")
    if not isinstance(content, str) or not isinstance(revision, str):
        raise ValueError("invalid settings")
    data = content.encode("utf-8")
    digest = hashlib.sha256(data).hexdigest()
    if digest != revision:
        raise ValueError("revision mismatch")
    try:
        parsed = json.loads(content)
    except (ValueError, TypeError):
        raise ValueError("invalid settings")
    if not isinstance(parsed, dict):
        raise ValueError("invalid settings")
    if "env" in parsed and not isinstance(parsed["env"], dict):
        raise ValueError("invalid environment")

    target_home = Path(home) if home is not None else Path.home()
    directory = target_home / ".claude"
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    target = directory / "settings.json"
    if target.exists() and target.read_bytes() == data:
        os.chmod(str(target), 0o600)
        return {"revision": revision, "verified": True}

    temp = directory / (".settings.json." + secrets.token_hex(12) + ".tmp")
    try:
        fd = os.open(str(temp), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.chmod(str(temp), 0o600)
        os.replace(str(temp), str(target))
        try:
            dir_fd = os.open(str(directory), os.O_RDONLY)
            try:
                os.fsync(dir_fd)
            finally:
                os.close(dir_fd)
        except OSError:
            pass
    finally:
        try:
            temp.unlink()
        except FileNotFoundError:
            pass

    actual = target.read_bytes()
    verified = hashlib.sha256(actual).hexdigest() == revision
    if not verified:
        raise ValueError("verification failed")
    return {"revision": revision, "verified": True}


def main():
    try:
        request = json.load(sys.stdin)
        result = install_settings(request)
        sys.stdout.write(json.dumps(result, separators=(",", ":")))
    except Exception:
        # Do not expose file paths, exceptions, settings, or remote account data.
        sys.stdout.write(json.dumps({"verified": False, "error": "remote settings sync failed"}))
        raise SystemExit(1)


if __name__ == "__main__":
    main()
