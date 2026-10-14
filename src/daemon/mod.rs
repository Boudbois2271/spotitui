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
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::{Notify, broadcast, watch},
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
/// How long the speaker gets to tell Spotify it is leaving.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long stopping waits for the session to wind down, goodbye included.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a renewed login the keyring refused is offered to it again.
const SAVE_LOGIN_INTERVAL: Duration = Duration::from_secs(60);

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
    let mut supervisor = tokio::spawn(supervise(daemon.clone(), mpris));
    let mut acceptor = tokio::spawn(server::accept(daemon.clone(), listener));

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    // A hangup only happens when started from a terminal that closed.
    let mut hangup = signal(SignalKind::hangup())?;
    // Both tasks run for the service's whole life. One that ends has crashed;
    // exiting with an error lets systemd start a fresh service.
    let crashed = tokio::select! {
        _ = terminate.recv() => None,
        _ = interrupt.recv() => None,
        _ = hangup.recv() => None,
        result = &mut supervisor => Some(("the Spotify session keeper", result)),
        result = &mut acceptor => Some(("the window listener", result)),
    };
    if let Some((task, result)) = crashed {
        let reason = match result {
            Err(error) if error.is_panic() => "panicked",
            _ => "stopped",
        };
        log::error!("{task} {reason}; exiting so the service restarts");
        queue::clear();
        let _ = std::fs::remove_file(&path);
        return Err(format!("{task} {reason}").into());
    }
    log::info!("spotitui service stopping");
    acceptor.abort();
    // The session ends as for a reconnect, so the speaker tells Spotify it's leaving.
    daemon.stopping.store(true, Ordering::SeqCst);
    daemon.reconnect.notify_one();
    if tokio::time::timeout(STOP_TIMEOUT, supervisor).await.is_err() {
        log::warn!("the session didn't end in time");
    }
    // A renewed login only in memory would be lost with the process.
    if daemon.save_login().await.is_some() {
        log::error!("the renewed login couldn't be saved; you may need to log in again");
    }
    queue::clear();
    let _ = std::fs::remove_file(&path);
    Ok(())
}

pub struct Daemon {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    /// Ends the current session (or a wait for one) to read the login again.
    reconnect: Notify,
    /// Set when the service stops: the session ends and no new one starts.
    stopping: AtomicBool,
    /// The track the speaker plays, for the play queue observer's watchdog.
    playing: watch::Sender<Option<String>>,
    /// A renewed login the keyring wouldn't take yet; saved on the next attempt.
    /// Spotify may stop accepting the old one once it hands out a new one.
    unsaved_login: Mutex<Option<String>>,
    slot: SpircSlot,
    http: reqwest::Client,
}

impl Daemon {
    fn new(slot: SpircSlot) -> Self {
        Self {
            state: Mutex::new(State::new()),
            events: broadcast::channel(EVENT_BACKLOG).0,
            reconnect: Notify::new(),
            stopping: AtomicBool::new(false),
            unsaved_login: Mutex::new(None),
            playing: watch::Sender::new(None),
            slot,
            http: spotify::http(),
        }
    }

    fn unsaved_login(&self) -> MutexGuard<'_, Option<String>> {
        self.unsaved_login
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Tries again to save a renewed login the keyring refused; returns it if
    /// it is still unsaved.
    async fn save_login(&self) -> Option<String> {
        let token = self.unsaved_login().clone()?;
        match spotify::store_refresh_token(&token).await {
            Ok(()) => {
                log::info!("saved the renewed login to the keyring");
                let mut unsaved = self.unsaved_login();
                if unsaved.as_deref() == Some(token.as_str()) {
                    *unsaved = None;
                }
                None
            }
            Err(error) => {
                log::warn!("still cannot save the renewed login: {error}");
                Some(token)
            }
        }
    }

    /// The login to use: one still waiting to be saved, else the keyring's.
    async fn login(&self) -> std::result::Result<Option<String>, spotify::Error> {
        if let Some(token) = self.save_login().await {
            return Ok(Some(token));
        }
        spotify::stored_refresh_token().await
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
                let mut liked = self.liked(&catalog).await.map_err(text)?;
                let added = !liked.contains(&uri);
                if added {
                    catalog.set_liked(uri.clone(), true).await.map_err(text)?;
                    liked.insert(uri);
                }
                self.state().liked = Some(liked);
                Ok(Response::Liked { added })
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
                // A window just stored a fresh login; it replaces any unsaved one.
                *self.unsaved_login() = None;
                self.reconnect.notify_one();
                Ok(Response::Done)
            }
            Request::Logout => {
                *self.unsaved_login() = None;
                spotify::forget().await.map_err(text)?;
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

    /// Liked Songs as they are now: the kept set brought up to date with one
    /// small request, or read whole the first time (or when Spotify can't say
    /// what changed).
    async fn liked(&self, catalog: &Catalog) -> std::result::Result<spotify::Liked, spotify::Error> {
        let kept = self.state().liked.take();
        if let Some(mut liked) = kept
            && catalog.update_liked(&mut liked).await?
        {
            return Ok(liked);
        }
        let liked = catalog.liked().await?;
        log::info!("read Liked Songs: {} items", liked.len());
        Ok(liked)
    }

    /// Takes a speaker event: updates the state, MPRIS and every window.
    async fn on_player_event(&self, event: PlayerEvent, mpris: Option<&Mpris>) {
        if let PlayerEvent::TrackChanged { audio_item } = &event {
            self.playing.send_replace(Some(audio_item.uri.clone()));
        }
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
        spotify::Error::Accounts(429) => Kind::RateLimited,
        spotify::Error::Accounts(_) => Kind::Network,
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
    while !daemon.stopping.load(Ordering::SeqCst) {
        let refresh_token = match daemon.login().await {
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
                if daemon.stopping.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            Err(spotify::Error::Unauthorized)
                if matches!(
                    spotify::stored_refresh_token().await,
                    Ok(Some(stored)) if stored != refresh_token
                ) =>
            {
                // Another spotitui (a window logging in, a test) renewed the login
                // meanwhile, and Spotify retired the one read before. Use the new one.
                log::info!("the login was renewed elsewhere; trying it");
                let mut unsaved = daemon.unsaved_login();
                if unsaved.as_deref() == Some(refresh_token.as_str()) {
                    *unsaved = None;
                }
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
    if token.refresh_token() != refresh_token
        && let Err(error) = token.store().await
    {
        // Keep going with the session; the login is saved on the next attempt.
        log::warn!("cannot save the renewed login, keeping it for now: {error}");
        *daemon.unsaved_login() = Some(token.refresh_token().to_owned());
    }
    let volume = daemon.state().player.volume;
    Speaker::connect(token.access, volume).await
}

/// Serves one session until it ends or a reconnect is asked for.
async fn run_session(daemon: &Arc<Daemon>, connection: Connection, mpris: Option<&Mpris>) {
    let Connection {
        speaker,
        mut events,
        catalog,
        mut ended,
    } = connection;
    let speaker = Arc::new(speaker);
    // Muting belongs to the service, not the session: a new speaker starts muted too.
    speaker.set_muted(daemon.state().muted);
    set_slot(&daemon.slot, Some(speaker.spirc()));
    let observer = {
        let daemon = daemon.clone();
        queue::observe(catalog.clone(), daemon.playing.subscribe(), move |around| {
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

    let mut save_login = tokio::time::interval(SAVE_LOGIN_INTERVAL);
    save_login.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let session_ended = loop {
        tokio::select! {
            _ = save_login.tick() => {
                daemon.save_login().await;
            }
            event = events.recv() => match event {
                Some(event) => daemon.on_player_event(event, mpris).await,
                None => break false,
            },
            _ = &mut ended => {
                log::info!("the Spotify session ended");
                break true;
            }
            () = daemon.reconnect.notified() => break false,
        }
    };

    observer.abort();
    daemon.playing.send_replace(None);
    set_slot(&daemon.slot, None);
    let live = daemon.state().disconnect();
    if let Some(live) = &live {
        // Shut down explicitly: a request still holding the speaker would keep it alive.
        let _ = live.speaker.spirc().shutdown();
    }
    drop(live);
    // Spirc pauses, marks this device inactive and removes it from Spotify's
    // device list before its task ends; give it the time to.
    if !session_ended {
        match tokio::time::timeout(GOODBYE_TIMEOUT, ended).await {
            Ok(_) => log::info!("the speaker left Spotify"),
            Err(_) => log::warn!("the speaker didn't leave Spotify in time"),
        }
    }
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
