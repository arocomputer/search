#!/bin/sh
# Releases search, from an up-to-date main checkout, in two runs of the same command:
#
#   scripts/release.sh v0.2.0    opens a PR that names CHANGELOG.md's Unreleased section v0.2.0
#   scripts/release.sh v0.2.0    once that PR is merged: checks CI, then asks Actions to tag
#
# The workflow pushes the tag with RELEASE_TOKEN; that tag starts the release
# pipeline. Actions builds, publishes, updates the Homebrew formula and tests
# installation. Needs git and gh. Your checkout stays on main.
set -eu

fail() { echo "release: $*" >&2; exit 1; }
tag=${1:-}
case $tag in v[0-9]*.[0-9]*.[0-9]*) ;; *) fail "usage: scripts/release.sh vX.Y.Z" ;; esac
command -v gh >/dev/null 2>&1 || fail "needs gh, the GitHub CLI"
cd "$(git rev-parse --show-toplevel)"

[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || fail "run it on main"
[ -z "$(git status --porcelain)" ] || fail "commit or set aside your changes first"
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || fail "main is not origin/main; pull first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && fail "$tag already exists"

if ! grep -q "^## $tag " CHANGELOG.md; then
  # First run: name the Unreleased section in a pull request.
  grep -q '^## Unreleased$' CHANGELOG.md || fail "CHANGELOG.md has no ## Unreleased section to release"
  notes=$(awk '/^## /{on = ($0 == "## Unreleased"); next} on' CHANGELOG.md | grep -c '[^[:space:]]' || true)
  [ "$notes" -gt 0 ] || fail "the Unreleased section is empty"
  work=$(mktemp -d "${TMPDIR:-/tmp}/search-release.XXXXXX")
  trap 'git worktree remove --force "$work" >/dev/null 2>&1; rm -rf "$work"' EXIT
  git worktree add -q -b "release/$tag" "$work" origin/main
  sed "s/^## Unreleased\$/## $tag · $(date -u +%Y-%m-%d)/" CHANGELOG.md > "$work/CHANGELOG.md"
  git -C "$work" commit -qam "release: $tag"
  git -C "$work" push -q -u origin "release/$tag"
  (cd "$work" && gh pr create --title "release: $tag" --body "Names the Unreleased changelog section $tag. After merging, run \`scripts/release.sh $tag\` on main to start the GitHub Actions release.")
  echo "Merge that pull request, pull main, then run scripts/release.sh $tag again."
  exit 0
fi

# Second run: the section is on main. Ask Actions to tag it once CI has passed.
commit=$(git rev-parse HEAD)
result=$(gh run list --workflow ci.yml --commit "$commit" --json status,conclusion -q '.[0] | "\(.status) \(.conclusion)"')
case $result in
  "completed success") ;;
  "") fail "CI has not run on $commit yet" ;;
  *) fail "CI on $commit is \"$result\"; release once it passes" ;;
esac
gh workflow run release.yml --ref main -f "tag=$tag"
echo "Asked GitHub Actions to tag $tag and start the release. Watch with: gh run watch \$(gh run list --workflow release.yml -L 1 --json databaseId -q '.[0].databaseId')"
