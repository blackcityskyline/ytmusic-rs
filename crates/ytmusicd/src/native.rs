//! The in-process output: decode on one thread, play from a ring buffer filled for the
//! sound card's callback.
//!
//! Energy rules, same as the daemon's: the decoder thread sleeps (`park`) whenever there is
//! nothing to do — stopped, paused, or its ring full — and is woken by a command or by the
//! device draining the ring past a mark. While paused or stopped the device is stopped too,
//! so nothing wakes at all. No timers, no polling.
//!
//! The device is behind [`Device`], so everything above the sound card (gapless joins, seeks,
//! boundaries, events) is tested with a fake one that "plays" when the test says so.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::{self, Thread};

use anyhow::Result;
use rtrb::{Consumer, Producer, RingBuffer};
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedSender;

use crate::data::Resolver;
use crate::decode::{Decoder, SpoolSource};
use crate::player::{Output, OutputEvent};
use crate::protocol::Entry;

/// How much decoded audio the ring holds. Long enough to ride out a slow wake-up, short
/// enough that a seek or a volume change is not a half-second late.
const RING_MS: u64 = 500;
/// Audio queued before the device starts, so the first callbacks do not underrun.
const PREROLL_MS: u64 = 150;

// --- sample conversion ------------------------------------------------------------------

/// Channel layout and sample rate conversion into the device's format. Linear interpolation:
/// cheap, and inaudible for the 44.1 → 48 kHz case that is all that is left once the device
/// runs at the track's own rate, which it usually does.
pub struct Converter {
    in_channels: usize,
    out_channels: usize,
    /// Input frames advanced per output frame.
    step: f64,
    /// Position of the next output frame, in input frames from the start of the next block;
    /// -1..0 is between the previous block's last frame and this block's first.
    at: f64,
    prev: Vec<f32>,
    started: bool,
}

impl Converter {
    pub fn new(in_rate: u32, in_channels: usize, out_rate: u32, out_channels: usize) -> Self {
        Self { in_channels, out_channels, step: in_rate as f64 / out_rate as f64, at: 0.0, prev: vec![0.0; out_channels], started: false }
    }

    fn remix(&self, frame: &[f32], out: &mut [f32]) {
        let (i, o) = (self.in_channels, self.out_channels);
        if i == o {
            out.copy_from_slice(frame);
        } else if i == 1 {
            out.fill(frame[0]);
        } else if o == 1 {
            out[0] = frame.iter().sum::<f32>() / i as f32;
        } else {
            for (c, slot) in out.iter_mut().enumerate() {
                *slot = frame.get(c).copied().unwrap_or(0.0);
            }
        }
    }

    /// Appends the converted `input` (interleaved, `in_channels` wide) to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let (ic, oc) = (self.in_channels, self.out_channels);
        let frames = input.len() / ic;
        if frames == 0 {
            return;
        }
        let mut mapped = vec![0.0f32; frames * oc];
        for (frame, dst) in input.chunks_exact(ic).zip(mapped.chunks_exact_mut(oc)) {
            self.remix(frame, dst);
        }
        if self.step == 1.0 {
            out.extend_from_slice(&mapped);
            return;
        }
        if !self.started {
            self.prev.copy_from_slice(&mapped[..oc]);
            self.started = true;
        }
        let n = frames as f64;
        while self.at < n - 1.0 {
            let floor = self.at.floor();
            let frac = (self.at - floor) as f32;
            let a: &[f32] = if floor < 0.0 { &self.prev } else { &mapped[floor as usize * oc..floor as usize * oc + oc] };
            let b = &mapped[(floor + 1.0) as usize * oc..(floor + 1.0) as usize * oc + oc];
            for c in 0..oc {
                out.push(a[c] + (b[c] - a[c]) * frac);
            }
            self.at += self.step;
        }
        self.at -= n;
        self.prev.copy_from_slice(&mapped[(frames - 1) * oc..]);
    }
}

// --- the device side ----------------------------------------------------------------------

/// What the device callback and the engine share.
pub struct Shared {
    /// Linear gain as `f32` bits.
    gain: AtomicU32,
    /// Frames the device has taken from the current ring.
    consumed: AtomicU64,
    /// The device wakes the engine when `consumed` reaches this (a track boundary).
    watch: AtomicU64,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self { gain: AtomicU32::new(1.0f32.to_bits()), consumed: AtomicU64::new(0), watch: AtomicU64::new(u64::MAX) })
    }
}

/// The consumer half the device callback runs. `fill` is the whole callback.
pub struct Pull {
    ring: Consumer<f32>,
    shared: Arc<Shared>,
    wake: Thread,
    channels: usize,
    capacity: usize,
}

impl Pull {
    /// Writes `out` from the ring, scaled by the gain, silence where the ring ran dry.
    /// Wakes the engine when half the ring is free again or a boundary was passed — not on
    /// every callback. Returns how many samples came from the ring (the rest is silence).
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let (taken, _) = self.ring.pop_partial_slice(out);
        let n = taken.len();
        let gain = f32::from_bits(self.shared.gain.load(Ordering::Relaxed));
        for sample in &mut out[..n] {
            *sample *= gain;
        }
        out[n..].fill(0.0);

        let before = self.shared.consumed.load(Ordering::Relaxed);
        let after = before + (n / self.channels) as u64;
        self.shared.consumed.store(after, Ordering::Release);

        let crossed = before < self.shared.watch.load(Ordering::Relaxed) && after >= self.shared.watch.load(Ordering::Relaxed);
        let half_free = self.ring.slots() >= self.capacity / 2 && n > 0;
        if crossed || half_free {
            self.wake.unpark();
        }
        n
    }
}

/// The sound card. `start` hands it the ring; it calls [`Pull::fill`] from its callback.
pub trait Device: 'static {
    /// Sample rate and channel count the device will run at.
    fn format(&mut self) -> Result<(u32, usize)>;
    fn start(&mut self, pull: Pull, playing: bool) -> Result<()>;
    fn pause(&mut self);
    fn resume(&mut self);
    /// Releases the device entirely.
    fn stop(&mut self);
}

// --- the engine -----------------------------------------------------------------------

enum Cmd {
    Load { video_id: String, start_ms: u64, paused: bool },
    Preload(Option<String>),
    Pause(bool),
    Seek(u64),
    Volume { percent: u8, muted: bool },
    Stop,
}

#[derive(Debug)]
enum Boundary {
    /// The next track's first sample is at this frame.
    Advance(String),
    /// The last sample of the final track is just before this frame.
    End,
}

struct Open {
    id: String,
    decoder: Decoder,
    convert: Converter,
    done: bool,
}

struct Engine<D: Device> {
    device: D,
    runtime: Handle,
    resolver: Resolver,
    events: UnboundedSender<OutputEvent>,
    shared: Arc<Shared>,
    me: Thread,

    format: Option<(u32, usize)>,
    producer: Option<Producer<f32>>,
    /// The consumer half, between making the ring and handing it to the device.
    pending_pull: Option<Pull>,
    capacity: usize,
    /// Converted samples waiting for room in the ring.
    pending: Vec<f32>,
    pending_at: usize,
    pushed: u64,

    current: Option<Open>,
    next: Option<Open>,
    boundaries: VecDeque<(u64, Boundary)>,
    end_marked: bool,
    /// The id of the track that is audible now; the decoder may already be one ahead.
    audible: Option<String>,
    /// The engine moved to the preloaded track by itself; the core's `load` of it is a no-op.
    advanced: bool,
    paused: bool,
}

impl<D: Device> Engine<D> {
    fn emit(&self, event: OutputEvent) {
        let _ = self.events.send(event);
    }

    fn open(&self, video_id: &str) -> Result<Open> {
        let (rate, channels) = self.format.expect("format is read before the first open");
        let spool = self.resolver.spool(video_id);
        let source = SpoolSource::open(spool, self.runtime.clone())?;
        let decoder = Decoder::open(Box::new(source), None)?;
        let convert = Converter::new(decoder.rate, decoder.channels, rate, channels);
        Ok(Open { id: video_id.to_string(), decoder, convert, done: false })
    }

    fn reset_ring(&mut self) -> Result<()> {
        self.device.stop();
        let (rate, channels) = self.format()?;
        self.capacity = (rate as u64 * RING_MS / 1000) as usize * channels;
        let (producer, consumer) = RingBuffer::new(self.capacity);
        self.producer = Some(producer);
        self.shared.consumed.store(0, Ordering::Release);
        self.shared.watch.store(u64::MAX, Ordering::Release);
        self.pushed = 0;
        self.pending.clear();
        self.pending_at = 0;
        self.boundaries.clear();
        self.end_marked = false;
        self.pending_pull =
            Some(Pull { ring: consumer, shared: self.shared.clone(), wake: self.me.clone(), channels, capacity: self.capacity });
        Ok(())
    }

    fn format(&mut self) -> Result<(u32, usize)> {
        if let Some(format) = self.format {
            return Ok(format);
        }
        let format = self.device.format()?;
        self.format = Some(format);
        Ok(format)
    }

    fn fail(&mut self, reason: String) {
        self.teardown();
        self.emit(OutputEvent::Failed { reason });
    }

    fn teardown(&mut self) {
        self.device.stop();
        self.producer = None;
        self.pending_pull = None;
        self.current = None;
        self.next = None;
        self.boundaries.clear();
        self.pending.clear();
        self.pending_at = 0;
        self.audible = None;
        self.advanced = false;
        self.end_marked = false;
    }

    /// Starts `video_id` at `start_ms`: new ring, decode a preroll, start the device.
    fn begin(&mut self, video_id: &str, start_ms: u64, paused: bool) -> Result<()> {
        self.format()?;
        self.reset_ring()?;
        let mut track = self.open(video_id)?;
        if start_ms > 0 {
            track.decoder.seek(start_ms)?;
        }
        self.current = Some(track);
        self.next = None;
        self.audible = Some(video_id.to_string());
        self.advanced = false;

        let (rate, channels) = self.format.unwrap();
        let preroll = (rate as u64 * PREROLL_MS / 1000) * channels as u64;
        while self.queued() < preroll {
            if !self.produce()? {
                break; // finished or the ring is full
            }
        }
        let pull = self.pending_pull.take().expect("ring was just made");
        self.device.start(pull, !paused)?;
        self.paused = paused;
        Ok(())
    }

    /// Samples decoded but not yet taken by the device.
    fn queued(&self) -> u64 {
        let channels = self.format.map(|f| f.1).unwrap_or(2) as u64;
        let consumed = self.shared.consumed.load(Ordering::Acquire) * channels;
        (self.pushed * channels).saturating_sub(consumed) + (self.pending.len() - self.pending_at) as u64
    }

    /// Moves audio towards the ring. Returns whether anything moved.
    fn produce(&mut self) -> Result<bool> {
        let channels = self.format.map(|f| f.1).unwrap_or(2);
        let Some(producer) = self.producer.as_mut() else {
            return Ok(false);
        };

        if self.pending_at < self.pending.len() {
            let (_, rest) = producer.push_partial_slice(&self.pending[self.pending_at..]);
            let moved = self.pending.len() - self.pending_at - rest.len();
            self.pending_at += moved;
            self.pushed += (moved / channels) as u64;
            if self.pending_at == self.pending.len() {
                self.pending.clear();
                self.pending_at = 0;
            }
            return Ok(moved > 0);
        }

        let Some(track) = self.current.as_mut() else {
            return Ok(false);
        };
        if !track.done {
            match track.decoder.next_block()? {
                Some(block) => {
                    let mut out = Vec::with_capacity(block.len());
                    track.convert.push(block, &mut out);
                    self.pending = out;
                    self.pending_at = 0;
                    return Ok(true);
                }
                None => track.done = true,
            }
        }

        // The track is exhausted and fully queued: join the next one or mark the end.
        if let Some(next) = self.next.take() {
            self.boundaries.push_back((self.pushed, Boundary::Advance(next.id.clone())));
            self.current = Some(next);
            self.arm_watch();
            return Ok(true);
        }
        if !self.end_marked {
            self.end_marked = true;
            self.boundaries.push_back((self.pushed, Boundary::End));
            self.arm_watch();
        }
        Ok(false)
    }

    fn arm_watch(&self) {
        let at = self.boundaries.front().map(|b| b.0).unwrap_or(u64::MAX);
        self.shared.watch.store(at, Ordering::Release);
    }

    /// Reports the boundaries the device has played past.
    fn check_boundaries(&mut self) {
        let consumed = self.shared.consumed.load(Ordering::Acquire);
        while self.boundaries.front().is_some_and(|b| b.0 <= consumed) {
            let (_, boundary) = self.boundaries.pop_front().unwrap();
            self.arm_watch();
            match boundary {
                Boundary::Advance(id) => {
                    self.audible = Some(id);
                    self.advanced = true;
                    self.emit(OutputEvent::Ended);
                    self.emit(OutputEvent::Started { position_ms: 0 });
                }
                Boundary::End => {
                    self.teardown();
                    self.emit(OutputEvent::Ended);
                }
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load { video_id, start_ms, paused } => {
                if self.advanced && self.audible.as_deref() == Some(video_id.as_str()) {
                    self.advanced = false;
                    return;
                }
                match self.begin(&video_id, start_ms, paused) {
                    Ok(()) => self.emit(OutputEvent::Started { position_ms: start_ms }),
                    Err(error) => self.fail(format!("{error:#}")),
                }
            }
            Cmd::Preload(next) => {
                let same = |open: &Option<Open>| open.as_ref().map(|o| &o.id) == next.as_ref();
                if self.current.is_none() || same(&self.next) || same(&self.current) {
                    return;
                }
                self.next = None;
                if let Some(id) = next {
                    // A failure here is not reported: the core's `load` of the same track
                    // will fail the same way and be handled as a failed track.
                    self.next = self.open(&id).ok();
                }
            }
            Cmd::Pause(on) => {
                if self.current.is_none() && self.audible.is_none() {
                    return;
                }
                self.paused = on;
                if on {
                    self.device.pause();
                } else {
                    self.device.resume();
                }
            }
            Cmd::Seek(to_ms) => {
                let Some(id) = self.audible.clone() else {
                    return;
                };
                let paused = self.paused;
                match self.begin(&id, to_ms, paused) {
                    Ok(()) => {}
                    Err(error) => self.fail(format!("{error:#}")),
                }
            }
            Cmd::Volume { percent, muted } => {
                let linear = if muted { 0.0 } else { (percent as f32 / 100.0).powi(2) };
                self.shared.gain.store(linear.to_bits(), Ordering::Relaxed);
            }
            Cmd::Stop => self.teardown(),
        }
    }
}

fn run<D: Device>(mut engine: Engine<D>, rx: Receiver<Cmd>) {
    loop {
        loop {
            match rx.try_recv() {
                Ok(cmd) => engine.command(cmd),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    engine.teardown();
                    return;
                }
            }
        }
        engine.check_boundaries();
        let moved = match engine.produce() {
            Ok(moved) => moved,
            Err(error) => {
                engine.fail(format!("{error:#}"));
                false
            }
        };
        if !moved {
            thread::park();
        }
    }
}

/// The handle the player core holds.
pub struct NativeOut {
    tx: Sender<Cmd>,
    thread: Thread,
}

impl NativeOut {
    /// Must be called inside a Tokio runtime (its handle serves the decoder's reads). The
    /// device is built on the audio thread (sound-card streams are often not `Send`) and not
    /// opened until the first `load`.
    pub fn spawn<D: Device>(
        make_device: impl FnOnce() -> D + Send + 'static,
        resolver: Resolver,
        events: UnboundedSender<OutputEvent>,
    ) -> Self {
        let (tx, rx) = channel();
        let runtime = Handle::current();
        let handle = thread::Builder::new()
            .name("ytmusicd-audio".into())
            .spawn(move || {
                let engine = Engine {
                    device: make_device(),
                    runtime,
                    resolver,
                    events,
                    shared: Shared::new(),
                    me: thread::current(),
                    format: None,
                    producer: None,
                    pending_pull: None,
                    capacity: 0,
                    pending: Vec::new(),
                    pending_at: 0,
                    pushed: 0,
                    current: None,
                    next: None,
                    boundaries: VecDeque::new(),
                    end_marked: false,
                    audible: None,
                    advanced: false,
                    paused: false,
                };
                run(engine, rx)
            })
            .expect("spawn the audio thread");
        let thread = handle.thread().clone();
        Self { tx, thread }
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.thread.unpark();
        }
    }
}

impl Output for NativeOut {
    fn load(&mut self, entry: &Entry, start_ms: u64, paused: bool) {
        if let Some(video_id) = &entry.track.video_id {
            self.send(Cmd::Load { video_id: video_id.clone(), start_ms, paused });
        }
    }
    fn preload(&mut self, next: Option<&Entry>) {
        self.send(Cmd::Preload(next.and_then(|e| e.track.video_id.clone())));
    }
    fn pause(&mut self, on: bool) {
        self.send(Cmd::Pause(on));
    }
    fn seek(&mut self, to_ms: u64) {
        self.send(Cmd::Seek(to_ms));
    }
    fn set_volume(&mut self, percent: u8, muted: bool) {
        self.send(Cmd::Volume { percent, muted });
    }
    fn stop(&mut self) {
        self.send(Cmd::Stop);
    }
}
