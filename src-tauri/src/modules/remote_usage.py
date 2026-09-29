"""Install the shared Rust usage helper once, then return computed reports only."""
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tempfile

SOURCE_NAMES = ('Cargo.toml', 'Cargo.lock', 'src/lib.rs', 'src/main.rs')


def source_digest(sources):
    digest = hashlib.sha256()
    for name in SOURCE_NAMES:
        digest.update(name.encode())
        digest.update(b'\0')
        digest.update(sources[name].encode())
        digest.update(b'\0')
    return digest.hexdigest()


def build_helper(cache, binary, sources, version):
    if set(sources) != set(SOURCE_NAMES) or source_digest(sources) != version:
        raise ValueError('远端统计程序的源码校验失败')
    cargo = shutil.which('cargo')
    if not cargo:
        raise ValueError('远端首次启用用量统计需要 Cargo 和 C 编译器；未安装任何工具链')
    with tempfile.TemporaryDirectory(prefix='usage-build-', dir=cache) as temporary:
        root = pathlib.Path(temporary)
        for name in SOURCE_NAMES:
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(sources[name])
        with (root / 'build.log').open('w+b') as log:
            env = dict(os.environ, CARGO_TARGET_DIR=str(root / 'target'))
            process = subprocess.Popen(
                [cargo, 'build', '--release', '--locked', '--manifest-path', str(root / 'Cargo.toml')],
                cwd=root, env=env, stdout=log, stderr=log, start_new_session=True)
            try:
                status = process.wait(timeout=900)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
                raise ValueError('远端统计程序编译超时；临时构建文件已清理')
            if status:
                log.seek(max(0, log.tell() - 6000))
                raise ValueError('远端统计程序编译失败：' + log.read().decode(errors='replace'))
        built = root / 'target/release/cockpit-session-usage'
        built.chmod(0o700)
        os.replace(built, binary)
    for old in cache.glob('helper-*'):
        if old != binary and re.fullmatch(r'helper-[0-9a-f]{64}', old.name):
            old.unlink()


def main(request):
    root = pathlib.Path(os.path.expanduser(request['codex_home'])).resolve(strict=True)
    if not root.is_dir():
        raise ValueError('Codex home is not a directory')
    version = request['version']
    if not isinstance(version, str) or not re.fullmatch('[0-9a-f]{64}', version):
        raise ValueError('Invalid usage helper version')
    identity = hashlib.sha256(str(root).encode()).hexdigest()[:24]
    cache = pathlib.Path.home() / '.cache/cockpit-tools/session-usage' / identity
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    descriptor = os.open(cache, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
    try:
        # Serialize install/sync/query across desktop requests without lock files.
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        binary = cache / ('helper-' + version)
        if not binary.is_file():
            sources = request.get('sources')
            if sources is None:
                return {'needsInstall': True}
            build_helper(cache, binary, sources, version)
        action = request['action']
        if action not in ('query', 'sync'):
            raise ValueError('Invalid usage action')
        payload = {
            'action': action, 'codexHome': str(root), 'dbPath': str(cache / 'usage.sqlite'),
            'instanceId': request['instanceId'], 'instanceName': request['instanceName'],
            'rebuild': bool(request.get('rebuild', False)), 'query': request.get('query') or {},
        }
        root_fd = os.open(root, os.O_RDONLY | getattr(os, 'O_DIRECTORY', 0))
        try:
            fcntl.flock(root_fd, fcntl.LOCK_SH)
            result = subprocess.run([str(binary)], input=json.dumps(payload), text=True,
                                    capture_output=True, timeout=900)
        finally:
            os.close(root_fd)
        if result.returncode:
            raise ValueError('远端用量统计失败：' + result.stderr[-6000:])
        return {'report': json.loads(result.stdout)}
    finally:
        os.close(descriptor)


if __name__ == '__main__':
    try:
        print(json.dumps({'ok': True, 'result': main(json.load(sys.stdin))}, ensure_ascii=False))
    except Exception as error:
        print(json.dumps({'ok': False, 'error': str(error)}, ensure_ascii=False))
