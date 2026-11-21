mod common;

use common::{track, tracks};
use ytmusicd::protocol::{ErrorCode, RepeatMode};
use ytmusicd::queue::Queue;

fn queue_of(count: usize) -> Queue {
    let mut queue = Queue::new(7);
    queue.replace(tracks(count), 0);
    queue
}

fn ids(queue: &Queue) -> Vec<String> {
    queue.snapshot().entries.iter().map(|e| e.track.video_id.clone().unwrap()).collect()
}

fn current(queue: &Queue) -> String {
    queue.current_entry().unwrap().track.video_id.clone().unwrap()
}

#[test]
fn replace_makes_the_chosen_entry_current_and_clamps_the_index() {
    let mut queue = Queue::new(1);
    queue.replace(tracks(3), 2);
    assert_eq!(current(&queue), "v2");
    queue.replace(tracks(3), 99);
    assert_eq!(current(&queue), "v2", "an index past the end lands on the last entry");
}

#[test]
fn entry_ids_are_never_reused() {
    let mut queue = queue_of(2);
    let first: Vec<_> = queue.snapshot().entries.iter().map(|e| e.entry).collect();
    queue.replace(tracks(2), 0);
    let second: Vec<_> = queue.snapshot().entries.iter().map(|e| e.entry).collect();
    assert!(first.iter().all(|id| !second.contains(id)));
}

#[test]
fn add_goes_after_the_current_entry_or_to_the_end() {
    let mut queue = queue_of(3);
    queue.add(vec![track("n1")], true);
    assert_eq!(ids(&queue), ["v0", "n1", "v1", "v2"]);
    queue.add(vec![track("e1")], false);
    assert_eq!(ids(&queue), ["v0", "n1", "v1", "v2", "e1"]);
}

#[test]
fn adding_to_an_empty_queue_makes_the_first_entry_current() {
    let mut queue = Queue::new(1);
    assert_eq!(queue.current(), None);
    queue.add(vec![track("a"), track("b")], false);
    assert_eq!(current(&queue), "a");
}

#[test]
fn an_edit_against_a_stale_revision_is_refused() {
    let mut queue = queue_of(2);
    let seen = queue.rev();
    queue.add(vec![track("x")], false);
    assert_eq!(queue.check(Some(seen)).unwrap_err().code, ErrorCode::RevConflict);
    assert!(queue.check(Some(queue.rev())).is_ok());
    assert!(queue.check(None).is_ok(), "without a revision the last writer wins");
}

#[test]
fn moving_to_another_entry_does_not_change_the_revision() {
    let mut queue = queue_of(3);
    let rev = queue.rev();
    queue.advance(true, false);
    assert_eq!(queue.rev(), rev);
}

#[test]
fn removing_the_current_entry_hands_over_to_the_next_one() {
    let mut queue = queue_of(3);
    let first = queue.current().unwrap();
    assert!(queue.remove(first).unwrap(), "the current entry went");
    assert_eq!(current(&queue), "v1");
    assert_eq!(ids(&queue), ["v1", "v2"]);
}

#[test]
fn removing_the_last_current_entry_leaves_nothing_to_play() {
    let mut queue = queue_of(1);
    let only = queue.current().unwrap();
    queue.remove(only).unwrap();
    assert_eq!(queue.current(), None);
}

#[test]
fn removing_an_entry_that_is_not_there_is_not_found() {
    let mut queue = queue_of(1);
    assert_eq!(queue.remove(999).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn move_lands_in_front_of_the_named_entry_or_at_the_end() {
    let mut queue = queue_of(4);
    let entries = queue.snapshot().entries;
    queue.move_entry(entries[3].entry, Some(entries[1].entry)).unwrap();
    assert_eq!(ids(&queue), ["v0", "v3", "v1", "v2"]);
    queue.move_entry(entries[0].entry, None).unwrap();
    assert_eq!(ids(&queue), ["v3", "v1", "v2", "v0"]);
    assert_eq!(queue.move_entry(entries[0].entry, Some(999)).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn playback_follows_a_moved_entry_when_not_shuffled() {
    let mut queue = queue_of(3);
    let entries = queue.snapshot().entries;
    queue.move_entry(entries[2].entry, Some(entries[1].entry)).unwrap();
    queue.advance(true, false);
    assert_eq!(current(&queue), "v2");
}

#[test]
fn clear_keeps_only_the_current_entry() {
    let mut queue = queue_of(3);
    queue.advance(true, false);
    queue.clear();
    assert_eq!(ids(&queue), ["v1"]);
    assert_eq!(current(&queue), "v1");
}

#[test]
fn advancing_past_the_end_answers_none_and_stays_put() {
    let mut queue = queue_of(2);
    assert!(queue.advance(true, false).is_some());
    assert_eq!(queue.advance(true, false), None);
    assert_eq!(current(&queue), "v1");
}

#[test]
fn repeat_all_wraps_in_both_directions() {
    let mut queue = queue_of(2);
    queue.set_repeat(RepeatMode::All);
    queue.advance(true, false);
    queue.advance(true, false);
    assert_eq!(current(&queue), "v0");
    queue.advance(false, false);
    assert_eq!(current(&queue), "v1");
}

#[test]
fn repeat_one_replays_on_its_own_but_obeys_the_next_button() {
    let mut queue = queue_of(2);
    queue.set_repeat(RepeatMode::One);
    let same = queue.current();
    assert_eq!(queue.advance(true, true), same, "a track that ends repeats");
    assert_ne!(queue.advance(true, false), same, "next still moves on");
}

#[test]
fn previous_at_the_start_has_no_target_unless_repeating_all() {
    let mut queue = queue_of(2);
    assert_eq!(queue.advance(false, false), None);
    queue.set_repeat(RepeatMode::All);
    assert!(queue.advance(false, false).is_some());
    assert_eq!(current(&queue), "v1");
}

#[test]
fn shuffle_opens_with_the_current_entry_and_keeps_every_entry_once() {
    let mut queue = queue_of(20);
    queue.advance(true, false);
    let current_id = queue.current().unwrap();
    queue.set_shuffle(true);

    let mut seen = vec![current_id];
    while let Some(next) = queue.advance(true, false) {
        seen.push(next);
    }
    assert_eq!(seen[0], current_id);
    assert_eq!(seen.len(), 20, "a pass visits every entry");
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), 20, "and none twice");
}

#[test]
fn turning_shuffle_off_returns_to_the_order_the_user_built() {
    let mut queue = queue_of(10);
    queue.set_shuffle(true);
    queue.set_shuffle(false);
    queue.replace(tracks(10), 0);
    let mut order = vec![current(&queue)];
    while queue.advance(true, false).is_some() {
        order.push(current(&queue));
    }
    assert_eq!(order, ids(&queue));
}

#[test]
fn a_shuffled_new_lap_does_not_open_with_the_entry_that_closed_the_last_one() {
    let mut queue = queue_of(6);
    queue.set_repeat(RepeatMode::All);
    queue.set_shuffle(true);

    for lap in 0..40 {
        for _ in 0..5 {
            queue.advance(true, false);
        }
        let last = queue.current();
        queue.advance(true, false);
        assert_ne!(queue.current(), last, "lap {lap} opened with the entry that closed the one before");
    }
}

#[test]
fn peek_next_names_the_entry_a_gapless_join_continues_with() {
    let mut queue = queue_of(2);
    assert_eq!(queue.peek_next().unwrap().track.video_id.as_deref(), Some("v1"));
    queue.advance(true, false);
    assert!(queue.peek_next().is_none(), "nothing follows the last entry");
    queue.set_repeat(RepeatMode::All);
    assert_eq!(queue.peek_next().unwrap().track.video_id.as_deref(), Some("v0"));
    queue.set_repeat(RepeatMode::One);
    assert!(queue.peek_next().is_none(), "repeat-one has nothing to prepare");
}
