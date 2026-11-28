# spotify-player-custom

Builds of [spotify-player](https://github.com/aome510/spotify-player) for Arch Linux that pick up where they left off. The branch `spotify-player-custom` is a spotify-player release tag plus this fork's commits: the changes below, the played history listener and this build setup. GitHub Actions builds every new spotify-player release with them and publishes the Arch package as a release and in a pacman repository hosted on this repository's releases.

| Path | What it is |
|---|---|
| `soloist_history/`, `custom/soloist-history.service` | The played history: its record format, shared with the app, and the `soloist-history` listener with its user service (see [Played history](#played-history)), installed by the package |
| `custom/ci/` | The release announcement (`announce.sh`), the checks (`check.sh`) and the build: `plan.sh`, `rebase.sh`, `build.sh` with `container-build.sh` and `PKGBUILD`, `publish.sh` |
| `.github/workflows/` | `upstream_release.yml` announces new releases; `spotify_player_custom.yml` builds when started by hand. Both run `custom/ci/` scripts; GitHub only runs workflows from that folder |

## The changes

| Change | What it does |
|---|---|
| Session | The app reopens on the pages that were open when it closed, with the same rows selected and the same search. It's stored once a second when it changes, and on quit, in `Session_cache.json` in the cache folder, so it survives a closed terminal or a reboot. |
| Playing list | A list the app plays without Spotify knowing its context (a radio, the liked tracks) stays the playing context across restarts, kept in `PlayingTracks_cache.json`. It's dropped once Spotify plays something outside it with none of its tracks queued after; a track queued in between keeps it. |
| `continue_track_lists` | On by default. Spotify is given at most `tracks_playback_limit` (50) tracks of a list the app plays (the liked tracks, a radio), so a radio (about 50 tracks) stopped at its end and the liked tracks about 25 tracks after the one started. Once Spotify's queue holds none of the list after the playing track, the app adds the list's next track to the queue while that track plays: one request per track, never a batch. Tracks queued by hand play first. With shuffle on, a random other track of the list follows; with repeat on, nothing is added. At the last track of a radio, the app fetches a new radio of that track and appends the tracks the list doesn't have yet, kept with the radio (`Radios_cache.json`) and the playing list, so the radio's page shows the longer list. |
| Resume | When no device has a playback on startup, the app connects to `preferred_device`, looking for 30 seconds while Spotify lists no device (one reconnecting after a resume from hibernation shows up late). If Spotify doesn't resume its own last playback there, the previous session's playback is started again, muted, paused at the same position (to the 10 seconds) and unmuted. On a Soloist running on this machine this goes through Soloist's own API, without a sound: its volume is held at 0, its playback paused the moment it starts, and the restore is skipped when Soloist already holds the track. |
| `preferred_device` | Name of the Spotify Connect device to use when no device is active, also when a track is started with no playback, and to take over playback on. Unset: the Spotify Soloist device running on this machine, found by its process's `-n` argument; without one, as upstream, the integrated device, then the first device Spotify lists. |
| Instant tracks | A track started from a list shows the moment it's asked for, with the cover of its album if it was played before. **Next** and **Previous** show the next track of the queue and the one before at once when no Soloist is followed; with one, its report shows the right track about 100 ms later, as guesses fall behind a quick series of presses. The covers of the next queued tracks are loaded ahead. If the request fails, the track shown before comes back. |
| Soloist events | With a Spotify Soloist running on this machine, the app follows its playback through Soloist's local WebSocket API: track changes, pauses and positions show as they happen, also when they come from elsewhere, and a playback command checks Spotify once afterwards instead of twice. Starting a track sends the shuffle state again only on the app's own device, which needs it. |
| Played history | With the listener below (which also presents Soloist as an MPRIS player), the recently played tracks page lists the tracks played on the Soloist device instead of Spotify's recently played tracks of every device. |
| Other devices | `g d` shows the list playing now on any device (the phone may only show it), or else the list played last: a playlist, album, artist or the liked tracks, with its track selected. The whole list when Spotify gives it to this app; for the mixes and radios Spotify makes (which it refuses this app), the tracks played from it, the track it's at and Spotify's queue while it plays. Enter on the track the device plays moves its playback here with its position and queue; on another track it plays the list here from that track. `g D` lists the track another device plays first, then Spotify's recently played tracks of every device, newest first (Spotify counts a track once it has played 30 seconds, as the next one starts). Both pages are fetched again 3 seconds after each new track while they're shown. Nothing else is sent to the other device. |
| `played_history_size`, `played_history_min_played_secs` | Tracks kept in the played history (1500), and the time a track has to play to be listed (10 seconds). |
| Startup | The app starts with the playback and the page it reopens on. The library (playlists, saved albums, followed artists, saved shows) and the liked tracks show what's cached, and are requested when the library or the liked tracks are opened; the liked tracks then only take the ones liked since, and tracks unliked in another app are found in the background with a few requests, a few seconds apart. The liked tracks are also refreshed in the background 6 seconds after startup and every 10 minutes while nothing plays (one request when nothing changed), for the liked marks. The account is cached too, and `spotify_player authenticate` clears the cached library and session when another account logs in. Upstream requests all of it on every start, a request per 50 liked tracks, which can run into Spotify's rate limit. |
| Cover first | On a new track the cover loads before the artist's genres, which used to cost the cover a request to Spotify on every new artist. |
| Small covers | A pixelated cover is kept at its pixelated size (`cover_img_pixels`) instead of being scaled back up to the original 640 px, and at most 32 covers are kept: at 256 px a cover takes 0.2 MB of memory instead of 1.2 MB. It's scaled up when drawn, with sharp blocks as before. |
| Memory trim | Once a minute the app hands memory its allocator kept after freeing it back to the system (glibc's `malloc_trim`). glibc otherwise keeps each thread's heap at its peak, so the memory used for decoding covers or parsing the liked tracks stayed taken. |
| `take_over_playback` | On by default: a playback command from this app (play, pause, next, seek, volume, starting a track, media keys, CLI) first moves the playback to `preferred_device` when another device has it, unless that device was chosen in this app (`D`, or `spotify_player connect`). |

## After a spotify-player release

Nothing happens on its own. Once a day `upstream_release.yml` (`custom/ci/announce.sh`) compares spotify-player's latest release with the release this branch sits on, and opens an issue for a new one: its release notes and the files both the release and this branch change. Rebase by hand, check and push:

```sh
git fetch upstream tag <new tag>
git rebase --onto <new tag> <old tag> spotify-player-custom
custom/ci/check.sh
git push --force-with-lease origin spotify-player-custom
```

Then build: Actions → Build spotify-player-custom → Run workflow.

## How a build happens

The build workflow runs only when started by hand; its schedule and push triggers are off.

1. The `checks` job runs `custom/ci/check.sh`; a failure stops the build.
2. The `build` job cherry-picks this branch's own commits onto the requested release, the latest by default (`custom/ci/rebase.sh`). On a branch rebased onto that release they apply as they are.
3. It builds `custom/ci/PKGBUILD` (Arch's `extra/spotify-player` without streaming, which a Soloist device does, with the `pixelate` feature and the listener added, built for Zen 5, `pkgrel=1.<run number>` so that every build is an upgrade for pacman, also a rebuild of the same release) in an `archlinux:base-devel` container (`custom/ci/build.sh`, `custom/ci/container-build.sh`).
4. The `release` job (`custom/ci/publish.sh`) publishes the package as release `spotify-player-<version>-custom-znver5.<id>` (the CPU from the PKGBUILD's `_cpu`, also in its title), with the applied commits in its notes, and replaces the package and database in the `pacman-repo` release, which is the pacman repository.

## Played history

Spotify Soloist is Spotify's own headless Connect player. `soloist-history` (the `soloist_history` crate) follows the tracks a Soloist device plays, through its local WebSocket API (`soloist -w 127.0.0.1:0`), and appends one MessagePack record per track to `PlayedHistory.msgpack` in spotify-player's cache folder: when it started, how long it played (pauses and time on other devices don't count), what it played from, and the track's ID, name, artists, album, cover and length. The app reads the records with the same crate. The listener runs whether the app is open or not and is the file's only writer: it keeps the last `played_history_size` records of `app.toml` (1500), and drops a record a crash cut short. Stopping the service writes the track playing at that moment; when Soloist restarts, the service connects to the new one.

It also presents Soloist to the desktop as an MPRIS player, `org.mpris.MediaPlayer2.soloist` ("Spotify Soloist"): the track, its cover and position as Soloist's events report them, so media widgets follow at once and without the app; and play, pause, next, previous and seek from media widgets and media keys go to Soloist as commands. The app's own MPRIS player would show the same playback a second time, so set `enable_media_control = false` in `app.toml`.

```sh
systemctl --user enable --now soloist-history
```

It finds Soloist's WebSocket address in Soloist's data directory, `$SOLOIST_DATA_DIR` or `~/.local/share/soloist`. Without the history file the app shows Spotify's recently played tracks, as upstream does.

## Built for Zen 5

The app (`/usr/bin/spotify_player`) and the listener are built with `-C target-cpu=znver5`, so the package runs on Zen 5 CPUs only; other CPUs stop it with "illegal instruction". The runner's CPU can't run it either, so the flag only reaches the app itself, through `--target`: build scripts and proc macros, which run during the build, stay generic, and so do the tests `check()` runs. For other CPUs change `_cpu` in `custom/ci/PKGBUILD`, or use `x86-64-v3` (any CPU with AVX2).

## Rebasing these changes

The changes keep to their own modules (`state/session.rs`, `state/played_history.rs`), their own crate (`soloist_history`) and additions next to upstream's code: of upstream's own lines they change about 60, most of them one-line edits (serde derives on the page state types, `use` lines, the cache file write in `state/data.rs`). The larger ones are in `client/mod.rs`: `find_available_device`, the start of `handle_player_request`, the `ClientRequest::Player` and `GetContext` branches, and `handle_new_playback_event`, where the genre lookup moved after the cover. A release that rewrites one of those stops the build at that commit (see above). `soloist_history/` and `custom/` aren't upstream files, so they never conflict; the root `Cargo.toml` changes by one line, adding the crate to the workspace.

## Install

Add the repository to `/etc/pacman.conf`, above `[extra]`:

```ini
[spotify-player-custom]
SigLevel = Optional TrustAll
Server = https://github.com/nonamekk/spotify-player/releases/download/pacman-repo
```

Then `sudo pacman -Syu spotify-player` (it replaces `spotify-player-feats-git`). The packages aren't signed; `SigLevel = Optional TrustAll` accepts them, so the trust is in this GitHub account. To go back to Arch's package, remove the block and run `sudo pacman -Syuu`.

## Setup on GitHub

- `spotify-player-custom` is the repository's default branch: the daily release check is a scheduled workflow, and those only start from it.
- Issues are on (a fork starts with them off): new releases are announced as issues. With them off the daily check fails instead, and GitHub mails the failed run.
- Actions are enabled for the repository (a fork starts with them off). Upstream's workflows are removed from this branch, so only this build runs, and so are its Nix files (`flake.nix`, `flake.lock`, `default.nix`), which nothing here uses. `rebase.sh` leaves commits that only remove those out of the build, so upstream changing them never stops it; keep them in a commit of their own.
- GitHub turns scheduled workflows off after 60 days without activity in a public repository; a manual run of "Announce spotify-player releases" turns it back on.
