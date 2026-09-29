#!/bin/bash
# Local macOS release: build, ad-hoc sign, verify, and install.
set -euo pipefail

# Parse the whole release flow before starting a long build. Editing this file
# during compilation must not change the installer read by the running shell.
main() {
repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mode="release"
fast_args=()
case "${1:-}" in
  "") ;;
  --fast) fast_args=(-- --config 'profile.release.package.cockpit-tools.opt-level=0' --config 'profile.release.package.cockpit-tools.incremental=true') ;;
  --build-only) mode="build" ;;
  --install-only) mode="install" ;;
  --check) mode="check" ;;
  -h|--help)
    cat <<'HELP'
Usage: ./scripts/release.sh [--fast | --build-only | --install-only | --check]

Default: build and install /Applications/Cockpit Tools.app.
--fast          Build with less optimization and incremental compilation, then install.
--build-only    Build and sign the .app without installing.
--install-only Install the existing signed build without rebuilding.
--check        Check build prerequisites and print paths only.

Quit Cockpit before installation. Old applications move to ~/.Trash.
This is a local ad-hoc signed build, not a notarized public release.
Build tools are managed by mise.toml; run mise install once before building.
Respects DEVELOPER_DIR, CARGO_TARGET_DIR, GOCACHE and GOMODCACHE.
Does not change accounts.
HELP
    exit 0 ;;
  *) echo "Unknown option: $1" >&2; exit 2 ;;
esac
[[ $# -le 1 ]] || { echo 'Use only one option.' >&2; exit 2; }
[[ "$(uname -s)" == Darwin ]] || { echo 'This script requires macOS.' >&2; exit 1; }
cd "$repo_dir"

# Reuse the existing local build cache; callers can override every path.
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/private/tmp/cockpit-target}"
[[ "$CARGO_TARGET_DIR" == /* ]] || export CARGO_TARGET_DIR="$repo_dir/$CARGO_TARGET_DIR"
# Enter the project tool environment even from shells without mise activation.
if [[ "$mode" != install && "${COCKPIT_RELEASE_MISE:-}" != 1 ]]; then
  command -v mise >/dev/null || { echo 'Install mise, then run mise install.' >&2; exit 1; }
  exec env MISE_AUTO_INSTALL=false MISE_EXEC_AUTO_INSTALL=false mise exec -- \
    env COCKPIT_RELEASE_MISE=1 /bin/bash "$repo_dir/scripts/release.sh" "$@"
fi
bundle="$CARGO_TARGET_DIR/release/bundle/macos/Cockpit Tools.app"

if [[ "$mode" != install ]]; then
  [[ -d "$DEVELOPER_DIR" ]] || { echo 'Install Xcode or set DEVELOPER_DIR.' >&2; exit 1; }
  for tool in node npm cargo rustc go python3 codesign xcrun; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
  done
  [[ -x node_modules/.bin/tauri ]] || { echo 'Run npm ci to install project dependencies first.' >&2; exit 1; }
  xcrun --find swift >/dev/null
  cargo --version
  rustc --version
  go version
fi
printf 'Build: %s\nInstall: /Applications/Cockpit Tools.app\n' "$bundle"
[[ "$mode" != check ]] || exit 0

if [[ "$mode" != install ]]; then
  # Inline override avoids maintaining a second Tauri configuration file.
  ./node_modules/.bin/tauri build --bundles app --no-sign --config \
    '{"bundle":{"createUpdaterArtifacts":false}}' ${fast_args[@]+"${fast_args[@]}"}
  codesign --force --deep --sign - "$bundle"
fi
codesign --verify --deep --strict "$bundle"
[[ "$mode" != build ]] || { printf 'Build ready: %s\n' "$bundle"; exit 0; }

python3 - "$bundle" <<'PY'
import datetime
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import uuid

source = Path(sys.argv[1])
target = Path('/Applications/Cockpit Tools.app')
relative = Path('Contents/MacOS/cockpit-tools')

def ensure_stopped():
    result = subprocess.run(['pgrep', '-x', 'cockpit-tools'], capture_output=True)
    if result.returncode != 1:
        raise SystemExit('Quit Cockpit first, then run ./scripts/release.sh --install-only. The build is ready.')

def verify(app):
    subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
    return hashlib.sha256((app / relative).read_bytes()).hexdigest()

ensure_stopped()
expected = verify(source)
staging = target.parent / ('.Cockpit Tools.update-' + uuid.uuid4().hex + '.app')
trash = Path.home() / '.Trash'
previous = trash / ('Cockpit Tools-' + datetime.datetime.now().strftime('%Y%m%d-%H%M%S') + '-' + uuid.uuid4().hex[:8] + '.app')
moved_old = False
installed = False
try:
    subprocess.run(['ditto', str(source), str(staging)], check=True)
    if verify(staging) != expected:
        raise RuntimeError('Staging binary differs from build')
    ensure_stopped()
    trash.mkdir(exist_ok=True)
    if target.exists():
        os.rename(target, previous)
        moved_old = True
    os.rename(staging, target)
    installed = True
    if verify(target) != expected:
        raise RuntimeError('Installed binary differs from build')
except BaseException:
    if installed:
        os.rename(target, staging)
    if moved_old:
        os.rename(previous, target)
    raise
finally:
    if staging.exists():
        shutil.rmtree(staging)
print('Installed:', target)
if moved_old:
    print('Previous version:', previous)
print('SHA256:', expected)
PY
}

main "$@"
