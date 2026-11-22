//! The data plane against a scripted source and a real loopback TCP client.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use ytmusicd::data::{KEEP, Resolver};
use ytmusicd::spool::{BoxFuture, Opened, Source};

type Chunk = std::io::Result<Bytes>;

/// Opens succeed with a channel the test feeds by hand, so progress is under its control.
#[derive(Default)]
struct Script {
    opens: AtomicUsize,
    feeds: Mutex<HashMap<String, mpsc::Sender<Chunk>>>,
    total: Mutex<HashMap<String, Option<u64>>>,
    refuse: Mutex<Vec<String>>,
}

impl Script {
    fn feed(&self, id: &str) -> mpsc::Sender<Chunk> {
        self.feeds.lock().unwrap()[id].clone()
    }
    fn announce(&self, id: &str, total: Option<u64>) {
        self.total.lock().unwrap().insert(id.into(), total);
    }
}

impl Source for Script {
    fn open(&self, id: &str) -> BoxFuture<'_, anyhow::Result<Opened>> {
        let id = id.to_string();
        Box::pin(async move {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.refuse.lock().unwrap().contains(&id) {
                anyhow::bail!("video unavailable");
            }
            let (tx, rx) = mpsc::channel(16);
            self.feeds.lock().unwrap().insert(id.clone(), tx);
            let total = self.total.lock().unwrap().get(&id).copied().flatten();
            Ok(Opened { mime: "audio/webm".into(), total, chunks: rx })
        })
    }
}

async fn setup() -> (Arc<Script>, Resolver, tempfile::TempDir) {
    let script = Arc::new(Script::default());
    let dir = tempfile::tempdir().unwrap();
    let resolver = Resolver::start(script.clone(), dir.path().to_path_buf()).await.unwrap();
    (script, resolver, dir)
}

/// Sends one request, returns (status line, headers lowercased, body) once the server closes.
async fn fetch(url: &str, method: &str, range: Option<&str>) -> (String, String, Vec<u8>) {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut s = TcpStream::connect(host).await.unwrap();
    let mut req = format!("{method} /{path} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(r) = range {
        req += &format!("Range: {r}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut raw)).await.expect("server closed").unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let (status, headers) = head.split_once("\r\n").unwrap_or((&head, ""));
    (status.to_string(), headers.to_lowercase(), raw[split + 4..].to_vec())
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

async fn fill(script: &Script, id: &str, data: &[u8]) {
    script.feed(id).send(Ok(Bytes::copy_from_slice(data))).await.unwrap();
}

#[tokio::test]
async fn serves_the_whole_track_with_its_type_and_length() {
    let (script, resolver, _d) = setup().await;
    let data = body(1000);
    script.announce("a", Some(1000));
    let url = resolver.url("a");
    let client = tokio::spawn(async move { fetch(&url, "GET", None).await });
    while script.opens.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    fill(&script, "a", &data[..400]).await;
    fill(&script, "a", &data[400..]).await;
    drop(script.feed("a"));
    let (status, headers, got) = client.await.unwrap();
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains("content-type: audio/webm"));
    assert!(headers.contains("content-length: 1000"));
    assert!(headers.contains("accept-ranges: bytes"));
    assert_eq!(got, data);
}

#[tokio::test]
async fn nothing_is_opened_until_something_reads() {
    let (script, resolver, _d) = setup().await;
    let _ = resolver.url("a");
    let _ = resolver.url("b");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(script.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ranges_answer_206_with_the_slice_and_content_range() {
    let (script, resolver, _d) = setup().await;
    let data = body(1000);
    script.announce("a", Some(1000));
    let url = resolver.url("a");
    let feeder = script.clone();
    let d2 = data.clone();
    tokio::spawn(async move {
        while feeder.opens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        fill(&feeder, "a", &d2).await;
    });

    let (status, headers, got) = fetch(&url, "GET", Some("bytes=100-199")).await;
    assert_eq!(status, "HTTP/1.1 206 Partial Content");
    assert!(headers.contains("content-range: bytes 100-199/1000"));
    assert_eq!(got, &data[100..200]);

    let (_, headers, got) = fetch(&url, "GET", Some("bytes=900-")).await;
    assert!(headers.contains("content-range: bytes 900-999/1000"));
    assert_eq!(got, &data[900..]);

    let (_, _, got) = fetch(&url, "GET", Some("bytes=-10")).await;
    assert_eq!(got, &data[990..]);

    let (status, headers, _) = fetch(&url, "GET", Some("bytes=5000-")).await;
    assert_eq!(status, "HTTP/1.1 416 Range Not Satisfiable");
    assert!(headers.contains("content-range: bytes */1000"));
}

#[tokio::test]
async fn head_has_the_headers_and_no_body() {
    let (script, resolver, _d) = setup().await;
    script.announce("a", Some(10));
    let url = resolver.url("a");
    let s = script.clone();
    tokio::spawn(async move {
        while s.opens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        fill(&s, "a", &[7u8; 10]).await;
    });
    let (status, headers, got) = fetch(&url, "HEAD", None).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert!(headers.contains("content-length: 10"));
    assert!(got.is_empty());
}

#[tokio::test]
async fn unknown_token_and_wrong_path_are_404_and_open_nothing() {
    let (script, resolver, _d) = setup().await;
    let url = resolver.url("a");
    let base = url.rsplit_once('/').unwrap().0;
    let (status, _, _) = fetch(&format!("{base}/{}", "0".repeat(32)), "GET", None).await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    let (status, _, _) = fetch(&url.replace("/s/", "/x/"), "GET", None).await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    let (status, _, _) = fetch(&url, "POST", None).await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    assert_eq!(script.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_refused_open_is_a_502_not_a_hang() {
    let (script, resolver, _d) = setup().await;
    script.refuse.lock().unwrap().push("a".into());
    let url = resolver.url("a");
    let (status, _, _) = fetch(&url, "GET", None).await;
    assert_eq!(status, "HTTP/1.1 502 Bad Gateway");
}

#[tokio::test]
async fn a_failure_midway_keeps_what_arrived_and_cuts_the_rest() {
    let (script, resolver, _d) = setup().await;
    script.announce("a", Some(1000));
    let url = resolver.url("a");
    let s = script.clone();
    tokio::spawn(async move {
        while s.opens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let tx = s.feed("a");
        tx.send(Ok(Bytes::from(vec![1u8; 300]))).await.unwrap();
        tx.send(Err(std::io::Error::other("stall"))).await.unwrap();
    });
    let (status, _, got) = fetch(&url, "GET", None).await;
    assert_eq!(status, "HTTP/1.1 200 OK");
    assert_eq!(got.len(), 300, "the bytes before the failure are served, then the body ends short");
}

#[tokio::test]
async fn an_unknown_length_is_learned_when_the_download_finishes() {
    let (script, resolver, _d) = setup().await;
    let url = resolver.url("a");
    let s = script.clone();
    tokio::spawn(async move {
        while s.opens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        fill(&s, "a", &[9u8; 123]).await;
        drop(s.feed("a")); // the script map still holds a sender
        s.feeds.lock().unwrap().remove("a");
    });
    let (_, headers, got) = fetch(&url, "GET", None).await;
    assert!(headers.contains("content-length: 123"));
    assert_eq!(got.len(), 123);
}

#[tokio::test]
async fn asking_again_reuses_the_track_and_does_not_reopen_it() {
    let (script, resolver, _d) = setup().await;
    script.announce("a", Some(4));
    let u1 = resolver.url("a");
    let u2 = resolver.url("a");
    assert_eq!(u1, u2);
    let s = script.clone();
    tokio::spawn(async move {
        while s.opens.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        fill(&s, "a", &[1, 2, 3, 4]).await;
    });
    let (_, _, a) = fetch(&u1, "GET", None).await;
    let (_, _, b) = fetch(&u2, "GET", None).await;
    assert_eq!((a, b), (vec![1, 2, 3, 4], vec![1, 2, 3, 4]));
    assert_eq!(script.opens.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_oldest_track_is_evicted_beyond_the_keep_limit() {
    let (_script, resolver, _d) = setup().await;
    let first = resolver.url("v0");
    for i in 1..=KEEP {
        resolver.url(&format!("v{i}"));
    }
    let (status, _, _) = fetch(&first, "GET", None).await;
    assert_eq!(status, "HTTP/1.1 404 Not Found");
    // Asking for it again gives a fresh token, not the dead one.
    assert_ne!(resolver.url("v0"), first);
}

#[tokio::test]
async fn tokens_are_128_bit_and_differ_per_track() {
    let (_s, resolver, _d) = setup().await;
    let a = resolver.url("a");
    let b = resolver.url("b");
    let token = |u: &str| u.rsplit('/').next().unwrap().to_string();
    assert_eq!(token(&a).len(), 32);
    assert!(token(&a).bytes().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(token(&a), token(&b));
    assert!(a.starts_with("http://127.0.0.1:"));
}

#[tokio::test]
async fn an_oversized_header_block_is_refused() {
    let (_s, resolver, _d) = setup().await;
    let url = resolver.url("a");
    let host = url.strip_prefix("http://").unwrap().split_once('/').unwrap().0.to_string();
    let mut s = TcpStream::connect(host).await.unwrap();
    let junk = "X-Junk: ".to_string() + &"a".repeat(1000) + "\r\n";
    let mut req = String::from("GET /s/x HTTP/1.1\r\n");
    for _ in 0..20 {
        req += &junk;
    }
    let _ = s.write_all(req.as_bytes()).await;
    let mut raw = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut raw)).await.expect("closed");
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 400"));
}
