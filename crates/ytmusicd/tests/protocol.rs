use serde_json::json;
use ytmusicd::protocol::{Command, Event, Request, State, Status};

fn parse(text: &str) -> Result<Request, serde_json::Error> {
    serde_json::from_str(text)
}

#[test]
fn a_request_carries_its_id_beside_the_command() {
    let request = parse(r#"{"id":7,"cmd":"seek","args":{"to":93000}}"#).unwrap();
    assert_eq!(request.id, 7);
    let Command::Seek(args) = request.command else { panic!("not a seek") };
    assert_eq!((args.to, args.by), (Some(93_000), None));
}

#[test]
fn a_command_without_arguments_needs_no_args_field() {
    for (text, expected) in [
        (r#"{"id":1,"cmd":"pause"}"#, Command::Pause),
        (r#"{"id":2,"cmd":"queue.clear"}"#, Command::QueueClear),
        (r#"{"id":3,"cmd":"state"}"#, Command::State),
    ] {
        assert_eq!(parse(text).unwrap().command, expected);
    }
}

#[test]
fn queue_commands_use_dotted_names_and_optional_revision() {
    let request = parse(r#"{"id":4,"cmd":"queue.add","args":{"tracks":[],"next":true,"if_rev":9}}"#).unwrap();
    let Command::QueueAdd(args) = request.command else { panic!("not queue.add") };
    assert!(args.next);
    assert_eq!(args.if_rev, Some(9));

    let request = parse(r#"{"id":5,"cmd":"queue.remove","args":{"entry":3}}"#).unwrap();
    let Command::QueueRemove(args) = request.command else { panic!("not queue.remove") };
    assert_eq!((args.entry, args.if_rev), (3, None));
}

#[test]
fn an_unknown_command_is_refused() {
    assert!(parse(r#"{"id":1,"cmd":"explode"}"#).is_err());
    assert!(parse(r#"{"cmd":"pause"}"#).is_err(), "a request without an id has no reply to go to");
}

#[test]
fn state_is_the_shape_clients_are_told_to_expect() {
    let state: State = serde_json::from_value(json!({
        "rev": 3, "status": "playing", "entry": 2,
        "clock": {"position_ms": 1000, "at_ms": 1700000000000u64, "rate": 1.0},
        "volume": 80, "muted": false, "shuffle": true, "repeat": "all", "queue_rev": 5
    }))
    .unwrap();
    assert_eq!(state.status, Status::Playing);

    let stopped = State { entry: None, status: Status::Stopped, ..state };
    let value = serde_json::to_value(&stopped).unwrap();
    assert!(value.get("entry").is_none(), "an absent field is absent, never null");
    assert_eq!(value["status"], "stopped");
    assert_eq!(value["repeat"], "all");
}

#[test]
fn events_are_tagged_by_name() {
    let notice = serde_json::to_value(Event::Notice { message: "x".into() }).unwrap();
    assert_eq!(notice, json!({"event": "notice", "message": "x"}));
}

#[test]
fn the_socket_lives_where_every_client_will_look() {
    use std::ffi::OsString;
    use ytmusicd::socket_path;

    let os = |s: &str| Some(OsString::from(s));
    assert_eq!(socket_path(os("/run/user/1000"), os("/home/u")).to_str(), Some("/run/user/1000/ytmusic/ytmusicd.sock"));
    assert_eq!(socket_path(os(""), os("/home/u")).to_str(), Some("/home/u/.cache/ytmusic/ytmusicd.sock"), "an empty runtime dir is no runtime dir");
    assert_eq!(socket_path(None, None).to_str(), Some("/tmp/ytmusic/ytmusicd.sock"));
}

/// Copied from what `ytmusic-cli`'s `track_json` prints: the shape clients already hold.
const CLI_TRACK: &str = r#"{
    "videoId": "dQw4w9WgXcQ", "title": "Never Gonna Give You Up",
    "artists": [{"name": "Rick Astley", "id": "UCuAXFkgsw1L7xaCfnd5JJOw"}],
    "album": {"name": "Whenever You Need Somebody", "id": "MPREb_x"},
    "duration": 213,
    "thumbnails": [{"url": "https://i.ytimg.com/a.jpg", "width": 60, "height": 60}],
    "explicit": false, "kind": "song"
}"#;

#[test]
fn a_track_as_the_browse_commands_print_it_is_a_track_as_play_takes_it() {
    use ytmusicd::protocol::{Track, TrackKind};

    let track: Track = serde_json::from_str(CLI_TRACK).unwrap();
    assert_eq!(track.video_id.as_deref(), Some("dQw4w9WgXcQ"));
    assert_eq!(track.duration, Some(213), "seconds, as the CLI prints them");
    assert_eq!(track.kind, TrackKind::Song);
    assert!(track.available, "the CLI does not print `available`: a track it prints is playable");

    let request = format!(r#"{{"id":1,"cmd":"play","args":{{"tracks":[{CLI_TRACK}]}}}}"#);
    assert!(matches!(parse(&request).unwrap().command, Command::Play(args) if args.tracks.len() == 1));
}

#[test]
fn a_track_needs_only_a_title_and_is_written_back_without_nulls() {
    use ytmusicd::protocol::Track;

    let track: Track = serde_json::from_str(r#"{"title":"T","album":null,"duration":null}"#).unwrap();
    assert_eq!(track.video_id, None);
    let written = serde_json::to_value(&track).unwrap();
    assert!(written.get("videoId").is_none() && written.get("album").is_none() && written.get("duration").is_none());
    assert_eq!(written["kind"], "song");
}
