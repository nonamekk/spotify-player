#!/usr/bin/env bash
# Decides whether to build: the release tag (INPUT_TAG, else spotify-player's latest release) and
# the release name, which changes with that tag and with this branch's head. A release that already
# exists isn't built again unless FORCE is true. Writes tag, release and build to $GITHUB_OUTPUT.
set -euo pipefail
tag=${INPUT_TAG:-$(gh api repos/aome510/spotify-player/releases/latest --jq .tag_name)}
if [[ ! $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "::error::not a spotify-player release tag: $tag"
  exit 1
fi
id=$(printf '%s\n%s\n' "$tag" "$GITHUB_SHA" | sha256sum | cut -c1-8)
release="spotify-player-${tag#v}-custom.$id"
build=true
if gh release view "$release" --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  if [ "${FORCE:-false}" = true ]; then
    release="$release-r$GITHUB_RUN_NUMBER"
  else
    build=false
  fi
fi
echo "spotify-player $tag, branch head $GITHUB_SHA, release $release, build $build"
{
  echo "tag=$tag"
  echo "release=$release"
  echo "build=$build"
} >> "$GITHUB_OUTPUT"
