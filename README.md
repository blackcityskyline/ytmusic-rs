# ytmusic-cli

A fork of [`sonorahq/ytmusic-rs`](https://github.com/sonorahq/ytmusic-rs)
(GPL-3.0-or-later, see `COPYING`) that adds what the Vicinae YouTube Music
extension draws — a playlist's description, owner and length, and the header's
count read whole — and the JSON bridge on top: the extension speaks JSON to this
binary and never links the crate. `upstream` points at the original, so its
changes merge in with `git merge upstream/main`.

One request on stdin, one answer on stdout, one process per call:

```console
$ echo '{"command":"search","args":{"query":"daft punk","filter":"albums"}}' | ytmusic-cli
{"ok":true,"data":[{"items":[{"album":{...}}]}]}
```

An error is `{"ok":false,"error":"..."}`, still on stdout and still exit `0`:
the caller reads the flag. An absent field is absent, never `null`.

## Commands

| `command` | `args` | answer |
| --- | --- | --- |
| `album` | `browseId` | `{album, description?, tracks}` |
| `artist` | `browseId` | `{browseId, name, description?, subscribers?, thumbnails, topTracks, albums, singles}` |
| `discography` | `channelId` | `[album]` |
| `playlist` | `playlistId` | `{playlist, description?, tracks, continuation?}` |
| `playlistContinuation` | `continuation` | `{tracks, continuation?}` |
| `playlistSummaries` | `playlistIds` | `[playlist]`, rows that could not be fetched left out |
| `search` | `query`, `filter?` (`all`, `songs`, `videos`, `albums`, `artists`, `playlists`) | `[shelf]` |
| `shelves` | `browseId`, `params?` | `[shelf]` |
| `library` | `browseId` or `continuation` | `{items, continuation?}` |
| `like` | `videoId`, `liked` | `null` |
| `likePlaylist` | `playlistId`, `saved` | `null` |
| `addTrack` | `playlistId`, `videoId` | `null` |

A shelf is `{title?, items}` where an item is one of `{track}`, `{album}`,
`{playlist}` or `{page}`.

A playlist's `duration` is the length the same answer can prove: the summed tracks
when the page holds the whole playlist (so a rounded header "12 hours" becomes
"12 hr 10 min"), otherwise what the header states. A header that names only a
count ("423 songs") states none, and the answer does not page through the
playlist to invent one.

`trackCount` is the header's own count — the line is read whole, so
"100 views • 172 tracks • 12 hours" is 172. A page that still has a continuation
holds only the first part of the list and states no count rather than passing off
the tracks it happens to hold.

## The session

The environment alone, never `argv`:

| Variable | Meaning |
| --- | --- |
| `YTMUSIC_COOKIES` | the `Cookie` header of a signed-in session |
| `YTMUSIC_AUTHUSER` | account index, default `0` |
| `YTMUSIC_CACHE` | player cache directory, default `$XDG_CACHE_HOME/ytmusic` |

Without `YTMUSIC_COOKIES` the client is a guest: public pages answer, the
library and likes do not.

## Install

```bash
cargo install --path . --bin ytmusic-cli
```

That puts `ytmusic-cli` in `~/.cargo/bin`. A launcher started by a compositor
does not always have that directory on its `PATH`, so the Vicinae extension also
tries `~/.cargo/bin/ytmusic-cli` and `/usr/local/bin/ytmusic-cli`, and honours
`YTMUSIC_BINARY`.

## What this repository adds to the crate

- `PlaylistDetail.description` and `.duration`, so a playlist card can carry the
  owner's own words and its length.
- `parse::duration_of_text`: the header's `6 hours, 5 minutes` as a length.
- `owner_of()`: the facepile names a playlist's owner; the sharing state
  (`Public`, `Private`, `Unlisted`), the kind (`Playlist`, `Album`, `Chart`, …)
  and a year never do.
- The second subtitle is joined before it is read, so a length split over several
  runs is not lost.

`cargo test --lib` covers these alongside the crate's own tests.

## Daemon (work in progress)

`crates/ytmusicd` is a workspace member next to this crate, which it uses as a library and
does not modify. It is a daemon that owns the queue and playback state, plus two binaries:

- `ytmusicd` listens on `$XDG_RUNTIME_DIR/ytmusic/ytmusicd.sock`, drives one long-lived
  `mpv` (or, with `--output native` in a `--features native` build, plays in-process), and exits when nothing plays and nobody is connected.
- `ytmusicctl` is a scripting client: `ytmusicctl play '{"tracks":[…]}'`, `ytmusicctl watch`.
  It starts the daemon if it is not running.

The protocol is in [`docs/protocol.md`](docs/protocol.md). `cargo test -p ytmusicd` runs its
tests, none of which need a network, audio or `mpv`; what only a real machine can check is in
[`docs/testing-on-device.md`](docs/testing-on-device.md). Not built yet: browse through the
daemon, MPRIS, the `resolve` command.
