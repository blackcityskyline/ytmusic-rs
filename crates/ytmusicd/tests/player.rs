mod common;

use common::{Call, FakeOutput, track, tracks};
use ytmusicd::player::{Outcome, OutputEvent, Player};
use ytmusicd::protocol::{
    Command, ErrorCode, Event, MuteArgs, PlayArgs, QueueAddArgs, QueueJumpArgs, QueueRemoveArgs, RepeatArgs, RepeatMode, SeekArgs, Status, Track,
    VolumeArgs,
};

const T0: u64 = 1_000_000;

fn player() -> Player<FakeOutput> {
    Player::new(FakeOutput::default(), 1)
}

fn play(player: &mut Player<FakeOutput>, list: Vec<Track>, now: u64) -> Outcome {
    player.handle(Command::Play(PlayArgs { tracks: list, index: 0, paused: false }), now).unwrap()
}

/// Playing and audible: what a client sees once the output answers.
fn playing(count: usize) -> Player<FakeOutput> {
    let mut player = player();
    play(&mut player, tracks(count), T0);
    player.on_output(OutputEvent::Started { position_ms: 0 }, T0);
    player
}

fn current(player: &Player<FakeOutput>) -> Option<u64> {
    player.state().entry
}

fn states(events: &[Event]) -> usize {
    events.iter().filter(|event| matches!(event, Event::State(_))).count()
}

#[test]
fn play_loads_the_first_entry_and_waits_for_the_output() {
    let mut player = player();
    play(&mut player, tracks(2), T0);

    assert_eq!(player.output().calls[0], Call::Load { id: "v0".into(), start_ms: 0, paused: false });
    assert_eq!(player.state().status, Status::Loading);
    assert_eq!(player.state().clock.rate, 0.0, "nothing is moving until the output says so");

    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 100);
    assert_eq!(player.state().status, Status::Playing);
    assert_eq!(player.state().clock.rate, 1.0);
    assert_eq!(player.state().clock.at_ms, T0 + 100);
}

#[test]
fn the_position_is_an_anchor_that_pause_freezes_and_resume_moves_again() {
    let mut player = playing(1);
    assert_eq!(player.position(T0 + 2_000), 2_000);

    player.handle(Command::Pause, T0 + 2_000).unwrap();
    assert_eq!(player.state().status, Status::Paused);
    assert_eq!(player.position(T0 + 9_000), 2_000, "a paused track stays where it was");

    player.handle(Command::Resume, T0 + 9_000).unwrap();
    assert_eq!(player.position(T0 + 10_000), 3_000);
    assert_eq!(player.output().calls.iter().filter(|c| matches!(c, Call::Pause(_))).count(), 2);
}

#[test]
fn a_track_loaded_paused_stays_paused_when_it_starts() {
    let mut player = player();
    player.handle(Command::Play(PlayArgs { tracks: tracks(1), index: 0, paused: true }), T0).unwrap();
    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 50);
    assert_eq!(player.state().status, Status::Paused);
    assert_eq!(player.state().clock.rate, 0.0);
}

#[test]
fn tracks_that_cannot_be_played_are_left_out_and_the_index_follows() {
    let mut player = player();
    let mut dead = track("dead");
    dead.available = false;
    let mut no_id = track("x");
    no_id.video_id = None;

    play_at(&mut player, vec![dead.clone(), no_id.clone(), track("a"), track("b")], 3);
    assert_eq!(player.output().last_load(), Some("b"), "index 3 was `b` before the others went");
    assert!(player.state().entry.is_some());

    let mut empty = Player::new(FakeOutput::default(), 1);
    let error = empty.handle(Command::Play(PlayArgs { tracks: vec![dead, no_id], index: 0, paused: false }), T0).unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(empty.output().calls.is_empty(), "nothing playable, nothing asked of the output");
}

fn play_at(player: &mut Player<FakeOutput>, list: Vec<Track>, index: usize) {
    player.handle(Command::Play(PlayArgs { tracks: list, index, paused: false }), T0).unwrap();
}

#[test]
fn seek_clamps_to_the_track_and_wants_exactly_one_of_to_and_by() {
    let mut player = playing(1);

    player.handle(Command::Seek(SeekArgs { to: Some(999_999), by: None }), T0 + 1_000).unwrap();
    assert_eq!(player.state().clock.position_ms, 200_000, "the fake track is 200 s long");
    assert_eq!(player.output().calls.last(), Some(&Call::Seek(200_000)));

    player.handle(Command::Seek(SeekArgs { to: Some(2_000), by: None }), T0 + 1_000).unwrap();
    player.handle(Command::Seek(SeekArgs { to: None, by: Some(-5_000) }), T0 + 1_000).unwrap();
    assert_eq!(player.state().clock.position_ms, 0, "never before the start");

    for args in [SeekArgs::default(), SeekArgs { to: Some(1), by: Some(1) }] {
        assert_eq!(player.handle(Command::Seek(args), T0).unwrap_err().code, ErrorCode::BadRequest);
    }
}

#[test]
fn seeking_with_nothing_playing_is_refused() {
    let mut player = player();
    let error = player.handle(Command::Seek(SeekArgs { to: Some(1), by: None }), T0).unwrap_err();
    assert_eq!(error.code, ErrorCode::BadRequest);
}

#[test]
fn a_track_that_ends_moves_to_the_next_and_the_last_one_stops_the_player() {
    let mut player = playing(2);
    player.on_output(OutputEvent::Ended, T0 + 1);
    assert_eq!(player.output().last_load(), Some("v1"));
    assert_eq!(player.state().status, Status::Loading);

    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 2);
    player.on_output(OutputEvent::Ended, T0 + 3);
    assert_eq!(player.state().status, Status::Stopped);
    assert_eq!(player.output().calls.last(), Some(&Call::Stop));
    assert_eq!(player.output().loads(), 2, "the end of the queue loads nothing");
}

#[test]
fn repeat_one_plays_the_same_entry_again_when_it_ends() {
    let mut player = playing(2);
    player.handle(Command::Repeat(RepeatArgs { mode: RepeatMode::One }), T0).unwrap();
    let entry = current(&player);

    player.on_output(OutputEvent::Ended, T0 + 1);
    assert_eq!(current(&player), entry);
    assert_eq!(player.output().loads(), 2);
    assert_eq!(player.output().last_load(), Some("v0"));
}

#[test]
fn next_past_the_end_stops_and_previous_restarts_a_track_that_is_well_underway() {
    let mut player = playing(2);
    player.handle(Command::Next, T0 + 10).unwrap();
    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 20);
    let second = current(&player);

    player.handle(Command::Prev, T0 + 20 + 5_000).unwrap();
    assert_eq!(current(&player), second, "five seconds in: previous restarts");
    assert_eq!(player.output().calls.last(), Some(&Call::Seek(0)));
    assert_eq!(player.output().loads(), 2);

    player.handle(Command::Prev, T0 + 20 + 5_100).unwrap();
    assert_eq!(player.output().last_load(), Some("v0"), "right after the start: previous goes back");

    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 30_000);
    player.handle(Command::Next, T0 + 30_001).unwrap();
    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 30_002);
    player.handle(Command::Next, T0 + 30_003).unwrap();
    assert_eq!(player.state().status, Status::Stopped, "next on the last entry stops");
}

#[test]
fn a_failing_entry_is_skipped_and_a_queue_of_failures_stops_instead_of_spinning() {
    let mut player = playing(3);

    let events = player.on_output(OutputEvent::Failed { reason: "gone".into() }, T0 + 1);
    assert!(matches!(&events[0], Event::Notice { message } if message == "gone"));
    assert_eq!(player.output().last_load(), Some("v1"));

    player.on_output(OutputEvent::Failed { reason: "gone".into() }, T0 + 2);
    assert_eq!(player.output().last_load(), Some("v2"));
    player.on_output(OutputEvent::Failed { reason: "gone".into() }, T0 + 3);

    assert_eq!(player.state().status, Status::Stopped);
    assert_eq!(player.output().loads(), 3, "each entry was tried once");
}

#[test]
fn under_repeat_all_a_queue_of_dead_entries_still_stops() {
    let mut player = playing(3);
    player.handle(Command::Repeat(RepeatArgs { mode: RepeatMode::All }), T0).unwrap();

    for tick in 1..=3 {
        player.on_output(OutputEvent::Failed { reason: "gone".into() }, T0 + tick);
    }
    assert_eq!(player.state().status, Status::Stopped, "wrapping around must not retry the same dead entries forever");
    assert_eq!(player.output().loads(), 3);
}

#[test]
fn a_success_clears_the_count_of_failures() {
    let mut player = playing(3);
    player.on_output(OutputEvent::Failed { reason: "x".into() }, T0 + 1);
    player.on_output(OutputEvent::Started { position_ms: 0 }, T0 + 2);
    player.on_output(OutputEvent::Failed { reason: "x".into() }, T0 + 3);
    assert_eq!(player.state().status, Status::Loading, "two failures apart do not add up to a dead queue");
}

#[test]
fn removing_the_playing_entry_loads_its_successor_and_removing_the_only_one_stops() {
    let mut player = playing(2);
    let first = current(&player).unwrap();
    player.handle(Command::QueueRemove(QueueRemoveArgs { entry: first, if_rev: None }), T0 + 1).unwrap();
    assert_eq!(player.output().last_load(), Some("v1"));

    let only = current(&player).unwrap();
    player.handle(Command::QueueRemove(QueueRemoveArgs { entry: only, if_rev: None }), T0 + 2).unwrap();
    assert_eq!(player.state().status, Status::Stopped);
    assert_eq!(player.state().entry, None);
}

#[test]
fn an_edit_against_a_stale_queue_changes_nothing_and_says_nothing() {
    let mut player = playing(2);
    let stale = player.state().queue_rev - 1;
    let calls = player.output().calls.len();

    let error = player.handle(Command::QueueAdd(QueueAddArgs { tracks: tracks(1), next: false, if_rev: Some(stale) }), T0).unwrap_err();
    assert_eq!(error.code, ErrorCode::RevConflict);
    assert_eq!(player.output().calls.len(), calls);
    assert_eq!(player.handle(Command::QueueGet, T0).unwrap().data["entries"].as_array().unwrap().len(), 2);
}

#[test]
fn only_a_change_is_announced_and_each_announcement_has_a_new_revision() {
    let mut player = player();
    assert!(player.handle(Command::Pause, T0).unwrap().events.is_empty(), "pausing a stopped player is nothing");
    assert!(player.handle(Command::State, T0).unwrap().events.is_empty());
    assert!(player.handle(Command::QueueGet, T0).unwrap().events.is_empty());

    let started = play(&mut player, tracks(2), T0);
    assert!(started.events.iter().any(|e| matches!(e, Event::Queue(_))), "a new queue is announced");
    assert_eq!(states(&started.events), 1);
    let first_rev = player.state().rev;

    let added = player.handle(Command::QueueAdd(QueueAddArgs { tracks: tracks(1), next: false, if_rev: None }), T0 + 1).unwrap();
    assert!(added.events.iter().any(|e| matches!(e, Event::Queue(_))));
    assert_eq!(states(&added.events), 1, "queue_rev is part of the state");
    assert!(player.state().rev > first_rev);

    let paused = player.handle(Command::Pause, T0 + 2).unwrap();
    assert!(paused.events.iter().all(|e| !matches!(e, Event::Queue(_))), "a pause does not resend the queue");
}

#[test]
fn volume_is_kept_between_zero_and_a_hundred_and_mute_toggles() {
    let mut player = player();
    player.handle(Command::Volume(VolumeArgs { set: Some(250), by: None }), T0).unwrap();
    assert_eq!(player.state().volume, 100, "a value past the top is the top, not an error");
    assert_eq!(player.handle(Command::Volume(VolumeArgs::default()), T0).unwrap_err().code, ErrorCode::BadRequest);

    player.handle(Command::Volume(VolumeArgs { set: None, by: Some(500) }), T0).unwrap();
    assert_eq!(player.state().volume, 100);
    player.handle(Command::Volume(VolumeArgs { set: None, by: Some(-30) }), T0).unwrap();
    assert_eq!(player.state().volume, 70);
    player.handle(Command::Volume(VolumeArgs { set: None, by: Some(-500) }), T0).unwrap();
    assert_eq!(player.state().volume, 0);

    player.handle(Command::Mute(MuteArgs::default()), T0).unwrap();
    assert!(player.state().muted);
    assert_eq!(player.output().calls.last(), Some(&Call::Volume(0, true)));
    player.handle(Command::Mute(MuteArgs { on: Some(false) }), T0).unwrap();
    assert!(!player.state().muted);
}

#[test]
fn the_output_is_told_which_entry_follows_so_it_can_prepare_a_gapless_join() {
    let mut player = playing(2);
    assert_eq!(player.output().last_preload(), Some(Some("v1")));

    player.handle(Command::Next, T0 + 1).unwrap();
    assert_eq!(player.output().last_preload(), Some(None), "nothing follows the last entry");

    player.handle(Command::QueueAdd(QueueAddArgs { tracks: vec![track("late")], next: false, if_rev: None }), T0 + 2).unwrap();
    assert_eq!(player.output().last_preload(), Some(Some("late")), "an entry added later is announced");
}

#[test]
fn toggle_pauses_resumes_and_restarts_a_stopped_player() {
    let mut player = playing(1);
    player.handle(Command::Toggle, T0 + 1).unwrap();
    assert_eq!(player.state().status, Status::Paused);
    player.handle(Command::Toggle, T0 + 2).unwrap();
    assert_eq!(player.state().status, Status::Playing);

    player.handle(Command::Stop, T0 + 3).unwrap();
    assert_eq!(player.state().status, Status::Stopped);
    assert!(player.state().entry.is_some(), "stopping keeps the entry, so resume has something to play");
    player.handle(Command::Toggle, T0 + 4).unwrap();
    assert_eq!(player.state().status, Status::Loading);
    assert_eq!(player.output().loads(), 2);
}

#[test]
fn a_jump_the_output_reports_moves_the_anchor() {
    let mut player = playing(1);
    player.on_output(OutputEvent::Discontinuity { position_ms: 42_000 }, T0 + 5_000);
    assert_eq!(player.position(T0 + 6_000), 43_000);
}

#[test]
fn jumping_to_an_entry_plays_it_even_when_paused_and_leaves_the_queue_alone() {
    let mut player = playing(4);
    let listed = player.handle(Command::QueueGet, T0).unwrap().data;
    let third = listed["entries"][2]["entry"].as_u64().unwrap();
    player.handle(Command::Pause, T0 + 1).unwrap();
    let rev = player.state().queue_rev;

    player.handle(Command::QueueJump(QueueJumpArgs { entry: third }), T0 + 2).unwrap();
    assert_eq!(current(&player), Some(third));
    assert_eq!(player.output().last_load(), Some("v2"));
    assert!(matches!(player.output().calls.iter().rev().find(|c| matches!(c, Call::Load { .. })), Some(Call::Load { paused: false, .. })), "a jump resumes");
    assert_eq!(player.state().queue_rev, rev, "nothing was edited");

    let error = player.handle(Command::QueueJump(QueueJumpArgs { entry: 999 }), T0 + 3).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    assert_eq!(current(&player), Some(third), "a refused jump changes nothing");
}
