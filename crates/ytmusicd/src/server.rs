//! The daemon's loop: one task owns the [`Player`], every connection feeds it lines, and it
//! answers and broadcasts. Nothing wakes while idle: the only timer is the exit deadline, and
//! it exists only while nothing plays and nobody is connected.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::player::{Output, OutputEvent, Player};
use crate::protocol::{Command, ErrorCode, Event, EventKind, PROTOCOL, ProtocolError, Reply, Request, Status, SubscribeArgs};

pub struct ServerConfig {
    pub daemon_version: &'static str,
    /// The output in use and every output this build has, as `hello` reports them.
    pub output: String,
    pub outputs: Vec<String>,
    /// Exit after this long with nothing playing and nobody connected. `None` never exits.
    pub idle_timeout: Option<Duration>,
    /// Where a stopped player writes its queue and position, so a daemon that exits and a new
    /// one that starts both resume the same playback.
    pub state: Option<PathBuf>,
}

type ConnId = u64;

enum Msg {
    Open { id: ConnId, tx: UnboundedSender<String>, writer: JoinHandle<()> },
    Line { id: ConnId, text: String },
    Closed { id: ConnId },
}

struct Conn {
    tx: UnboundedSender<String>,
    subs: HashSet<EventKind>,
    writer: JoinHandle<()>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Serves `listener` until `shutdown`, or until the idle timeout runs out.
pub async fn serve<O: Output>(
    listener: UnixListener,
    mut player: Player<O>,
    mut output_events: UnboundedReceiver<OutputEvent>,
    config: ServerConfig,
) -> io::Result<()> {
    let (msg_tx, mut msg_rx) = unbounded_channel::<Msg>();
    let mut conns: HashMap<ConnId, Conn> = HashMap::new();
    let mut next_conn: ConnId = 1;
    let mut idle_since: Option<Instant> = None;
    let mut previous = player.state().status;

    loop {
        // Judged before waiting, so a daemon that starts idle starts its clock too.
        let idle = player.state().status == Status::Stopped && conns.is_empty();
        idle_since = if idle { idle_since.or_else(|| Some(Instant::now())) } else { None };
        let deadline = idle_since.zip(config.idle_timeout).map(|(since, timeout)| since + timeout);

        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let id = next_conn;
                next_conn += 1;
                tokio::spawn(connection(id, stream, msg_tx.clone()));
            }
            Some(msg) = msg_rx.recv() => match msg {
                Msg::Open { id, tx, writer } => {
                    conns.insert(id, Conn { tx, subs: HashSet::new(), writer });
                }
                Msg::Closed { id } => {
                    if let Some(conn) = conns.remove(&id) {
                        drop(conn.tx);
                        conn.writer.abort();
                    }
                }
                Msg::Line { id, text } => {
                    if handle_line(id, &text, &mut conns, &mut player, &config) {
                        flush_and_close(conns).await;
                        return Ok(());
                    }
                }
            },
            Some(event) = output_events.recv() => {
                let events = player.on_output(event, now_ms());
                broadcast(&conns, &events);
            }
            () = async {
                match deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                flush_and_close(conns).await;
                return Ok(());
            }
        }

        // Playback stopped: the queue and the position go to disk before the daemon can exit.
        let status = player.state().status;
        if previous != Status::Stopped && status == Status::Stopped {
            save_state(config.state.as_deref(), &player);
        }
        previous = status;
    }
}

fn save_state<O: Output>(path: Option<&Path>, player: &Player<O>) {
    let Some(path) = path else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(&player.saved()) {
        let _ = std::fs::write(path, bytes);
    }
}

/// Lets every connection's pending lines (a `shutdown` reply, say) reach its socket.
async fn flush_and_close(conns: HashMap<ConnId, Conn>) {
    for (_, conn) in conns {
        drop(conn.tx);
        let _ = conn.writer.await;
    }
}

/// Reads lines off one client and hands them to the loop. The loop answers through the channel
/// this sends it in `Open`; a task of its own writes that channel to the socket.
async fn connection(id: ConnId, stream: UnixStream, msg_tx: UnboundedSender<Msg>) {
    let (read, write) = stream.into_split();
    let (tx, rx) = unbounded_channel::<String>();
    let writer = tokio::spawn(socket_writer(write, rx));
    let _ = msg_tx.send(Msg::Open { id, tx, writer });

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(text)) = lines.next_line().await {
        if msg_tx.send(Msg::Line { id, text }).is_err() {
            return;
        }
    }
    let _ = msg_tx.send(Msg::Closed { id });
}

async fn socket_writer(mut write: tokio::net::unix::OwnedWriteHalf, mut rx: UnboundedReceiver<String>) {
    while let Some(line) = rx.recv().await {
        if write.write_all(line.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn send(conns: &HashMap<ConnId, Conn>, id: ConnId, value: &impl serde::Serialize) {
    if let (Some(conn), Ok(mut line)) = (conns.get(&id), serde_json::to_string(value)) {
        line.push('\n');
        let _ = conn.tx.send(line);
    }
}

fn broadcast(conns: &HashMap<ConnId, Conn>, events: &[Event]) {
    for event in events {
        let Ok(mut line) = serde_json::to_string(event) else {
            continue;
        };
        line.push('\n');
        for conn in conns.values().filter(|conn| conn.subs.contains(&event.kind())) {
            let _ = conn.tx.send(line.clone());
        }
    }
}

/// Answers one line. Returns true when the daemon must shut down.
fn handle_line<O: Output>(
    id: ConnId,
    text: &str,
    conns: &mut HashMap<ConnId, Conn>,
    player: &mut Player<O>,
    config: &ServerConfig,
) -> bool {
    let request = match serde_json::from_str::<Request>(text) {
        Ok(request) => request,
        Err(error) => {
            // Echo the id when there is one to echo; without it the reply has nowhere to go.
            let seen = serde_json::from_str::<Value>(text).ok().and_then(|v| v.get("id")?.as_u64());
            let error = ProtocolError::bad_request(format!("not a command: {error}"));
            match seen {
                Some(seen) => send(conns, id, &Reply::err(seen, error)),
                None => send(conns, id, &json!({ "ok": false, "error": error })),
            }
            return false;
        }
    };

    let reply = match request.command {
        Command::Hello(args) if args.protocol != PROTOCOL => Reply::err(
            request.id,
            ProtocolError::new(ErrorCode::Unsupported, format!("this daemon speaks protocol {PROTOCOL}, not {}", args.protocol)),
        ),
        Command::Hello(_) => Reply::ok(
            request.id,
            json!({
                "protocol": PROTOCOL,
                "daemon": config.daemon_version,
                "outputs": config.outputs,
                "output": config.output,
                "caps": { "audio": true, "video": false },
            }),
        ),
        Command::Subscribe(SubscribeArgs { events }) => {
            let kinds = if events.is_empty() { EventKind::ALL.to_vec() } else { events };
            if let Some(conn) = conns.get_mut(&id) {
                conn.subs.extend(kinds);
            }
            Reply::ok(request.id, Value::Null)
        }
        Command::Unsubscribe(SubscribeArgs { events }) => {
            let kinds = if events.is_empty() { EventKind::ALL.to_vec() } else { events };
            if let Some(conn) = conns.get_mut(&id) {
                for kind in kinds {
                    conn.subs.remove(&kind);
                }
            }
            Reply::ok(request.id, Value::Null)
        }
        Command::Shutdown => {
            send(conns, id, &Reply::ok(request.id, Value::Null));
            return true;
        }
        command => match player.handle(command, now_ms()) {
            Ok(outcome) => {
                // The reply first, so a caller reads its answer before the events it caused.
                send(conns, id, &Reply::ok(request.id, outcome.data));
                broadcast(conns, &outcome.events);
                return false;
            }
            Err(error) => Reply::err(request.id, error),
        },
    };

    send(conns, id, &reply);
    false
}
