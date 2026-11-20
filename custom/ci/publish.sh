#!/usr/bin/env bash
# Publishes the package in ./dist (from build.sh) as release RELEASE, then makes it the package of
# the pacman repository kept as the assets of the `pacman-repo` release.
set -euo pipefail
{
  echo "spotify-player ${TAG#v} ([upstream release](https://github.com/aome510/spotify-player/releases/tag/$TAG)) with these commits of \`spotify-player-custom\` ($GITHUB_SHA):"
  echo
  cat dist/commits.md
  echo
  echo "Install through the pacman repository (see \`custom/README.md\`), or once: \`sudo pacman -U <package URL>\`."
} > notes.md
gh release create "$RELEASE" dist/*.pkg.tar.zst --repo "$GITHUB_REPOSITORY" \
  --target "$GITHUB_SHA" --title "spotify-player ${TAG#v}, custom" --notes-file notes.md

mkdir repo
cp dist/*.pkg.tar.zst repo/
docker run --rm -v "$PWD/repo:/repo" archlinux:base \
  bash -c 'cd /repo && repo-add -q spotify-player-custom.db.tar.gz ./*.pkg.tar.zst'
sudo chown -R "$(id -u):$(id -g)" repo
# Release assets can't be symlinks, so the names pacman asks for are copies.
cd repo
rm spotify-player-custom.db spotify-player-custom.files
cp spotify-player-custom.db.tar.gz spotify-player-custom.db
cp spotify-player-custom.files.tar.gz spotify-player-custom.files
if ! gh release view pacman-repo --repo "$GITHUB_REPOSITORY" >/dev/null 2>&1; then
  gh release create pacman-repo --repo "$GITHUB_REPOSITORY" --target "$GITHUB_SHA" --latest=false \
    --title "pacman repository" \
    --notes "pacman repository of spotify-player-custom: Server = https://github.com/$GITHUB_REPOSITORY/releases/download/pacman-repo"
fi
stale=$(gh release view pacman-repo --repo "$GITHUB_REPOSITORY" --json assets --jq '.assets[].name' \
  | grep '\.pkg\.tar\.zst$' | grep -vxF "$(basename -a ./*.pkg.tar.zst)" || true)
# The package goes up before the database that lists it, and old packages go after.
gh release upload pacman-repo ./*.pkg.tar.zst --repo "$GITHUB_REPOSITORY" --clobber
gh release upload pacman-repo spotify-player-custom.db spotify-player-custom.files \
  spotify-player-custom.db.tar.gz spotify-player-custom.files.tar.gz --repo "$GITHUB_REPOSITORY" --clobber
for asset in $stale; do
  gh release delete-asset pacman-repo "$asset" --repo "$GITHUB_REPOSITORY" --yes
done
