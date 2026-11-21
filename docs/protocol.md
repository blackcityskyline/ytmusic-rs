# ytmusicd protocol v1 (draft)

Status: draft. Implemented in `crates/ytmusicd` and tested: the wire types, the queue,
the player state machine, the socket server (`hello`, `subscribe`, `unsubscribe`,
`shutdown`, every playback command, idle exit), the `mpv` output (against a stand-in
for `mpv`'s IPC), and the `ytmusicd` and `ytmusicctl` binaries. Not built yet: the
native output, `resolve` and the data-plane endpoint, browse through the daemon, MPRIS.
What only a real machine can check is in `docs/testing-on-device.md`.
Facts about the current code are marked *(today)*.

## Goals, in priority order

1. **Idle costs nothing.** No timers, no polling, no periodic events. A daemon
   with no playback and no clients exits.
2. **One owner of state.** Queue, position, volume, history and settings live in
   the daemon. Clients are views: the Vicinae extension, a TUI, a GUI, `ytmusicctl`.
3. **Outputs are interchangeable.** `mpv` and `native` implement one interface;
   no client can tell which one plays.
4. **Video is a seam, not a feature.** The control socket never carries media.

## Transport

- Unix stream socket `$XDG_RUNTIME_DIR/ytmusic/ytmusicd.sock` (`~/.cache/ytmusic/` without a
  runtime dir), directory `0700`, socket `0600`.
- UTF-8, one JSON object per line. An absent field is absent, never `null` *(today:
  the CLI bridge)*.
- Whoever needs the daemon and finds no socket starts it, then connects. A second daemon
  that finds the socket answering exits at once, so a race between two starters is harmless.
- Secrets are never in the protocol. The daemon reads `YTMUSIC_COOKIES` and
  `YTMUSIC_AUTHUSER` from its own environment or config *(today: the CLI reads
  them from its environment)*.

## Envelope

```
→ {"id":7,"cmd":"seek","args":{"to":93000}}
← {"id":7,"ok":true,"data":{...}}
← {"id":7,"ok":false,"error":{"code":"rev_conflict","message":"queue changed"}}
← {"event":"state", ...}                       // only after subscribe
```

`id` is chosen by the client and echoed. A line that is not a command is answered with
`bad_request`; it carries the `id` when the line had one and has none otherwise. The
connection stays open. `args` is an object and is required by every command that takes
arguments, even when all of them are optional: `subscribe` is `{"events":[]}` or `{}`, never
bare. Commands without arguments (`pause`, `state`, `shutdown`, …) take no `args`.

## Handshake and versions

First message of every connection:

```
→ {"id":1,"cmd":"hello","args":{"protocol":1,"client":"vicinae-ytmusic"}}
← {"id":1,"ok":true,"data":{"protocol":1,"daemon":"0.1.0",
     "outputs":["mpv","native"],"output":"mpv","caps":{"audio":true,"video":false}}}
```

A different major `protocol` is refused with `unsupported`. Minor changes only
add commands, fields and events; clients ignore what they do not know.

## Browse (unchanged)

`album`, `artist`, `discography`, `playlist`, `playlistContinuation`,
`playlistSummaries`, `search`, `shelves`, `library`, `like`, `likePlaylist`,
`addTrack`: same `args` and answers as the CLI *(today: README table)*. The CLI
becomes a thin client of the daemon, which keeps the client and the page cache warm.

## Playback

| `cmd` | `args` | notes |
| --- | --- | --- |
| `play` | `tracks`, `index?`, `paused?` | replaces the queue; tracks without an id or marked unavailable are left out and `index` follows. A `videoId` shorthand and `video` come with the resolver |
| `queue.add` | `tracks`, `next?` | after the current entry when `next` |
| `queue.remove` | `entry` | by `entry` id, never by index |
| `queue.move` | `entry`, `before?` | |
| `queue.clear` | | keeps the current entry; takes no `if_rev` |
| `queue.get` | | same payload as the `queue` event |
| `next`, `prev`, `pause`, `resume`, `toggle`, `stop` | | |
| `seek` | `to` or `by`, milliseconds | |
| `volume` | `set` or `by`, 0–100 | |
| `mute` | `on?` | toggles without `on` |
| `shuffle` | `on` | |
| `repeat` | `mode`: `off`, `all`, `one` | |
| `state` | | current snapshot |
| `subscribe` / `unsubscribe` | `events` | `state`, `queue`, `notice` |
| `output.set` | `name` | switches while keeping queue and position |
| `resolve` | `videoId`, `video?` | stream descriptor, below |
| `shutdown` | | |

A *track* is a browse `Track` *(today: `models.rs`, with `kind: Song | Video`)*,
serialised by the library's own serde derives.
Every queue item gets an `entry` id from the daemon, stable for its life.
Commands that edit the queue take `if_rev` (the `queue_rev` the client saw); a
mismatch answers `rev_conflict` instead of acting on a shifted list. Without
`if_rev` the last writer wins.

## State and the clock

```
state = { rev, status: "stopped"|"loading"|"playing"|"paused",
          entry?, clock: { position_ms, at_ms, rate },
          volume, muted, shuffle, repeat, output, queue_rev }
```

`at_ms` is Unix time in milliseconds when `position_ms` was true; `rate` is `0`
unless playing. A client shows `position_ms + (now - at_ms) × rate` and draws it
itself. **The daemon never sends a tick.** A `state` event goes out only when
something changed: play, pause, seek, track change, volume, output, error. The
`queue` event carries the whole list and `queue_rev`; it is separate so a volume
change does not resend a long queue.

## Stream descriptors and the data plane

`resolve` answers:

```
{ "videoId": "...", "expires_at": 1763650000000,
  "streams": [ {"role":"audio","itag":251,"mime":"audio/webm","codec":"opus",
                "bitrate":131072,"content_length":4210000} ],
  "headers": {"User-Agent":"..."},
  "local": "http://127.0.0.1:PORT/s/<token>" }
```

- `local` is the daemon's loopback endpoint with `Range` support, behind an
  unguessable token. It is the data plane: `mpv`, a GUI's player or any other
  renderer can read from it. It exists because the stream host serves a plain GET
  at a trickle and drops long connections *(today: `stream.rs`)*, so the daemon
  reads in bounded ranges and renews a refused URL; a renderer given the raw URL
  would hit that.
- `streams` may hold several entries (`role`: `audio`, `video`, `muxed`). Today
  only audio exists *(today: `AudioFormat`; there is no video format model, it
  must be added)*.
- Media never goes through the control socket.

## Video

- `play {video:true}` asks for an output with video. If the active output has
  none, the answer is `unsupported`; nothing falls back silently.
- Default is audio only: less traffic, less CPU. A `Track` with `kind: Video` is
  a hint for a client that wants to offer it.
- `mpv`: video is a window of `mpv` itself, hardware decoding when available.
- `native`: no video. A GUI client decodes video from the data plane and keeps
  A/V sync itself; the daemon stays the owner of queue and state.
- Left open on purpose: a client registering as an output. Nothing in the
  protocol forbids it, nothing builds it yet.

## Outputs (internal, not on the wire)

```
trait Output {
    fn load(&mut self, entry: &Entry, start_ms: u64, paused: bool);
    fn preload(&mut self, next: Option<&Entry>);  // the entry after this one, if any
    fn pause(&mut self, on: bool);
    fn seek(&mut self, to_ms: u64);
    fn set_volume(&mut self, percent: u8, muted: bool);
    fn stop(&mut self);
}
// Events back: Started{position_ms}, Discontinuity{position_ms}, Ended, Failed{reason}.
```

The output resolves the stream itself (through the resolver), and decides when to
prepare the entry `preload` names, typically near the end of the current one. It
reports only discontinuities; the player core holds the clock. Capabilities
(`audio`, `video`) are added with the first output that has a say in them. One
conformance suite runs against every output: play, pause, seek, track change,
gapless join, failure on a dead URL, stop.

## Errors

`bad_request`, `unsupported`, `not_found`, `rev_conflict`, `unavailable` (the
track cannot be played), `sign_in_required` *(today: `SignInRequired`)*,
`network`, `output_failed`, `internal`. `message` is English and for humans;
clients branch on `code`.

## Resource rules (normative)

- No timer runs while nothing plays and nobody subscribes.
- No event is sent on an interval.
- Single-threaded async runtime for control and downloads; one thread for the
  audio callback in `native`.
- Track spools are unlinked files in the cache dir, not heap *(today: Sonora's
  `stream.rs` does this; borrowing it is GPL-3.0-or-later only)*.
- URL cache is bounded in entries and by `expires_at`.
- The signature interpreter (`rquickjs`) is created on demand and dropped after
  the resolve.
- The next entry is preloaded only near the end of the current one.
- The daemon exits after a quiet period with no playback and no clients.

## Open questions

1. History: the daemon owns it, the extension's local file goes away?
2. Settings: one `config.toml` for the daemon; do client preferences override per
   connection (`hello`) or not at all?
3. Library writes (`like`, `addTrack`) through the daemon only, or still direct?
4. Gapless between `mpv` entries: `append` the next one, or keep one entry and
   `loadfile` on `Ended`?
5. Normalisation: `loudness_db` is already parsed *(today: `AudioFormat`)*; does
   `mpv` get a `volume` offset from it, or only `native` applies it?
6. What is "quiet period" for the exit, and is it configurable?
