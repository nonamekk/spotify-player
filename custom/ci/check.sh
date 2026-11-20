#!/usr/bin/env bash
# The checks of upstream's CI on this branch, Linux only: formatting, clippy with CI's features and
# without features, and the tests, for spotify_player and for the soloist_history listener.
# Clippy's map_unwrap_or (newer than upstream's code) is allowed, so only this branch's warnings fail.
#
#   custom/ci/check.sh    (from the repository root)
set -euo pipefail
features=rodio-backend,media-control,image,notify,fzf
lints=(-D warnings -A clippy::map_unwrap_or)

cargo fmt --all -- --check
cargo clippy --locked -p spotify_player --no-default-features --features "$features" -- "${lints[@]}"
cargo clippy --locked -p spotify_player --no-default-features -- "${lints[@]}"
cargo clippy --locked -p soloist_history --all-targets -- "${lints[@]}"
cargo test --locked -p spotify_player --no-default-features --features "$features"
cargo test --locked -p soloist_history
