//! The daemon: `ytmusicd [--socket PATH] [--idle-timeout SECS] [--mpv PROGRAM] [--output mpv|native] [--window] [--no-resolver] [-- MPV_ARGS…]`.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use ytmusicd::data::Resolver;
use ytmusicd::mpv::{MpvConfig, MpvOut};
use ytmusicd::player::{Output, OutputEvent, Player, Saved};
use ytmusicd::server::{ServerConfig, serve};
use ytmusicd::socket_path;
use ytmusicd::youtube::{YtSource, cache_dir, session_from_env};

const USAGE: &str = "usage: ytmusicd [--socket PATH] [--idle-timeout SECS] [--mpv PROGRAM] [--output mpv|native] [--window] [--no-resolver] [-- MPV_ARGS...]
  --no-resolver   hand mpv the YouTube page (it runs yt-dlp) instead of the daemon's local stream
  --output        mpv (default) or native (decodes in the daemon; needs a build with --features native)
  --idle-timeout  exit after this many seconds with nothing playing and nobody connected (default 600, 0 = never)";

struct Options {
    socket: PathBuf,
    idle_timeout: Option<Duration>,
    mpv: String,
    window: bool,
    native: bool,
    resolver: bool,
    mpv_args: Vec<String>,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        socket: socket_path(std::env::var_os("XDG_RUNTIME_DIR"), std::env::var_os("HOME")),
        idle_timeout: Some(Duration::from_secs(600)),
        mpv: "mpv".to_string(),
        window: false,
        native: false,
        resolver: true,
        mpv_args: Vec::new(),
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" => options.socket = args.next().ok_or("--socket needs a path")?.into(),
            "--idle-timeout" => {
                let secs: u64 = args.next().ok_or("--idle-timeout needs seconds")?.parse().map_err(|_| "--idle-timeout is not a number")?;
                options.idle_timeout = (secs > 0).then(|| Duration::from_secs(secs));
            }
            "--mpv" => options.mpv = args.next().ok_or("--mpv needs a program")?,
            "--output" => {
                options.native = match args.next().ok_or("--output needs mpv or native")?.as_str() {
                    "mpv" => false,
                    "native" => true,
                    other => return Err(format!("unknown output {other}")),
                }
            }
            "--window" => options.window = true,
            "--no-resolver" => options.resolver = false,
            "--" => {
                options.mpv_args = args.by_ref().collect();
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let options = match parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("ytmusicd: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ytmusicd: {error}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ytmusicd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(options: Options) -> std::io::Result<()> {
    if let Some(dir) = options.socket.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }

    // A socket that answers is a daemon already running: a client that raced another to start
    // one gets the same result either way.
    if std::os::unix::net::UnixStream::connect(&options.socket).is_ok() {
        eprintln!("ytmusicd: already running on {}", options.socket.display());
        return Ok(());
    }
    let _ = std::fs::remove_file(&options.socket);

    let listener = UnixListener::bind(&options.socket)?;
    std::fs::set_permissions(&options.socket, std::fs::Permissions::from_mode(0o600))?;

    let (events_tx, events_rx) = unbounded_channel();
    let resolver = if options.resolver || options.native {
        let source = Arc::new(YtSource::new(Arc::new(session_from_env())));
        Some(Resolver::start(source, cache_dir().join("spool")).await?)
    } else {
        None
    };
    let idle_timeout = options.idle_timeout;

    let result = if options.native {
        #[cfg(feature = "native")]
        {
            let out = ytmusicd::native::NativeOut::spawn(
                ytmusicd::cpal_device::CpalDevice::default,
                resolver.expect("native output always has a resolver"),
                events_tx,
            );
            finish(listener, out, events_rx, "native", idle_timeout).await
        }
        #[cfg(not(feature = "native"))]
        {
            let _ = (resolver, events_tx, events_rx);
            Err(std::io::Error::other("this build has no native output; rebuild with --features native"))
        }
    } else {
        let out = MpvOut::spawn(
            MpvConfig {
                program: options.mpv,
                socket: options.socket.with_file_name("ytmusicd-mpv.sock"),
                window: options.window,
                extra_args: options.mpv_args,
                resolver,
            },
            events_tx,
        );
        finish(listener, out, events_rx, "mpv", idle_timeout).await
    };

    let _ = std::fs::remove_file(&options.socket);
    result
}

async fn finish<O: Output>(
    listener: UnixListener,
    out: O,
    events: UnboundedReceiver<OutputEvent>,
    name: &str,
    idle_timeout: Option<Duration>,
) -> std::io::Result<()> {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let state = cache_dir().join("daemon-state.json");
    let mut player = Player::new(out, seed);
    if let Ok(bytes) = std::fs::read(&state) {
        if let Ok(saved) = serde_json::from_slice::<Saved>(&bytes) {
            player.restore(saved);
        }
    }
    serve(listener, player, events, ServerConfig { daemon_version: env!("CARGO_PKG_VERSION"), output: name.to_string(), outputs: vec![name.to_string()], idle_timeout, state: Some(state) }).await
}
