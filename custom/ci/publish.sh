#!/usr/bin/env bash
# Publishes the package in ./dist (from build.sh) as release RELEASE, then makes it the package of
# the pacman repository kept as the assets of the `pacman-repo` release.
set -euo pipefail
cpu=$(sed -n 's/^_cpu=//p' "$(dirname "$(readlink -f "$0")")/PKGBUILD")
case $cpu in
  znver5) cpu_name="Zen 5" ;;
  *) cpu_name=$cpu ;;
esac
readme="https://github.com/$GITHUB_REPOSITORY/blob/spotify-player-custom/custom/README.md"
{
  echo "Runs only on $cpu_name CPUs; other CPUs stop it with \"illegal instruction\"."
  echo
  echo "spotify-player ${TAG#v} ([upstream release](https://github.com/aome510/spotify-player/releases/tag/$TAG)) with these commits of \`spotify-player-custom\` ($GITHUB_SHA):"
  echo
  cat dist/commits.md
  echo
  echo "Install through the pacman repository (see \`custom/README.md\`), or once: \`sudo pacman -U <package URL>\`."
  cat <<EOF

<details>
<summary>First install</summary>

1. **The package.** Add the repository to \`/etc/pacman.conf\`, above \`[extra]\`, then run \`sudo pacman -Syu spotify-player\`:
   \`\`\`ini
   [spotify-player-custom]
   SigLevel = Optional TrustAll
   Server = https://github.com/$GITHUB_REPOSITORY/releases/download/pacman-repo
   \`\`\`
2. **The player.** Set up Spotify Soloist, Spotify's headless Connect player, as a user service with the files in \`custom/soloist/\`: see [Soloist]($readme#soloist). The package itself doesn't play anything.
3. **The played history and media controls:** \`systemctl --user enable --now soloist-history\` (see [Played history]($readme#played-history)).
4. **The app.** In \`~/.config/spotify-player/app.toml\`, set \`client_id\` to your own Spotify app's ([how](https://github.com/aome510/spotify-player#using-a-custom-client-id)), \`enable_streaming = "Never"\` and \`enable_media_control = false\` (soloist-history is the MPRIS player). Start \`spotify_player\` and log in once in the browser; it plays on the Soloist device by itself.

</details>
EOF
} > notes.md
gh release create "$RELEASE" dist/*.pkg.tar.zst --repo "$GITHUB_REPOSITORY" \
  --target "$GITHUB_SHA" --title "spotify-player ${TAG#v}, custom, $cpu" --notes-file notes.md

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
