#!/bin/sh
# Install a build and register its icon and application ID with the desktop.
# Usage: scripts/install-linux.sh [release|dist|debug] [destination]
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
profile=${1:-release}
case "$profile" in
    release|dist|debug) ;;
    *) printf 'Unknown build profile: %s\n' "$profile" >&2; exit 1 ;;
esac
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
destination=${2:-"$data_home/next_code/bin"}
build_dir="$root/target/$profile"
for binary in next_code next_code_editor; do
    if [ ! -x "$build_dir/$binary" ]; then
        printf 'Missing %s; run cargo build --profile %s --workspace --locked first.\n' "$binary" "$profile" >&2
        exit 1
    fi
done

mkdir -p "$destination"
destination=$(CDPATH= cd -- "$destination" && pwd)
# Replace executable files by rename so updates also work while an older
# installed editor is running.
staging_dir=$(mktemp -d "$destination/.install-XXXXXX")
trap 'rm -rf -- "$staging_dir"' 0
for binary in next_code next_code_editor next_code_agent; do
    if [ -f "$build_dir/$binary" ]; then
        install -m 755 "$build_dir/$binary" "$staging_dir/$binary"
    fi
done
for grammar in "$build_dir"/deps/libnc_grammar_*.so "$build_dir"/libnc_grammar_*.so; do
    if [ -f "$grammar" ]; then
        install -m 755 "$grammar" "$staging_dir/"
    fi
done
for artifact in "$staging_dir"/*; do
    mv -f -- "$artifact" "$destination/"
done
rmdir "$staging_dir"
trap - 0

mkdir -p "$data_home/applications" "$data_home/icons/hicolor/64x64/apps"
install -m 644 "$root/crates/ui/assets/next_code-64.png" \
    "$data_home/icons/hicolor/64x64/apps/next_code.png"
python3 - "$data_home/applications/next_code.desktop" "$destination/next_code" <<'PY'
from pathlib import Path
import sys

# Desktop-entry strings and Exec arguments each have an escaping layer.
executable = sys.argv[2]
if any(character in executable for character in '\n\r\t'):
    raise SystemExit('The installation path cannot contain control characters')
executable = (executable.replace('\\', '\\\\\\\\').replace('"', '\\\\"')
              .replace('`', '\\\\`').replace('$', '\\\\$').replace('%', '%%'))
Path(sys.argv[1]).write_text(f'''[Desktop Entry]
Type=Application
Version=1.0
Name=NextCode
Comment=Native code editor
Exec=/usr/bin/env "{executable}" %F
Icon=next_code
Terminal=false
StartupWMClass=next_code
Categories=Development;IDE;
MimeType=text/plain;
Keywords=editor;code;development;
''')
PY
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data_home/applications"
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true
fi
printf 'Installed NextCode in %s\n' "$destination"
printf 'Open NextCode from the applications menu, or run %s\n' "$destination/next_code"
