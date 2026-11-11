//! Store-and-forward relay for nearpush events.
//!
//! Producers (pods) POST events with a per-producer source token. The laptop
//! daemon reads them with the consumer token and acks a watermark. Events are
//! kept in one SQLite file until they are pruned by count or age.

pub mod auth;
pub mod config;
pub mod http;
pub mod store;
