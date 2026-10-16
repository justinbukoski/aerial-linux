#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "$0")/.."
if [[ $(uname -s) != Linux ]]; then
    printf 'Aerial for Linux must be installed on Linux.\n' >&2
    exit 1
fi
if [[ ! -x target/release/aerial-linux ]]; then
    printf 'Build first: cargo build --locked --release\n' >&2
    exit 1
fi
if ! command -v flatpak >/dev/null || ! flatpak info --user io.mpv.Mpv >/dev/null 2>&1; then
    printf 'Install the player first: flatpak install --user flathub io.mpv.Mpv\n' >&2
    exit 1
fi
install -d "$HOME/.local/bin"
install -m 755 target/release/aerial-linux "$HOME/.local/bin/aerial-linux"
printf 'Installed ~/.local/bin/aerial-linux\nNext: aerial-linux refresh, then aerial-linux download --count 6\n'
