//! Everything that talks to Spotify: login, the library and catalog, the
//! built-in speaker, lyrics and the MPRIS bridge.

mod auth;
mod catalog;
mod library;
#[cfg(test)]
mod live_tests;
mod lyrics;
mod model;
pub mod mpris;
pub mod queue;
mod speaker;

pub use auth::{Authorization, Token, forget, stored_refresh_token};
pub use catalog::{Catalog, search_uri};
pub use lyrics::Lyrics;
pub use model::{Album, Artist, NowTrack, Playlist, Repeat, Track};
pub use speaker::{Command, Connection, DEVICE_NAME, Mode, Play, Speaker, now_track};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Cannot reach Spotify; check your network connection ({0})")]
    Network(#[from] reqwest::Error),
    #[error("Spotify: {0}")]
    Librespot(#[from] librespot_core::Error),
    #[error("Cannot read Spotify's response ({0})")]
    Protobuf(#[from] protobuf::Error),
    #[error("Cannot read Spotify's response ({0})")]
    Json(#[from] serde_json::Error),
    #[error("Cannot read Spotify's response ({0})")]
    Response(String),
    #[error("Spotify has no lyrics for this track, and LRCLIB (the fallback) didn't answer ({0})")]
    Lrclib(reqwest::Error),
    #[error("Your Spotify login expired or was revoked; log in again")]
    Unauthorized,
    #[error("Login failed: {0}")]
    Callback(String),
    #[error("Login failed while talking to the browser: {0}")]
    Browser(#[from] std::io::Error),
    #[error("Cannot use the system keyring for your login ({0})")]
    Keyring(#[from] keyring::Error),
    #[error("No audio output is available on this computer")]
    NoAudio,
    #[error("A Spotify request stopped unexpectedly ({0})")]
    Stopped(#[from] tokio::task::JoinError),
}
