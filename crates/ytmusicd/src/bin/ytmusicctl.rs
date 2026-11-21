//! A scripting client for the daemon: not a UI, a way to drive and watch the protocol.
//!
//! `ytmusicctl [--socket PATH] [--no-spawn] <cmd> [JSON-ARGS]` sends one command and prints the
//! reply line; `ytmusicctl watch` subscribes to every event and prints them until killed.
//! A daemon that is not running is started first, unless `--no-spawn`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use ytmusicd::socket_path;

const USAGE: &str = "usage: ytmusicctl [--socket PATH] [--no-spawn] <cmd> [JSON-ARGS]
       ytmusicctl [--socket PATH] [--no-spawn] watch";

fn connect(socket: &PathBuf, spawn: bool) -> std::io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    if !spawn {
        return UnixStream::connect(socket);
    }

    // The daemon next to this binary, else the one on PATH. Its own session, so it outlives us.
    let sibling = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("ytmusicd")));
    let program = sibling.filter(|path| path.exists()).unwrap_or_else(|| PathBuf::from("ytmusicd"));
    Command::new(program)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;

    let mut delay = Duration::from_millis(10);
    for _ in 0..30 {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(150));
    }
    UnixStream::connect(socket)
}

fn run() -> Result<(), String> {
    let mut socket = socket_path(std::env::var_os("XDG_RUNTIME_DIR"), std::env::var_os("HOME"));
    let mut spawn = true;
    let mut rest = Vec::new();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => socket = args.next().ok_or("--socket needs a path")?.into(),
            "--no-spawn" => spawn = false,
            _ => rest.push(arg),
        }
    }
    let Some(cmd) = rest.first().cloned() else {
        return Err(USAGE.to_string());
    };

    let stream = connect(&socket, spawn).map_err(|error| format!("cannot reach {}: {error}", socket.display()))?;
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);

    let call = |writer: &mut UnixStream, reader: &mut BufReader<UnixStream>, id: u64, cmd: &str, args: Value| {
        let mut request = json!({ "id": id, "cmd": cmd });
        if !args.is_null() {
            request["args"] = args;
        }
        writeln!(writer, "{request}").map_err(|e| e.to_string())?;
        // Events can arrive between the request and its reply once subscribed.
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Err("the daemon closed the connection".to_string());
            }
            let value: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(line);
            }
        }
    };

    // A refused `hello` or `subscribe` ends the run: carrying on would watch nothing, quietly.
    let accepted = |line: String| -> Result<(), String> {
        match serde_json::from_str::<Value>(&line) {
            Ok(value) if value.get("ok") == Some(&Value::Bool(true)) => Ok(()),
            _ => Err(format!("the daemon refused: {}", line.trim())),
        }
    };
    accepted(call(&mut writer, &mut reader, 1, "hello", json!({ "protocol": 1, "client": "ytmusicctl" }))?)?;

    if cmd == "watch" {
        accepted(call(&mut writer, &mut reader, 2, "subscribe", json!({}))?)?;
        let mut line = String::new();
        while reader.read_line(&mut line).map_err(|e| e.to_string())? > 0 {
            print!("{line}");
            std::io::stdout().flush().map_err(|e| e.to_string())?;
            line.clear();
        }
        return Ok(());
    }

    let args = match rest.get(1) {
        Some(text) => serde_json::from_str(text).map_err(|e| format!("the arguments are not JSON: {e}"))?,
        None => Value::Null,
    };
    let reply = call(&mut writer, &mut reader, 2, &cmd, args)?;
    print!("{reply}");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ytmusicctl: {message}");
            ExitCode::FAILURE
        }
    }
}
