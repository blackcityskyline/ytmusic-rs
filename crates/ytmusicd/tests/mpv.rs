mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::entry;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::timeout;
use ytmusicd::mpv::{Cmd, Incoming, Link, MpvConfig, MpvOut, Wire, args, classify, encode, stream_url};
use ytmusicd::player::{Output, OutputEvent};

fn config(window: bool) -> MpvConfig {
    MpvConfig {
        program: "mpv".into(),
        socket: PathBuf::from("/run/x/m.sock"),
        window,
        extra_args: vec!["--volume=50".into()],
        resolver: None,
    }
}

fn load(id: &str) -> Cmd {
    Cmd::Load { video_id: id.into(), start_ms: 0, paused: false }
}

fn names(wires: &[Wire]) -> Vec<String> {
    wires
        .iter()
        .map(|wire| match wire {
            Wire::Command(c) | Wire::Request(c) => {
                c.iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect::<Vec<_>>().join(" ")
            }
        })
        .collect()
}

#[test]
fn mpv_runs_idle_without_a_window_unless_video_is_asked_for() {
    let audio = args(&config(false));
    assert!(audio.contains(&"--idle=yes".to_string()));
    assert!(audio.contains(&"--no-video".to_string()));
    assert!(audio.contains(&"--force-window=no".to_string()));
    assert!(audio.contains(&"--prefetch-playlist=yes".to_string()), "the appended entry is resolved ahead of time");
    assert!(audio.contains(&"--input-ipc-server=/run/x/m.sock".to_string()));
    assert_eq!(audio.last().map(String::as_str), Some("--volume=50"), "the user's own arguments come last and win");

    let video = args(&config(true));
    assert!(!video.contains(&"--no-video".to_string()));
    assert!(video.contains(&"--force-window=yes".to_string()));
}

#[test]
fn a_command_is_one_json_line_with_an_optional_request_id() {
    let plain = encode(&[json!("stop")], None);
    assert_eq!(plain, "{\"command\":[\"stop\"]}\n");
    let request: Value = serde_json::from_str(&encode(&[json!("loadfile"), json!("u")], Some(7))).unwrap();
    assert_eq!(request, json!({ "command": ["loadfile", "u"], "request_id": 7 }));
}

#[test]
fn lines_from_mpv_are_reduced_to_what_matters() {
    assert_eq!(classify(r#"{"event":"file-loaded"}"#), Incoming::FileLoaded);
    assert_eq!(classify(r#"{"event":"end-file","reason":"eof"}"#), Incoming::EndFile { reason: "eof".into(), error: None });
    assert_eq!(
        classify(r#"{"event":"end-file","reason":"error","file_error":"loading failed"}"#),
        Incoming::EndFile { reason: "error".into(), error: Some("loading failed".into()) }
    );
    assert_eq!(
        classify(r#"{"request_id":4,"error":"invalid parameter"}"#),
        Incoming::Reply { request_id: 4, error: "invalid parameter".into() }
    );
    for ignored in [r#"{"request_id":4,"error":"success"}"#, r#"{"event":"property-change","name":"pause"}"#, "garbage", ""] {
        assert_eq!(classify(ignored), Incoming::Other, "{ignored:?}");
    }
}

#[test]
fn load_sets_the_pause_state_then_replaces_the_file() {
    let mut link = Link::default();
    let wires = link.command(Cmd::Load { video_id: "abc".into(), start_ms: 0, paused: true });
    assert_eq!(names(&wires), ["set_property pause true".to_string(), format!("loadfile {} replace", stream_url("abc"))]);
    assert!(matches!(wires[1], Wire::Request(_)), "a failed loadfile must reach the core");
}

#[test]
fn a_start_offset_is_a_seek_once_the_file_is_loaded() {
    let mut link = Link::default();
    link.command(Cmd::Load { video_id: "a".into(), start_ms: 12_500, paused: false });
    let (wires, events) = link.incoming(Incoming::FileLoaded);
    assert_eq!(names(&wires), ["seek 12.5 absolute"]);
    assert_eq!(events, [OutputEvent::Started { position_ms: 12_500 }]);

    let (wires, events) = link.incoming(Incoming::FileLoaded);
    assert!(wires.is_empty());
    assert_eq!(events, [OutputEvent::Started { position_ms: 0 }], "the offset applies once");
}

#[test]
fn the_entry_to_prepare_is_appended_and_replaced_only_when_it_changes() {
    let mut link = Link::default();
    assert!(link.command(Cmd::Preload(Some("n".into()))).is_empty(), "nothing is playing yet, nothing to prepare behind");

    link.command(load("a"));
    assert_eq!(names(&link.command(Cmd::Preload(Some("b".into())))), [format!("loadfile {} append", stream_url("b"))]);
    assert!(link.command(Cmd::Preload(Some("b".into()))).is_empty(), "the same entry again is no change");

    let swapped = link.command(Cmd::Preload(Some("c".into())));
    assert_eq!(names(&swapped), ["playlist-clear".to_string(), format!("loadfile {} append", stream_url("c"))]);

    assert_eq!(names(&link.command(Cmd::Preload(None))), ["playlist-clear"]);
    assert!(link.command(Cmd::Preload(None)).is_empty());
}

#[test]
fn a_gapless_join_is_reported_as_an_end_and_the_following_load_sends_nothing() {
    let mut link = Link::default();
    link.command(load("a"));
    link.command(Cmd::Preload(Some("b".into())));

    let (_, events) = link.incoming(Incoming::EndFile { reason: "eof".into(), error: None });
    assert_eq!(events, [OutputEvent::Ended]);

    assert!(link.command(load("b")).is_empty(), "mpv is already on `b`: reloading it would be the gap");
    assert!(!link.command(load("b")).is_empty(), "the shortcut is for one join, not for every later load of `b`");
}

#[test]
fn an_end_with_nothing_appended_is_a_plain_end_and_repeat_reloads() {
    let mut link = Link::default();
    link.command(load("a"));
    let (_, events) = link.incoming(Incoming::EndFile { reason: "eof".into(), error: None });
    assert_eq!(events, [OutputEvent::Ended]);
    assert!(!link.command(load("a")).is_empty(), "repeat-one loads the same entry again");
}

#[test]
fn a_file_that_fails_to_load_reports_the_reason_mpv_gave() {
    let mut link = Link::default();
    link.command(load("a"));
    link.command(Cmd::Preload(Some("b".into())));
    link.incoming(Incoming::EndFile { reason: "eof".into(), error: None });
    link.command(load("b"));

    let (_, events) = link.incoming(Incoming::EndFile { reason: "error".into(), error: Some("403".into()) });
    assert_eq!(events, [OutputEvent::Failed { reason: "403".into() }]);
}

#[test]
fn the_end_of_a_file_we_replaced_ourselves_says_nothing() {
    let mut link = Link::default();
    link.command(load("a"));
    for reason in ["stop", "quit", "redirect"] {
        let (wires, events) = link.incoming(Incoming::EndFile { reason: reason.into(), error: None });
        assert!(wires.is_empty() && events.is_empty(), "{reason}");
    }
}

#[test]
fn a_command_mpv_refuses_becomes_a_failure() {
    let mut link = Link::default();
    let (_, events) = link.incoming(Incoming::Reply { request_id: 3, error: "invalid parameter".into() });
    assert_eq!(events, [OutputEvent::Failed { reason: "invalid parameter".into() }]);
}

#[test]
fn losing_mpv_fails_only_what_was_playing() {
    let mut link = Link::default();
    assert!(link.lost().is_empty(), "mpv dying while idle is not news");
    link.command(load("a"));
    assert_eq!(link.lost(), [OutputEvent::Failed { reason: "mpv exited".into() }]);
    assert!(link.lost().is_empty());
}

#[test]
fn seek_volume_pause_and_stop_map_to_one_command_each() {
    let mut link = Link::default();
    assert_eq!(names(&link.command(Cmd::Seek(93_250))), ["seek 93.25 absolute"]);
    assert_eq!(names(&link.command(Cmd::Pause(true))), ["set_property pause true"]);
    assert_eq!(names(&link.command(Cmd::Volume { percent: 40, muted: true })), ["set_property volume 40", "set_property mute true"]);
    assert_eq!(names(&link.command(Cmd::Stop)), ["stop"]);
}

/// A stand-in for `mpv`: listens on the socket and lets a test read the lines sent to it and
/// write lines back.
struct FakeMpv {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl FakeMpv {
    async fn accept(listener: &UnixListener) -> Self {
        let (stream, _) = timeout(Duration::from_secs(2), listener.accept()).await.expect("nothing connected").unwrap();
        let (read, writer) = stream.into_split();
        Self { reader: BufReader::new(read), writer }
    }

    async fn line(&mut self) -> Value {
        let mut line = String::new();
        timeout(Duration::from_secs(2), self.reader.read_line(&mut line)).await.expect("no line from the output").unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn say(&mut self, value: Value) {
        self.writer.write_all(format!("{value}\n").as_bytes()).await.unwrap();
    }
}

async fn event(rx: &mut UnboundedReceiver<OutputEvent>) -> OutputEvent {
    timeout(Duration::from_secs(2), rx.recv()).await.expect("no event from the output").unwrap()
}

fn fake_setup() -> (tempfile::TempDir, UnixListener, MpvOut, UnboundedReceiver<OutputEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("mpv.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = unbounded_channel();
    // The socket answers, so the output connects to it and never starts a process.
    let out = MpvOut::spawn(MpvConfig { program: "no-such-mpv".into(), socket, window: false, extra_args: vec![], resolver: None }, tx);
    (dir, listener, out, rx)
}

#[tokio::test]
async fn the_output_talks_to_mpv_over_the_socket_and_reports_back() {
    let (_dir, listener, mut out, mut events) = fake_setup();

    out.load(&entry("abc"), 0, false);
    let mut mpv = FakeMpv::accept(&listener).await;
    assert_eq!(mpv.line().await["command"], json!(["set_property", "pause", false]));
    let loadfile = mpv.line().await;
    assert_eq!(loadfile["command"], json!(["loadfile", stream_url("abc"), "replace"]));
    let request_id = loadfile["request_id"].as_u64().expect("loadfile asks for an answer");

    mpv.say(json!({ "event": "start-file" })).await;
    mpv.say(json!({ "event": "file-loaded" })).await;
    assert_eq!(event(&mut events).await, OutputEvent::Started { position_ms: 0 });

    mpv.say(json!({ "event": "end-file", "reason": "eof" })).await;
    assert_eq!(event(&mut events).await, OutputEvent::Ended);

    out.load(&entry("def"), 0, false);
    mpv.line().await;
    let second = mpv.line().await;
    mpv.say(json!({ "request_id": second["request_id"], "error": "loading failed" })).await;
    assert_eq!(event(&mut events).await, OutputEvent::Failed { reason: "loading failed".into() });
    assert_ne!(second["request_id"].as_u64(), Some(request_id), "each request has its own id");
}

#[tokio::test]
async fn a_gapless_join_through_the_socket_sends_no_second_loadfile() {
    let (_dir, listener, mut out, mut events) = fake_setup();
    out.load(&entry("a"), 0, false);
    let mut mpv = FakeMpv::accept(&listener).await;
    mpv.line().await;
    mpv.line().await;

    out.preload(Some(&entry("b")));
    assert_eq!(mpv.line().await["command"], json!(["loadfile", stream_url("b"), "append"]));

    mpv.say(json!({ "event": "end-file", "reason": "eof" })).await;
    assert_eq!(event(&mut events).await, OutputEvent::Ended);

    // What the core does on `Ended`: load the entry that follows.
    out.load(&entry("b"), 0, false);
    out.set_volume(30, false);
    assert_eq!(mpv.line().await["command"], json!(["set_property", "volume", 30]), "the next line is the volume, not a loadfile");
}

#[tokio::test]
async fn mpv_going_away_is_reported_once_and_the_output_connects_again() {
    let (_dir, listener, mut out, mut events) = fake_setup();
    out.load(&entry("a"), 0, false);
    let mut mpv = FakeMpv::accept(&listener).await;
    mpv.line().await;
    mpv.line().await;

    drop(mpv);
    assert_eq!(event(&mut events).await, OutputEvent::Failed { reason: "mpv exited".into() });

    out.load(&entry("b"), 0, false);
    let mut again = FakeMpv::accept(&listener).await;
    assert_eq!(again.line().await["command"], json!(["set_property", "pause", false]));
}

#[tokio::test]
async fn an_mpv_that_cannot_be_started_is_a_failure_not_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut events) = unbounded_channel();
    let mut out = MpvOut::spawn(
        MpvConfig {
            program: "/no/such/mpv".into(),
            socket: dir.path().join("none.sock"),
            window: false,
            extra_args: vec![],
            resolver: None,
        },
        tx,
    );
    out.load(&entry("a"), 0, false);
    match event(&mut events).await {
        OutputEvent::Failed { reason } => assert!(reason.starts_with("mpv:"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
}

struct Nothing;
impl ytmusicd::spool::Source for Nothing {
    fn open(&self, _: &str) -> ytmusicd::spool::BoxFuture<'_, anyhow::Result<ytmusicd::spool::Opened>> {
        Box::pin(async { anyhow::bail!("not used") })
    }
}

#[tokio::test]
async fn with_a_resolver_mpv_is_given_the_local_stream_for_loads_and_preloads_even_after_a_stop() {
    let dir = tempfile::tempdir().unwrap();
    let resolver = ytmusicd::data::Resolver::start(std::sync::Arc::new(Nothing), dir.path().into()).await.unwrap();
    let mut link = Link::with_resolver(Some(resolver.clone()));
    let local = |id: &str| format!("loadfile {} ", resolver.url(id));

    let wires = link.command(Cmd::Load { video_id: "a".into(), start_ms: 0, paused: false });
    assert!(names(&wires)[1].starts_with(&local("a")), "{:?}", names(&wires));
    let wires = link.command(Cmd::Preload(Some("b".into())));
    assert!(names(&wires)[0].starts_with(&local("b")), "{:?}", names(&wires));

    link.command(Cmd::Stop);
    let wires = link.command(Cmd::Load { video_id: "c".into(), start_ms: 0, paused: false });
    assert!(names(&wires)[1].starts_with("loadfile http://127.0.0.1:"), "a stop must not forget the resolver: {:?}", names(&wires));
}
