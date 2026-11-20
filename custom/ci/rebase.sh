#!/usr/bin/env bash
# The source to build: this branch's own commits (those after the release tag it sits on),
# cherry-picked onto the requested spotify-player release tag. A conflict stops the build;
# the branch then needs rebasing by hand. Commits that only change upstream's workflows and Nix
# files (this branch removes them) stay out of the build, so their changes upstream can't stop it.
#
#   custom/ci/rebase.sh <release tag> <branch commit> <dir>
#
# Prints one Markdown line per applied commit, for the release notes.
set -euo pipefail
tag=$1 head=$2 dir=$3

git clone -q --filter=blob:none --no-checkout https://github.com/aome510/spotify-player "$dir"
cd "$dir"
git fetch -q "https://github.com/$GITHUB_REPOSITORY" "$head"
base=$(git describe --tags --abbrev=0 --match 'v[0-9]*' "$head")
git -c advice.detachedHead=false checkout -q "refs/tags/$tag"
export GIT_AUTHOR_NAME=spotify-player-custom GIT_AUTHOR_EMAIL=ci@invalid
export GIT_COMMITTER_NAME=spotify-player-custom GIT_COMMITTER_EMAIL=ci@invalid

for commit in $(git rev-list --reverse "$base..$head" -- . ':!.github/workflows' ':!flake.nix' ':!flake.lock' ':!default.nix'); do
  if ! git cherry-pick --allow-empty -x "$commit" >/dev/null; then
    git cherry-pick --abort || true
    echo "::error::$(git log -1 --format='%h %s' "$commit") doesn't apply to $tag; rebase spotify-player-custom onto $tag and push it" >&2
    exit 1
  fi
  echo "- $(git log -1 --format='%h %s' "$commit") (from $base)"
done
