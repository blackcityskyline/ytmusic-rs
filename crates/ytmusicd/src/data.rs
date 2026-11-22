//! The data plane: a loopback HTTP endpoint that serves each track's spool with `Range`
//! support, behind an unguessable token. `mpv` reads from it today, a native output or a
//! GUI's decoder tomorrow. The reason it exists: the stream host serves a plain GET at a
//! trickle and drops long connections, so the daemon downloads in bounded ranges and a
//! renderer only ever sees a well-behaved local server.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rand::RngCore;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::spool::{Source, Spool};

/// Tracks kept (and so still downloadable) at once. Older ones are dropped, which also stops
/// their downloads; the files go with them.
pub const KEEP: usize = 4;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const CHUNK: usize = 64 * 1024;

struct Slot {
    token: String,
    spool: Arc<Spool>,
}

impl std::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Resolver(127.0.0.1:{})", self.port)
    }
}

/// Maps tokens to spools and hands out URLs. Cheap to clone.
#[derive(Clone)]
pub struct Resolver {
    source: Arc<dyn Source>,
    dir: PathBuf,
    port: u16,
    slots: Arc<Mutex<VecDeque<Slot>>>,
}

impl Resolver {
    /// Binds the loopback listener and serves it on the current runtime.
    pub async fn start(source: Arc<dyn Source>, dir: PathBuf) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let resolver = Self { source, dir, port, slots: Arc::default() };
        tokio::spawn(accept(listener, resolver.clone()));
        Ok(resolver)
    }

    /// The URL for a track. Nothing is downloaded until something reads it. Asking again
    /// for a track still held reuses its spool (and its already-downloaded bytes).
    pub fn url(&self, video_id: &str) -> String {
        let mut slots = self.slots.lock().unwrap();
        if let Some(at) = slots.iter().position(|s| s.spool.video_id() == video_id && s.spool.failure().is_none()) {
            let slot = slots.remove(at).unwrap();
            let url = self.format(&slot.token);
            slots.push_back(slot);
            return url;
        }
        let mut raw = [0u8; 16];
        rand::rng().fill_bytes(&mut raw);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let spool = Arc::new(Spool::new(self.source.clone(), video_id, self.dir.clone()));
        let url = self.format(&token);
        slots.push_back(Slot { token, spool });
        while slots.len() > KEEP {
            slots.pop_front();
        }
        url
    }

    fn format(&self, token: &str) -> String {
        format!("http://127.0.0.1:{}/s/{token}", self.port)
    }

    fn find(&self, token: &str) -> Option<Arc<Spool>> {
        self.slots.lock().unwrap().iter().find(|s| s.token == token).map(|s| s.spool.clone())
    }
}

async fn accept(listener: TcpListener, resolver: Resolver) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        tokio::spawn(serve(stream, resolver.clone()));
    }
}

struct Req {
    head: bool,
    token: Option<String>,
    range: Option<String>,
}

async fn read_request(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<Req> {
    let mut line = String::new();
    let mut seen = 0;
    if reader.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    seen += line.len();
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut range = None;
    loop {
        let mut header = String::new();
        let n = reader.read_line(&mut header).await.ok()?;
        seen += n;
        if n == 0 || seen > MAX_HEADER_BYTES {
            return None;
        }
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_string());
        }
    }
    let ok_method = method == "GET" || method == "HEAD";
    let token = path.strip_prefix("/s/").map(str::to_string).filter(|_| ok_method);
    Some(Req { head: method == "HEAD", token, range })
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n`; one range only. `None` = unsatisfiable or malformed.
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') || total == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (total.saturating_sub(n.min(total)), total - 1)
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() { total - 1 } else { b.parse::<u64>().ok()?.min(total - 1) };
        (start, end)
    };
    (start <= end && start < total).then_some((start, end))
}

async fn respond(stream: &mut tokio::net::tcp::OwnedWriteHalf, status: &str, extra: &str) -> std::io::Result<()> {
    let text = format!("HTTP/1.1 {status}\r\n{extra}Connection: close\r\n\r\n");
    stream.write_all(text.as_bytes()).await
}

async fn serve(stream: TcpStream, resolver: Resolver) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let _ = handle(&mut reader, &mut write, &resolver).await;
    let _ = write.shutdown().await;
}

async fn handle(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    resolver: &Resolver,
) -> std::io::Result<()> {
    let Some(req) = read_request(reader).await else {
        return respond(write, "400 Bad Request", "Content-Length: 0\r\n").await;
    };
    let Some(spool) = req.token.as_deref().and_then(|t| resolver.find(t)) else {
        return respond(write, "404 Not Found", "Content-Length: 0\r\n").await;
    };
    let head = match spool.head().await {
        Ok(head) => head,
        Err(_) => return respond(write, "502 Bad Gateway", "Content-Length: 0\r\n").await,
    };
    let total = match spool.total().await {
        Ok(total) => total,
        Err(_) => return respond(write, "502 Bad Gateway", "Content-Length: 0\r\n").await,
    };

    let (status, start, end, extra) = match req.range.as_deref() {
        None => ("200 OK", 0, total.saturating_sub(1), String::new()),
        Some(value) => match parse_range(value, total) {
            Some((a, b)) => ("206 Partial Content", a, b, format!("Content-Range: bytes {a}-{b}/{total}\r\n")),
            None => {
                return respond(
                    write,
                    "416 Range Not Satisfiable",
                    &format!("Content-Range: bytes */{total}\r\nContent-Length: 0\r\n"),
                )
                .await;
            }
        },
    };
    let len = if total == 0 { 0 } else { end - start + 1 };
    respond_head(write, status, &head.mime, len, &extra).await?;
    if req.head || len == 0 {
        return Ok(());
    }

    let mut buf = vec![0u8; CHUNK];
    let mut at = start;
    while at <= end {
        let want = buf.len().min((end - at + 1) as usize);
        match spool.read_at(at, &mut buf[..want]).await {
            Ok(0) | Err(_) => return Ok(()), // cut short: the client sees a short body
            Ok(n) => {
                write.write_all(&buf[..n]).await?;
                at += n as u64;
            }
        }
    }
    Ok(())
}

async fn respond_head(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    mime: &str,
    len: u64,
    extra: &str,
) -> std::io::Result<()> {
    let text = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\n{extra}Connection: close\r\n\r\n"
    );
    write.write_all(text.as_bytes()).await
}
