#!/usr/bin/env bash
# Cut a release end to end: bump the version, gate on checks, tag, push,
# wait for the GitHub release build, and publish to the pacman repo.
#
#   ./packaging/release.sh minor     # 0.2.0 -> 0.3.0  (the usual)
#   ./packaging/release.sh patch     # 0.2.0 -> 0.2.1
#   ./packaging/release.sh major     # 0.2.0 -> 1.0.0
#   ./packaging/release.sh 1.2.3     # explicit version
#
# Add --no-changelog to skip the CHANGELOG entry gate.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

bump="${1:-minor}"
skip_changelog=false
[[ "${2:-}" == "--no-changelog" ]] && skip_changelog=true

[[ -n $(git status --porcelain) ]] && {
  echo "error: working tree not clean — commit or stash first" >&2
  exit 1
}
[[ $(git branch --show-current) == main ]] || {
  echo "error: releases cut from main only" >&2
  exit 1
}
git pull --ff-only --quiet

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
IFS=. read -r major minor patch <<<"$current"
case "$bump" in
  major) next="$((major + 1)).0.0" ;;
  minor) next="$major.$((minor + 1)).0" ;;
  patch) next="$major.$minor.$((patch + 1))" ;;
  *.*.*) next="$bump" ;;
  *)
    echo "usage: $0 [major|minor|patch|X.Y.Z] [--no-changelog]" >&2
    exit 2
    ;;
esac
echo "==> Releasing v$next (current v$current)"

if ! $skip_changelog && ! grep -q "$next" CHANGELOG.md; then
  echo "error: CHANGELOG.md has no entry for $next" >&2
  echo "       add one (or pass --no-changelog)" >&2
  exit 1
fi

echo "==> Running checks (fmt + clippy + tests)"
./contrib/check.sh

echo "==> Bumping versions"
sed -i "s/^version = \"$current\"/version = \"$next\"/" Cargo.toml
sed -i "s/^pkgver=.*/pkgver=$next/; s/^pkgrel=.*/pkgrel=1/" packaging/PKGBUILD
cargo check --workspace --quiet # refresh Cargo.lock

git add Cargo.toml Cargo.lock packaging/PKGBUILD CHANGELOG.md
git commit -q -m "Release v$next"
git tag -a "v$next" -m "v$next"
git push --quiet origin main "v$next"

echo "==> Waiting for the GitHub release build"
run_id=""
for _ in $(seq 30); do
  run_id=$(gh run list --workflow Release --limit 1 \
    --json databaseId,headBranch \
    --jq ".[] | select(.headBranch == \"v$next\") | .databaseId" || true)
  [[ -n "$run_id" ]] && break
  sleep 5
done
[[ -n "$run_id" ]] || {
  echo "error: release workflow run never appeared" >&2
  exit 1
}
gh run watch "$run_id" --exit-status >/dev/null

echo "==> Publishing to the pacman repository"
./packaging/publish-repo.sh

echo "==> v$next released: GitHub release + pacman repo updated."
echo "    Users get it on their next 'omarchy update'."
