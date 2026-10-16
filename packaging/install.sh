#!/usr/bin/env bash
set -euo pipefail
if [[ $(uname -s) != Linux ]]; then
    printf 'Aerial for Linux must be installed on Linux.\n' >&2
    exit 1
fi
cd -- "$(dirname -- "$(readlink -f -- "$0")")/.."
if [[ ! -x target/release/aerial-linux ]]; then
    printf 'Build first: cargo build --locked --release\n' >&2
    exit 1
fi
if ! command -v flatpak >/dev/null; then
    printf 'Install Flatpak through your distribution first.\n' >&2
    exit 1
fi
if ! flatpak info io.mpv.Mpv >/dev/null 2>&1; then
    printf 'Install the player first: flatpak install --user flathub io.mpv.Mpv\n' >&2
    exit 1
fi
install -d "$HOME/.local/bin"
staged_binary=$(mktemp "$HOME/.local/bin/.aerial-linux.XXXXXXXX")
trap 'rm -f -- "$staged_binary"' EXIT
install -m 755 target/release/aerial-linux "$staged_binary"
mv -f -- "$staged_binary" "$HOME/.local/bin/aerial-linux"
printf 'Installed ~/.local/bin/aerial-linux\n'
case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) printf 'Add ~/.local/bin to your PATH before using the short command.\n' ;;
esac
printf 'Next: %q refresh\nThen: %q download --count 6\n' "$HOME/.local/bin/aerial-linux" "$HOME/.local/bin/aerial-linux"
