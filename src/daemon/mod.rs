//! The background service (`spotitui daemon`): owns the Spotify session, the
//! speaker, MPRIS and the bar widget's queue file, and serves player windows.

mod server;
mod state;

use crate::{
    protocol::{self, Event, Failure, Kind, PlayerUpdate, Request, Response, Status},
    spotify::{
        self, Catalog, Command, Connection, Lyrics, Speaker, Token,
        mpris::{self, Mpris, SpircSlot},
        queue::{self, Around},
    },
};
use librespot_playback::player::PlayerEvent;
use state::{Live, State};
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::{Notify, broadcast},
};

/// Waits between attempts to reach Spotify after failures, growing to the last.
const BACKOFF: [Duration; 5] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// A pause after a session ends, before connecting again.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const EVENT_BACKLOG: usize = 256;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn run() -> Result<()> {
    // SAFETY: no other threads exist yet. PulseAudio names the stream in volume mixers after these.
    unsafe {
        std::env::set_var("PULSE_PROP_application.name", "spotitui");
        std::env::set_var("PULSE_PROP_stream.description", "Music");
    }
    crate::logger::init_daemon();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve());
    // librespot's player thread may still be winding down; don't wait on it.
    runtime.shutdown_timeout(Duration::from_millis(500));
    result
}

async fn serve() -> Result<()> {
    let path = protocol::socket_path().ok_or("XDG_RUNTIME_DIR is not set")?;
    let Some(listener) = server::bind(&path).await? else {
        log::info!("another spotitui service is already running");
        return Ok(());
    };
    let slot = SpircSlot::default();
    let mpris = match mpris::serve(slot.clone()).await {
        Ok(mpris) => Some(Arc::new(mpris)),
        // Without a session bus the player still works, just without media keys.
        Err(error) => {
            log::warn!("cannot publish the MPRIS player: {error}");
            None
        }
    };
    let daemon = Arc::new(Daemon::new(slot));
    log::info!("spotitui service listening on {}", path.display());
    tokio::spawn(supervise(daemon.clone(), mpris));
    tokio::spawn(server::accept(daemon.clone(), listener));

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // A hangup only happens when started from a terminal that closed.
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::select! {
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
        _ = hangup.recv() => {}
    }
    log::info!("spotitui service stopping");
    // Dropping the speaker tells Spotify this device is leaving.
    drop(daemon.state().disconnect());
    queue::clear();
    let _ = std::fs::remove_file(&path);
    Ok(())
}

pub struct Daemon {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    /// Ends the current session (or a wait for one) to read the login again.
    reconnect: Notify,
    slot: SpircSlot,
    http: reqwest::Client,
}

impl Daemon {
    fn new(slot: SpircSlot) -> Self {
        Self {
            state: Mutex::new(State::new()),
            events: broadcast::channel(EVENT_BACKLOG).0,
            reconnect: Notify::new(),
            slot,
            http: spotify::http(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data behind; keep serving it.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn snapshot(&self) -> protocol::Snapshot {
        self.state().snapshot()
    }

    fn broadcast(&self, event: Event) {
        // No window open is the normal case for a background service.
        let _ = self.events.send(event);
    }

    fn set_status(&self, status: Status) {
        {
            let mut state = self.state();
            if state.status == status {
                return;
            }
            state.status = status.clone();
        }
        log::info!("status: {status:?}");
        self.broadcast(Event::Status(status));
    }

    fn catalog(&self) -> std::result::Result<Catalog, Failure> {
        self.state()
            .live
            .as_ref()
            .map(|live| live.catalog.clone())
            .ok_or_else(not_connected)
    }

    fn speaker(&self) -> std::result::Result<Arc<Speaker>, Failure> {
        self.state()
            .live
            .as_ref()
            .map(|live| live.speaker.clone())
            .ok_or_else(not_connected)
    }

    /// Answers one request from a window.
    pub async fn handle(&self, request: Request) -> std::result::Result<Response, Failure> {
        let text = |error: spotify::Error| failure(&error);
        match request {
            Request::Playlists => {
                let playlists = self.catalog()?.playlists().await.map_err(text)?;
                self.state().remember_playlists(&playlists);
                Ok(Response::Playlists(playlists))
            }
            Request::ContextTracks(uri) => Ok(Response::Uris(
                self.catalog()?.context_tracks(uri).await.map_err(text)?,
            )),
            Request::RecentTracks => Ok(Response::Uris(
                self.catalog()?.recent_tracks().await.map_err(text)?,
            )),
            Request::Tracks(uris) => Ok(Response::Tracks(
                self.catalog()?.tracks(uris).await.map_err(text)?,
            )),
            Request::Lyrics(track) => {
                let session = self.catalog()?.session().clone();
                Ok(Response::Lyrics(
                    Lyrics::fetch(session, self.http.clone(), track)
                        .await
                        .map_err(text)?,
                ))
            }
            Request::Play {
                play,
                mode,
                context_name,
            } => {
                self.state().remember_play(&play, &context_name);
                self.speaker()?.play(play, mode).map_err(text)?;
                Ok(Response::Done)
            }
            Request::Command(command) => self.command(command),
            Request::PlayNext(uri) => {
                check_track(&uri)?;
                // Only a queue that describes what plays now can be reordered; an
                // older one would bring back the previous list's tracks.
                let queue = {
                    let state = self.state();
                    let playing = state.player.track.as_ref().map(|track| track.uri.clone());
                    state
                        .live_queue
                        .clone()
                        .filter(|queue| Some(&queue.current_uri) == playing.as_ref())
                };
                self.speaker()?.play_next(uri, queue).await.map_err(text)?;
                Ok(Response::Done)
            }
            Request::ToggleMute => {
                let muted = {
                    let mut state = self.state();
                    state.muted = !state.muted;
                    state.muted
                };
                state::save_muted(muted);
                if let Ok(speaker) = self.speaker() {
                    speaker.set_muted(muted);
                }
                log::info!("{}", if muted { "muted" } else { "unmuted" });
                self.broadcast(Event::Player(PlayerUpdate::Muted(muted)));
                Ok(Response::Done)
            }
            Request::Like(uri) => {
                check_track(&uri)?;
                let catalog = self.catalog()?;
                if catalog.is_liked(&uri).await.map_err(text)? {
                    return Ok(Response::Liked { added: false });
                }
                catalog.set_liked(uri, true).await.map_err(text)?;
                Ok(Response::Liked { added: true })
            }
            Request::AddToPlaylists { track, playlists } => {
                check_track(&track)?;
                let catalog = self.catalog()?;
                let mut failed = Vec::new();
                for playlist in playlists {
                    if let Err(error) = catalog
                        .add_to_playlist(playlist.clone(), track.clone())
                        .await
                    {
                        log::warn!("cannot add {track} to {playlist}: {error}");
                        failed.push(failure(&error));
                    }
                }
                match failed.first() {
                    None => Ok(Response::Done),
                    Some(error) => Err(error.clone()),
                }
            }
            Request::AddToQueue(uri) => {
                check_track(&uri)?;
                self.speaker()?.queue(uri).await.map_err(text)?;
                Ok(Response::Done)
            }
            Request::Reconnect => {
                self.reconnect.notify_one();
                Ok(Response::Done)
            }
            Request::Logout => {
                spotify::forget().map_err(text)?;
                self.reconnect.notify_one();
                Ok(Response::Done)
            }
        }
    }

    fn command(&self, command: Command) -> std::result::Result<Response, Failure> {
        if let Command::Volume(volume) = command {
            // Kept for when the speaker becomes active, and for the next connection.
            self.state().wanted_volume = Some(volume);
            state::save_volume(volume);
            // Windows show it now; an inactive speaker won't report it until it plays.
            self.broadcast(Event::Player(PlayerUpdate::Volume(volume)));
            if self.speaker().is_err() {
                return Ok(Response::Done);
            }
        }
        self.speaker()?
            .command(command)
            .map_err(|error| failure(&error))?;
        Ok(Response::Done)
    }

    /// Takes a speaker event: updates the state, MPRIS and every window.
    async fn on_player_event(&self, event: PlayerEvent, mpris: Option<&Mpris>) {
        let (update, resend_volume) = {
            let mut state = self.state();
            let update = state.apply(&event);
            // The speaker is active once it plays; a volume it ignored before applies now.
            let resend = match (&update, state.wanted_volume) {
                (Some(PlayerUpdate::Playing { .. }), Some(volume))
                    if volume != state.player.volume =>
                {
                    Some(volume)
                }
                _ => None,
            };
            (update, resend)
        };
        if let Some(volume) = resend_volume
            && let Ok(speaker) = self.speaker()
            && let Err(error) = speaker.command(Command::Volume(volume))
        {
            log::warn!("cannot set the volume: {error}");
        }
        if let Some(mpris) = mpris {
            mpris.apply(event).await;
        }
        if let Some(update) = update {
            self.broadcast(Event::Player(update));
        }
    }

    /// Writes the tracks around the playing one for the bar widget.
    fn publish_queue(&self, around: &Around) {
        let name = {
            let mut state = self.state();
            state.live_queue = around.queue.clone();
            state.context_name(&around.context_uri, &around.context_name)
        };
        queue::write(&name, &around.tracks);
    }
}

/// Spotify reads an empty URI in a reordered queue as "the current track".
fn check_track(uri: &str) -> std::result::Result<(), Failure> {
    let playable = ["spotify:track:", "spotify:episode:"]
        .iter()
        .any(|prefix| uri.strip_prefix(prefix).is_some_and(|id| !id.is_empty()));
    if playable {
        Ok(())
    } else {
        Err(Failure::new(
            Kind::Other,
            format!("Can't queue {uri:?}: not a track"),
        ))
    }
}

fn not_connected() -> Failure {
    Failure::new(
        Kind::NotConnected,
        "spotitui isn't connected to Spotify right now",
    )
}

/// What a window needs to know about a failure: its message and what kind it is.
fn failure(error: &spotify::Error) -> Failure {
    use librespot_core::error::ErrorKind;
    let kind = match error {
        spotify::Error::Unauthorized => Kind::NeedsLogin,
        spotify::Error::Network(error) if error.status().is_some_and(|s| s.as_u16() == 429) => {
            Kind::RateLimited
        }
        spotify::Error::Network(_) | spotify::Error::Lrclib(_) | spotify::Error::Timeout(_) => {
            Kind::Network
        }
        spotify::Error::Librespot(error) => match error.kind {
            ErrorKind::ResourceExhausted => Kind::RateLimited,
            ErrorKind::NotFound => Kind::NotFound,
            ErrorKind::Unavailable | ErrorKind::DeadlineExceeded | ErrorKind::Aborted => {
                Kind::Network
            }
            _ => Kind::Other,
        },
        _ => Kind::Other,
    };
    Failure::new(kind, error.to_string())
}

/// Keeps a session to Spotify for as long as the service runs: logs in with the
/// stored login, reconnects after the session ends, and waits while logged out.
async fn supervise(daemon: Arc<Daemon>, mpris: Option<Arc<Mpris>>) {
    let mut failures = 0;
    loop {
        let refresh_token = match spotify::stored_refresh_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                daemon.set_status(Status::LoggedOut);
                daemon.reconnect.notified().await;
                continue;
            }
            // The keyring may still be locked right after login.
            Err(error) => {
                daemon.set_status(Status::Failed(failure(&error)));
                wait(&daemon, &mut failures).await;
                continue;
            }
        };
        daemon.set_status(Status::Connecting);
        match connect(&daemon, &refresh_token).await {
            Ok(connection) => {
                failures = 0;
                run_session(&daemon, connection, mpris.as_deref()).await;
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(error) => {
                log::warn!("cannot connect to Spotify: {error}");
                daemon.set_status(Status::Failed(failure(&error)));
                wait(&daemon, &mut failures).await;
            }
        }
    }
}

/// Waits before the next attempt, longer after each failure; a login cuts it short.
async fn wait(daemon: &Daemon, failures: &mut usize) {
    let delay = BACKOFF[(*failures).min(BACKOFF.len() - 1)];
    *failures += 1;
    tokio::select! {
        () = tokio::time::sleep(delay) => {}
        () = daemon.reconnect.notified() => {}
    }
}

async fn connect(
    daemon: &Daemon,
    refresh_token: &str,
) -> std::result::Result<Connection, spotify::Error> {
    let token = Token::refresh(&daemon.http, refresh_token).await?;
    token.store()?;
    let volume = daemon.state().player.volume;
    Speaker::connect(token.access, volume).await
}

/// Serves one session until it ends or a reconnect is asked for.
async fn run_session(daemon: &Arc<Daemon>, connection: Connection, mpris: Option<&Mpris>) {
    let Connection {
        speaker,
        mut events,
        catalog,
        access_token,
        mut ended,
    } = connection;
    let speaker = Arc::new(speaker);
    // Muting belongs to the service, not the session: a new speaker starts muted too.
    speaker.set_muted(daemon.state().muted);
    set_slot(&daemon.slot, Some(speaker.spirc()));
    let observer = {
        let daemon = daemon.clone();
        queue::observe(access_token, catalog.clone(), move |around| {
            daemon.publish_queue(&around);
        })
    };
    let liked_uri = catalog.liked_uri();
    daemon.state().live = Some(Live {
        speaker,
        catalog: catalog.clone(),
    });
    daemon.set_status(Status::Connected { liked_uri });
    // Playlist names label the bar widget's queue; a failure only costs labels.
    let names = daemon.clone();
    tokio::spawn(async move {
        if let Ok(playlists) = catalog.playlists().await {
            names.state().remember_playlists(&playlists);
        }
    });

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => daemon.on_player_event(event, mpris).await,
                None => break,
            },
            _ = &mut ended => {
                log::info!("the Spotify session ended");
                break;
            }
            () = daemon.reconnect.notified() => break,
        }
    }

    observer.abort();
    set_slot(&daemon.slot, None);
    // Dropping the speaker shuts its Spirc down.
    drop(daemon.state().disconnect());
    queue::clear();
    if let Some(mpris) = mpris {
        mpris.reset().await;
    }
    daemon.set_status(Status::Connecting);
    let snapshot = daemon.snapshot();
    daemon.broadcast(Event::Snapshot(snapshot));
}

fn set_slot(slot: &SpircSlot, spirc: Option<Arc<librespot_connect::Spirc>>) {
    match slot.write() {
        Ok(mut current) => *current = spirc,
        Err(poisoned) => *poisoned.into_inner() = spirc,
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, failure};
    use crate::spotify::Error;

    #[test]
    fn failures_say_what_kind_they_are() {
        assert_eq!(failure(&Error::Unauthorized).kind, Kind::NeedsLogin);
        let not_found = librespot_core::Error::not_found("no lyrics");
        assert_eq!(failure(&Error::Librespot(not_found)).kind, Kind::NotFound);
        let busy = librespot_core::Error::resource_exhausted("429");
        assert_eq!(failure(&Error::Librespot(busy)).kind, Kind::RateLimited);
        assert_eq!(failure(&Error::NoAudio).kind, Kind::Other);
    }
}
