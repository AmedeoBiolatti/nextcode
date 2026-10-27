#!/bin/sh
# Installs next_code from a GitHub release: the latest, or NEXT_CODE_VERSION
# (a tag such as v0.1.0). x86_64 Linux only. The files go to
# ~/.local/share/next_code/bin, with a link in ~/.local/bin and an entry in
# the applications menu. Running it again updates the installation.
#
#   curl -fsSL https://raw.githubusercontent.com/AmedeoBiolatti/nextcode/main/scripts/install-release.sh | sh
#
# NEXT_CODE_DOWNLOAD_URL replaces the release's download address (a mirror,
# or a local copy of the release files).
set -eu

repo=AmedeoBiolatti/nextcode
archive=next_code-x86_64-linux.tar.gz

if [ "$(uname -s)" != Linux ] || [ "$(uname -m)" != x86_64 ]; then
    printf 'No prebuilt next_code for %s %s; see the README to build it from source.\n' \
        "$(uname -s)" "$(uname -m)" >&2
    exit 1
fi
if [ -n "${NEXT_CODE_DOWNLOAD_URL:-}" ]; then
    url=$NEXT_CODE_DOWNLOAD_URL
elif [ -n "${NEXT_CODE_VERSION:-}" ]; then
    url="https://github.com/$repo/releases/download/$NEXT_CODE_VERSION"
else
    url="https://github.com/$repo/releases/latest/download"
fi
data_home=${XDG_DATA_HOME:-"$HOME/.local/share"}
destination="$data_home/next_code/bin"
bin_dir="$HOME/.local/bin"
case $destination in
    *'
'*) printf 'The installation path cannot contain a newline\n' >&2; exit 1 ;;
esac

download=$(mktemp -d)
trap 'rm -rf -- "$download"' 0
printf 'Downloading %s\n' "$url/$archive"
curl -fsSL -o "$download/$archive" "$url/$archive"
curl -fsSL -o "$download/SHA256SUMS" "$url/SHA256SUMS"
expected=$(grep "  $archive\$" "$download/SHA256SUMS" | cut -d ' ' -f 1 || true)
actual=$(sha256sum "$download/$archive" | cut -d ' ' -f 1)
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
    printf '%s does not match the release'\''s SHA256SUMS\n' "$archive" >&2
    exit 1
fi
tar -xzf "$download/$archive" -C "$download"

mkdir -p "$destination"
# Replace files by rename, so an editor still running keeps its own (a
# resident editor exits by itself once its executable is replaced).
for file in "$download"/next_code/*; do
    name=${file##*/}
    case $name in
        *.png) continue ;;
        LICENSE-*) mode=644 ;;
        *) mode=755 ;;
    esac
    install -m "$mode" "$file" "$destination/.$name.new"
    mv -f -- "$destination/.$name.new" "$destination/$name"
done

mkdir -p "$bin_dir"
if [ -e "$bin_dir/next_code" ] && [ ! -L "$bin_dir/next_code" ]; then
    printf 'Not linking %s: a file of that name is already there\n' "$bin_dir/next_code" >&2
else
    ln -sfn "$destination/next_code" "$bin_dir/next_code"
fi

mkdir -p "$data_home/applications" "$data_home/icons/hicolor/64x64/apps"
install -m 644 "$download/next_code/next_code.png" "$data_home/icons/hicolor/64x64/apps/next_code.png"
# Exec's quoting, then the desktop entry's string escapes on top of it.
executable=$(printf '%s' "$destination/next_code" |
    sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g')
cat > "$data_home/applications/next_code.desktop" <<EOF
[Desktop Entry]
Type=Application
Version=1.0
Name=NextCode
Comment=Native code editor
Exec=/usr/bin/env "$executable" %F
Icon=next_code
Terminal=false
StartupWMClass=next_code
Categories=Development;IDE;
MimeType=text/plain;
Keywords=editor;code;development;
EOF
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$data_home/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true
fi

printf 'Installed next_code in %s\n' "$destination"
case ":$PATH:" in
    *":$bin_dir:"*) printf 'Run next_code, or open NextCode from the applications menu.\n' ;;
    *) printf 'Add %s to PATH to run next_code from a shell, or open NextCode from the applications menu.\n' "$bin_dir" ;;
esac
