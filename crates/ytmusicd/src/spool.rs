//! One track's audio, downloaded once into an unlinked file and read from there at any offset.
//!
//! The file is the kernel's to hold in its page cache and to drop under pressure, not the
//! heap's. Nothing is opened or downloaded until a reader asks: a track that is resolved and
//! never played costs nothing. A reader asking for a byte not yet downloaded waits for it,
//! woken by the download, never by a timer.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tokio::task::AbortHandle;

pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A stream that has been opened: what it is, how long, and its body in order.
pub struct Opened {
    pub mime: String,
    pub total: Option<u64>,
    pub chunks: mpsc::Receiver<std::io::Result<Bytes>>,
}

/// Where a track's bytes come from: YouTube in the daemon, a script in a test.
pub trait Source: Send + Sync + 'static {
    fn open(&self, video_id: &str) -> BoxFuture<'_, anyhow::Result<Opened>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub mime: String,
    pub total: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Opening,
    Streaming,
    Done,
    Failed(String),
}

#[derive(Clone, Debug)]
struct Progress {
    phase: Phase,
    head: Option<Head>,
    written: u64,
}

struct Inner {
    source: Arc<dyn Source>,
    video_id: String,
    dir: PathBuf,
    file: OnceLock<File>,
    tx: watch::Sender<Progress>,
}

pub struct Spool {
    inner: Arc<Inner>,
    abort: Mutex<Option<AbortHandle>>,
}

impl Spool {
    pub fn new(source: Arc<dyn Source>, video_id: &str, dir: PathBuf) -> Self {
        let (tx, _) = watch::channel(Progress { phase: Phase::Idle, head: None, written: 0 });
        Self {
            inner: Arc::new(Inner { source, video_id: video_id.to_string(), dir, file: OnceLock::new(), tx }),
            abort: Mutex::new(None),
        }
    }

    pub fn video_id(&self) -> &str {
        &self.inner.video_id
    }

    /// Why the download failed, if it did.
    pub fn failure(&self) -> Option<String> {
        match &self.inner.tx.borrow().phase {
            Phase::Failed(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    /// Starts the download if nobody has. Idempotent.
    fn start(&self) {
        let first = self.inner.tx.send_if_modified(|progress| {
            let idle = progress.phase == Phase::Idle;
            if idle {
                progress.phase = Phase::Opening;
            }
            idle
        });
        if first {
            let handle = tokio::spawn(download(self.inner.clone()));
            *self.abort.lock().unwrap() = Some(handle.abort_handle());
        }
    }

    /// The stream's type and length, once it is open. The length is absent while a source that
    /// did not know it is still downloading; [`total`](Self::total) waits for it.
    pub async fn head(&self) -> Result<Head, String> {
        self.start();
        let mut rx = self.inner.tx.subscribe();
        let progress = rx
            .wait_for(|p| p.head.is_some() || matches!(p.phase, Phase::Failed(_)))
            .await
            .map_err(|_| "the spool was dropped".to_string())?
            .clone();
        match (progress.phase, progress.head) {
            (Phase::Failed(reason), _) => Err(reason),
            (_, Some(head)) => Ok(head),
            _ => Err("no stream".to_string()),
        }
    }

    /// The length in bytes, waiting for the whole download when the source did not say.
    pub async fn total(&self) -> Result<u64, String> {
        self.start();
        let mut rx = self.inner.tx.subscribe();
        let progress = rx
            .wait_for(|p| p.head.as_ref().is_some_and(|h| h.total.is_some()) || matches!(p.phase, Phase::Failed(_)))
            .await
            .map_err(|_| "the spool was dropped".to_string())?
            .clone();
        match progress.phase {
            Phase::Failed(reason) => Err(reason),
            _ => progress.head.and_then(|h| h.total).ok_or_else(|| "no length".to_string()),
        }
    }

    /// Reads from `offset` into `buf`, waiting until at least one byte there has been
    /// downloaded. `Ok(0)` is the end of the file. Bytes that arrived before a failure are
    /// still served; the failure is the answer only for bytes that never will.
    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, String> {
        self.start();
        let mut rx = self.inner.tx.subscribe();
        let progress = rx
            .wait_for(|p| p.written > offset || matches!(p.phase, Phase::Done | Phase::Failed(_)))
            .await
            .map_err(|_| "the spool was dropped".to_string())?
            .clone();

        if progress.written <= offset {
            return match progress.phase {
                Phase::Failed(reason) => Err(reason),
                _ => Ok(0),
            };
        }
        let want = buf.len().min((progress.written - offset) as usize);
        let file = self.inner.file.get().ok_or_else(|| "no spool file".to_string())?;
        file.read_at(&mut buf[..want], offset).map_err(|error| error.to_string())
    }
}

impl Drop for Spool {
    /// An evicted track stops downloading.
    fn drop(&mut self) {
        if let Some(handle) = self.abort.lock().unwrap().take() {
            handle.abort();
        }
    }
}

fn fail(inner: &Inner, reason: String) {
    inner.tx.send_modify(|progress| progress.phase = Phase::Failed(reason));
}

async fn download(inner: Arc<Inner>) {
    let mut opened = match inner.source.open(&inner.video_id).await {
        Ok(opened) => opened,
        Err(error) => return fail(&inner, format!("{error:#}")),
    };

    let file = match std::fs::create_dir_all(&inner.dir).and_then(|()| tempfile::tempfile_in(&inner.dir)) {
        Ok(file) => file,
        Err(error) => return fail(&inner, format!("no room to spool the track: {error}")),
    };
    let _ = inner.file.set(file);

    inner.tx.send_modify(|progress| {
        progress.head = Some(Head { mime: opened.mime.clone(), total: opened.total });
        progress.phase = Phase::Streaming;
    });

    let mut written = 0u64;
    while let Some(chunk) = opened.chunks.recv().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return fail(&inner, error.to_string()),
        };
        let Some(file) = inner.file.get() else { return };
        // A write to the page cache: short, and the only thing this task does between awaits.
        if let Err(error) = file.write_all_at(&chunk, written) {
            return fail(&inner, format!("writing the spool failed: {error}"));
        }
        written += chunk.len() as u64;
        inner.tx.send_modify(|progress| progress.written = written);
    }

    match opened.total {
        Some(total) if total != written => fail(&inner, format!("the stream ended after {written} of {total} bytes")),
        _ => inner.tx.send_modify(|progress| {
            if let Some(head) = progress.head.as_mut() {
                head.total = Some(written);
            }
            progress.phase = Phase::Done;
        }),
    }
}
