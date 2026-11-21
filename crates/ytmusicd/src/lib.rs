//! The daemon's brain: the protocol types, the queue, and the player state machine that
//! drives an [`player::Output`]. Nothing here touches a socket, a clock or a sound card, so
//! every rule is tested without them. See `docs/protocol.md`.

pub mod player;
pub mod protocol;
pub mod queue;
