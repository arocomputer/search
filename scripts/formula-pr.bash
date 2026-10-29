#!/usr/bin/env bash
# Publish one formula-only bot PR per release. Existing branches are verified,
# never rewritten; reruns recover a push completed before PR creation.
set -euo pipefail
tag=${1:?usage: formula-pr.bash vX.Y.Z FORMULA}
formula=${2:?usage: formula-pr.bash vX.Y.Z FORMULA}
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Invalid release tag.' >&2; exit 1; }
[[ "$formula" == search.rb || "$formula" == tap.rb ]] || exit 1
branch="release/formula-$tag"
ref="refs/heads/$branch"
bot='github-actions[bot]'
email='41898282+github-actions[bot]@users.noreply.github.com'
# Report unexpected state without changing a remote branch.
fail() { echo "formula-pr: $*" >&2; exit 1; }
# Stage only the generated formula, including its first creation.
git add -- "$formula"
git diff --cached --quiet && exit 0
git diff --cached --check
expected=$(git rev-parse ":$formula")
gh auth setup-git
remote=$(git ls-remote --heads origin "$ref")
if [[ -n "$remote" ]]; then
    git fetch --no-tags origin "$ref"
    head=$(git rev-parse FETCH_HEAD)
    parent=$(git rev-parse "$head^")
    git merge-base --is-ancestor "$parent" origin/main || fail 'Formula branch has an unexpected base.'
    [[ "$(git rev-list --count "$parent..$head")" == 1 ]] || fail 'Formula branch contains extra commits.'
    [[ "$(git show -s --format=%an:%ae:%cn:%ce "$head")" == "$bot:$email:$bot:$email" ]] || fail 'Formula branch is not bot-owned.'
    [[ "$(git diff-tree --no-commit-id --name-only -r "$head")" == "$formula" ]] || fail 'Formula branch contains unexpected edits.'
    [[ "$(git ls-tree "$head" -- "$formula" | cut -d " " -f1)" == 100644 ]] || fail 'Formula branch changes the file mode.'
    [[ "$(git rev-parse "$head:$formula")" == "$expected" ]] || fail 'Formula branch differs from the generated formula.'
else
    git config user.name "$bot"
    git config user.email "$email"
    git -c core.hooksPath=/dev/null commit -m "chore: update formula for $tag"
    head=$(git rev-parse HEAD)
    # An empty lease permits creation only, even if another actor races this push.
    git push --force-with-lease="$ref:" origin "HEAD:$ref"
fi
prs=$(gh pr list --state all --base main --head "$branch" --json number,state,author,headRefOid,isCrossRepository)
if [[ "$(jq length <<< "$prs")" != 0 ]]; then
    [[ "$(jq length <<< "$prs")" == 1 ]] || fail 'Multiple formula PRs need manual review.'
    jq -e --arg head "$head" '.[0] | .state == "OPEN" and (.author.login == "github-actions[bot]" or .author.login == "app/github-actions") and .headRefOid == $head and .isCrossRepository == false' <<< "$prs" >/dev/null || fail 'Existing formula PR needs manual review.'
else
    gh pr create --base main --head "$branch" --title "chore: update formula for $tag" \
        --body "Updates the Homebrew formula to $tag. Review and merge manually after validation."
fi
# GITHUB_TOKEN-created PRs need an explicit CI dispatch; retrying also retries CI.
gh workflow run ci.yml --ref "$branch"
