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
pub mod playing;
pub mod queue;
mod secrets;
mod speaker;

pub use auth::{Authorization, Token, forget, store_refresh_token, stored_refresh_token};
pub use catalog::{Catalog, search_uri};
pub use library::Liked;
pub use lyrics::Lyrics;
pub use model::{Album, Artist, NowTrack, Playlist, Repeat, Track};
pub use speaker::{Command, Connection, DEVICE_NAME, Mode, Play, Speaker};

use std::time::Duration;

/// How long a plain HTTP request may take to connect, and in all.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP client for everything outside librespot (login, LRCLIB, covers).
/// Without timeouts a request on a half-dead connection, as after waking from
/// sleep, could wait forever.
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_TIMEOUT)
        .build()
        // Only a broken TLS setup fails here; a client without timeouts still works.
        .unwrap_or_default()
}

/// A device ID that stays the same across reconnects and restarts, so Spotify
/// sees one spotitui per computer rather than a new device each time (librespot's
/// default is a random UUID per session). Hex, as librespot's own player uses.
pub(crate) fn device_id(purpose: &str, length: usize) -> String {
    use sha2::{Digest, Sha256};
    let machine = std::fs::read_to_string("/etc/machine-id")
        .or_else(|_| std::fs::read_to_string("/var/lib/dbus/machine-id"))
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{}\n{DEVICE_NAME}\n{purpose}", machine.trim()));
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
        .chars()
        .take(length)
        .collect()
}

/// Runs `task` on the runtime's workers, giving up (and stopping it) after
/// `limit`. librespot's own HTTP requests have no timeout.
pub(crate) async fn bounded<T: Send + 'static>(
    limit: Duration,
    what: &'static str,
    task: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Error> {
    let handle = tokio::spawn(task);
    let abort = handle.abort_handle();
    match tokio::time::timeout(limit, handle).await {
        Ok(joined) => joined?,
        Err(_) => {
            abort.abort();
            Err(Error::Timeout(what))
        }
    }
}

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
    Keyring(#[from] secret_service::Error),
    #[error("Spotify's login service is having trouble (HTTP {0}); trying again")]
    Accounts(u16),
    #[error("No audio output is available on this computer")]
    NoAudio,
    /// The play queue observer's own trouble, said as is.
    #[error("{0}")]
    Observer(String),
    #[error("Spotify didn't answer in time ({0})")]
    Timeout(&'static str),
    #[error("A Spotify request stopped unexpectedly ({0})")]
    Stopped(#[from] tokio::task::JoinError),
}

#[cfg(test)]
mod tests {
    use super::{Error, bounded};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    #[test]
    fn device_ids_are_stable_and_distinct() {
        let speaker = super::device_id("speaker", 40);
        assert_eq!(speaker.len(), 40);
        assert!(speaker.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(speaker, super::device_id("speaker", 40));
        assert_ne!(speaker, super::device_id("observer", 40));
    }

    #[tokio::test]
    async fn slow_requests_give_up_and_stop() {
        let finished = Arc::new(AtomicBool::new(false));
        let flag = finished.clone();
        let result = bounded(Duration::from_millis(50), "a test", async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flag.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(result, Err(Error::Timeout("a test"))));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!finished.load(Ordering::SeqCst), "the task was stopped");
        let quick = bounded(Duration::from_secs(1), "a test", async { Ok(7) }).await;
        assert!(matches!(quick, Ok(7)));
    }
}
