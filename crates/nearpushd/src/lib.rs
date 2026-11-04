//! nearpushd: receives events from the `nearpush` CLI on loopback and streams them
//! over WebSocket to a home-screen web page reached through `tailscale serve`.

pub mod assets;
pub mod config;
pub mod event;
pub mod server;
pub mod store;
