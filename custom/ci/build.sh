#!/usr/bin/env bash
# Builds the Arch package of this branch's commits applied to release TAG into ./out:
# rebase.sh makes the source in ./src, its custom/ci/PKGBUILD gets the version, and
# container-build.sh runs makepkg in an archlinux:base-devel container. ./out also gets
# commits.md, the applied commits for the release notes.
set -euo pipefail
here=$(dirname "$(readlink -f "$0")")
bash "$here/rebase.sh" "$TAG" "$GITHUB_SHA" src | tee commits.md

mkdir -p pkg out
git -C src archive --format=tar.gz --prefix=spotify-player/ -o "$PWD/pkg/spotify-player.tar.gz" HEAD
sed "s/^pkgver=.*/pkgver=${TAG#v}/" src/custom/ci/PKGBUILD > pkg/PKGBUILD

docker run --rm \
  -v "$PWD/pkg:/pkg" -v "$PWD/out:/out" -v "$PWD/src/custom/ci:/ci:ro" \
  archlinux:base-devel bash /ci/container-build.sh
sudo chown -R "$(id -u):$(id -g)" out
mv commits.md out/
ls -l out
