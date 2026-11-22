//! The daemon's brain: the protocol types, the queue, and the player state machine that
//! drives an [`player::Output`]. Nothing here touches a socket, a clock or a sound card, so
//! every rule is tested without them. See `docs/protocol.md`.

pub mod data;
#[cfg(feature = "native")]
pub mod decode;
pub mod mpv;
pub mod player;
pub mod protocol;
pub mod queue;
pub mod server;
pub mod youtube;
pub mod spool;

use std::ffi::OsString;
use std::path::PathBuf;

/// Where the daemon listens and clients look: `$XDG_RUNTIME_DIR/ytmusic/ytmusicd.sock`, or
/// `~/.cache/ytmusic/` when there is no runtime directory. The same file for both, because a
/// client that looks elsewhere would start a second daemon.
pub fn socket_path(runtime_dir: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let base = match (runtime_dir.filter(|dir| !dir.is_empty()), home) {
        (Some(runtime), _) => PathBuf::from(runtime),
        (None, Some(home)) => PathBuf::from(home).join(".cache"),
        (None, None) => PathBuf::from("/tmp"),
    };
    base.join("ytmusic").join("ytmusicd.sock")
}
