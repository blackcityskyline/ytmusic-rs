//! The wire types of `docs/protocol.md`: what a client sends and what the daemon answers.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ytmusic::models::Track;

pub const PROTOCOL: u32 = 1;

/// Stable for the life of a queue item; indexes shift under concurrent clients, ids do not.
pub type EntryId = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub entry: EntryId,
    pub track: Track,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// Where playback was at `at_ms` (Unix milliseconds), and how fast it moves. A client shows
/// `position_ms + (now - at_ms) × rate` and draws it itself: the daemon never ticks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clock {
    pub position_ms: u64,
    pub at_ms: u64,
    pub rate: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub rev: u64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    pub clock: Clock,
    pub volume: u8,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub queue_rev: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub queue_rev: u64,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    State(State),
    Queue(QueueSnapshot),
    Notice { message: String },
}

/// The kinds of event a client can subscribe to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    State,
    Queue,
    Notice,
}

impl EventKind {
    pub const ALL: [EventKind; 3] = [EventKind::State, EventKind::Queue, EventKind::Notice];
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self {
            Event::State(_) => EventKind::State,
            Event::Queue(_) => EventKind::Queue,
            Event::Notice { .. } => EventKind::Notice,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unsupported,
    NotFound,
    RevConflict,
    Unavailable,
    SignInRequired,
    Network,
    OutputFailed,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }
}

/// What the daemon writes back for a request: the same `id`, then `data` or `error`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

impl Reply {
    pub fn ok(id: u64, data: Value) -> Self {
        Self { id, ok: true, data: (!data.is_null()).then_some(data), error: None }
    }

    pub fn err(id: u64, error: ProtocolError) -> Self {
        Self { id, ok: false, data: None, error: Some(error) }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct PlayArgs {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct QueueAddArgs {
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub next: bool,
    #[serde(default)]
    pub if_rev: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct QueueRemoveArgs {
    pub entry: EntryId,
    #[serde(default)]
    pub if_rev: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct QueueMoveArgs {
    pub entry: EntryId,
    /// The entry to land in front of; the end of the queue when absent.
    #[serde(default)]
    pub before: Option<EntryId>,
    #[serde(default)]
    pub if_rev: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SeekArgs {
    #[serde(default)]
    pub to: Option<u64>,
    #[serde(default)]
    pub by: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct VolumeArgs {
    #[serde(default)]
    pub set: Option<u8>,
    #[serde(default)]
    pub by: Option<i16>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct MuteArgs {
    #[serde(default)]
    pub on: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ShuffleArgs {
    pub on: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RepeatArgs {
    pub mode: RepeatMode,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct HelloArgs {
    pub protocol: u32,
    #[serde(default)]
    pub client: String,
}

/// An empty or absent list means every kind.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct SubscribeArgs {
    #[serde(default)]
    pub events: Vec<EventKind>,
}

/// Every command the daemon understands. The first group is the connection's own and is
/// answered by the server; the player core refuses it. Browse and `resolve` are not here yet.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "cmd", content = "args", rename_all = "lowercase")]
pub enum Command {
    Hello(HelloArgs),
    Subscribe(SubscribeArgs),
    Unsubscribe(SubscribeArgs),
    Shutdown,
    Play(PlayArgs),
    #[serde(rename = "queue.add")]
    QueueAdd(QueueAddArgs),
    #[serde(rename = "queue.remove")]
    QueueRemove(QueueRemoveArgs),
    #[serde(rename = "queue.move")]
    QueueMove(QueueMoveArgs),
    #[serde(rename = "queue.clear")]
    QueueClear,
    #[serde(rename = "queue.get")]
    QueueGet,
    Next,
    Prev,
    Pause,
    Resume,
    Toggle,
    Stop,
    Seek(SeekArgs),
    Volume(VolumeArgs),
    Mute(MuteArgs),
    Shuffle(ShuffleArgs),
    Repeat(RepeatArgs),
    State,
}
