#![cfg(feature = "native")]
//! The decoder over a real spool, with a WAV the test writes itself. Opus and AAC need
//! encoders to fabricate, so they are checked on a real machine (docs/testing-on-device.md).

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use ytmusicd::decode::{Decoder, SpoolSource};
use ytmusicd::spool::{BoxFuture, Opened, Source, Spool};

const RATE: u32 = 8000;

/// One second of a ramp, mono 16-bit: sample `i` is `i` (so position is readable from value).
fn wav(frames: u32) -> Vec<u8> {
    let data_len = frames * 2;
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for i in 0..frames {
        out.extend((i as i16).to_le_bytes());
    }
    out
}

struct Fixed(Vec<u8>);
impl Source for Fixed {
    fn open(&self, _: &str) -> BoxFuture<'_, anyhow::Result<Opened>> {
        let data = self.0.clone();
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(4);
            let total = data.len() as u64;
            tokio::spawn(async move {
                for piece in data.chunks(1000) {
                    if tx.send(Ok(Bytes::copy_from_slice(piece))).await.is_err() {
                        return;
                    }
                }
            });
            Ok(Opened { mime: "audio/wav".into(), total: Some(total), chunks: rx })
        })
    }
}

fn spool(data: Vec<u8>, dir: &std::path::Path) -> Arc<Spool> {
    Arc::new(Spool::new(Arc::new(Fixed(data)), "t", dir.to_path_buf()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decodes_every_sample_of_a_spooled_file_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(wav(4000), dir.path());
    let handle = tokio::runtime::Handle::current();
    let (rate, channels, samples) = tokio::task::spawn_blocking(move || {
        let source = SpoolSource::open(spool, handle).unwrap();
        let mut decoder = Decoder::open(Box::new(source), Some("wav")).unwrap();
        let mut all = Vec::new();
        while let Some(block) = decoder.next_block().unwrap() {
            all.extend_from_slice(block);
        }
        (decoder.rate, decoder.channels, all)
    })
    .await
    .unwrap();

    assert_eq!((rate, channels), (RATE, 1));
    assert_eq!(samples.len(), 4000);
    // The ramp survived: strictly increasing and the right scale (i16 → f32 in -1..1).
    assert!(samples.windows(2).all(|w| w[1] > w[0]));
    assert!((samples[3999] - 3999.0 / 32768.0).abs() < 1e-6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeking_lands_near_the_requested_time_and_decoding_continues_from_there() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(wav(8000), dir.path());
    let handle = tokio::runtime::Handle::current();
    let first = tokio::task::spawn_blocking(move || {
        let source = SpoolSource::open(spool, handle).unwrap();
        let mut decoder = Decoder::open(Box::new(source), Some("wav")).unwrap();
        decoder.seek(500).unwrap(); // half a second = frame 4000
        decoder.next_block().unwrap().unwrap()[0]
    })
    .await
    .unwrap();
    let frame = (first * 32768.0).round() as i64;
    assert!((frame - 4000).abs() <= 1024, "landed at frame {frame}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn garbage_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let spool = spool(vec![0x42; 5000], dir.path());
    let handle = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        let source = SpoolSource::open(spool, handle).unwrap();
        Decoder::open(Box::new(source), None).map(|_| ())
    })
    .await
    .unwrap();
    assert!(result.is_err());
}
