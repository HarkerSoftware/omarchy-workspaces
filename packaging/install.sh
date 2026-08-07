#!/usr/bin/env bash
# Installer for omarchy-workspaces release binaries.
#
#   curl -fsSL https://github.com/HarkerSoftware/omarchy-workspaces/raw/main/packaging/install.sh | bash
#
# Installs to ~/.local/bin (no sudo). Verifies checksums. Never edits your
# Hyprland config — it prints the enable commands instead.
set -euo pipefail

REPO="HarkerSoftware/omarchy-workspaces"
DEST="${DEST:-$HOME/.local/bin}"

if [[ "$(uname -m)" != "x86_64" ]]; then
  echo "error: only x86_64 builds are published (build from source instead)" >&2
  exit 1
fi

if command -v pacman >/dev/null && ! pacman -Q gtk4 gtk4-layer-shell >/dev/null 2>&1; then
  echo "The panel needs gtk4 and gtk4-layer-shell:"
  echo "  sudo pacman -S --needed gtk4 gtk4-layer-shell"
fi

tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
  grep -m1 '"tag_name"' | cut -d'"' -f4)
[[ -n "$tag" ]] || { echo "error: cannot determine the latest release" >&2; exit 1; }
echo "installing omarchy-workspaces $tag to $DEST"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/$REPO/releases/download/$tag"
tarball="omarchy-workspaces-${tag#v}-x86_64.tar.gz"

curl -fsSL -o "$tmp/$tarball" "$base/$tarball"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS"
(cd "$tmp" && sha256sum --check --ignore-missing SHA256SUMS)

mkdir -p "$DEST"
tar -xzf "$tmp/$tarball" -C "$tmp"
install -m755 "$tmp"/workspace "$tmp"/workspace-daemon "$tmp"/workspace-panel "$DEST/"

echo
echo "Installed. Next steps:"
echo "  1. Make sure $DEST is on your PATH."
echo "  2. Start the daemon:   systemd unit in the tarball's contrib/, or"
echo "     add 'exec-once = uwsm-app -- workspace-daemon' to ~/.config/hypr/autostart.conf"
echo "  3. Enable the sidebar: workspace panel enable"
echo "  4. Check health:       workspace doctor"
