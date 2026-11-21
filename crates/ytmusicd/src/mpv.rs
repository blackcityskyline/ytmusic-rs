//! The `mpv` output: one long-lived `mpv` driven over its JSON IPC socket.
//!
//! [`Link`] is the whole policy and is pure: it turns the core's calls into `mpv` commands and
//! `mpv`'s lines into [`OutputEvent`]s. A task owns the process and the socket and does nothing
//! but move lines between them and the `Link`.
//!
//! Gapless: the entry the core names in `preload` is appended to `mpv`'s playlist, which
//! prefetches it. When the current file ends, `mpv` goes on to the appended one by itself; the
//! `Link` reports `Ended`, the core then asks to `load` that same entry, and the `Link`
//! recognises it as already playing and sends nothing.
//!
//! Nothing here asks `mpv` for the position: the core keeps the clock, so there is no polling
//! and no property observer to wake the process.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::player::{Output, OutputEvent};
use crate::protocol::Entry;

#[derive(Clone, Debug)]
pub struct MpvConfig {
    pub program: String,
    pub socket: PathBuf,
    /// Show `mpv`'s own window (for video); audio only otherwise.
    pub window: bool,
    pub extra_args: Vec<String>,
}

/// The command line of the `mpv` the daemon owns. `--prefetch-playlist` makes `mpv` resolve
/// the appended entry ahead of time instead of when the current one ends.
pub fn args(config: &MpvConfig) -> Vec<String> {
    let mut args = vec![
        "--idle=yes".to_string(),
        if config.window { "--force-window=yes" } else { "--force-window=no" }.to_string(),
    ];
    if !config.window {
        args.push("--no-video".into());
    }
    args.extend([
        "--no-terminal".to_string(),
        "--prefetch-playlist=yes".to_string(),
        format!("--input-ipc-server={}", config.socket.display()),
    ]);
    args.extend(config.extra_args.iter().cloned());
    args
}

/// What `mpv` is given for an entry until the resolver hands the daemon a stream of its own.
pub fn stream_url(video_id: &str) -> String {
    format!("https://music.youtube.com/watch?v={video_id}")
}

/// One line of `mpv`'s IPC: `{"command": [...]}`, with a `request_id` when the answer matters.
pub fn encode(command: &[Value], request_id: Option<u64>) -> String {
    let mut line = json!({ "command": command });
    if let Some(id) = request_id {
        line["request_id"] = json!(id);
    }
    format!("{line}\n")
}

/// A line from `mpv`, reduced to what the output reacts to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    FileLoaded,
    EndFile { reason: String, error: Option<String> },
    /// The answer to a command that carried a `request_id`.
    Reply { request_id: u64, error: String },
    Other,
}

pub fn classify(line: &str) -> Incoming {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Incoming::Other;
    };

    match value.get("event").and_then(Value::as_str) {
        Some("file-loaded") => Incoming::FileLoaded,
        Some("end-file") => Incoming::EndFile {
            reason: value.get("reason").and_then(Value::as_str).unwrap_or_default().to_string(),
            error: value.get("file_error").and_then(Value::as_str).map(str::to_string),
        },
        Some(_) => Incoming::Other,
        None => match (value.get("request_id").and_then(Value::as_u64), value.get("error").and_then(Value::as_str)) {
            (Some(request_id), Some(error)) if error != "success" => {
                Incoming::Reply { request_id, error: error.to_string() }
            }
            _ => Incoming::Other,
        },
    }
}

/// What the core asked for, as the task receives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cmd {
    Load { video_id: String, start_ms: u64, paused: bool },
    Preload(Option<String>),
    Pause(bool),
    Seek(u64),
    Volume { percent: u8, muted: bool },
    Stop,
}

/// A line to send. A `Request` is one whose failure must reach the core.
#[derive(Clone, Debug, PartialEq)]
pub enum Wire {
    Command(Vec<Value>),
    Request(Vec<Value>),
}

#[derive(Debug, Default)]
pub struct Link {
    /// The video id `mpv` is playing, or about to play.
    playing: Option<String>,
    /// The video id appended behind it.
    appended: Option<String>,
    /// `mpv` moved on to the appended entry by itself; the core has not been told to load it.
    advanced: bool,
    pending_seek_ms: Option<u64>,
}

impl Link {
    pub fn command(&mut self, cmd: Cmd) -> Vec<Wire> {
        match cmd {
            Cmd::Load { video_id, start_ms, paused } => {
                if self.advanced && self.playing.as_deref() == Some(video_id.as_str()) {
                    self.advanced = false;
                    return vec![];
                }
                self.advanced = false;
                self.appended = None;
                self.pending_seek_ms = (start_ms > 0).then_some(start_ms);
                let url = stream_url(&video_id);
                self.playing = Some(video_id);
                vec![
                    Wire::Command(vec![json!("set_property"), json!("pause"), json!(paused)]),
                    Wire::Request(vec![json!("loadfile"), json!(url), json!("replace")]),
                ]
            }
            Cmd::Preload(next) => {
                if self.playing.is_none() || self.appended == next {
                    return vec![];
                }
                let mut wires = Vec::new();
                if self.appended.is_some() {
                    wires.push(Wire::Command(vec![json!("playlist-clear")]));
                }
                if let Some(next) = &next {
                    wires.push(Wire::Request(vec![json!("loadfile"), json!(stream_url(next)), json!("append")]));
                }
                self.appended = next;
                wires
            }
            Cmd::Pause(on) => vec![Wire::Command(vec![json!("set_property"), json!("pause"), json!(on)])],
            Cmd::Seek(to_ms) => vec![Wire::Command(vec![json!("seek"), json!(to_ms as f64 / 1000.0), json!("absolute")])],
            Cmd::Volume { percent, muted } => vec![
                Wire::Command(vec![json!("set_property"), json!("volume"), json!(percent)]),
                Wire::Command(vec![json!("set_property"), json!("mute"), json!(muted)]),
            ],
            Cmd::Stop => {
                *self = Link::default();
                vec![Wire::Command(vec![json!("stop")])]
            }
        }
    }

    pub fn incoming(&mut self, incoming: Incoming) -> (Vec<Wire>, Vec<OutputEvent>) {
        match incoming {
            Incoming::FileLoaded => {
                let position_ms = self.pending_seek_ms.take().unwrap_or(0);
                let wires = if position_ms > 0 { self.command(Cmd::Seek(position_ms)) } else { vec![] };
                (wires, vec![OutputEvent::Started { position_ms }])
            }
            Incoming::EndFile { reason, error } => match reason.as_str() {
                "eof" => {
                    self.move_on();
                    (vec![], vec![OutputEvent::Ended])
                }
                "error" => {
                    self.move_on();
                    let reason = error.unwrap_or_else(|| "mpv could not play the entry".to_string());
                    (vec![], vec![OutputEvent::Failed { reason }])
                }
                // Our own `loadfile replace` and `stop` end the old file this way.
                _ => (vec![], vec![]),
            },
            Incoming::Reply { error, .. } => (vec![], vec![OutputEvent::Failed { reason: error }]),
            Incoming::Other => (vec![], vec![]),
        }
    }

    /// A file ended: `mpv` goes on to the appended entry if there is one.
    fn move_on(&mut self) {
        match self.appended.take() {
            Some(next) => {
                self.playing = Some(next);
                self.advanced = true;
            }
            None => self.playing = None,
        }
    }

    /// The socket closed under us: `mpv` is gone and whatever it was playing with it.
    pub fn lost(&mut self) -> Vec<OutputEvent> {
        let was_playing = self.playing.is_some();
        *self = Link::default();
        if was_playing { vec![OutputEvent::Failed { reason: "mpv exited".to_string() }] } else { vec![] }
    }
}

/// The handle the player core holds. It sends and returns; the task does the waiting.
pub struct MpvOut {
    tx: UnboundedSender<Cmd>,
}

impl MpvOut {
    /// Must be called inside a Tokio runtime. `mpv` is started on the first command, not here.
    pub fn spawn(config: MpvConfig, events: UnboundedSender<OutputEvent>) -> Self {
        let (tx, rx) = unbounded_channel();
        tokio::spawn(io_task(config, rx, events));
        Self { tx }
    }

    fn send(&self, cmd: Cmd) {
        // The task ends only when this handle is dropped.
        let _ = self.tx.send(cmd);
    }
}

impl Output for MpvOut {
    fn load(&mut self, entry: &Entry, start_ms: u64, paused: bool) {
        if let Some(video_id) = &entry.track.video_id {
            self.send(Cmd::Load { video_id: video_id.clone(), start_ms, paused });
        }
    }

    fn preload(&mut self, next: Option<&Entry>) {
        self.send(Cmd::Preload(next.and_then(|entry| entry.track.video_id.clone())));
    }

    fn pause(&mut self, on: bool) {
        self.send(Cmd::Pause(on));
    }

    fn seek(&mut self, to_ms: u64) {
        self.send(Cmd::Seek(to_ms));
    }

    fn set_volume(&mut self, percent: u8, muted: bool) {
        self.send(Cmd::Volume { percent, muted });
    }

    fn stop(&mut self) {
        self.send(Cmd::Stop);
    }
}

struct Conn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    /// Kept so the process dies with the daemon (`kill_on_drop`).
    _child: Option<Child>,
}

impl Conn {
    fn new(stream: UnixStream, child: Option<Child>) -> Self {
        let (read, writer) = stream.into_split();
        Self { reader: BufReader::new(read), writer, _child: child }
    }
}

/// Connects to the daemon's `mpv`, starting it first when nobody answers on the socket.
async fn connect_or_spawn(config: &MpvConfig) -> std::io::Result<Conn> {
    if let Ok(stream) = UnixStream::connect(&config.socket).await {
        return Ok(Conn::new(stream, None));
    }

    // Nobody answered, so whatever file is there is a dead `mpv`'s.
    let _ = std::fs::remove_file(&config.socket);
    let child = Command::new(&config.program)
        .args(args(config))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    // Startup only: back off from 10 ms to 200 ms, give up after about six seconds.
    let mut delay = Duration::from_millis(10);
    for _ in 0..40 {
        if let Ok(stream) = UnixStream::connect(&config.socket).await {
            return Ok(Conn::new(stream, Some(child)));
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(200));
    }
    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "mpv did not open its socket"))
}

async fn next_line(conn: &mut Option<Conn>) -> std::io::Result<Option<String>> {
    let Some(conn) = conn else {
        return std::future::pending().await;
    };
    let mut line = String::new();
    match conn.reader.read_line(&mut line).await? {
        0 => Ok(None),
        _ => Ok(Some(line)),
    }
}

async fn io_task(config: MpvConfig, mut rx: UnboundedReceiver<Cmd>, events: UnboundedSender<OutputEvent>) {
    let mut link = Link::default();
    let mut conn: Option<Conn> = None;
    let mut next_request = 1u64;

    loop {
        let mut wires = Vec::new();
        let mut out_events = Vec::new();
        let mut lost = false;

        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                if conn.is_none() {
                    match connect_or_spawn(&config).await {
                        Ok(connected) => conn = Some(connected),
                        Err(error) => {
                            let _ = events.send(OutputEvent::Failed { reason: format!("mpv: {error}") });
                            continue;
                        }
                    }
                }
                wires = link.command(cmd);
            }
            line = next_line(&mut conn) => match line {
                Ok(Some(line)) => {
                    let (more_wires, more_events) = link.incoming(classify(&line));
                    wires = more_wires;
                    out_events = more_events;
                }
                Ok(None) | Err(_) => lost = true,
            },
        }

        if let Some(open) = conn.as_mut() {
            for wire in wires {
                let line = match wire {
                    Wire::Command(command) => encode(&command, None),
                    Wire::Request(command) => {
                        next_request += 1;
                        encode(&command, Some(next_request))
                    }
                };
                if open.writer.write_all(line.as_bytes()).await.is_err() {
                    lost = true;
                    break;
                }
            }
        }

        if lost {
            conn = None;
            out_events.extend(link.lost());
        }
        for event in out_events {
            let _ = events.send(event);
        }
    }
}
