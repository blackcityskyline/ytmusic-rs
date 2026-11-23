mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{FakeOutput, tracks};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use ytmusicd::player::{OutputEvent, Player};
use ytmusicd::server::{ServerConfig, serve};

struct Daemon {
    _dir: TempDir,
    socket: PathBuf,
    output: UnboundedSender<OutputEvent>,
    task: JoinHandle<std::io::Result<()>>,
}

fn start(idle_timeout: Option<Duration>) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (output, events) = unbounded_channel();
    let task = tokio::spawn(serve(
        listener,
        Player::new(FakeOutput::default(), 1),
        events,
        ServerConfig { daemon_version: "test", output: "fake".into(), outputs: vec!["fake".into()], idle_timeout, state: None },
    ));
    Daemon { _dir: dir, socket, output, task }
}

struct Client {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Client {
    async fn connect(daemon: &Daemon) -> Self {
        let (read, writer) = UnixStream::connect(&daemon.socket).await.unwrap().into_split();
        Self { reader: BufReader::new(read), writer }
    }

    async fn raw(&mut self, line: &str) {
        self.writer.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }

    async fn send(&mut self, id: u64, cmd: &str, args: Value) {
        let mut request = json!({ "id": id, "cmd": cmd });
        if !args.is_null() {
            request["args"] = args;
        }
        self.raw(&request.to_string()).await;
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let read = timeout(Duration::from_secs(2), self.reader.read_line(&mut line)).await.expect("no line in 2 s").unwrap();
        assert!(read > 0, "the daemon closed the connection");
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, id: u64, cmd: &str, args: Value) -> Value {
        self.send(id, cmd, args).await;
        let reply = self.recv().await;
        assert_eq!(reply["id"], id, "the reply carries the request's id");
        reply
    }

    async fn hello(&mut self) {
        assert_eq!(self.call(1, "hello", json!({ "protocol": 1 })).await["ok"], true);
    }

    /// True when nothing arrives for a short while.
    async fn silent(&mut self) -> bool {
        let mut line = String::new();
        timeout(Duration::from_millis(150), self.reader.read_line(&mut line)).await.is_err()
    }
}

fn play_args() -> Value {
    json!({ "tracks": tracks(2) })
}

#[tokio::test]
async fn hello_reports_the_daemon_and_refuses_another_protocol() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;

    let reply = client.call(1, "hello", json!({ "protocol": 1, "client": "test" })).await;
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["data"]["protocol"], 1);
    assert_eq!(reply["data"]["daemon"], "test");
    assert_eq!(reply["data"]["output"], "fake");
    assert_eq!(reply["data"]["caps"], json!({ "audio": true, "video": false }));

    let refused = client.call(2, "hello", json!({ "protocol": 2 })).await;
    assert_eq!(refused["ok"], false);
    assert_eq!(refused["error"]["code"], "unsupported");
}

#[tokio::test]
async fn a_subscriber_hears_what_another_client_does_and_a_stranger_does_not() {
    let daemon = start(None);
    let mut watcher = Client::connect(&daemon).await;
    let mut other = Client::connect(&daemon).await;
    let mut caller = Client::connect(&daemon).await;
    for client in [&mut watcher, &mut other, &mut caller] {
        client.hello().await;
    }
    assert_eq!(watcher.call(2, "subscribe", json!({})).await["ok"], true);

    assert_eq!(caller.call(2, "play", play_args()).await["ok"], true);

    assert_eq!(watcher.recv().await["event"], "queue");
    let state = watcher.recv().await;
    assert_eq!(state["event"], "state");
    assert_eq!(state["status"], "loading");
    assert!(other.silent().await, "a client that did not subscribe hears nothing");
    assert!(caller.silent().await, "neither does the one that asked, unless it subscribed");
}

#[tokio::test]
async fn a_caller_that_subscribed_reads_its_reply_before_the_events_it_caused() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client.call(2, "subscribe", json!({})).await;

    client.send(3, "play", play_args()).await;
    let first = client.recv().await;
    assert_eq!(first["id"], 3, "the answer comes first");
    assert_eq!(client.recv().await["event"], "queue");
    assert_eq!(client.recv().await["event"], "state");
}

#[tokio::test]
async fn a_subscription_can_name_the_kinds_it_wants_and_be_withdrawn() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;
    let mut caller = Client::connect(&daemon).await;
    client.hello().await;
    caller.hello().await;

    client.call(2, "subscribe", json!({ "events": ["notice"] })).await;
    caller.call(2, "play", play_args()).await;
    assert!(client.silent().await, "queue and state were not asked for");

    daemon.output.send(OutputEvent::Failed { reason: "gone".into() }).unwrap();
    let notice = client.recv().await;
    assert_eq!(notice["event"], "notice");
    assert_eq!(notice["message"], "gone");

    client.call(3, "unsubscribe", json!({})).await;
    daemon.output.send(OutputEvent::Failed { reason: "again".into() }).unwrap();
    assert!(client.silent().await, "after unsubscribing nothing arrives");
}

#[tokio::test]
async fn a_bad_line_is_answered_and_the_connection_stays_usable() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    client.raw("not json").await;
    let no_id = client.recv().await;
    assert_eq!(no_id["ok"], false);
    assert_eq!(no_id["error"]["code"], "bad_request");
    assert!(no_id.get("id").is_none(), "there is no id to echo");

    client.raw(r#"{"id":9,"cmd":"explode"}"#).await;
    let unknown = client.recv().await;
    assert_eq!(unknown["id"], 9);
    assert_eq!(unknown["error"]["code"], "bad_request");

    // `subscribe` takes an `args` object, even an empty one.
    client.raw(r#"{"id":10,"cmd":"subscribe"}"#).await;
    assert_eq!(client.recv().await["error"]["code"], "bad_request");

    assert_eq!(client.call(11, "state", Value::Null).await["data"]["status"], "stopped");
}

#[tokio::test]
async fn the_output_ending_a_track_reaches_subscribers_as_a_state_change() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client.call(2, "subscribe", json!({})).await;
    client.call(3, "play", play_args()).await;
    client.recv().await;
    client.recv().await;

    daemon.output.send(OutputEvent::Started { position_ms: 0 }).unwrap();
    let state = client.recv().await;
    assert_eq!((state["event"].as_str(), state["status"].as_str()), (Some("state"), Some("playing")));
    assert_eq!(state["clock"]["rate"], 1.0);
}

#[tokio::test]
async fn shutdown_answers_and_then_the_daemon_ends() {
    let daemon = start(None);
    let mut client = Client::connect(&daemon).await;
    client.hello().await;

    let reply = client.call(2, "shutdown", Value::Null).await;
    assert_eq!(reply["ok"], true, "the reply was flushed before the exit");
    assert!(timeout(Duration::from_secs(2), daemon.task).await.expect("the daemon did not end").unwrap().is_ok());
}

#[tokio::test]
async fn an_idle_daemon_with_nobody_connected_exits_by_itself() {
    let daemon = start(Some(Duration::from_millis(100)));
    let ended = timeout(Duration::from_secs(2), daemon.task).await.expect("an idle daemon must end");
    assert!(ended.unwrap().is_ok());
}

#[tokio::test]
async fn a_connected_client_or_a_playing_track_keeps_the_daemon_alive() {
    let daemon = start(Some(Duration::from_millis(100)));
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!daemon.task.is_finished(), "a connection is a reason to stay");

    client.call(2, "play", play_args()).await;
    drop(client);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!daemon.task.is_finished(), "playback is a reason to stay with nobody connected");

    // Stopping and leaving starts the clock.
    let mut client = Client::connect(&daemon).await;
    client.hello().await;
    client.call(3, "stop", Value::Null).await;
    drop(client);
    let ended = timeout(Duration::from_secs(2), daemon.task).await.expect("stopped and alone, it must end");
    assert!(ended.unwrap().is_ok());
}
