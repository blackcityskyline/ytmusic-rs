//! One JSON request on stdin, one JSON answer on stdout.
//!
//! The Vicinae launcher spawns this for every call — a page, a search, a like — and reads the
//! answer, so this file is the whole surface between that launcher and this crate. Cookies
//! arrive in the environment, never in argv: `YTMUSIC_COOKIES` for the `Cookie` header,
//! `YTMUSIC_AUTHUSER` for the account index, `YTMUSIC_CACHE` for the player cache directory.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::stream::{self, StreamExt as _};
use serde_json::{Value, json};

use ytmusic::nav::Nav as _;
use ytmusic::parse;
use ytmusic::{
    Album, AlbumKind, Client, Playlist, PlaylistDetail, Thumbnail, Track, TrackKind, YtMusic,
};

#[tokio::main]
async fn main() {
    let answer = match run().await {
        Ok(data) => json!({ "ok": true, "data": clean(data) }),
        Err(error) => json!({ "ok": false, "error": format!("{error:#}") }),
    };
    println!("{answer}");
}

async fn run() -> Result<Value> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading the request")?;
    let request: Value = serde_json::from_str(&input).context("the request is not JSON")?;
    let command = request
        .get("command")
        .and_then(Value::as_str)
        .context("the request names no command")?
        .to_string();
    let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
    let api = session();

    match command.as_str() {
        "album" => album(&api, &args).await,
        "artist" => artist(&api, &args).await,
        "discography" => discography(&api, &args).await,
        "playlist" => playlist(&api, &args).await,
        "playlistContinuation" => playlist_continuation(&api, &args).await,
        "playlistSummaries" => playlist_summaries(&api, &args).await,
        "search" => search(&api, &args).await,
        "shelves" => shelves_of(&api, &args).await,
        "library" => library(&api, &args).await,
        "like" => like(&api, &args).await,
        "likePlaylist" => like_playlist(&api, &args).await,
        "addTrack" => add_track(&api, &args).await,
        other => bail!("unknown command `{other}`"),
    }
}

// --- the session -----------------------------------------------------------

fn session() -> YtMusic {
    let api = match std::env::var("YTMUSIC_COOKIES") {
        Ok(cookies) if !cookies.trim().is_empty() => {
            let authuser = std::env::var("YTMUSIC_AUTHUSER")
                .ok()
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            YtMusic::with_cookies(cookies.trim()).as_user(authuser)
        }
        _ => YtMusic::anonymous(),
    };
    api.cache_player(cache_dir().join("player.json"))
}

fn cache_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("YTMUSIC_CACHE") {
        return PathBuf::from(path);
    }
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("ytmusic");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/ytmusic")
}

// --- the commands ----------------------------------------------------------

async fn album(api: &YtMusic, args: &Value) -> Result<Value> {
    let detail = api.album(&text(args, "browseId")?).await?;
    Ok(json!({
        "album": album_json(&detail.album),
        "description": detail.description,
        "tracks": detail.tracks.iter().map(track_json).collect::<Vec<_>>(),
    }))
}

async fn artist(api: &YtMusic, args: &Value) -> Result<Value> {
    let artist = api.artist(&text(args, "browseId")?).await?;
    Ok(json!({
        "browseId": artist.browse_id,
        "name": artist.name,
        "description": artist.description,
        "subscribers": artist.subscribers,
        "thumbnails": thumbs_json(&artist.thumbnails),
        "topTracks": artist.top_tracks.iter().map(track_json).collect::<Vec<_>>(),
        "albums": artist.albums.iter().map(album_json).collect::<Vec<_>>(),
        "singles": artist.singles.iter().map(album_json).collect::<Vec<_>>(),
    }))
}

async fn discography(api: &YtMusic, args: &Value) -> Result<Value> {
    let albums = api.discography(&text(args, "channelId")?).await?;
    Ok(json!(albums.iter().map(album_json).collect::<Vec<_>>()))
}

async fn playlist(api: &YtMusic, args: &Value) -> Result<Value> {
    let detail = api.playlist_page(&text(args, "playlistId")?).await?;
    Ok(json!({
        "playlist": playlist_json(&detail.playlist, card_duration(&detail)),
        "description": detail.description,
        "tracks": detail.tracks.iter().map(track_json).collect::<Vec<_>>(),
        "continuation": detail.continuation,
    }))
}

async fn playlist_continuation(api: &YtMusic, args: &Value) -> Result<Value> {
    let page = api
        .playlist_continuation(&text(args, "continuation")?)
        .await?;
    Ok(json!({
        "tracks": page.tracks.iter().map(track_json).collect::<Vec<_>>(),
        "continuation": page.continuation,
    }))
}

/// The playlists themselves in one process — the card fields of a list of rows — so no row has to
/// grow a `Duration` after the arrow key settles. Rows that could not be fetched are left out.
async fn playlist_summaries(api: &YtMusic, args: &Value) -> Result<Value> {
    let ids: Vec<String> = args
        .get("playlistIds")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // ponytail: 24 at a time is one row-list; a library larger than that waits in bursts
    let summaries = stream::iter(ids)
        .map(|id| async move {
            let detail = api.playlist_page(&id).await.ok()?;
            Some(playlist_json(&detail.playlist, card_duration(&detail)))
        })
        .buffer_unordered(24)
        .filter_map(|summary| async move { summary })
        .collect::<Vec<_>>()
        .await;
    Ok(Value::Array(summaries))
}

/// The length that comes with the page, so a card never has to grow a `Duration` row a second
/// after it is drawn: the tracks of a page that holds the whole playlist are the exact length
/// ("12 hr 10 min", not the header's rounded "12 hours"); a longer one keeps what its header
/// states, and a header that names only a count ("423 songs") states none.
fn card_duration(detail: &PlaylistDetail) -> Option<Duration> {
    if detail.continuation.is_some() {
        return detail.duration;
    }

    match sum_seconds(&detail.tracks) {
        0 => detail.duration,
        seconds => Some(Duration::from_secs(seconds)),
    }
}

fn sum_seconds(tracks: &[Track]) -> u64 {
    tracks
        .iter()
        .filter_map(|track| track.duration)
        .map(|duration| duration.as_secs())
        .sum()
}

async fn search(api: &YtMusic, args: &Value) -> Result<Value> {
    let query = text(args, "query")?;
    let params = match args.get("filter").and_then(Value::as_str).unwrap_or("all") {
        "songs" => Some("EgWKAQIIAWoMEA4QChADEAQQCRAF"),
        "videos" => Some("EgWKAQIQAWoMEA4QChADEAQQCRAF"),
        "albums" => Some("EgWKAQIYAWoMEA4QChADEAQQCRAF"),
        "artists" => Some("EgWKAQIgAWoMEA4QChADEAQQCRAF"),
        "playlists" => Some("EgWKAQIoAWoMEA4QChADEAQQCRAF"),
        _ => None,
    };

    let payload = match params {
        Some(params) => json!({ "query": query, "params": params }),
        None => json!({ "query": query }),
    };
    let response = api.execute("search", Client::Music, payload).await?;
    Ok(shelves_json(&response))
}

async fn shelves_of(api: &YtMusic, args: &Value) -> Result<Value> {
    let browse_id = text(args, "browseId")?;
    let payload = match args.get("params").and_then(Value::as_str) {
        Some(params) => json!({ "browseId": browse_id, "params": params }),
        None => json!({ "browseId": browse_id }),
    };
    let response = api.execute("browse", Client::Music, payload).await?;
    Ok(shelves_json(&response))
}

/// One page of a signed-in library page: the caller asks again with the continuation.
async fn library(api: &YtMusic, args: &Value) -> Result<Value> {
    let payload = match args.get("continuation").and_then(Value::as_str) {
        Some(token) => json!({ "continuation": token }),
        None => json!({ "browseId": text(args, "browseId")? }),
    };
    let response = api.execute("browse", Client::Music, payload).await?;
    Ok(json!({
        "items": items_of(&response),
        "continuation": continuation_of(&response),
    }))
}

async fn like(api: &YtMusic, args: &Value) -> Result<Value> {
    api.rate_track(&text(args, "videoId")?, flag(args, "liked"))
        .await?;
    Ok(Value::Null)
}

async fn like_playlist(api: &YtMusic, args: &Value) -> Result<Value> {
    api.rate_playlist(&text(args, "playlistId")?, flag(args, "saved"))
        .await?;
    Ok(Value::Null)
}

async fn add_track(api: &YtMusic, args: &Value) -> Result<Value> {
    api.add_playlist_track(&text(args, "playlistId")?, &text(args, "videoId")?)
        .await?;
    Ok(Value::Null)
}

// --- shelves ---------------------------------------------------------------

fn shelves_json(response: &Value) -> Value {
    Value::Array(shelves_of_response(response))
}

fn shelves_of_response(response: &Value) -> Vec<Value> {
    let mut shelves = Vec::new();
    let mut loose = Vec::new();
    walk_shelves(response, &mut shelves, &mut loose);
    shelves.extend(group_items(loose));
    shelves.retain(|shelf| {
        shelf
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
    });
    shelves
}

fn walk_shelves(node: &Value, shelves: &mut Vec<Value>, loose: &mut Vec<Value>) {
    match node {
        Value::Array(list) => {
            for item in list {
                walk_shelves(item, shelves, loose);
            }
        }
        Value::Object(map) => {
            if let Some(shelf) = map.get("musicCarouselShelfRenderer") {
                let title = shelf
                    .at(&["header", "musicCarouselShelfBasicHeaderRenderer", "title"])
                    .and_then(text_of);
                push_shelf(shelves, title, shelf.items(&["contents"]));
                return;
            }
            if let Some(shelf) = map.get("musicShelfRenderer") {
                push_shelf(shelves, shelf.at(&["title"]).and_then(text_of), shelf.items(&["contents"]));
                return;
            }
            if let Some(grid) = map.get("gridRenderer") {
                let title = grid
                    .at(&["header", "gridHeaderRenderer", "title"])
                    .and_then(text_of);
                push_shelf(shelves, title, grid.items(&["items"]));
                return;
            }
            for key in ["gridContinuation", "musicShelfContinuation"] {
                if let Some(continuation) = map.get(key) {
                    let items = match continuation.items(&["items"]).is_empty() {
                        true => continuation.items(&["contents"]),
                        false => continuation.items(&["items"]),
                    };
                    push_shelf(shelves, None, items);
                    return;
                }
            }
            if let Some(section) = map.get("itemSectionRenderer") {
                for content in section.items(&["contents"]) {
                    if let Some(item) = item_of(content) {
                        loose.push(item);
                    }
                }
                return;
            }
            for value in map.values() {
                walk_shelves(value, shelves, loose);
            }
        }
        _ => {}
    }
}

fn push_shelf(shelves: &mut Vec<Value>, title: Option<String>, contents: &[Value]) {
    let items: Vec<Value> = contents.iter().filter_map(item_of).collect();
    shelves.push(shelf_json(title, items));
}

fn shelf_json(title: Option<String>, items: Vec<Value>) -> Value {
    match title
        .map(|title| title.trim().to_string())
        .filter(|title| !title.is_empty())
    {
        Some(title) => json!({ "title": title, "items": items }),
        None => json!({ "items": items }),
    }
}

const GROUPS: [(&str, &str); 8] = [
    ("track", "Songs"),
    ("album", "Albums"),
    ("artist", "Artists"),
    ("playlist", "Playlists"),
    ("podcast", "Podcasts"),
    ("profile", "Profiles"),
    ("genre", "Genres"),
    ("page", "Pages"),
];

/// Unshelved rows of a page that lists one kind after another (`itemSectionRenderer`).
fn group_items(items: Vec<Value>) -> Vec<Value> {
    let mut shelves = Vec::new();
    for (key, title) in GROUPS {
        let bucket: Vec<Value> = items
            .iter()
            .filter(|item| item_kind(item) == key)
            .cloned()
            .collect();
        if !bucket.is_empty() {
            shelves.push(shelf_json(Some(title.to_string()), bucket));
        }
    }
    shelves
}

fn item_kind(item: &Value) -> &str {
    if item.get("track").is_some() {
        "track"
    } else if item.get("album").is_some() {
        "album"
    } else if item.get("playlist").is_some() {
        "playlist"
    } else {
        item.get("page")
            .and_then(|page| page.get("pageType"))
            .and_then(Value::as_str)
            .unwrap_or("page")
    }
}

fn item_of(node: &Value) -> Option<Value> {
    if let Some(renderer) = node.at(&["musicTwoRowItemRenderer"]) {
        if let Some(album) = parse::two_row_album(node) {
            return Some(json!({ "album": album_json(&album) }));
        }
        if let Some(playlist) = parse::two_row_playlist(node) {
            return Some(json!({ "playlist": playlist_json(&playlist, None) }));
        }
        if let Some(page) = page_of(renderer, renderer.run_text(&["title"])) {
            return Some(json!({ "page": page }));
        }
        if let Some(track) = parse::two_row_video(node) {
            return Some(json!({ "track": track_json(&track) }));
        }
        return None;
    }

    if let Some(renderer) = node.at(&["musicResponsiveListItemRenderer"]) {
        if let Some(track) = parse::list_item_track(node).filter(|track| track.video_id.is_some()) {
            return Some(json!({ "track": track_json(&track) }));
        }
        if let Some(album) = parse::list_item_album(node) {
            return Some(json!({ "album": album_json(&album) }));
        }
        if let Some(playlist) = parse::list_item_playlist(node) {
            return Some(json!({ "playlist": playlist_json(&playlist, None) }));
        }
        let title = renderer
            .at(&[
                "flexColumns",
                "0",
                "musicResponsiveListItemFlexColumnRenderer",
                "text",
            ])
            .and_then(text_of);
        if let Some(page) = page_of(renderer, title) {
            return Some(json!({ "page": page }));
        }
        return None;
    }

    if let Some(button) = node.at(&["musicNavigationButtonRenderer"]) {
        return genre_of(button).map(|page| json!({ "page": page }));
    }

    None
}

fn page_of(renderer: &Value, title: Option<String>) -> Option<Value> {
    let browse_id = renderer.str_at(&["navigationEndpoint", "browseEndpoint", "browseId"])?;
    let params = renderer.str_at(&["navigationEndpoint", "browseEndpoint", "params"]);
    let page_type = renderer
        .str_at(&[
            "navigationEndpoint",
            "browseEndpoint",
            "browseEndpointContextSupportedConfigs",
            "browseEndpointContextMusicConfig",
            "pageType",
        ])
        .map(|page_type| match page_type {
            "MUSIC_PAGE_TYPE_ARTIST" => "artist",
            "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE" => "podcast",
            "MUSIC_PAGE_TYPE_USER_CHANNEL" => "profile",
            _ => "page",
        })
        .unwrap_or(match browse_id.starts_with("UC") {
            true => "artist",
            false => "page",
        });

    let mut page = json!({
        "browseId": browse_id,
        "title": title.unwrap_or_else(|| browse_id.to_string()),
        "pageType": page_type,
        "thumbnails": thumbs_json(&parse::thumbnails(renderer)),
    });
    if let Some(params) = params {
        page["params"] = json!(params);
    }
    Some(page)
}

fn genre_of(button: &Value) -> Option<Value> {
    let title = button.run_text(&["buttonText"])?;
    let browse_id = button.str_at(&["clickCommand", "browseEndpoint", "browseId"])?;
    let params = button.str_at(&["clickCommand", "browseEndpoint", "params"]);

    let mut page = json!({
        "browseId": browse_id,
        "title": title,
        "pageType": "genre",
        "thumbnails": thumbs_json(&parse::thumbnails(button)),
    });
    if let Some(params) = params {
        page["params"] = json!(params);
    }
    Some(page)
}

fn continuation_of(response: &Value) -> Option<String> {
    for key in [
        "musicPlaylistShelfRenderer",
        "musicShelfRenderer",
        "musicPlaylistShelfContinuation",
        "musicShelfContinuation",
        "gridRenderer",
    ] {
        for node in parse::find_renderers(response, key) {
            if let Some(token) = parse::shelf_continuation(node) {
                return Some(token);
            }
        }
    }

    parse::find_renderers(response, "continuationItemRenderer")
        .into_iter()
        .find_map(|item| {
            item.str_at(&["continuationEndpoint", "continuationCommand", "token"])
                .map(str::to_string)
        })
}

fn items_of(response: &Value) -> Vec<Value> {
    shelves_of_response(response)
        .iter()
        .flat_map(|shelf| shelf["items"].as_array().cloned().unwrap_or_default())
        .collect()
}

// --- the shapes the launcher reads -----------------------------------------

fn track_json(track: &Track) -> Value {
    json!({
        "videoId": track.video_id,
        "title": track.title,
        "artists": track.artists.iter().map(|artist| json!({
            "name": artist.name,
            "id": artist.id,
        })).collect::<Vec<_>>(),
        "album": track.album.as_ref().map(|album| json!({
            "name": album.name,
            "id": album.id,
        })),
        "duration": track.duration.map(|duration| duration.as_secs()),
        "thumbnails": thumbs_json(&track.thumbnails),
        "explicit": track.explicit,
        "kind": match track.kind {
            TrackKind::Song => "song",
            TrackKind::Video => "video",
        },
    })
}

fn album_json(album: &Album) -> Value {
    json!({
        "browseId": album.browse_id,
        "playlistId": album.playlist_id,
        "title": album.title,
        "artists": album.artists.iter().map(|artist| json!({
            "name": artist.name,
            "id": artist.id,
        })).collect::<Vec<_>>(),
        "kind": match album.kind {
            AlbumKind::Album => "Album",
            AlbumKind::Single => "Single",
            AlbumKind::Ep => "EP",
            AlbumKind::Compilation => "Compilation",
        },
        "year": album.year,
        "trackCount": album.track_count,
        "thumbnails": thumbs_json(&album.thumbnails),
    })
}

fn playlist_json(playlist: &Playlist, duration: Option<Duration>) -> Value {
    json!({
        "id": playlist.id,
        "title": playlist.title,
        "author": playlist.author,
        "trackCount": playlist.track_count,
        "duration": duration.map(|duration| duration.as_secs()),
        "thumbnails": thumbs_json(&playlist.thumbnails),
    })
}

fn thumbs_json(thumbnails: &[Thumbnail]) -> Value {
    Value::Array(
        thumbnails
            .iter()
            .map(|thumbnail| {
                json!({
                    "url": thumbnail.url,
                    "width": thumbnail.width,
                    "height": thumbnail.height,
                })
            })
            .collect(),
    )
}

// --- plumbing --------------------------------------------------------------

fn text_of(node: &Value) -> Option<String> {
    node.run_text(&[])
}

fn text(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("the request has no `{key}`"))
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A field the launcher does not have is absent, not `null`: it types every one of them as
/// optional and tests them against `undefined`.
fn clean(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, clean(value)))
                .collect(),
        ),
        Value::Array(list) => Value::Array(list.into_iter().map(clean).collect()),
        other => other,
    }
}
