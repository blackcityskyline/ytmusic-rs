//! The player state machine. It owns the queue and the clock, tells an [`Output`] what to do,
//! and answers with the events clients must hear. It reads no clock of its own: every call is
//! given `now_ms`, so a test decides what time it is.
//!
//! Position is an anchor, not a counter: `clock` holds where playback was at an instant and how
//! fast it moves. Nothing here wakes up on its own.

use serde_json::{Value, json};

use crate::protocol::{Clock, Command, Entry, ErrorCode, Event, ProtocolError, SeekArgs, State, Status, Track, VolumeArgs};
use crate::queue::Queue;

/// Pressing previous this far into a track restarts it instead of leaving it.
const RESTART_AFTER_MS: u64 = 3_000;

/// What plays the sound. An implementation reports back through [`OutputEvent`]; it never
/// speaks to clients, and it reports only what it cannot be assumed to have done as told.
pub trait Output {
    /// Start `entry` at `start_ms`. The output resolves the stream itself.
    fn load(&mut self, entry: &Entry, start_ms: u64, paused: bool);
    /// The entry that follows the loaded one, or none. The output decides when to prepare it,
    /// typically near the end of the current track, so the join is gapless.
    fn preload(&mut self, next: Option<&Entry>);
    fn pause(&mut self, on: bool);
    fn seek(&mut self, to_ms: u64);
    fn set_volume(&mut self, percent: u8, muted: bool);
    fn stop(&mut self);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// The loaded entry is audible (or ready, when it was loaded paused) at this position.
    Started {
        position_ms: u64,
    },
    /// The position jumped on the output's own: a buffering stall, a seek it had to correct.
    Discontinuity {
        position_ms: u64,
    },
    /// The entry played to its end.
    Ended,
    Failed {
        reason: String,
    },
}

/// The reply data of a command and the events it caused.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    pub data: Value,
    pub events: Vec<Event>,
}

pub struct Player<O: Output> {
    out: O,
    queue: Queue,
    status: Status,
    /// What the user asked for while the entry was loading.
    want_paused: bool,
    clock: Clock,
    volume: u8,
    muted: bool,
    rev: u64,
    /// Entries that failed in a row; once it reaches the queue length the player stops.
    failures: usize,
}

impl<O: Output> Player<O> {
    pub fn new(out: O, seed: u64) -> Self {
        Self {
            out,
            queue: Queue::new(seed),
            status: Status::Stopped,
            want_paused: false,
            clock: Clock { position_ms: 0, at_ms: 0, rate: 0.0 },
            volume: 100,
            muted: false,
            rev: 0,
            failures: 0,
        }
    }

    pub fn output(&self) -> &O {
        &self.out
    }

    pub fn state(&self) -> State {
        State {
            rev: self.rev,
            status: self.status,
            entry: self.queue.current(),
            clock: self.clock,
            volume: self.volume,
            muted: self.muted,
            shuffle: self.queue.shuffle(),
            repeat: self.queue.repeat(),
            queue_rev: self.queue.rev(),
        }
    }

    /// Where playback is at `now_ms`, the way a client computes it.
    pub fn position(&self, now_ms: u64) -> u64 {
        let elapsed = now_ms.saturating_sub(self.clock.at_ms) as f64 * self.clock.rate;
        self.clock.position_ms + elapsed as u64
    }

    fn duration_ms(&self) -> Option<u64> {
        self.queue.current_entry()?.track.duration.map(|seconds| seconds * 1000)
    }

    fn anchor(&mut self, now_ms: u64, position_ms: u64, playing: bool) {
        self.clock = Clock { position_ms, at_ms: now_ms, rate: if playing { 1.0 } else { 0.0 } };
    }

    pub fn handle(&mut self, command: Command, now_ms: u64) -> Result<Outcome, ProtocolError> {
        let before = self.state();
        let queue_before = self.queue.rev();
        let mut data = Value::Null;

        match command {
            Command::Hello(_) | Command::Subscribe(_) | Command::Unsubscribe(_) | Command::Shutdown => {
                return Err(ProtocolError::new(ErrorCode::Internal, "a connection command reached the player"));
            }
            Command::State => {
                return Ok(Outcome { data: json!(self.state()), events: vec![] });
            }
            Command::QueueGet => {
                return Ok(Outcome { data: json!(self.queue.snapshot()), events: vec![] });
            }

            Command::Play(args) => {
                let tracks = playable(args.tracks, args.index);
                let start = tracks.1;
                if tracks.0.is_empty() {
                    return Err(ProtocolError::bad_request("none of the tracks can be played"));
                }
                self.queue.replace(tracks.0, start);
                self.start_current(now_ms, 0, args.paused);
            }
            Command::QueueAdd(args) => {
                self.queue.check(args.if_rev)?;
                let (tracks, _) = playable(args.tracks, 0);
                let ids = self.queue.add(tracks, args.next);
                data = json!({ "entries": ids });
                self.refresh_preload();
            }
            Command::QueueRemove(args) => {
                self.queue.check(args.if_rev)?;
                let was_current = self.queue.remove(args.entry)?;
                if was_current && self.status != Status::Stopped {
                    if self.queue.current().is_some() {
                        self.start_current(now_ms, 0, self.status == Status::Paused);
                    } else {
                        self.halt(now_ms);
                    }
                } else {
                    self.refresh_preload();
                }
            }
            Command::QueueMove(args) => {
                self.queue.check(args.if_rev)?;
                self.queue.move_entry(args.entry, args.before)?;
                self.refresh_preload();
            }
            Command::QueueJump(args) => {
                self.queue.set_current(args.entry)?;
                self.start_current(now_ms, 0, false);
            }
            Command::QueueClear => {
                self.queue.clear();
                self.refresh_preload();
            }

            Command::Next => match self.queue.advance(true, false) {
                Some(_) => self.start_current(now_ms, 0, false),
                None => self.halt(now_ms),
            },
            Command::Prev => {
                if self.position(now_ms) > RESTART_AFTER_MS {
                    self.restart(now_ms);
                } else if self.queue.advance(false, false).is_some() {
                    self.start_current(now_ms, 0, false);
                } else {
                    self.restart(now_ms);
                }
            }

            Command::Pause => self.set_paused(true, now_ms),
            Command::Resume => self.resume(now_ms),
            Command::Toggle => {
                if self.status == Status::Playing || (self.status == Status::Loading && !self.want_paused) {
                    self.set_paused(true, now_ms);
                } else {
                    self.resume(now_ms);
                }
            }
            Command::Stop => self.halt(now_ms),

            Command::Seek(args) => self.seek(args, now_ms)?,
            Command::Volume(args) => self.set_volume(args)?,
            Command::Mute(args) => {
                self.muted = args.on.unwrap_or(!self.muted);
                self.out.set_volume(self.volume, self.muted);
            }
            Command::Shuffle(args) => self.queue.set_shuffle(args.on),
            Command::Repeat(args) => {
                self.queue.set_repeat(args.mode);
                self.refresh_preload();
            }
        }

        Ok(Outcome { data, events: self.events_since(before, queue_before) })
    }

    pub fn on_output(&mut self, event: OutputEvent, now_ms: u64) -> Vec<Event> {
        let before = self.state();
        let queue_before = self.queue.rev();
        let mut notices = Vec::new();

        match event {
            OutputEvent::Started { position_ms } => {
                if self.status == Status::Loading {
                    self.failures = 0;
                    if self.want_paused {
                        self.status = Status::Paused;
                        self.anchor(now_ms, position_ms, false);
                    } else {
                        self.status = Status::Playing;
                        self.anchor(now_ms, position_ms, true);
                    }
                }
            }
            OutputEvent::Discontinuity { position_ms } => {
                let playing = self.status == Status::Playing;
                self.anchor(now_ms, position_ms, playing);
            }
            OutputEvent::Ended => {
                if matches!(self.status, Status::Playing | Status::Loading) {
                    match self.queue.advance(true, true) {
                        Some(_) => self.start_current(now_ms, 0, false),
                        None => self.halt(now_ms),
                    }
                }
            }
            OutputEvent::Failed { reason } => {
                if self.status != Status::Stopped {
                    self.failures += 1;
                    notices.push(Event::Notice { message: reason });
                    // A dead entry is skipped, but a queue of dead entries must not spin.
                    let skipped = if self.failures < self.queue.len() { self.queue.advance(true, false) } else { None };
                    match skipped {
                        Some(_) => self.start_current(now_ms, 0, false),
                        None => self.halt(now_ms),
                    }
                }
            }
        }

        let mut events = notices;
        events.extend(self.events_since(before, queue_before));
        events
    }

    fn events_since(&mut self, before: State, queue_before: u64) -> Vec<Event> {
        let mut events = Vec::new();
        if self.queue.rev() != queue_before {
            events.push(Event::Queue(self.queue.snapshot()));
        }
        if self.state() != before {
            self.rev += 1;
            events.push(Event::State(self.state()));
        }
        events
    }

    fn start_current(&mut self, now_ms: u64, start_ms: u64, paused: bool) {
        let Some(entry) = self.queue.current_entry().cloned() else {
            return;
        };
        self.status = Status::Loading;
        self.want_paused = paused;
        self.anchor(now_ms, start_ms, false);
        self.out.load(&entry, start_ms, paused);
        self.refresh_preload();
    }

    fn restart(&mut self, now_ms: u64) {
        if self.status == Status::Stopped {
            return;
        }
        self.out.seek(0);
        let playing = self.status == Status::Playing;
        self.anchor(now_ms, 0, playing);
    }

    fn refresh_preload(&mut self) {
        if self.status != Status::Stopped {
            self.out.preload(self.queue.peek_next());
        }
    }

    fn halt(&mut self, now_ms: u64) {
        if self.status != Status::Stopped {
            self.out.stop();
        }
        self.status = Status::Stopped;
        self.want_paused = false;
        self.anchor(now_ms, 0, false);
    }

    fn set_paused(&mut self, on: bool, now_ms: u64) {
        match self.status {
            Status::Playing if on => {
                let position = self.position(now_ms);
                self.out.pause(true);
                self.status = Status::Paused;
                self.anchor(now_ms, position, false);
            }
            Status::Paused if !on => {
                self.out.pause(false);
                self.status = Status::Playing;
                let position = self.clock.position_ms;
                self.anchor(now_ms, position, true);
            }
            Status::Loading => {
                self.want_paused = on;
                self.out.pause(on);
            }
            _ => {}
        }
    }

    fn resume(&mut self, now_ms: u64) {
        if self.status == Status::Stopped {
            if self.queue.current().is_some() {
                self.start_current(now_ms, 0, false);
            }
        } else {
            self.set_paused(false, now_ms);
        }
    }

    fn seek(&mut self, args: SeekArgs, now_ms: u64) -> Result<(), ProtocolError> {
        if self.status == Status::Stopped {
            return Err(ProtocolError::bad_request("nothing is playing"));
        }
        let target = match (args.to, args.by) {
            (Some(to), None) => to as i64,
            (None, Some(by)) => self.position(now_ms) as i64 + by,
            _ => {
                return Err(ProtocolError::bad_request("seek takes `to` or `by`, one of them"));
            }
        };

        let mut target = target.max(0) as u64;
        if let Some(duration) = self.duration_ms() {
            target = target.min(duration);
        }
        self.out.seek(target);
        let playing = self.status == Status::Playing;
        self.anchor(now_ms, target, playing);
        Ok(())
    }

    fn set_volume(&mut self, args: VolumeArgs) -> Result<(), ProtocolError> {
        let percent = match (args.set, args.by) {
            (Some(set), None) => i16::from(set),
            (None, Some(by)) => i16::from(self.volume) + by,
            _ => {
                return Err(ProtocolError::bad_request("volume takes `set` or `by`, one of them"));
            }
        };
        self.volume = percent.clamp(0, 100) as u8;
        self.out.set_volume(self.volume, self.muted);
        Ok(())
    }
}

/// The tracks that can be played, and where `index` lands once the others are gone.
fn playable(tracks: Vec<Track>, index: usize) -> (Vec<Track>, usize) {
    let usable = |track: &Track| track.video_id.is_some() && track.available;
    let start = tracks.iter().take(index).filter(|track| usable(track)).count();
    (tracks.into_iter().filter(usable).collect(), start)
}
