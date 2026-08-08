#!/usr/bin/env bash
# Cut a release. Everything runs in GitHub Actions (see
# .github/workflows/release.yml): version bump, CHANGELOG roll, tag,
# build, GitHub release, pacman-repo publish. This wrapper just presses
# the button and watches. The same button exists in the GitHub UI:
# Actions → Release → Run workflow.
#
#   ./packaging/release.sh minor     # 0.2.0 -> 0.3.0  (the usual)
#   ./packaging/release.sh patch
#   ./packaging/release.sh major
set -euo pipefail

bump="${1:-minor}"
case "$bump" in
  major | minor | patch) ;;
  *)
    echo "usage: $0 [major|minor|patch]" >&2
    exit 2
    ;;
esac

repo="HarkerSoftware/omarchy-workspaces"
echo "==> Dispatching $bump release"
gh workflow run Release -R "$repo" -f "bump=$bump"

echo "==> Waiting for the run to appear"
run_id=""
for _ in $(seq 30); do
  sleep 3
  run_id=$(gh run list -R "$repo" --workflow Release \
    --event workflow_dispatch --limit 1 \
    --json databaseId,status \
    --jq '.[] | select(.status != "completed") | .databaseId' || true)
  [[ -n "$run_id" ]] && break
done
[[ -n "$run_id" ]] || {
  echo "error: dispatched run never appeared; check the Actions tab" >&2
  exit 1
}

echo "==> Watching run $run_id (safe to Ctrl-C; the release continues in CI)"
gh run watch "$run_id" -R "$repo" --exit-status
echo "==> Done: GitHub release published, pacman repo updated."
