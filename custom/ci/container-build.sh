#!/usr/bin/env bash
# Runs inside archlinux:base-devel, started by build.sh: installs the PKGBUILD's dependencies, then
# makepkg as a plain user (makepkg refuses root). /pkg holds the PKGBUILD and spotify-player.tar.gz,
# /out receives the package.
set -euo pipefail
pacman -Syu --noconfirm --needed git >/dev/null
# shellcheck source=/dev/null
source /pkg/PKGBUILD
pacman -S --noconfirm --needed "${depends[@]}" "${makedepends[@]}" >/dev/null
useradd -m builder
chown -R builder /pkg /out
runuser -u builder -- env \
  BUILDDIR=/tmp/build PKGDEST=/out SRCDEST=/pkg PACKAGER="spotify-player-custom (GitHub Actions)" \
  CARGO_BUILD_JOBS="$(nproc)" CARGO_INCREMENTAL=0 \
  bash -c 'cd /pkg && makepkg -f --nodeps --skipchecksums'
