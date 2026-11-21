#!/usr/bin/env bash
# Builds the Arch package of this branch's commits applied to release TAG into ./out:
# rebase.sh makes the source in ./src, its custom/ci/PKGBUILD gets the version and pkgrel, and
# container-build.sh runs makepkg in an archlinux:base-devel container. ./out also gets
# commits.md, the applied commits for the release notes.
set -euo pipefail
here=$(dirname "$(readlink -f "$0")")
bash "$here/rebase.sh" "$TAG" "$GITHUB_SHA" src | tee commits.md

mkdir -p pkg out
git -C src archive --format=tar.gz --prefix=spotify-player/ -o "$PWD/pkg/spotify-player.tar.gz" HEAD
# pkgrel 1.<run number>: every build is newer than the last for pacman, also a rebuild of the same
# release with new commits of this branch (outside Actions the PKGBUILD's own pkgrel stays)
sed -e "s/^pkgver=.*/pkgver=${TAG#v}/" \
  ${GITHUB_RUN_NUMBER:+-e "s/^pkgrel=.*/pkgrel=1.$GITHUB_RUN_NUMBER/"} \
  src/custom/ci/PKGBUILD > pkg/PKGBUILD

docker run --rm \
  -v "$PWD/pkg:/pkg" -v "$PWD/out:/out" -v "$PWD/src/custom/ci:/ci:ro" \
  archlinux:base-devel bash /ci/container-build.sh
sudo chown -R "$(id -u):$(id -g)" out
mv commits.md out/
ls -l out
