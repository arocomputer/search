#!/bin/sh
# Publish a generated formula through an immutable, bot-owned release PR.
# Run on a clean main checkout with GH_TOKEN set by Actions; never replaces a ref.
set -eu
tag=$1
formula=$2
fail() { echo "formula-pr: $*" >&2; exit 1; }
printf '%s\n' "$tag" | LC_ALL=C grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || fail 'expected vX.Y.Z'
case "$formula" in tap.rb|search.rb) ;; *) fail 'unexpected formula path' ;; esac
branch="release/formula-$tag"
ref="refs/heads/$branch"
git diff --quiet && git diff --cached --quiet || fail 'checkout must be clean'
[ -z "$(git ls-files --others --exclude-standard)" ] || fail 'checkout has untracked files'
gh release download "$tag" -R "$GITHUB_REPOSITORY" -p checksums.txt -O "$RUNNER_TEMP/checksums.txt" --clobber
scripts/formula.sh "$tag" "$RUNNER_TEMP/checksums.txt" > "$RUNNER_TEMP/formula.rb"
cp "$RUNNER_TEMP/formula.rb" "$formula"
git add -- "$formula"
git diff --cached --quiet && exit 0
git diff --cached --check
ownership=$(gh pr list --state open --base main --head "$branch" --json number,author,isCrossRepository \
  --jq '[length, all(.[]; (.author.login == "github-actions[bot]" or .author.login == "app/github-actions") and (.isCrossRepository == false))] | map(tostring) | join(" ")')
case "$ownership" in
  '0 true') count=0 ;;
  '1 true') count=1 ;;
  *) fail 'formula PR is ambiguous or not owned by the bot' ;;
esac
remote=$(git ls-remote origin "$ref")
if [ -n "$remote" ]; then
  git fetch --no-tags origin "$ref"
  head=$(git rev-parse FETCH_HEAD)
  base=$(git merge-base origin/main "$head")
  [ "$(git rev-list --count "$base..$head")" = 1 ] || fail 'formula branch has additional commits'
  [ "$(git diff --name-only "$base" "$head")" = "$formula" ] || fail 'formula branch changes other paths'
  [ -z "$(git diff --summary "$base" "$head")" ] || fail 'formula branch changes file types'
  git show "$head:$formula" > "$RUNNER_TEMP/existing-formula.rb"
  cmp -s "$RUNNER_TEMP/formula.rb" "$RUNNER_TEMP/existing-formula.rb" || fail 'formula branch has different content'
  [ "$(gh api "repos/$GITHUB_REPOSITORY/commits/$head" --jq '.author.login')" = 'github-actions[bot]' ] ||
    fail 'formula commit is not owned by the bot'
else
  [ "$count" -eq 0 ] || fail 'formula PR branch is missing'
  git config user.name 'github-actions[bot]'
  git config user.email '41898282+github-actions[bot]@users.noreply.github.com'
  git -c core.hooksPath=/dev/null commit -m "release: formula $tag"
  gh auth setup-git
  git push origin "HEAD:$ref"
fi
# GITHUB_TOKEN events do not start CI automatically.
gh workflow run ci.yml --ref "$branch"
if [ "$count" -eq 0 ]; then
  gh pr create --base main --head "$branch" --title "release: formula $tag" \
    --body 'Updates the Homebrew formula from published release checksums. CI is dispatched on the formula branch. Review and merge manually.'
fi
