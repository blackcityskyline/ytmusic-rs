#![cfg(feature = "native")]
//! The native output above the sound card: a fake device "plays" only when the test says so,
//! so gapless joins, seeks and boundaries are exercised without audio hardware.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::entry;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::time::timeout;
use ytmusicd::data::Resolver;
use ytmusicd::native::{Converter, Device, NativeOut, Pull};
use ytmusicd::player::{Output, OutputEvent};
use ytmusicd::spool::{BoxFuture, Opened, Source};

const RATE: u32 = 8000;

fn wav(frames: u32, offset: i16) -> Vec<u8> {
    let data_len = frames * 2;
    let mut out = Vec::new();
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(RATE.to_le_bytes());
    out.extend((RATE * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for i in 0..frames {
        out.extend((offset + i as i16).to_le_bytes());
    }
    out
}

/// `a` and `b` are 4000-frame ramps starting at 1000 and 20000; anything else is garbage.
struct Library;
impl Source for Library {
    fn open(&self, id: &str) -> BoxFuture<'_, anyhow::Result<Opened>> {
        let data = match id {
            "a" => wav(4000, 1000),
            "b" => wav(4000, 20000),
            _ => vec![0x42; 3000],
        };
        Box::pin(async move {
            let (tx, rx) = tokio::sync::mpsc::channel(4);
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

#[derive(Clone, Default)]
struct Fake {
    pull: Arc<Mutex<Option<Pull>>>,
    log: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    /// One callback's worth: returns the samples that came from the ring.
    fn play(&self, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; frames];
        let mut guard = self.pull.lock().unwrap();
        let Some(pull) = guard.as_mut() else {
            return vec![];
        };
        let n = pull.fill(&mut out);
        out.truncate(n);
        out
    }
}

impl Device for Fake {
    fn format(&mut self) -> anyhow::Result<(u32, usize)> {
        Ok((RATE, 1))
    }
    fn start(&mut self, pull: Pull, playing: bool) -> anyhow::Result<()> {
        self.log.lock().unwrap().push(format!("start playing={playing}"));
        *self.pull.lock().unwrap() = Some(pull);
        Ok(())
    }
    fn pause(&mut self) {
        self.log.lock().unwrap().push("pause".into());
    }
    fn resume(&mut self) {
        self.log.lock().unwrap().push("resume".into());
    }
    fn stop(&mut self) {
        self.log.lock().unwrap().push("stop".into());
        *self.pull.lock().unwrap() = None;
    }
}

struct Rig {
    out: NativeOut,
    dev: Fake,
    events: UnboundedReceiver<OutputEvent>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let resolver = Resolver::start(Arc::new(Library), dir.path().into()).await.unwrap();
    let (tx, events) = unbounded_channel();
    let dev = Fake::default();
    Rig {
        out: NativeOut::spawn(
            {
                let d = dev.clone();
                move || d
            },
            resolver,
            tx,
        ),
        dev,
        events,
        _dir: dir,
    }
}

impl Rig {
    async fn event(&mut self) -> OutputEvent {
        timeout(Duration::from_secs(5), self.events.recv()).await.expect("an event").expect("open channel")
    }

    /// Plays 100-frame callbacks until `want` events have been seen; returns (samples, events).
    async fn play_until(&mut self, want: usize) -> (Vec<f32>, Vec<OutputEvent>) {
        let (mut samples, mut events) = (Vec::new(), Vec::new());
        for _ in 0..20_000 {
            samples.extend(self.dev.play(100));
            while let Ok(e) = self.events.try_recv() {
                events.push(e);
            }
            if events.len() >= want {
                return (samples, events);
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("only saw {events:?}");
    }
}

fn ramp(from: i16, n: usize) -> Vec<f32> {
    (0..n).map(|i| (from as f32 + i as f32) / 32768.0).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_load_reports_started_and_the_track_plays_to_its_end_exactly() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    assert_eq!(rig.event().await, OutputEvent::Started { position_ms: 0 });
    assert_eq!(rig.dev.calls(), ["stop", "start playing=true"]);

    let (samples, events) = rig.play_until(1).await;
    assert_eq!(events, [OutputEvent::Ended]);
    assert_eq!(samples, ramp(1000, 4000), "every sample, in order, none added");
    assert_eq!(rig.dev.calls().last().unwrap(), "stop", "the device is released at the end");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preloaded_track_joins_without_a_gap_and_its_load_is_then_a_no_op() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    assert_eq!(rig.event().await, OutputEvent::Started { position_ms: 0 });
    rig.out.preload(Some(&entry("b")));

    let (samples, events) = rig.play_until(2).await;
    assert_eq!(events, [OutputEvent::Ended, OutputEvent::Started { position_ms: 0 }]);
    let mut want = ramp(1000, 4000);
    want.extend(ramp(20000, 4000));
    assert_eq!(&samples[..4000], &want[..4000]);
    assert!(samples.len() >= 4000);
    assert_eq!(samples[3999], want[3999]);
    // Whatever of b arrived is b, immediately: no zero-length hole between the two.
    assert_eq!(samples[4000..], want[4000..samples.len()]);

    let starts = rig.dev.calls().iter().filter(|c| c.starts_with("start")).count();
    rig.out.load(&entry("b"), 0, false); // what the core does after `Ended`
    rig.out.pause(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.dev.calls().iter().filter(|c| c.starts_with("start")).count(), starts, "no restart of b");

    let (rest, events) = rig.play_until(1).await;
    assert_eq!(events, [OutputEvent::Ended]);
    assert_eq!([&samples[4000..], &rest[..]].concat(), want[4000..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_load_of_a_different_track_after_a_preload_starts_it_afresh() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    rig.event().await;
    rig.out.preload(Some(&entry("b")));
    rig.out.load(&entry("b"), 0, false);
    assert_eq!(rig.event().await, OutputEvent::Started { position_ms: 0 });
    let (samples, events) = rig.play_until(1).await;
    assert_eq!(events, [OutputEvent::Ended]);
    assert_eq!(samples, ramp(20000, 4000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_and_resume_drive_the_device_and_a_paused_load_starts_it_paused() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, true);
    assert_eq!(rig.event().await, OutputEvent::Started { position_ms: 0 });
    assert_eq!(rig.dev.calls(), ["stop", "start playing=false"]);
    rig.out.pause(false);
    rig.out.pause(true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.dev.calls()[2..], ["resume", "pause"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seek_restarts_the_stream_at_the_new_position_without_an_event() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    rig.event().await;
    rig.out.seek(250); // 2000 frames in
    tokio::time::sleep(Duration::from_millis(150)).await;
    let first = rig.dev.play(10);
    let at = (first[0] * 32768.0).round() as i64 - 1000;
    assert!((at - 2000).abs() <= 1024, "landed at frame {at}");
    assert!(rig.events.try_recv().is_err(), "the core's clock already moved; nothing to report");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volume_scales_the_samples_and_mute_silences_them() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    rig.event().await;
    rig.out.set_volume(50, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let s = rig.dev.play(4);
    assert!((s[3] - (1003.0 / 32768.0) * 0.25).abs() < 1e-6, "{s:?}");
    rig.out.set_volume(50, true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rig.dev.play(4).iter().all(|v| *v == 0.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unplayable_track_fails_instead_of_hanging_and_leaves_the_device_free() {
    let mut rig = rig().await;
    rig.out.load(&entry("garbage"), 0, false);
    assert!(matches!(rig.event().await, OutputEvent::Failed { .. }));
    assert_eq!(rig.dev.calls().last().unwrap(), "stop");
    // And the output still works afterwards.
    rig.out.load(&entry("a"), 0, false);
    assert_eq!(rig.event().await, OutputEvent::Started { position_ms: 0 });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_releases_the_device_and_reports_nothing() {
    let mut rig = rig().await;
    rig.out.load(&entry("a"), 0, false);
    rig.event().await;
    rig.out.stop();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.dev.calls().last().unwrap(), "stop");
    assert!(rig.dev.play(10).is_empty());
    assert!(rig.events.try_recv().is_err());
}

// --- conversion ---

fn convert(in_rate: u32, in_ch: usize, out_rate: u32, out_ch: usize, input: &[f32]) -> Vec<f32> {
    let mut c = Converter::new(in_rate, in_ch, out_rate, out_ch);
    let mut out = Vec::new();
    c.push(input, &mut out);
    out
}

#[test]
fn equal_formats_pass_through_untouched() {
    let input = [0.1, 0.2, 0.3, 0.4];
    assert_eq!(convert(48000, 2, 48000, 2, &input), input);
}

#[test]
fn channels_are_duplicated_or_averaged() {
    assert_eq!(convert(8000, 1, 8000, 2, &[0.5, 0.25]), [0.5, 0.5, 0.25, 0.25]);
    assert_eq!(convert(8000, 2, 8000, 1, &[0.2, 0.4, 1.0, 0.0]), [0.3, 0.5]);
}

#[test]
fn doubling_the_rate_interpolates_between_samples() {
    let out = convert(8000, 1, 16000, 1, &[0.0, 1.0, 0.0, 1.0]);
    assert_eq!(out[..5], [0.0, 0.5, 1.0, 0.5, 0.0]);
}

#[test]
fn resampling_across_blocks_equals_resampling_the_whole() {
    let input: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.01).sin()).collect();
    let whole = convert(44100, 1, 48000, 1, &input);
    let mut c = Converter::new(44100, 1, 48000, 1);
    let mut split = Vec::new();
    for block in input.chunks(333) {
        c.push(block, &mut split);
    }
    assert_eq!(split.len(), whole.len());
    assert!(split.iter().zip(&whole).all(|(a, b)| (a - b).abs() < 1e-5));
    let ratio = whole.len() as f64 / 2000.0;
    assert!((ratio - 48000.0 / 44100.0).abs() < 0.01, "{ratio}");
}
