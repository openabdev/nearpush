//! nearpushd: receives events from the `nearpush` CLI on loopback and streams them
//! over WebSocket to a home-screen web page reached through `tailscale serve`.

pub mod assets;
pub mod classifier;
pub mod config;
pub mod control;
pub mod event;
pub mod main_card;
pub mod relay;
pub mod server;
pub mod sessions;
pub mod store;
pub mod summarizer;
