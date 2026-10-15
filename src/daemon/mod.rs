//! The background service (`spotitui daemon`): owns the Spotify session, the
//! speaker, MPRIS and the bar widget's queue file, and serves player windows.

mod pace;
mod server;
mod state;

use crate::{
    protocol::{self, Event, Failure, Kind, PlayerUpdate, Request, Response, Status},
    spotify::{
        self, Catalog, Command, Connection, Lyrics, Speaker, Token,
        secrets::Unlock,
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
    sync::{Notify, broadcast, mpsc, watch},
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
/// A session that ends sooner than this counts as a failed attempt, so the
/// next one waits longer: Spotify dropping every session right away (another
/// player with this device ID, trouble on its side) must not turn into a
/// reconnection every second.
const SHORT_SESSION: Duration = Duration::from_secs(30);
const EVENT_BACKLOG: usize = 256;
/// How long the speaker gets to tell Spotify it is leaving.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long stopping waits for the session to wind down, goodbye included.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a renewed login the keyring refused is offered to it again.
const SAVE_LOGIN_INTERVAL: Duration = Duration::from_secs(60);
/// How often a locked keyring is checked, without asking, for being unlocked.
const KEYRING_POLL: Duration = Duration::from_secs(3);
/// How often the session is checked for a lost connection.
const SESSION_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Skips asked for within this of each other are made in one go.
const SKIP_QUIET: Duration = Duration::from_millis(400);
/// Skips keep being gathered for at most this long while presses go on.
const SKIP_HOLD: Duration = Duration::from_millis(1200);
/// How long a jump waits for the play queue to describe its target.
const SKIP_QUEUE_WAIT: Duration = Duration::from_millis(1500);
/// How long a new session waits for its first look at Spotify's playback
/// before restoring it; without one, it leaves the playback where it is.
const RESTORE_WAIT: Duration = Duration::from_secs(10);
/// Play asked for while the playback isn't here plays it once it is, if that
/// is within this; later, music would start out of the blue.
const PENDING_PLAY: Duration = Duration::from_secs(15);
/// A track cut short by a lost connection plays on if the connection is
/// back within this; later, music would start out of the blue.
const PLAY_ON: Duration = Duration::from_secs(60);
/// How long a take-over may take before another one is asked for.
const TAKE_OVER_WAIT: Duration = Duration::from_secs(5);
/// How often the position is saved while playing, for after a power cut.
const SAVE_PROGRESS_INTERVAL: Duration = Duration::from_secs(15);
/// This many tracks refused within [`REFUSAL_WINDOW`] make windows say why.
const REFUSAL_LIMIT: usize = 5;
const REFUSAL_WINDOW: Duration = Duration::from_secs(30);

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
    // What plays: the service's state and MPRIS read this one record.
    let playing = spotify::playing::shared(state::saved_volume());
    // What played last shows right away, before MPRIS clients first look; the
    // session brings it back.
    let last = state::saved_last();
    if let Some(last) = &last {
        spotify::playing::lock(&playing).show(
            last.track.clone(),
            last.position_ms,
            last.shuffle,
            last.repeat,
        );
    }
    let (skips, skip_requests) = mpsc::unbounded_channel();
    let (resumes, resume_requests) = mpsc::unbounded_channel();
    let (settings, setting_requests) = mpsc::unbounded_channel();
    let channels = mpris::Channels {
        skips: skips.clone(),
        resumes,
        settings: settings.clone(),
    };
    let mpris = match mpris::serve(slot.clone(), playing.clone(), channels).await {
        Ok(mpris) => Some(Arc::new(mpris)),
        // Without a session bus the player still works, just without media keys.
        Err(error) => {
            log::warn!("cannot publish the MPRIS player: {error}");
            None
        }
    };
    let daemon = Arc::new(Daemon::new(slot, playing, skips, settings, mpris));
    if let Some(last) = last {
        let track = last.track.clone();
        daemon.state().show_last(last);
        // Publishes its cover.
        if let Some(mpris) = daemon.mpris() {
            mpris.changed(&PlayerUpdate::Track(track)).await;
        }
    }
    daemon.publish_feed();
    tokio::spawn(gather_skips(daemon.clone(), skip_requests));
    tokio::spawn(pace_settings(daemon.clone(), setting_requests));
    tokio::spawn(take_resumes(daemon.clone(), resume_requests));
    log::info!("spotitui service listening on {}", path.display());
    let mut supervisor = tokio::spawn(supervise(daemon.clone()));
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
    daemon.save_last();
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
    /// Skips forward, from windows and MPRIS, gathered by [`gather_skips`].
    skips: mpsc::UnboundedSender<u32>,
    /// Seeks and volume changes, from windows and MPRIS, sent on by [`pace_settings`].
    settings: mpsc::UnboundedSender<Command>,
    /// Those waiting for their turn.
    paced: Mutex<pace::Settings>,
    mpris: Option<Arc<Mpris>>,
    /// Spotify's playback as the play queue observer last saw it, this session.
    cluster: watch::Sender<Option<Around>>,
}

impl Daemon {
    fn new(
        slot: SpircSlot,
        playing: spotify::playing::Shared,
        skips: mpsc::UnboundedSender<u32>,
        settings: mpsc::UnboundedSender<Command>,
        mpris: Option<Arc<Mpris>>,
    ) -> Self {
        Self {
            skips,
            settings,
            paced: Mutex::new(pace::Settings::default()),
            mpris,
            cluster: watch::Sender::new(None),
            state: Mutex::new(State::new(playing)),
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
        match spotify::store_refresh_token(&token, Unlock::Never).await {
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
    async fn login(&self, unlock: Unlock) -> std::result::Result<Option<String>, spotify::Error> {
        if let Some(token) = self.save_login().await {
            return Ok(Some(token));
        }
        spotify::stored_refresh_token(unlock).await
    }

    fn paced(&self) -> MutexGuard<'_, pace::Settings> {
        self.paced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    fn mpris(&self) -> Option<&Mpris> {
        self.mpris.as_deref()
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
        self.publish_feed();
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
                {
                    let mut state = self.state();
                    state.remember_play(&play, &context_name);
                    // A take-over on its way would keep this paused.
                    state.taking_over = None;
                    state.restore_queue = None;
                }
                self.paced().seek.clear();
                self.speaker()?.play(play, mode).map_err(text)?;
                Ok(Response::Done)
            }
            Request::Command(command) => self.command(command).await,
            Request::PlayNext(uri) => {
                check_track(&uri)?;
                // Only a queue that describes what plays now can be reordered; an
                // older one would bring back the previous list's tracks.
                let queue = {
                    let state = self.state();
                    let playing = state.playing().track.as_ref().map(|track| track.uri.clone());
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

    async fn command(&self, command: Command) -> std::result::Result<Response, Failure> {
        if command == Command::Resume {
            self.resume().await?;
            return Ok(Response::Done);
        }
        // The speaker ignores these until it is Spotify's player: say so,
        // rather than doing nothing.
        let here = self.state().playing().here;
        match command {
            // Nothing plays here; a Play on its way is called off.
            Command::Pause if !here => {
                self.state().pending_play = None;
                return Ok(Response::Done);
            }
            Command::Next
            | Command::Previous
            | Command::Seek(_)
            | Command::Shuffle(_)
            | Command::Repeat(_)
                if !here =>
            {
                self.speaker()?;
                return Err(self.not_here());
            }
            _ => {}
        }
        if command == Command::Next {
            // Gathered with other quick skips; said at once if there's no speaker.
            self.speaker()?;
            let _ = self.skips.send(1);
            return Ok(Response::Done);
        }
        if let Command::Seek(_) | Command::Volume(_) = command {
            // A volume is kept for when there's a speaker; a seek needs one.
            if let Command::Seek(_) = command {
                self.speaker()?;
            }
            let _ = self.settings.send(command);
            return Ok(Response::Done);
        }
        if command == Command::Previous {
            // A seek waiting its turn is for the track playing now.
            self.paced().seek.clear();
        }
        self.speaker()?
            .command(command)
            .map_err(|error| failure(&error))?;
        Ok(Response::Done)
    }

    /// Why a command can't work while the playback isn't here.
    fn not_here(&self) -> Failure {
        let device = self.state().feed.device.clone();
        let message = if device.is_empty() {
            "The playback isn't here yet; Play brings it here".to_owned()
        } else {
            format!("The music is on {device}; Play brings it here")
        };
        Failure::new(Kind::Other, message)
    }

    /// Skips `count` tracks forward now: one is librespot's own "next"; more
    /// jump straight to the target track with a single load. Without a queue
    /// that knows the target, it falls back to one "next" per track.
    async fn skip_now(&self, count: u32) {
        let Ok(speaker) = self.speaker() else {
            return;
        };
        // A seek waiting its turn is for the track playing now.
        self.paced().seek.clear();
        if count > 1 {
            // Right after a jump the observer may not have heard of it yet; its
            // queue then can't say what comes so far ahead. Give it a moment.
            let mut target = self.skip_target(count);
            let waited = tokio::time::Instant::now();
            while target.is_none() && waited.elapsed() < SKIP_QUEUE_WAIT {
                tokio::time::sleep(Duration::from_millis(100)).await;
                target = self.skip_target(count);
            }
            if target.is_none() {
                let state = self.state();
                let playing = state.playing().track.as_ref().map(|track| track.uri.clone());
                log::debug!(
                    "no queue target {count} ahead of {playing:?}; queue: {:?}",
                    state.live_queue.as_ref().map(|queue| (
                        &queue.current_uri,
                        queue.next.iter().take(6).map(|track| track.uri.as_str()).collect::<Vec<_>>()
                    ))
                );
            }
            if let Some((uri, uid)) = target {
                log::debug!("skipping {count} tracks to {uri}");
                match speaker.skip_to(uri, uid).await {
                    Ok(()) => return,
                    Err(error) => log::warn!("cannot skip ahead in one go: {error}"),
                }
            }
        }
        for _ in 0..count {
            if let Err(error) = speaker.command(Command::Next) {
                log::warn!("cannot skip: {error}");
                return;
            }
        }
    }

    /// Takes a volume asked for: windows show it at once, and it's kept for
    /// the next connection and for a speaker that isn't the player yet.
    fn want_volume(&self, volume: u16) {
        self.state().wanted_volume = Some(volume);
        state::save_volume(volume);
        self.broadcast(Event::Player(PlayerUpdate::Volume(volume)));
    }

    /// Sends a volume to the speaker. One that isn't Spotify's player would
    /// ignore it; it gets it once it plays.
    fn volume_now(&self, volume: u16) {
        if !self.state().playing().here {
            return;
        }
        if let Ok(speaker) = self.speaker()
            && let Err(error) = speaker.command(Command::Volume(volume))
        {
            log::warn!("cannot set the volume: {error}");
        }
    }

    /// Sends a seek to the speaker, if `track` still plays here.
    fn seek_now(&self, track: &str, position_ms: u32) {
        let current = {
            let state = self.state();
            let playing = state.playing();
            playing.here && playing.track.as_ref().is_some_and(|playing| playing.uri == track)
        };
        if !current {
            log::debug!("dropping a seek in {track}, which no longer plays here");
            return;
        }
        if let Ok(speaker) = self.speaker()
            && let Err(error) = speaker.command(Command::Seek(position_ms))
        {
            log::warn!("cannot seek: {error}");
        }
    }

    /// The URI and queue UID of the track `count` places ahead, if the play
    /// queue knows it.
    fn skip_target(&self, count: u32) -> Option<(String, String)> {
        let state = self.state();
        let playing = state.playing().track.as_ref()?.uri.clone();
        let queue = state.live_queue.as_ref()?;
        queue::skip_target(queue, &playing, count as usize)
            .map(|track| (track.uri.clone(), track.uid.clone()))
    }

    /// Notes a track the speaker couldn't play; true once too many came in a
    /// short while. When Spotify refuses decryption keys (for a minute or two
    /// after many skips), librespot fails each track and moves on by itself,
    /// asking for another key every time; nothing stops that short of a key
    /// coming through, so windows are told what goes on.
    fn refused(&self) -> bool {
        let mut state = self.state();
        let now = std::time::Instant::now();
        state
            .refusals
            .retain(|at| now.duration_since(*at) < REFUSAL_WINDOW);
        state.refusals.push(now);
        if state.refusals.len() < REFUSAL_LIMIT {
            return false;
        }
        state.refusals.clear();
        true
    }

    /// Liked Songs as they are now: the kept set brought up to date with one
    /// small request, or read whole the first time (or when Spotify can't say
    /// what changed).
    async fn liked(&self, catalog: &Catalog) -> std::result::Result<spotify::Liked, spotify::Error> {
        let kept = self.state().liked.take();
        if let Some(mut liked) = kept {
            match catalog.update_liked(&mut liked).await {
                Ok(true) => return Ok(liked),
                Ok(false) => {}
                // An expired sync token may be refused outright; a full read still works.
                Err(error) => log::info!("cannot update Liked Songs ({error}); reading it again"),
            }
        }
        let liked = catalog.liked().await?;
        log::info!("read Liked Songs: {} items", liked.len());
        Ok(liked)
    }

    /// Forgets what played: it belongs to the account, and the next login
    /// brings its own.
    async fn forget_playback(&self) {
        state::forget_last();
        let had_track = {
            let mut state = self.state();
            let had = state.playing().track.is_some();
            state.forget_playback();
            had
        };
        if had_track {
            if let Some(mpris) = self.mpris() {
                mpris.refresh().await;
            }
            let snapshot = self.snapshot();
            self.broadcast(Event::Snapshot(snapshot));
        }
    }

    /// Takes a speaker event: updates the state, MPRIS and every window.
    async fn on_player_event(self: &Arc<Self>, event: PlayerEvent) {
        let mpris = self.mpris();
        if let PlayerEvent::TrackChanged { audio_item } = &event {
            self.playing.send_replace(Some(audio_item.uri.clone()));
        }
        let (relinked, update, resend_volume, shown, here_changed, settle) = {
            let mut state = self.state();
            let relinked = state.playing().relink(&event);
            let (was_here, shown) = {
                let playing = state.playing();
                (playing.here, playing.track.clone())
            };
            let update = state.apply(&event);
            let here = state.playing().here;
            let arrived = here && !was_here;
            let taken = arrived && state.taking_over.take().is_some();
            if was_here && !here {
                // Another device took over; a Play from before isn't for it.
                state.pending_play = None;
            }
            // The playback is coming here: Spirc is loading the track, so a
            // command now applies once it's loaded. (Sent on the "loaded"
            // event instead, Spirc may take it before that event, then think
            // it's paused while it plays.) Played if asked while it wasn't
            // here; else kept paused, as Spotify hands over a playback cut
            // short by a crash or power cut as playing.
            let settle = if arrived && state.take_pending_play(PENDING_PLAY) {
                Some((Command::Resume, state.restore_seek.take()))
            } else if taken {
                Some((Command::Pause, state.restore_seek.take()))
            } else {
                None
            };
            if matches!(update, Some(PlayerUpdate::Playing { .. })) {
                state.pending_play = None;
            }
            // The speaker is active once it plays; a volume it ignored before applies now.
            let resend = match (&update, state.wanted_volume) {
                (Some(PlayerUpdate::Playing { .. }), Some(volume))
                    if volume != state.playing().volume =>
                {
                    Some(volume)
                }
                _ => None,
            };
            (relinked, update, resend, shown, was_here != here, settle)
        };
        if let Some((command, seek)) = settle
            && let Ok(speaker) = self.speaker()
        {
            if command == Command::Resume {
                log::info!("playing what was brought here");
            }
            if let Err(error) = speaker.command(command) {
                log::warn!("cannot {command:?}: {error}");
            }
            if let Some(position) = seek {
                speaker.start_at(position);
            }
        }
        if let Some(track) = relinked {
            self.playing.send_replace(Some(track.uri.clone()));
            let update = PlayerUpdate::Track(track);
            if let Some(mpris) = mpris {
                mpris.changed(&update).await;
            }
            self.broadcast(Event::Player(update));
        }
        if let Some(volume) = resend_volume
            && let Ok(speaker) = self.speaker()
            && let Err(error) = speaker.command(Command::Volume(volume))
        {
            log::warn!("cannot set the volume: {error}");
        }
        if matches!(update, Some(PlayerUpdate::Unavailable)) && self.refused() {
            log::warn!("Spotify refused {REFUSAL_LIMIT} tracks within {REFUSAL_WINDOW:?}");
            self.broadcast(Event::Player(PlayerUpdate::Refusing));
        }
        if let Some(update) = update {
            // The track shown before it got here is the one loaded: nothing to redraw.
            let same = matches!(&update, PlayerUpdate::Track(track) if shown.as_ref() == Some(track));
            if let Some(mpris) = mpris
                && !same
            {
                mpris.changed(&update).await;
            }
            let changed = !matches!(
                update,
                PlayerUpdate::Position { .. }
                    | PlayerUpdate::Volume(_)
                    | PlayerUpdate::Muted(_)
                    | PlayerUpdate::Unavailable
                    | PlayerUpdate::Refusing
            );
            self.broadcast(Event::Player(update));
            if changed {
                self.save_last();
            }
        }
        if here_changed {
            if let Some(mpris) = mpris {
                mpris.controls_changed().await;
            }
            self.publish_feed();
            // Another device took over: show what it plays.
            let around = self.cluster.borrow().clone();
            if let Some(around) = around {
                self.spawn_mirror(around);
            }
        }
    }

    /// Plays: resumes the playback here, or first brings it here, paused,
    /// from before the service started or from another device, then plays it.
    /// Asked before the service is connected, it plays once the session
    /// brings the playback back, if that's soon.
    async fn resume(&self) -> std::result::Result<(), Failure> {
        let (here, stopped) = {
            let state = self.state();
            let playing = state.playing();
            (playing.here, playing.status == spotify::playing::Status::Stopped)
        };
        if here && stopped {
            return self.play_again();
        }
        if here {
            return self
                .speaker()?
                .command(Command::Resume)
                .map_err(|error| failure(&error));
        }
        self.state().pending_play = Some(std::time::Instant::now());
        match self.speaker() {
            Ok(speaker) => self.take_over(&speaker).await,
            Err(_) if self.state().status == Status::Connecting => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Plays after the speaker stopped: the list ran out, or Previous went back
    /// past its first track. librespot ignores Play then, so the track Spotify
    /// shows as current loads again, from its start, within what it played from.
    fn play_again(&self) -> std::result::Result<(), Failure> {
        let (play, mode) = {
            let state = self.state();
            let playing = state.playing();
            let (context, track) = match &state.live_queue {
                Some(queue) => (queue.context_uri.clone(), Some(queue.current_uri.clone())),
                None => (state.context_uri.clone(), None),
            };
            let Some(track) = track.or_else(|| playing.track.as_ref().map(|track| track.uri.clone()))
            else {
                return Err(Failure::new(Kind::NotFound, "Nothing played yet"));
            };
            let mode = spotify::Mode {
                shuffle: playing.shuffle,
                repeat: playing.repeat,
            };
            (state::play_within(&context, track), mode)
        };
        log::info!("the speaker had stopped; playing again");
        self.speaker()?
            .load(play, mode, true, 0)
            .map_err(|error| failure(&error))
    }

    /// Makes the speaker Spotify's player, paused. Spotify hands over its
    /// playback (what plays elsewhere, or what played last anywhere); if it has
    /// none, the last track played here is loaded again.
    async fn take_over(&self, speaker: &Speaker) -> std::result::Result<(), Failure> {
        {
            let mut state = self.state();
            if state
                .taking_over
                .is_some_and(|asked| asked.elapsed() < TAKE_OVER_WAIT)
            {
                return Ok(());
            }
            state.taking_over = Some(std::time::Instant::now());
        }
        let spotify_has_none = self
            .cluster
            .borrow()
            .as_ref()
            .is_some_and(|around| around.queue.is_none());
        self.state().adopt_options = true;
        let result = if spotify_has_none {
            match state::saved_last() {
                Some(last) => {
                    log::info!("Spotify has no playback; loading {} again", last.track.uri);
                    let mode = spotify::Mode {
                        shuffle: last.shuffle,
                        repeat: last.repeat,
                    };
                    speaker
                        .load(last.play(), mode, false, last.position_ms)
                        .map_err(|error| failure(&error))
                }
                None => Err(Failure::new(Kind::NotFound, "Nothing played yet")),
            }
        } else {
            log::info!("taking the playback over");
            // librespot shuffles anew when it takes a shuffled playback over;
            // the order Spotify kept is put back once it's here.
            let queue = self
                .cluster
                .borrow()
                .as_ref()
                .and_then(|around| around.queue.clone());
            self.state().restore_queue = queue;
            speaker.take_over().await.map_err(|error| failure(&error))
        };
        if result.is_err() {
            // Nothing is on its way: another Play may ask again.
            let mut state = self.state();
            state.taking_over = None;
            state.restore_queue = None;
        }
        result
    }

    /// Shows Spotify's playback while it isn't here; looking the track up may
    /// take a moment, which the session's events don't wait for.
    fn spawn_mirror(self: &Arc<Self>, around: Around) {
        let daemon = self.clone();
        tokio::spawn(async move { daemon.mirror(around).await });
    }

    /// While the playback isn't here, shows Spotify's instead: what another
    /// device plays, or what played last anywhere. Not before the session has
    /// decided whether to bring it back: until then, the track shown is the
    /// one saved here, and the position to bring back with it. Nor once the
    /// connection is lost (librespot then says the speaker left, and Spotify
    /// hears a paused track moved on): what played stays shown as it was, for
    /// the next session to pick up.
    async fn mirror(&self, around: Around) {
        let Some(uri) = around.queue.as_ref().map(|queue| queue.current_uri.clone()) else {
            return;
        };
        let shown = {
            let state = self.state();
            let playing = state.playing();
            if playing.here || !state.restored || !state.connected() {
                return;
            }
            playing.track.clone().filter(|track| track.uri == uri)
        };
        let (track, new) = match shown {
            Some(track) => (track, false),
            None => {
                let Ok(catalog) = self.catalog() else {
                    return;
                };
                match catalog.now_track(uri.clone()).await {
                    Ok(track) => (track, true),
                    Err(error) => {
                        log::info!("cannot show {uri}: {error}");
                        return;
                    }
                }
            }
        };
        let (shuffle, repeat) = {
            let state = self.state();
            let mut playing = state.playing();
            // It got here, or Spotify moved on, while the track was looked up.
            let current = self
                .cluster
                .borrow()
                .as_ref()
                .and_then(|around| around.queue.as_ref())
                .is_some_and(|queue| queue.current_uri == uri);
            if playing.here || !state.restored || !state.connected() || !current {
                return;
            }
            let before = (playing.shuffle, playing.repeat);
            playing.show(
                track.clone(),
                around.position_ms,
                around.shuffle,
                around.repeat,
            );
            before
        };
        let mut updates = Vec::new();
        if new {
            updates.push(PlayerUpdate::Track(track));
        }
        updates.push(PlayerUpdate::Paused {
            position_ms: around.position_ms,
        });
        if shuffle != around.shuffle {
            updates.push(PlayerUpdate::Shuffle(around.shuffle));
        }
        if repeat != around.repeat {
            updates.push(PlayerUpdate::Repeat(around.repeat));
        }
        for update in updates {
            if let Some(mpris) = self.mpris() {
                mpris.changed(&update).await;
            }
            self.broadcast(Event::Player(update));
        }
        self.save_last();
    }

    /// Saves what plays, if it changed, for the next start.
    fn save_last(&self) {
        let last = self.state().unsaved_last();
        if let Some(last) = last {
            state::save_last(&last);
        }
    }

    /// Writes what the bar widget shows besides MPRIS: the tracks around the
    /// playing one, the service's status, and the device Spotify plays on when
    /// it isn't this one.
    fn publish_feed(&self) {
        let feed = {
            let mut state = self.state();
            let own = state
                .live
                .as_ref()
                .map(|live| live.catalog.session().device_id().to_owned());
            let elsewhere = self.cluster.borrow().as_ref().and_then(|around| {
                around
                    .device
                    .as_ref()
                    .filter(|device| Some(&device.id) != own.as_ref())
                    .map(|device| (device.name.clone(), !around.paused))
            });
            state.feed.status = feed_status(&state.status).into();
            (state.feed.device, state.feed.device_playing) = elsewhere.unwrap_or_default();
            state.feed.clone()
        };
        queue::write(&feed);
    }

    /// Takes the observer's look at Spotify's playback: the bar widget's list,
    /// and what to show while the playback isn't here.
    async fn on_cluster(self: &Arc<Self>, around: Around) {
        let (here, adopt, reorder) = {
            let mut state = self.state();
            state.live_queue = around.queue.clone();
            let own = state
                .live
                .as_ref()
                .map(|live| live.catalog.session().device_id().to_owned());
            let here = state.playing().here;
            // Spotify's own record of this speaker, once it's the player.
            let ours = here && around.device.as_ref().map(|device| &device.id) == own.as_ref();
            let adopt = ours && std::mem::take(&mut state.adopt_options);
            // The playback taken over settles here: librespot first loads the
            // track alone, then its list, shuffled anew if shuffled. The order
            // from before is put back, and the list from before stays shown
            // meanwhile.
            let settling = match (&state.restore_queue, &around.queue) {
                (Some(saved), Some(queue)) => ours && saved.current_uri == queue.current_uri,
                _ => false,
            };
            let reorder = match &around.queue {
                Some(queue) if settling && !queue.next.is_empty() => state
                    .restore_queue
                    .take()
                    .filter(|saved| !same_tracks(&saved.next, &queue.next))
                    .map(|saved| (saved, queue.revision.clone())),
                _ => None,
            };
            if !settling || (reorder.is_none() && state.restore_queue.is_none()) {
                state.feed.context = state.context_name(&around.context_uri, &around.context_name);
                state.feed.tracks = around.tracks.clone();
                state.context_uri = around.context_uri.clone();
            }
            (here, adopt, reorder)
        };
        self.cluster.send_replace(Some(around.clone()));
        self.publish_feed();
        if adopt {
            self.adopt_options(&around).await;
        }
        if let Some((saved, revision)) = reorder
            && let Ok(speaker) = self.speaker()
        {
            log::info!("putting the play order back");
            if let Err(error) = speaker.restore_queue(saved, revision).await {
                log::warn!("cannot put the play order back: {error}");
            }
        }
        if here {
            self.save_last();
        } else {
            self.spawn_mirror(around);
        }
    }

    /// librespot takes over shuffle and repeat with Spotify's playback without
    /// saying so; Spotify's record of this speaker says what they are.
    async fn adopt_options(&self, around: &Around) {
        let mut updates = Vec::new();
        {
            let state = self.state();
            let mut playing = state.playing();
            if playing.shuffle != around.shuffle {
                playing.shuffle = around.shuffle;
                updates.push(PlayerUpdate::Shuffle(around.shuffle));
            }
            if playing.repeat != around.repeat {
                playing.repeat = around.repeat;
                updates.push(PlayerUpdate::Repeat(around.repeat));
            }
        }
        for update in updates {
            if let Some(mpris) = self.mpris() {
                mpris.changed(&update).await;
            }
            self.broadcast(Event::Player(update));
        }
    }
}

/// Whether two queues list the same tracks in the same order.
fn same_tracks(
    a: &[librespot_protocol::player::ProvidedTrack],
    b: &[librespot_protocol::player::ProvidedTrack],
) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.uri == b.uri)
}

/// What the bar widget says about the service while it can't play: empty when connected.
fn feed_status(status: &Status) -> &'static str {
    match status {
        Status::Connected { .. } => "",
        Status::Connecting => "connecting",
        Status::LoggedOut => "logged out",
        Status::Failed(failure) => match failure.kind {
            Kind::NeedsLogin => "logged out",
            Kind::Network | Kind::NotConnected => "offline",
            _ => "not connected",
        },
    }
}

/// Takes Play from MPRIS while the playback isn't here.
async fn take_resumes(daemon: Arc<Daemon>, mut requests: mpsc::UnboundedReceiver<()>) {
    while requests.recv().await.is_some() {
        if let Err(failure) = daemon.resume().await {
            log::info!("cannot play: {failure}");
        }
    }
}

/// Brings back the playback as a session starts, paused: Spotify's (what
/// played last, here or on another device), unless another device is playing
/// it, or else what played last here. A Play asked for meanwhile plays it.
/// Left elsewhere, Spotify's playback is shown instead.
async fn restore(daemon: Arc<Daemon>, own: String) {
    let took_over = bring_back(&daemon, &own).await;
    daemon.state().restored = true;
    if !took_over {
        let around = daemon.cluster.borrow().clone();
        if let Some(around) = around {
            daemon.mirror(around).await;
        }
    }
}

/// Whether the session asked for the playback.
async fn bring_back(daemon: &Daemon, own: &str) -> bool {
    let mut cluster = daemon.cluster.subscribe();
    let seen = tokio::time::timeout(RESTORE_WAIT, cluster.wait_for(Option::is_some)).await;
    let Ok(Ok(around)) = seen.map(|seen| seen.map(|around| around.clone())) else {
        log::warn!("no look at Spotify's playback in {RESTORE_WAIT:?}; leaving it where it is");
        return false;
    };
    let Some(around) = around else {
        return false;
    };
    let (here, asked, lost) = {
        let mut state = daemon.state();
        let asked = state
            .pending_play
            .is_some_and(|asked| asked.elapsed() < PENDING_PLAY);
        let lost = state.lost.take();
        (state.playing().here, asked, lost)
    };
    // A window, or another Spotify app, started something meanwhile.
    if here {
        return true;
    }
    if around.plays_elsewhere(own) && !asked {
        let name = around.device.as_ref().map_or("", |device| &device.name);
        log::info!("{name} is playing; leaving the playback there");
        return false;
    }
    // The last session lost its connection, and no other device took the
    // playback since: back to where it stopped, playing on if it played a
    // moment ago.
    let untouched = around.device.as_ref().is_none_or(|device| device.id == own);
    let current = around.queue.as_ref().map(|queue| queue.current_uri.as_str());
    if let Some(lost) = lost.filter(|lost| untouched && current == Some(lost.track.as_str())) {
        let mut state = daemon.state();
        state.restore_seek = Some(lost.position_ms);
        if lost.playing && lost.at.elapsed() < PLAY_ON {
            log::info!("the connection is back; playing on");
            state.pending_play = Some(std::time::Instant::now());
        }
    }
    // Spotify says it plays, with no device playing it: the player died, as
    // in a crash or power cut. It reckons the position on through the time
    // since; the one saved here is closer, if it's the same track.
    else if !around.paused
        && let Some(last) = state::saved_last()
        && around
            .queue
            .as_ref()
            .is_some_and(|queue| queue.current_uri == last.track.uri)
    {
        log::info!("the playback was cut short; going back to where it was saved");
        daemon.state().restore_seek = Some(last.position_ms);
    }
    let Ok(speaker) = daemon.speaker() else {
        return false;
    };
    match daemon.take_over(&speaker).await {
        Ok(()) => true,
        Err(failure) if failure.kind == Kind::NotFound => {
            log::info!("nothing to bring back: {failure}");
            false
        }
        Err(failure) => {
            log::warn!("cannot bring the playback back: {failure}");
            false
        }
    }
}

/// Makes skips forward, gathering presses that come in quick succession: the
/// first one goes at once, the rest are added up until they stop for a moment
/// and made as one jump. Spamming "next" used to load every track on the way,
/// each asking Spotify for a decryption key, until Spotify refused them.
async fn gather_skips(daemon: Arc<Daemon>, mut requests: mpsc::UnboundedReceiver<u32>) {
    while let Some(first) = requests.recv().await {
        daemon.skip_now(first).await;
        let mut pending: u32 = 0;
        let held = tokio::time::Instant::now();
        loop {
            let quiet = tokio::time::Instant::now() + SKIP_QUIET;
            let deadline = quiet.min(held + SKIP_HOLD);
            match tokio::time::timeout_at(deadline, requests.recv()).await {
                Ok(Some(count)) => pending = pending.saturating_add(count),
                Ok(None) => return,
                Err(_) if pending == 0 => break,
                Err(_) => {
                    daemon.skip_now(pending).await;
                    pending = 0;
                    if tokio::time::Instant::now() >= held + SKIP_HOLD {
                        break;
                    }
                }
            }
        }
    }
}

/// Sends seeks and volume changes on to the speaker at a steady pace (see
/// [`pace`]); windows show them at once, and the last one asked for always goes.
async fn pace_settings(daemon: Arc<Daemon>, mut requests: mpsc::UnboundedReceiver<Command>) {
    loop {
        let due = daemon.paced().due();
        tokio::select! {
            request = requests.recv() => match request {
                Some(Command::Seek(position_ms)) => {
                    let track = daemon.state().playing().track.as_ref().map(|track| track.uri.clone());
                    if let Some(track) = track {
                        daemon.paced().seek.ask((track, position_ms));
                    }
                }
                Some(Command::Volume(volume)) => {
                    daemon.want_volume(volume);
                    daemon.paced().volume.ask(volume);
                }
                Some(other) => log::warn!("not a seek or a volume: {other:?}"),
                None => return,
            },
            () = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {}
        }
        let now = tokio::time::Instant::now();
        let (seek, volume) = {
            let mut paced = daemon.paced();
            (paced.seek.take(now), paced.volume.take(now))
        };
        if let Some((track, position_ms)) = seek {
            daemon.seek_now(&track, position_ms);
        }
        if let Some(volume) = volume {
            daemon.volume_now(volume);
        }
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
async fn supervise(daemon: Arc<Daemon>) {
    let mut failures = 0;
    // A locked keyring asks for its password once; then the service waits for
    // it to be unlocked, unless a window asks to be prompted again.
    let mut unlock = Unlock::Ask;
    while !daemon.stopping.load(Ordering::SeqCst) {
        let refresh_token = match daemon.login(unlock).await {
            Ok(Some(token)) => token,
            Ok(None) => {
                daemon.forget_playback().await;
                daemon.set_status(Status::LoggedOut);
                daemon.reconnect.notified().await;
                continue;
            }
            Err(error @ spotify::Error::KeyringLocked) => {
                daemon.set_status(Status::Failed(failure(&error)));
                if unlock == Unlock::Ask {
                    log::info!("the keyring stayed locked; waiting for it to be unlocked");
                }
                unlock = Unlock::Never;
                tokio::select! {
                    () = tokio::time::sleep(KEYRING_POLL) => {}
                    () = daemon.reconnect.notified() => unlock = Unlock::Ask,
                }
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
                let began = tokio::time::Instant::now();
                let lost = run_session(&daemon, connection).await;
                if daemon.stopping.load(Ordering::SeqCst) {
                    return;
                }
                let lasted = began.elapsed();
                if lost && lasted < SHORT_SESSION {
                    log::warn!("the session lasted only {lasted:.1?}; waiting before the next");
                    wait(&daemon, &mut failures).await;
                } else {
                    failures = 0;
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
            Err(spotify::Error::Unauthorized)
                if matches!(
                    spotify::stored_refresh_token(Unlock::Never).await,
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
        && let Err(error) = token.store(Unlock::Never).await
    {
        // Keep going with the session; the login is saved on the next attempt.
        log::warn!("cannot save the renewed login, keeping it for now: {error}");
        *daemon.unsaved_login() = Some(token.refresh_token().to_owned());
    }
    let volume = daemon.state().playing().volume;
    Speaker::connect(token.access, volume).await
}

/// Serves one session until it ends or a reconnect is asked for; returns
/// whether it ended by itself, as when the connection is lost.
async fn run_session(daemon: &Arc<Daemon>, connection: Connection) -> bool {
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
    daemon.cluster.send_replace(None);
    let (clusters, mut cluster_updates) = mpsc::unbounded_channel();
    let observer = queue::observe(catalog.clone(), daemon.playing.subscribe(), move |around| {
        let _ = clusters.send(around);
    });
    let liked_uri = catalog.liked_uri();
    daemon.state().live = Some(Live {
        speaker,
        catalog: catalog.clone(),
    });
    daemon.set_status(Status::Connected { liked_uri });
    let restoring = tokio::spawn(restore(daemon.clone(), session_device(&catalog)));
    // Playlist names label the bar widget's queue; a failure only costs labels.
    let names = daemon.clone();
    let session = catalog.session().clone();
    tokio::spawn(async move {
        if let Ok(playlists) = catalog.playlists().await {
            names.state().remember_playlists(&playlists);
        }
    });

    let mut save_login = tokio::time::interval(SAVE_LOGIN_INTERVAL);
    save_login.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut check_session = tokio::time::interval(SESSION_CHECK_INTERVAL);
    check_session.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut save_progress = tokio::time::interval(SAVE_PROGRESS_INTERVAL);
    save_progress.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let session_ended = loop {
        tokio::select! {
            // librespot marks the session invalid as soon as the server
            // connection closes, but Spirc only notices on its next event: a
            // command sent meanwhile would be lost. Reconnect right away instead.
            _ = check_session.tick() => {
                if session.is_invalid() {
                    log::info!("the connection to Spotify was lost");
                    break true;
                }
            }
            _ = save_login.tick() => {
                daemon.save_login().await;
            }
            _ = save_progress.tick() => {
                let playing = daemon.state().playing().playing();
                if playing {
                    daemon.save_last();
                }
            }
            event = events.recv() => match event {
                Some(event) => daemon.on_player_event(event).await,
                None => break false,
            },
            Some(around) = cluster_updates.recv() => daemon.on_cluster(around).await,
            _ = &mut ended => {
                log::info!("the Spotify session ended");
                break true;
            }
            () = daemon.reconnect.notified() => break false,
        }
    };

    restoring.abort();
    daemon.playing.send_replace(None);
    daemon.cluster.send_replace(None);
    set_slot(&daemon.slot, None);
    let paused_at = {
        let mut state = daemon.state();
        // A session asked to end (a login) starts afresh; one that lost its
        // connection leaves where it was for the next.
        state.lost = if session_ended {
            state.whereabouts()
        } else {
            None
        };
        let playing = state.playing();
        (playing.status == spotify::playing::Status::Paused).then(|| playing.clock.elapsed_ms())
    };
    let live = daemon.state().disconnect();
    if let Some(live) = &live {
        // Leaving, librespot moves a paused track's position on by the time it
        // sat paused, and Spotify keeps that: after an hour's pause, past the
        // track's end. A seek first leaves it nothing to add.
        if let Some(position) = paused_at {
            let _ = live.speaker.command(Command::Seek(position));
        }
        // Shut down explicitly: a request still holding the speaker would keep it alive.
        let _ = live.speaker.spirc().shutdown();
    }
    drop(live);
    // Spirc pauses, marks this device inactive and removes it from Spotify's
    // device list before its task ends; give it the time to, while the
    // observer closes its own session.
    let goodbye = async {
        if !session_ended {
            match tokio::time::timeout(GOODBYE_TIMEOUT, ended).await {
                Ok(_) => log::info!("the speaker left Spotify"),
                Err(_) => log::warn!("the speaker didn't leave Spotify in time"),
            }
        }
    };
    tokio::join!(observer.stop(), goodbye);
    // Spirc closes its dealer last, after requests that can hang on a dead
    // connection; make sure it is closed, or the session stays alive.
    spotify::close_session(&session).await;
    // The track stays shown, paused, until the next session brings it back.
    if let Some(mpris) = daemon.mpris() {
        let position_ms = daemon.state().playing().clock.elapsed_ms();
        mpris.changed(&PlayerUpdate::Paused { position_ms }).await;
        mpris.controls_changed().await;
    }
    daemon.save_last();
    daemon.set_status(Status::Connecting);
    let snapshot = daemon.snapshot();
    daemon.broadcast(Event::Snapshot(snapshot));
    session_ended
}

/// The device ID the session registered its speaker as.
fn session_device(catalog: &Catalog) -> String {
    catalog.session().device_id().to_owned()
}

fn set_slot(slot: &SpircSlot, spirc: Option<Arc<librespot_connect::Spirc>>) {
    match slot.write() {
        Ok(mut current) => *current = spirc,
        Err(poisoned) => *poisoned.into_inner() = spirc,
    }
}

#[cfg(test)]
mod tests {
    use super::{Daemon, Kind, REFUSAL_LIMIT, SpircSlot, Status, failure, feed_status};
    use crate::{
        protocol::Failure,
        spotify::{Error, playing},
    };

    #[test]
    fn windows_hear_of_refusals_once_per_run_of_them() {
        let (skips, _requests) = tokio::sync::mpsc::unbounded_channel();
        let (settings, _settings) = tokio::sync::mpsc::unbounded_channel();
        let daemon = Daemon::new(SpircSlot::default(), playing::shared(0), skips, settings, None);
        for _ in 1..REFUSAL_LIMIT {
            assert!(!daemon.refused());
        }
        assert!(daemon.refused(), "the limit is reached");
        for _ in 1..REFUSAL_LIMIT {
            assert!(!daemon.refused(), "counting starts over");
        }
    }

    #[test]
    fn the_widget_hears_why_nothing_can_play() {
        let failed = |kind| Status::Failed(Failure::new(kind, ""));
        assert_eq!(feed_status(&Status::Connected { liked_uri: String::new() }), "");
        assert_eq!(feed_status(&Status::Connecting), "connecting");
        assert_eq!(feed_status(&Status::LoggedOut), "logged out");
        assert_eq!(feed_status(&failed(Kind::NeedsLogin)), "logged out");
        assert_eq!(feed_status(&failed(Kind::Network)), "offline");
        assert_eq!(feed_status(&failed(Kind::Other)), "not connected");
    }

    #[test]
    fn failures_say_what_kind_they_are() {
        assert_eq!(failure(&Error::Unauthorized).kind, Kind::NeedsLogin);
        let not_found = librespot_core::Error::not_found("no lyrics");
        assert_eq!(failure(&Error::Librespot(not_found)).kind, Kind::NotFound);
        let busy = librespot_core::Error::resource_exhausted("429");
        assert_eq!(failure(&Error::Librespot(busy)).kind, Kind::RateLimited);
        assert_eq!(failure(&Error::Observer("lost".into())).kind, Kind::Other);
    }
}
