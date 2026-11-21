#![allow(dead_code)]

use std::time::Duration;

use ytmusic::models::Track;
use ytmusicd::player::Output;
use ytmusicd::protocol::Entry;

pub fn track(id: &str) -> Track {
    Track {
        video_id: Some(id.to_string()),
        title: id.to_string(),
        duration: Some(Duration::from_secs(200)),
        available: true,
        ..Track::default()
    }
}

pub fn entry(id: &str) -> Entry {
    Entry { entry: 1, track: track(id) }
}

pub fn tracks(count: usize) -> Vec<Track> {
    (0..count).map(|i| track(&format!("v{i}"))).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Load { id: String, start_ms: u64, paused: bool },
    Preload(Option<String>),
    Pause(bool),
    Seek(u64),
    Volume(u8, bool),
    Stop,
}

/// Records what the core asked for; plays nothing.
#[derive(Default)]
pub struct FakeOutput {
    pub calls: Vec<Call>,
}

impl FakeOutput {
    pub fn last_load(&self) -> Option<&str> {
        self.calls.iter().rev().find_map(|call| match call {
            Call::Load { id, .. } => Some(id.as_str()),
            _ => None,
        })
    }

    pub fn last_preload(&self) -> Option<Option<&str>> {
        self.calls.iter().rev().find_map(|call| match call {
            Call::Preload(next) => Some(next.as_deref()),
            _ => None,
        })
    }

    pub fn loads(&self) -> usize {
        self.calls.iter().filter(|call| matches!(call, Call::Load { .. })).count()
    }
}

impl Output for FakeOutput {
    fn load(&mut self, entry: &Entry, start_ms: u64, paused: bool) {
        let id = entry.track.video_id.clone().unwrap_or_default();
        self.calls.push(Call::Load { id, start_ms, paused });
    }
    fn preload(&mut self, next: Option<&Entry>) {
        self.calls.push(Call::Preload(next.and_then(|entry| entry.track.video_id.clone())));
    }
    fn pause(&mut self, on: bool) {
        self.calls.push(Call::Pause(on));
    }
    fn seek(&mut self, to_ms: u64) {
        self.calls.push(Call::Seek(to_ms));
    }
    fn set_volume(&mut self, percent: u8, muted: bool) {
        self.calls.push(Call::Volume(percent, muted));
    }
    fn stop(&mut self) {
        self.calls.push(Call::Stop);
    }
}
