#!/usr/bin/env bash
# Opens an issue for a spotify-player release newer than the one this branch sits on, once per
# release: its notes and the files both it and this branch change. Run from the repository root of
# a full clone, with GH_TOKEN set.
set -euo pipefail
upstream=aome510/spotify-player
latest=$(gh api "repos/$upstream/releases/latest" --jq .tag_name)
# spotify-player's release tags, which a fork doesn't have: the release this branch sits on is the
# newest of them in its history
git fetch --quiet --no-tags "https://github.com/$upstream" 'refs/tags/v*:refs/tags/v*'
base=$(git describe --tags --abbrev=0 --match 'v[0-9]*' HEAD)
echo "spotify-player $latest, this branch on $base"
[ "$latest" = "$base" ] && exit 0

title="spotify-player $latest released"
# a fork starts with issues off: then the failed run, which GitHub mails to the owner, announces it
if [ "$(gh api "repos/$GITHUB_REPOSITORY" --jq .has_issues)" != true ]; then
  echo "::error::$title (this branch is on $base); turn on Issues in the repository settings to get it as an issue"
  exit 1
fi
if [ -n "$(gh issue list --repo "$GITHUB_REPOSITORY" --state all --search "\"$title\" in:title" --json number --jq '.[].number')" ]; then
  echo "already announced"
  exit 0
fi

# the files changed upstream since $base and by this branch
upstream_files=$(gh api "repos/$upstream/compare/$base...$latest" --jq '.files[].filename' | sort -u)
branch_files=$(git diff --name-only "$base" HEAD | sort -u)
shared=$(comm -12 <(echo "$upstream_files") <(echo "$branch_files") | sed 's/^/- `/; s/$/`/')
notes=$(gh release view "$latest" --repo "$upstream" --json body --jq .body)

body=$(cat <<BODY
[spotify-player $latest](https://github.com/$upstream/releases/tag/$latest) is out; \`spotify-player-custom\` is on $base. ([all changes](https://github.com/$upstream/compare/$base...$latest))

Files this release and this branch both change:
${shared:-none}

<details><summary>Release notes</summary>

$notes

</details>
BODY
)
gh issue create --repo "$GITHUB_REPOSITORY" --title "$title" --body "$body"
