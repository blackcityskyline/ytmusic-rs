//! The play queue: what is in it, which entry is current, and what comes next.
//!
//! `items` is the order the user built; `order` is the order playback walks, the same list
//! unless shuffle is on. `rev` counts edits to the *content* (add, remove, move, clear,
//! replace), so a client can refuse to act on a list it has not seen. Moving to another entry
//! does not change it.

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use ytmusic::models::Track;

use crate::protocol::{Entry, EntryId, ErrorCode, ProtocolError, QueueSnapshot, RepeatMode};

pub struct Queue {
    items: Vec<Entry>,
    order: Vec<EntryId>,
    current: Option<EntryId>,
    shuffle: bool,
    repeat: RepeatMode,
    rev: u64,
    next_id: EntryId,
    rng: StdRng,
}

impl Queue {
    pub fn new(seed: u64) -> Self {
        Self {
            items: Vec::new(),
            order: Vec::new(),
            current: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            rev: 0,
            next_id: 1,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn shuffle(&self) -> bool {
        self.shuffle
    }

    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn current(&self) -> Option<EntryId> {
        self.current
    }

    pub fn current_entry(&self) -> Option<&Entry> {
        self.current.and_then(|id| self.get(id))
    }

    pub fn get(&self, id: EntryId) -> Option<&Entry> {
        self.items.iter().find(|entry| entry.entry == id)
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot { queue_rev: self.rev, entries: self.items.clone() }
    }

    /// Refuses an edit made against a list that has changed since the client saw it.
    pub fn check(&self, if_rev: Option<u64>) -> Result<(), ProtocolError> {
        match if_rev {
            Some(seen) if seen != self.rev => Err(ProtocolError::new(
                ErrorCode::RevConflict,
                format!("the queue is at revision {}, not {seen}", self.rev),
            )),
            _ => Ok(()),
        }
    }

    fn entries_of(&mut self, tracks: Vec<Track>) -> Vec<Entry> {
        tracks
            .into_iter()
            .map(|track| {
                let entry = self.next_id;
                self.next_id += 1;
                Entry { entry, track }
            })
            .collect()
    }

    /// Replaces everything and makes `start` (clamped) the current entry.
    pub fn replace(&mut self, tracks: Vec<Track>, start: usize) {
        self.items = self.entries_of(tracks);
        self.current = self.items.get(start.min(self.items.len().saturating_sub(1))).map(|e| e.entry);
        self.rev += 1;
        self.rebuild_order();
    }

    /// Adds after the current entry when `next`, else at the end. An empty queue makes the
    /// first new entry current, so `resume` has something to play, but nothing starts.
    pub fn add(&mut self, tracks: Vec<Track>, next: bool) -> Vec<EntryId> {
        let added = self.entries_of(tracks);
        let ids: Vec<EntryId> = added.iter().map(|e| e.entry).collect();
        if ids.is_empty() {
            return ids;
        }

        let at = match (next, self.current) {
            (true, Some(current)) => self.items.iter().position(|e| e.entry == current).map(|i| i + 1),
            _ => None,
        }
        .unwrap_or(self.items.len());
        self.items.splice(at..at, added);

        if self.current.is_none() {
            self.current = ids.first().copied();
        }
        self.rev += 1;

        if self.shuffle {
            let at = match (next, self.current) {
                (true, Some(current)) => self.order.iter().position(|id| *id == current).map(|i| i + 1),
                _ => None,
            }
            .unwrap_or(self.order.len());
            self.order.splice(at..at, ids.iter().copied());
        } else {
            self.rebuild_order();
        }
        ids
    }

    /// Removes an entry. Answers whether the current entry went with it; the new current is
    /// the one that would have played next, or none when nothing is left to play.
    pub fn remove(&mut self, id: EntryId) -> Result<bool, ProtocolError> {
        if self.get(id).is_none() {
            return Err(ProtocolError::new(ErrorCode::NotFound, format!("no entry {id} in the queue")));
        }

        let was_current = self.current == Some(id);
        if was_current {
            self.current = self.successor(id);
        }
        self.items.retain(|entry| entry.entry != id);
        self.order.retain(|entry| *entry != id);
        self.rev += 1;
        Ok(was_current)
    }

    /// The entry after `id` in play order, wrapping only under repeat-all, and never `id`.
    fn successor(&self, id: EntryId) -> Option<EntryId> {
        let at = self.order.iter().position(|entry| *entry == id)?;
        match self.order.get(at + 1) {
            Some(next) => Some(*next),
            None if self.repeat == RepeatMode::All => self.order.first().copied().filter(|first| *first != id),
            None => None,
        }
    }

    pub fn move_entry(&mut self, id: EntryId, before: Option<EntryId>) -> Result<(), ProtocolError> {
        let from = self
            .items
            .iter()
            .position(|entry| entry.entry == id)
            .ok_or_else(|| ProtocolError::new(ErrorCode::NotFound, format!("no entry {id} in the queue")))?;
        if let Some(before) = before
            && self.get(before).is_none()
        {
            return Err(ProtocolError::new(ErrorCode::NotFound, format!("no entry {before} in the queue")));
        }

        let moved = self.items.remove(from);
        let to = match before {
            Some(before) => self.items.iter().position(|entry| entry.entry == before).unwrap_or(self.items.len()),
            None => self.items.len(),
        };
        self.items.insert(to, moved);
        self.rev += 1;

        // A shuffled walk keeps its order: moving an entry is a statement about the list.
        if !self.shuffle {
            self.rebuild_order();
        }
        Ok(())
    }

    /// Drops everything but the current entry.
    pub fn clear(&mut self) {
        let keep = self.current;
        self.items.retain(|entry| Some(entry.entry) == keep);
        self.order.retain(|entry| Some(*entry) == keep);
        self.rev += 1;
    }

    pub fn set_shuffle(&mut self, on: bool) {
        self.shuffle = on;
        self.rebuild_order();
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    /// Current entry first, then the rest shuffled; or the list as built.
    fn rebuild_order(&mut self) {
        let mut order: Vec<EntryId> = self.items.iter().map(|entry| entry.entry).collect();
        if self.shuffle {
            order.retain(|id| Some(*id) != self.current);
            order.shuffle(&mut self.rng);
            if let Some(current) = self.current {
                order.insert(0, current);
            }
        }
        self.order = order;
    }

    /// Moves to the next or the previous entry and answers it, or `None` at the end of the
    /// queue (current stays). `auto` is a track ending by itself: under repeat-one that is the
    /// same entry again, where a user pressing next gets the following one.
    pub fn advance(&mut self, forward: bool, auto: bool) -> Option<EntryId> {
        let current = self.current?;
        if forward && auto && self.repeat == RepeatMode::One {
            return Some(current);
        }

        let at = self.order.iter().position(|id| *id == current)?;
        let target = if forward {
            match self.order.get(at + 1) {
                Some(next) => Some(*next),
                None if self.repeat == RepeatMode::All => {
                    if self.shuffle {
                        self.rebuild_order_keeping_last(current);
                    }
                    self.order.first().copied()
                }
                None => None,
            }
        } else if at > 0 {
            self.order.get(at - 1).copied()
        } else if self.repeat == RepeatMode::All {
            self.order.last().copied()
        } else {
            None
        };

        if let Some(target) = target {
            self.current = Some(target);
        }
        target
    }

    /// A new lap of a shuffled queue must not open with the entry that closed the last one.
    fn rebuild_order_keeping_last(&mut self, last: EntryId) {
        self.rebuild_order();
        if self.order.len() > 1 && self.order.first() == Some(&last) {
            let swap = self.order.len() - 1;
            self.order.swap(0, swap);
        }
    }

    /// The entry a gapless join would continue with, for the output to prepare.
    pub fn peek_next(&self) -> Option<&Entry> {
        let current = self.current?;
        if self.repeat == RepeatMode::One {
            return None;
        }
        let next = self.successor(current)?;
        self.get(next)
    }
}
