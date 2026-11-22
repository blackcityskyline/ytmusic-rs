//! The real [`Source`]: a track's audio straight from YouTube through the `ytmusic` crate,
//! which already reads in bounded ranges, retries a stalled one and renews a refused URL.

use std::sync::Arc;

use tokio::sync::mpsc;
use ytmusic::YtMusic;

use crate::spool::{BoxFuture, Opened, Source};

pub struct YtSource {
    client: Arc<YtMusic>,
}

impl YtSource {
    pub fn new(client: Arc<YtMusic>) -> Self {
        Self { client }
    }
}

impl Source for YtSource {
    fn open(&self, video_id: &str) -> BoxFuture<'_, anyhow::Result<Opened>> {
        let client = self.client.clone();
        let video_id = video_id.to_string();
        Box::pin(async move {
            let (format, mut stream) = client.open_audio(&video_id).await?;
            // Two chunks of slack: the pump runs ahead of the spool writer by a second or so
            // at most, never by a whole track.
            let (tx, rx) = mpsc::channel(2);
            tokio::spawn(async move {
                loop {
                    let item = match stream.chunk().await {
                        Ok(Some(bytes)) => Ok(bytes),
                        Ok(None) => break,
                        Err(error) => Err(std::io::Error::other(format!("{error:#}"))),
                    };
                    let failed = item.is_err();
                    if tx.send(item).await.is_err() || failed {
                        break; // the spool was dropped, or the stream is over
                    }
                }
            });
            Ok(Opened { mime: format.mime, total: format.content_length, chunks: rx })
        })
    }
}

/// The session the CLI builds: cookies and account from the environment (never argv), the
/// signature-player cache in the cache directory.
pub fn session_from_env() -> YtMusic {
    let api = match std::env::var("YTMUSIC_COOKIES") {
        Ok(cookies) if !cookies.trim().is_empty() => {
            let authuser = std::env::var("YTMUSIC_AUTHUSER").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            YtMusic::with_cookies(cookies.trim()).as_user(authuser)
        }
        _ => YtMusic::anonymous(),
    };
    api.cache_player(cache_dir().join("player.json"))
}

/// `YTMUSIC_CACHE`, else `$XDG_CACHE_HOME/ytmusic`, else `~/.cache/ytmusic`: the CLI's choice.
pub fn cache_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Some(path) = std::env::var_os("YTMUSIC_CACHE") {
        return PathBuf::from(path);
    }
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("ytmusic");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/ytmusic")
}
