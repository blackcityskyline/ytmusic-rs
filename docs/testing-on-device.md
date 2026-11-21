# Testing `ytmusicd` on a real machine

For an agent (or a person) with a shell on the machine that has `mpv`, `yt-dlp`, a sound
server and the network. The automated tests in this repository run without any of those.
This document covers what they cannot: a real `mpv`, real audio, real YouTube, real timing
and real power cost.

Rules that apply here as everywhere in this repo:

- Never report a check that was not run. "It compiled" is not "it played".
- Report what you measured, with the number and the command that produced it. If you
  could not measure something, say so and why.
- Secrets (cookies) stay in the environment. Never put them in an argument, a log, a file
  or your report.
- Do not change the code to make a scenario pass. A failing scenario is a finding: write
  down the command, the output and what you expected.

## 0. What is already covered, so you do not repeat it

`cargo test -p ytmusicd` runs about seventy tests with no network, audio or `mpv`:

- `tests/protocol.rs` the JSON shapes and the socket path.
- `tests/queue.rs` queue rules: order, shuffle, repeat, revisions, removal.
- `tests/player.rs` the state machine against a fake output: clock, pause, seek, skipping
  dead entries, announcements, preload hints.
- `tests/server.rs` a real socket and a fake output: handshake, subscriptions, ordering of
  reply and events, bad lines, shutdown, the idle exit.
- `tests/mpv.rs` the `mpv` output against a stand-in for `mpv`'s IPC socket. **The stand-in
  speaks `mpv`'s IPC as documented, not as measured.** Scenario 3 is where that is checked.

Run them first. If any fails on your machine, stop and report that before anything else.

## 1. Setup

```bash
cd ytmusic-rs
git checkout ytmusicd-core            # or the branch you were given
cargo build -p ytmusicd               # target/debug/ytmusicd and target/debug/ytmusicctl
cargo test -p ytmusicd
mpv --version; yt-dlp --version       # record both
```

Use a private socket so nothing touches the user's real session:

```bash
export S=$(mktemp -d)/d.sock
alias ctl="target/debug/ytmusicctl --socket $S"
```

For a real track you need a track object with a `videoId`. Take one from the output of the
existing CLI (see the README for `search`); `play` accepts it exactly as the CLI prints it.
Save three tracks as `t1.json`, `t2.json`, `t3.json`, then:

```bash
play3() { ctl play "{\"tracks\":[$(cat t1.json),$(cat t2.json),$(cat t3.json)]}"; }
```

Authentication, if the tracks need it, is whatever the CLI already uses (environment
variables). Do not print them.

## 2. Scenarios

Each has steps, the expected result, and what to record. Number them in your report.

### 2.1 Lifecycle

| Step | Expected |
| --- | --- |
| `ctl state` with no daemon running | prints a `stopped` state; a `ytmusicd` process now exists |
| `ls -ld $(dirname $S); ls -l $S` | directory `drwx------`, socket `srw-------` |
| `target/debug/ytmusicd --socket $S` while one runs | prints "already running", exits 0, the first daemon is unaffected |
| `pkill -9 ytmusicd; ctl state` | a stale socket file is replaced and a new daemon answers |
| `ctl shutdown` | `{"id":2,"ok":true}`, process gone, socket file gone |
| start with `--idle-timeout 10`, connect nothing | the process exits 10 s (±2) after start |
| same, but `ctl watch` open in another terminal | does not exit while watch is connected; exits ~10 s after it is closed |
| `play3`, close every client | keeps running while the track plays |
| `pgrep -a mpv` after `ctl shutdown`, and after an idle exit | the daemon's `mpv` (it has `ytmusicd-mpv.sock` in its arguments) is gone too |
| `pkill -9 ytmusicd` while playing, then `pgrep -a mpv`, then `ctl state` and `play3` | the `mpv` survives (nothing could kill it); the next daemon connects to it and plays. Report whether that works and whether any sound was left playing meanwhile |

Record: any row that differs.

### 2.2 First playback and the real `mpv` IPC

```bash
ctl watch > events.log &        # events with timestamps are in at_ms
date +%s%3N; play3
sleep 20; kill %1; cat events.log
```

Expected, in order: `queue`, `state` with `status: loading`, then `state` with
`status: playing` and `clock.rate: 1.0`. Audible sound on the default output.

This is the first time the output meets a real `mpv`. Check each of these and report
precisely what differs:

1. `pgrep -a mpv` shows `--idle=yes --no-video --force-window=no --no-terminal
   --prefetch-playlist=yes --input-ipc-server=…/ytmusicd-mpv.sock`.
2. The track plays. If `mpv` prints nothing (it runs with `--no-terminal`), run the same
   arguments by hand with `--msg-level=all=v` and `loadfile` the URL to see why.
3. A `playing` state arrives. If the track is audible but the state stays `loading`, the
   `file-loaded` event is not reaching the output: capture the raw IPC with
   `socat - UNIX-CONNECT:…/ytmusicd-mpv.sock` and report the actual event names.
4. `playlist-clear` and `loadfile … append` are accepted (`mpv` answers
   `"error":"success"`). The version you recorded matters: report it.

### 2.3 Latency (the reason this exists)

Measure with the timestamps in the events, not by ear. `play` sent at `T0` (milliseconds,
`date +%s%3N`), the `playing` state's `clock.at_ms` is `T1`. Latency is `T1 − T0`.

| Case | How | Record |
| --- | --- | --- |
| cold first track | stop the daemon, fresh start, `play` one track | `T1 − T0`, 5 runs, median and worst |
| warm, new track | daemon and `mpv` running, `play` another track | same |
| next track, gapless | `play3`, let track 1 reach its end (`ctl seek '{"to":N}'` to ~5 s before the end), watch the `Ended` → next `playing` | gap between the end and the next `playing` event, and whether there is an audible gap |
| manual next | `ctl next` | `T1 − T0` |

Also run for comparison, with nothing else changed:
`time yt-dlp -f bestaudio -g "https://music.youtube.com/watch?v=<id>"` three times. If
that alone is most of `T1 − T0`, the cost is the resolve and not the player; that is the
finding that decides the next step (a resolver in the daemon).

### 2.4 Controls

For each, run the command, then `ctl state`, and listen:

`pause`, `resume`, `toggle`, `seek {"to":60000}`, `seek {"by":-10000}`, `volume {"set":30}`,
`volume {"by":10}`, `mute`, `shuffle {"on":true}` then `next` several times,
`repeat {"mode":"one"}` then let a track end, `repeat {"mode":"all"}` then let the last
track end, `prev` after more than 3 s (restarts) and within 3 s (goes back), `stop`, then
`resume` (restarts the same entry).

Expected: the state matches what you hear. After `pause`, `position_ms + (now − at_ms) × rate`
stays constant. Report any command whose effect you could not hear or whose state is wrong.

### 2.5 Failure

| Step | Expected |
| --- | --- |
| `play` a track with `"video_id":"does-not-exist"` | a `notice` event with `mpv`'s reason, then the player skips or stops; it does not hang in `loading` |
| `play3`, then `pkill -f ytmusicd-mpv` | `notice` "mpv exited" and a `stopped` state; the next `play` starts a new `mpv` and plays |
| disconnect the network, `play` | a `notice` and no hang |
| a queue of three bad ids with `repeat all` | stops after trying each once, does not loop |

### 2.6 Several clients

Two `ctl watch` plus commands from a third. Both watchers get the same events in the same
order. `ctl queue.add '{"tracks":[…],"if_rev":0}'` after the queue changed answers
`rev_conflict`. Close one watcher: the other keeps receiving.

### 2.7 Cost

This is the priority of the project, so measure it and keep the numbers.

```bash
pid=$(pgrep -x ytmusicd); mpvpid=$(pgrep -f ytmusicd-mpv)
pidstat -w -p $pid,$mpvpid 1 60      # context switches per second
ps -o rss,vsz,cmd -p $pid,$mpvpid
```

Run it in each state and record the median per second:

| State | `ytmusicd` | `mpv` |
| --- | --- | --- |
| started, nothing connected | (it exits after the idle timeout; measure inside it) | not running |
| one `watch` connected, stopped | expected: close to 0 voluntary switches per second | not running or idle |
| playing, one `watch` connected | | |
| playing, nobody connected | | |
| paused | | |

Also count the lines `ctl watch` prints during 60 s of ordinary playback. Expected: only
the events of your own commands and track changes, none on an interval. Any periodic line
is a finding.

If `powertop` or `perf stat -p` is available, add wakeups per second for the playing case.
Do not invent a threshold: report the numbers, the next step is to set one from them.

### 2.8 Robustness

- Send a 20 MB line without a newline to the socket, and one with a newline. Known gap:
  the server has no line-length limit, so expect memory to grow. Record the daemon's RSS
  before and after; this is a finding for the author, not a failure of your run.
- `kill -STOP` the daemon for 5 s while a client sends commands, then `kill -CONT`.
- 50 connections opened and closed in a loop; RSS before and after.

## 3. Writing automated tests for what you find

Put them in `crates/ytmusicd/tests/`, next to the existing ones, with these rules:

- A test must fail on the broken code. Write it, break the code on purpose, watch it fail,
  put the code back. If you cannot make it fail, delete it.
- Assert against an external authority where there is one (what `mpv` actually sent,
  captured in 2.2) rather than against the code under test. If 2.2 shows `mpv` names an
  event differently from `tests/mpv.rs`, fix the stand-in to match the real `mpv` and add the
  captured line to the test as the evidence.
- No sleeping for effect. Wait on an event with a timeout (see `Client::recv`).
- Tests that need the real `mpv` or the network are `#[ignore]` and say so in the name or
  the comment; run them with `cargo test -p ytmusicd -- --ignored`. They are never part of
  the default run.
- `tests/common/mod.rs` has a `FakeOutput` and track builders; use them.

## 4. What to send back

One message with: the versions (`mpv`, `yt-dlp`, kernel, sound server), the result of
`cargo test -p ytmusicd`, then for each scenario above: pass, fail or not run, with the
command, the output that matters and the numbers from 2.3 and 2.7. Lead with what failed.
Do not summarise what passed beyond a line.

## 5. Known gaps (do not report these as new)

- No resolver yet: `mpv` is handed the YouTube page URL and runs `yt-dlp` itself, so
  the first-track latency still includes it.
- No `Discontinuity` from buffering stalls: the clock keeps running while `mpv` waits for
  data.
- No MPRIS, no native output, no browse through the daemon.
- No limit on the length of a line a client sends.
- `play` takes track objects only, with no `videoId` shorthand.
