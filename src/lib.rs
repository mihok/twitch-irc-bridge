//! Bridges a Twitch channel's chat with an IRC channel while the stream is live.
//! See DESIGN.md for the design.

pub mod bridge;
pub mod config;
pub mod irc;
pub mod lifecycle;
pub mod moderation;
pub mod naming;
pub mod puppets;
pub mod ratelimit;
pub mod relay;
pub mod twitch;
