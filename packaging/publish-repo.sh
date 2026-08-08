#!/usr/bin/env bash
# Publish the current release tag to the HarkerSoftware pacman repository.
#
#   ./packaging/publish-repo.sh          # builds from packaging/PKGBUILD
#
# Builds the package with makepkg (pinning the tag tarball's checksum),
# regenerates the repo database, and pushes to HarkerSoftware/arch-repo.
# Requires: makepkg, repo-add, gh (authenticated), and push access.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
pkgver=$(sed -n 's/^pkgver=//p' "$here/PKGBUILD")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "==> Building omarchy-workspaces $pkgver from the v$pkgver tag"
cp "$here/PKGBUILD" "$work/"
cd "$work"
url="https://github.com/HarkerSoftware/omarchy-workspaces/archive/refs/tags/v$pkgver.tar.gz"
sha=$(curl -fsSL "$url" | sha256sum | cut -d' ' -f1)
sed -i "s/^sha256sums=.*/sha256sums=('$sha')/" PKGBUILD
makepkg -df --noconfirm

echo "==> Updating HarkerSoftware/arch-repo"
gh repo clone HarkerSoftware/arch-repo "$work/arch-repo" -- --depth 1
cd "$work/arch-repo/x86_64"
cp "$work"/omarchy-workspaces-"$pkgver"-*-x86_64.pkg.tar.zst .
rm -f omarchy-workspaces-debug-*.pkg.tar.zst
repo-add harkersoftware.db.tar.gz omarchy-workspaces-"$pkgver"-*-x86_64.pkg.tar.zst
# GitHub raw serves symlinks as text; ship real copies.
rm harkersoftware.db harkersoftware.files
cp harkersoftware.db.tar.gz harkersoftware.db
cp harkersoftware.files.tar.gz harkersoftware.files
git add -A
git commit -m "omarchy-workspaces $pkgver"
git push
echo "==> Published omarchy-workspaces $pkgver"
