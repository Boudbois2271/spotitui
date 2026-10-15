//! Application state. Background work reports back through [`Msg`]s, so all
//! state lives on the main loop and is changed in one place.

mod finder;
mod input;
pub mod listing;
pub mod picker;
pub mod playback;

use crate::{
    client::{Notification, Service},
    protocol::{self, Event, Failure, Kind, PlayerUpdate, Request, Snapshot, Status},
    spotify::{self, Authorization, Command, Lyrics, Mode, Play, Playlist, Track},
    theme::{Theme, ThemeSource},
};
use listing::{Listing, Loading, Source, Step, Window};
use playback::NowPlaying;
use ratatui::layout::Rect;
use std::{
    process::Stdio,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;

/// Tracks whose details load in one request.
const CHUNK: usize = 100;
/// Rows below the window that load before they scroll into view.
const PREFETCH_ROWS: usize = 30;
const NOTICE_TIME: Duration = Duration::from_secs(4);
/// Lyrics that failed for a passing reason (the network, a reconnect) load
/// again after this, while shown.
const LYRICS_RETRY: Duration = Duration::from_secs(10);
const SEEK_STEP: Duration = Duration::from_secs(10);
/// Ticks between checks of the Omarchy theme file.
const THEME_POLL_TICKS: u64 = 4;
/// Playlists load again on entering the sidebar, at most this often.
const PLAYLISTS_REFRESH: Duration = Duration::from_secs(30);
/// The list on screen loads again on entering it from the sidebar, or after
/// a reconnection, if it was listed longer ago than this.
const LIST_REFRESH: Duration = Duration::from_secs(30);
/// The fixed sidebar entries before the playlists: Search, Liked Songs, Recently played.
pub const FIXED_SHELVES: usize = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Screen {
    Connecting,
    LoggedOut,
    Waiting(String),
    Failed(Failure),
    Library,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Tracks,
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Tracks,
    Lyrics,
}

#[derive(Clone, Debug)]
pub enum LyricsStatus {
    Loading,
    Ready(Rc<Lyrics>),
    Missing,
    Failed(Failure),
}

/// Lyrics of one track.
#[derive(Clone, Debug)]
pub struct LyricsCache {
    pub uri: String,
    pub status: LyricsStatus,
    /// When the status was last set.
    since: Instant,
}

impl LyricsCache {
    pub fn new(uri: String, status: LyricsStatus) -> Self {
        Self {
            uri,
            status,
            since: Instant::now(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    shown: Instant,
}

/// Something a key, button or click asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    TogglePlay,
    Next,
    Previous,
    Shuffle,
    Repeat,
    Search,
    Lyrics,
    Help,
    CloseHelp,
    ClosePicker,
    CloseFinder,
    LogIn,
}

/// A clickable or scrollable region from the last frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Shelf(usize),
    Row(usize),
    Lyric(usize),
    Action(Action),
    Progress,
    Volume,
    SearchInput,
    SidebarPane,
    TracksPane,
    LyricsPane,
    PickerRow(usize),
    PickerPane,
    FinderRow(usize),
    FinderPane,
}

pub enum Msg {
    Service(Notification),
    /// The browser login finished and the service was told to use it.
    LoggedIn,
    LoginFailed(Failure),
    Playlists(Result<Vec<Playlist>, Failure>),
    Uris {
        generation: u64,
        result: Result<Vec<String>, Failure>,
    },
    /// The list on screen, listed again in the background.
    Refreshed {
        generation: u64,
        result: Result<Vec<String>, Failure>,
    },
    Tracks {
        generation: u64,
        /// The row the chunk starts at, and its URIs.
        start: usize,
        chunk: Vec<String>,
        result: Result<Vec<Track>, Failure>,
    },
    Lyrics {
        uri: String,
        result: Result<Option<Lyrics>, Failure>,
    },
    Notice {
        text: String,
        error: bool,
    },
    /// A track was saved to Liked Songs from this window.
    Liked(Track),
}

pub struct App {
    pub theme: Theme,
    theme_source: ThemeSource,
    tx: UnboundedSender<Msg>,
    http: reqwest::Client,
    service: Service,
    pub screen: Screen,
    /// Why the service can't be reached, shown on the welcome screen.
    pub service_note: Option<String>,
    /// The Liked Songs context, known once the service is connected.
    liked_uri: Option<String>,
    pub playlists: Vec<Rc<Playlist>>,
    /// When the playlists were last asked for.
    playlists_asked: Option<Instant>,
    pub sidebar: Window,
    /// The scrolled part of the sidebar: the playlists.
    pub playlist_window: Window,
    pub listing: Listing,
    pub window: Window,
    history: Vec<(Listing, Window)>,
    generation: u64,
    pub search: String,
    pub focus: Focus,
    pub view: View,
    pub now: NowPlaying,
    pub lyrics: Option<LyricsCache>,
    /// Lyrics scroll: rows from the sung line when synced, the top line otherwise.
    pub lyrics_offset: isize,
    pub help: bool,
    /// The "add to playlist" picker, while open.
    pub picker: Option<picker::Picker>,
    /// "Find in this list", while open.
    pub finder: Option<finder::Finder>,
    notice: Option<Notice>,
    /// The service is another version than this window: shown whenever no
    /// newer notice is, until a matching service connects.
    mismatch: Option<Notice>,
    pub ticks: u64,
    pub hits: Vec<(Rect, Hit)>,
    pending_g: bool,
    /// Whether the selection follows the end of the list while it loads (after `G`).
    to_end: bool,
    last_click: Option<(Instant, Hit)>,
    pub quit: bool,
}

impl App {
    pub fn new(
        tx: UnboundedSender<Msg>,
        http: reqwest::Client,
        mut theme_source: ThemeSource,
        service: Service,
    ) -> Self {
        Self {
            theme: theme_source.load(),
            theme_source,
            tx,
            http,
            service,
            screen: Screen::Connecting,
            service_note: None,
            liked_uri: None,
            playlists: Vec::new(),
            playlists_asked: None,
            sidebar: Window::default(),
            playlist_window: Window::default(),
            listing: Listing::new(Source::Liked),
            window: Window::default(),
            history: Vec::new(),
            generation: 0,
            search: String::new(),
            focus: Focus::Tracks,
            view: View::Tracks,
            now: NowPlaying::default(),
            lyrics: None,
            lyrics_offset: 0,
            help: false,
            picker: None,
            finder: None,
            notice: None,
            mismatch: None,
            ticks: 0,
            hits: Vec::new(),
            pending_g: false,
            to_end: false,
            last_click: None,
            quit: false,
        }
    }

    // ── Login ───────────────────────────────────────────────────────────

    /// Whether the screen asks the user to log in.
    pub fn needs_login(&self) -> bool {
        match &self.screen {
            Screen::LoggedOut => true,
            Screen::Failed(failure) => failure.kind == Kind::NeedsLogin,
            _ => false,
        }
    }

    fn log_in(&mut self) {
        if !self.needs_login() {
            // Any other failure: try again now. A locked keyring asks for its
            // password again.
            if matches!(self.screen, Screen::Failed(_)) {
                self.send(Request::Reconnect);
            }
            return;
        }
        let authorization = Authorization::new();
        // The link stays on screen, so a browser that fails to open is not fatal.
        let _ = std::process::Command::new("xdg-open")
            .arg(&authorization.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        self.screen = Screen::Waiting(authorization.url.clone());
        let (http, tx, service) = (self.http.clone(), self.tx.clone(), self.service.clone());
        let page = callback_page(&self.theme);
        tokio::spawn(async move {
            // Still logged out, so the login button stays.
            let local = |error: spotify::Error| Failure::new(Kind::NeedsLogin, error.to_string());
            let result = async {
                let token = authorization.complete(&http, &page).await.map_err(local)?;
                // The user is at the window: the keyring may ask for its password.
                token
                    .store(spotify::secrets::Unlock::Ask)
                    .await
                    .map_err(local)?;
                // The service reads the login from the keyring.
                service.done(Request::Reconnect).await
            }
            .await;
            let _ = tx.send(match result {
                Ok(()) => Msg::LoggedIn,
                Err(error) => Msg::LoginFailed(error),
            });
        });
    }

    fn enter_library(&mut self) {
        self.screen = Screen::Library;
        self.load_playlists();
        self.open(Source::Liked);
        self.focus = Focus::Tracks;
    }

    // ── Messages ────────────────────────────────────────────────────────

    pub fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Service(Notification::Connected) => self.service_note = None,
            Msg::Service(Notification::Disconnected(text)) => self.service_lost(text),
            Msg::Service(Notification::Event(event)) => match event {
                Event::Snapshot(snapshot) => self.apply_snapshot(snapshot),
                Event::Status(status) => self.apply_status(status),
                Event::Player(update) => self.apply(update),
            },
            Msg::LoggedIn => self.screen = Screen::Connecting,
            Msg::LoginFailed(error) => self.screen = Screen::Failed(error),
            Msg::Playlists(Ok(playlists)) => self.playlists_arrived(playlists),
            Msg::Playlists(Err(error)) => self.warn(format!("Cannot load your playlists: {error}")),
            Msg::Uris { generation, result } if generation == self.generation => match result {
                Ok(uris) => {
                    self.listing.uris = uris.into();
                    self.listing.loading = Loading::Idle;
                    self.listing.loaded_at = Some(Instant::now());
                    self.load_more();
                }
                Err(error) => self.listing.fail(error),
            },
            Msg::Tracks {
                generation,
                start,
                chunk,
                result,
            } if generation == self.generation => match result {
                Ok(tracks) => {
                    self.listing.take_chunk(start, &chunk, tracks);
                    if self.to_end {
                        self.window.step(Step::Bottom, self.listing.tracks.len());
                    }
                    self.refilter();
                    self.load_more();
                }
                Err(error) => self.listing.fail(error),
            },
            // Results for a list the user already left.
            Msg::Uris { .. } | Msg::Tracks { .. } => {}
            Msg::Refreshed {
                generation,
                result: Ok(uris),
            } if generation == self.generation && self.listing.loading == Loading::Idle => {
                self.refreshed(uris);
            }
            // A failure keeps the list as it was; so does leaving it meanwhile.
            Msg::Refreshed { .. } => {}
            Msg::Lyrics { uri, result } => self.lyrics_arrived(uri, result),
            Msg::Notice { text, error } => self.notify(text, error),
            Msg::Liked(track) => self.liked_added(Rc::new(track)),
        }
    }

    fn service_lost(&mut self, text: String) {
        if self.screen == Screen::Library {
            self.warn(text.clone());
        } else if !matches!(self.screen, Screen::Waiting(_)) {
            self.screen = Screen::Connecting;
        }
        self.service_note = Some(text);
    }

    /// The service's whole state, sent when this window connects.
    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.mismatch = (snapshot.protocol != protocol::VERSION).then(|| {
            let text = protocol::mismatch(snapshot.protocol);
            log::warn!("{text}");
            self.service_note = Some(text.clone());
            Notice {
                text,
                error: true,
                shown: Instant::now(),
            }
        });
        match snapshot.track {
            Some(track) if self.now.uri() != Some(track.uri.as_str()) => self.track_changed(track),
            Some(_) => {}
            None => self.now.clear_track(),
        }
        self.now.report(snapshot.position_ms, snapshot.playing);
        self.now.volume = snapshot.volume;
        self.now.muted = snapshot.muted;
        self.now.shuffle = snapshot.shuffle;
        self.now.repeat = snapshot.repeat;
        self.apply_status(snapshot.status);
    }

    fn apply_status(&mut self, status: Status) {
        let waiting = matches!(self.screen, Screen::Waiting(_));
        let in_library = self.screen == Screen::Library;
        match status {
            Status::Connected { liked_uri } => {
                self.liked_uri = Some(liked_uri);
                if !in_library {
                    self.enter_library();
                } else {
                    // Back after a reconnect: playlists may have changed meanwhile,
                    // and a list that failed loads again.
                    self.load_playlists();
                    self.retry();
                    self.refresh();
                    if self.view == View::Lyrics {
                        self.ensure_lyrics();
                    }
                }
            }
            Status::Connecting if in_library => self.inform("Reconnecting to Spotify…"),
            Status::Connecting if !waiting => self.screen = Screen::Connecting,
            Status::LoggedOut if !waiting => self.screen = Screen::LoggedOut,
            // The login is gone: only logging in again helps, so show how.
            Status::Failed(error) if error.kind == Kind::NeedsLogin && !waiting => {
                self.screen = Screen::Failed(error);
            }
            Status::Failed(error) if in_library => self.warn(error.message),
            Status::Failed(error) if !waiting => self.screen = Screen::Failed(error),
            // A browser login in progress keeps its screen.
            Status::Connecting | Status::LoggedOut | Status::Failed(_) => {}
        }
    }

    fn track_changed(&mut self, track: spotify::NowTrack) {
        self.now.change_track(track);
        self.lyrics_offset = 0;
        if self.view == View::Lyrics {
            self.ensure_lyrics();
        }
    }

    fn apply(&mut self, update: PlayerUpdate) {
        log::debug!("player update: {update:?}");
        match update {
            PlayerUpdate::Track(track) => self.track_changed(track),
            PlayerUpdate::Playing { position_ms } => self.now.report(position_ms, true),
            PlayerUpdate::Paused { position_ms } => self.now.report(position_ms, false),
            PlayerUpdate::Seeked { position_ms } => self.now.seeked(position_ms),
            PlayerUpdate::Position { position_ms } => {
                let playing = self.now.playing();
                self.now.report(position_ms, playing);
            }
            PlayerUpdate::Stopped => self.now.stop(),
            PlayerUpdate::Volume(volume) => self.now.report_volume(volume),
            PlayerUpdate::Muted(muted) => self.now.muted = muted,
            PlayerUpdate::Shuffle(shuffle) => self.now.shuffle = shuffle,
            PlayerUpdate::Repeat(repeat) => self.now.repeat = repeat,
            PlayerUpdate::Unavailable => {
                self.warn("Spotify can't play this track here; skipping it.".into());
            }
            PlayerUpdate::Refusing => self.warn(
                "Spotify is refusing tracks for a moment (often after skipping fast); \
                 it moves on until one plays."
                    .into(),
            ),
        }
    }

    /// Called every quarter second: animations, notices and theme changes.
    pub fn tick(&mut self) {
        self.ticks += 1;
        self.now.settle_volume();
        if self.ticks.is_multiple_of(THEME_POLL_TICKS)
            && let Some(theme) = self.theme_source.poll()
        {
            self.theme = theme;
        }
        // Lyrics shown as failed for a passing reason try again by themselves.
        let retry = self.lyrics.as_ref().is_some_and(|cache| {
            matches!(&cache.status, LyricsStatus::Failed(failure) if failure.is_transient())
                && cache.since.elapsed() >= LYRICS_RETRY
        });
        if retry && self.view == View::Lyrics {
            self.ensure_lyrics();
        }
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice
            .as_ref()
            .filter(|notice| notice.shown.elapsed() < NOTICE_TIME)
            .or(self.mismatch.as_ref())
    }

    fn notify(&mut self, text: String, error: bool) {
        self.notice = Some(Notice {
            text,
            error,
            shown: Instant::now(),
        });
    }

    fn inform(&mut self, text: impl Into<String>) {
        self.notify(text.into(), false);
    }

    fn warn(&mut self, text: String) {
        self.notify(text, true);
    }

    // ── Browsing ────────────────────────────────────────────────────────

    /// Opens a library section, forgetting where the user drilled down from.
    fn open(&mut self, source: Source) {
        self.history.clear();
        self.view = View::Tracks;
        self.show(source);
    }

    fn drill(&mut self, source: Source) {
        let current = (self.listing.clone(), self.window);
        self.history.push(current);
        self.view = View::Tracks;
        self.show(source);
    }

    /// Returns to the previous list; false if there is none.
    fn back(&mut self) -> bool {
        let Some((listing, window)) = self.history.pop() else {
            return false;
        };
        self.generation += 1;
        self.to_end = false;
        let interrupted = listing.loading == Loading::Busy;
        self.listing = listing;
        self.window = window;
        if interrupted {
            self.listing.loading = Loading::Idle;
            self.reload();
        }
        true
    }

    /// Loads a failed list again, from where it stopped.
    fn retry(&mut self) {
        if matches!(self.listing.loading, Loading::Failed(_)) {
            self.listing.loading = Loading::Idle;
            self.reload();
        }
    }

    /// Restarts loading a list whose request was dropped.
    fn reload(&mut self) {
        if self.listing.uris.is_empty() {
            let source = self.listing.source.clone();
            self.show(source);
        } else {
            self.listing.requested = self.listing.tracks.len();
            self.load_more();
        }
    }

    fn show(&mut self, source: Source) {
        self.generation += 1;
        let generation = self.generation;
        self.to_end = false;
        self.listing = Listing::new(source.clone());
        self.window.select(0, 0);
        let target = Target::of(self.liked_uri.as_deref(), &source);
        let (tx, service) = (self.tx.clone(), self.service.clone());
        tokio::spawn(async move {
            let result = target.resolve(&service).await;
            let _ = tx.send(Msg::Uris { generation, result });
        });
    }

    /// Lists the list on screen again in the background, if it was listed a
    /// while ago: songs saved or added in another Spotify app show up. Its rows
    /// stay on screen meanwhile, and the highlighted track stays highlighted.
    fn refresh(&mut self) {
        let stale = self
            .listing
            .loaded_at
            .is_some_and(|at| at.elapsed() >= LIST_REFRESH);
        if !stale
            || self.listing.loading != Loading::Idle
            || matches!(self.listing.source, Source::Search(_))
        {
            return;
        }
        // One at a time. Moving to another list (or loading more rows) moves
        // the generation on, and this answer is dropped.
        self.listing.loaded_at = Some(Instant::now());
        let generation = self.generation;
        let target = Target::of(self.liked_uri.as_deref(), &self.listing.source);
        let (tx, service) = (self.tx.clone(), self.service.clone());
        tokio::spawn(async move {
            let result = target.resolve(&service).await;
            let _ = tx.send(Msg::Refreshed { generation, result });
        });
    }

    /// Takes the list on screen listed again: if it changed, its rows follow,
    /// with the highlighted track still highlighted wherever it moved.
    fn refreshed(&mut self, uris: Vec<String>) {
        let Some(listing) = self.listing.refreshed(uris) else {
            self.listing.loaded_at = Some(Instant::now());
            return;
        };
        log::info!("{} changed; showing it afresh", listing.source.title());
        let highlighted = self.selected().map(|track| track.uri.clone());
        // Rows on their way belong to the list before.
        self.generation += 1;
        self.listing = listing;
        let index = highlighted
            .and_then(|uri| self.listing.uris.iter().position(|listed| *listed == uri))
            .unwrap_or(self.window.selected);
        self.to_end = false;
        self.window.select(index, self.listing.uris.len());
        self.refilter();
        self.load_more();
    }

    /// Loads the details of the next tracks, if the window is close to the loaded end.
    pub fn load_more(&mut self) {
        if self.listing.retry_due() {
            self.retry();
            return;
        }
        if self.listing.loading != Loading::Idle {
            return;
        }
        // Finding needs every row; otherwise rows load as they come into view.
        let end = if self.finder.is_some() {
            self.listing.uris.len()
        } else {
            (self.window.end() + PREFETCH_ROWS).min(self.listing.uris.len())
        };
        if self.listing.requested >= end {
            return;
        }
        let start = self.listing.requested;
        self.listing.requested = (start + CHUNK).min(self.listing.uris.len());
        self.listing.loading = Loading::Busy;
        let chunk = self.listing.uris[start..self.listing.requested].to_vec();
        self.generation += 1;
        let generation = self.generation;
        let (tx, service) = (self.tx.clone(), self.service.clone());
        tokio::spawn(async move {
            let result = service.tracks(chunk.clone()).await;
            let _ = tx.send(Msg::Tracks {
                generation,
                start,
                chunk,
                result,
            });
        });
    }

    fn load_playlists(&mut self) {
        self.playlists_asked = Some(Instant::now());
        log::debug!("loading the playlists");
        let (tx, service) = (self.tx.clone(), self.service.clone());
        tokio::spawn(async move {
            let result = service.playlists().await;
            let _ = tx.send(Msg::Playlists(result));
        });
    }

    pub fn shelf_count(&self) -> usize {
        FIXED_SHELVES + self.playlists.len()
    }

    pub fn shelf_source(&self, index: usize) -> Option<Source> {
        match index {
            0 => Some(Source::Search(String::new())),
            1 => Some(Source::Liked),
            2 => Some(Source::Recent),
            _ => self
                .playlists
                .get(index - FIXED_SHELVES)
                .map(|playlist| Source::Playlist(playlist.clone())),
        }
    }

    /// Whether the sidebar entry is the list on screen.
    pub fn shelf_is_current(&self, index: usize) -> bool {
        match (index, &self.listing.source) {
            (0, Source::Search(_)) => true,
            (0, _) => false,
            _ => self
                .shelf_source(index)
                .is_some_and(|source| source == self.listing.source),
        }
    }

    /// `l` or Enter in the sidebar: into the highlighted list. The list already on
    /// screen is entered where it was left, as Yazi returns to a folder.
    fn enter_shelf(&mut self, index: usize) {
        if !self.shelf_is_current(index) {
            self.open_shelf(index);
            return;
        }
        // Entering a list that failed to load tries again; one listed a while
        // ago is listed again, as it may have changed elsewhere.
        self.retry();
        self.refresh();
        self.sidebar.select(index, self.shelf_count());
        self.view = View::Tracks;
        self.focus = match &self.listing.source {
            Source::Search(text) if text.is_empty() => Focus::Search,
            _ => Focus::Tracks,
        };
    }

    /// `h`: up one level, as in Yazi. Lyrics close, an opened album or artist
    /// returns to the list it came from, and a top-level list hands over to the
    /// sidebar with that list highlighted.
    fn go_back(&mut self) {
        if self.view == View::Lyrics {
            self.view = View::Tracks;
            return;
        }
        if self.focus == Focus::Sidebar || self.back() {
            return;
        }
        if let Some(index) = (0..self.shelf_count()).find(|index| self.shelf_is_current(*index)) {
            self.sidebar.select(index, self.shelf_count());
        }
        self.focus_sidebar();
    }

    /// Moves to the sidebar, loading the playlists again if they may be stale:
    /// one made or renamed in another Spotify app shows up.
    pub(crate) fn focus_sidebar(&mut self) {
        self.focus = Focus::Sidebar;
        if self
            .playlists_asked
            .is_none_or(|asked| asked.elapsed() >= PLAYLISTS_REFRESH)
        {
            self.load_playlists();
        }
    }

    /// New playlists replace the old; the highlighted one stays highlighted.
    fn playlists_arrived(&mut self, playlists: Vec<Playlist>) {
        let highlighted = self
            .sidebar
            .selected
            .checked_sub(FIXED_SHELVES)
            .and_then(|index| self.playlists.get(index))
            .map(|playlist| playlist.uri.clone());
        self.playlists = playlists.into_iter().map(Rc::new).collect();
        if let Some(uri) = highlighted {
            let index = self
                .playlists
                .iter()
                .position(|playlist| playlist.uri == uri)
                .map_or(self.sidebar.selected, |index| FIXED_SHELVES + index);
            self.sidebar.select(index, self.shelf_count());
        }
    }

    /// Shows a track just saved at the top of Liked Songs, wherever that list is
    /// loaded: on screen or back in the history. Highlights stay on their songs.
    fn liked_added(&mut self, track: Rc<Track>) {
        let add = |listing: &mut Listing, window: &mut Window| {
            if listing.source != Source::Liked || listing.uris.is_empty() {
                return false;
            }
            let had_rows = !listing.tracks.is_empty();
            if !listing.insert_first(track.clone()) {
                return false;
            }
            if had_rows {
                window.select(window.selected + 1, listing.tracks.len());
            }
            true
        };
        for (listing, window) in &mut self.history {
            add(listing, window);
        }
        if add(&mut self.listing, &mut self.window) && self.listing.loading == Loading::Busy {
            // The chunk on its way was asked for before the shift; ask again.
            self.generation += 1;
            self.listing.loading = Loading::Idle;
            self.listing.requested = self.listing.tracks.len();
            self.load_more();
        }
    }

    fn open_shelf(&mut self, index: usize) {
        self.sidebar.select(index, self.shelf_count());
        match self.shelf_source(index) {
            Some(Source::Search(_)) => self.start_search(),
            Some(source) => {
                self.open(source);
                self.focus = Focus::Tracks;
            }
            None => {}
        }
    }

    /// `/`: an empty search box, every time; results of a past search stay listed.
    fn start_search(&mut self) {
        self.search.clear();
        if !self.searching() {
            self.open(Source::Search(String::new()));
        }
        self.view = View::Tracks;
        self.focus = Focus::Search;
    }

    fn submit_search(&mut self) {
        let text = self.search.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.open(Source::Search(text));
        self.focus = Focus::Tracks;
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn searching(&self) -> bool {
        matches!(self.listing.source, Source::Search(_))
    }

    fn move_selection(&mut self, step: Step) {
        self.window.step(step, self.listing.tracks.len());
        // The last row may not be loaded yet; keep following the end as rows arrive.
        self.to_end = step == Step::Bottom;
        self.load_more();
    }

    fn selected(&self) -> Option<Rc<Track>> {
        self.listing.tracks.get(self.window.selected).cloned()
    }

    fn play_selected(&mut self) {
        let Some(track) = self.selected() else {
            return;
        };
        let uri = track.uri.clone();
        let play = match &self.listing.source {
            Source::Liked => {
                let Some(liked) = self.liked_uri.clone() else {
                    return;
                };
                Play::Context {
                    uri: liked,
                    track: uri,
                }
            }
            Source::Playlist(playlist) => Play::Context {
                uri: playlist.uri.clone(),
                track: uri,
            },
            Source::Album(album) => Play::Context {
                uri: album.uri.clone(),
                track: uri,
            },
            Source::Artist(artist) => Play::Context {
                uri: artist.uri.clone(),
                track: uri,
            },
            Source::Search(_) | Source::Recent => Play::Tracks {
                uris: self.listing.uris.to_vec(),
                track: uri,
            },
        };
        let mode = Mode {
            shuffle: self.now.shuffle,
            repeat: self.now.repeat,
        };
        let context_name = self.listing.source.title();
        self.send(Request::Play {
            play,
            mode,
            context_name,
        });
    }

    fn open_album(&mut self) {
        match self.selected() {
            Some(track) => self.drill(Source::Album(Rc::new(track.album.clone()))),
            None => self.inform("Select a track to open its album."),
        }
    }

    fn open_artist(&mut self) {
        match self
            .selected()
            .and_then(|track| track.artists.first().cloned())
        {
            Some(artist) => self.drill(Source::Artist(Rc::new(artist))),
            None => self.inform("Select a track to open its artist."),
        }
    }

    /// Queues the selected track: after the tracks queued before it, or with
    /// `next`, right after the playing one.
    fn queue_selected(&mut self, next: bool) {
        let Some(track) = self.selected() else {
            self.inform("Select a track to add it to the queue.");
            return;
        };
        if self.now.track.is_none() {
            self.inform("Play a track first; queued tracks play after it.");
            return;
        }
        let (tx, service, name) = (self.tx.clone(), self.service.clone(), track.name.clone());
        let uri = track.uri.clone();
        let (request, done) = if next {
            (Request::PlayNext(uri), format!("“{name}” plays next."))
        } else {
            (
                Request::AddToQueue(uri),
                format!("Added “{name}” to the queue."),
            )
        };
        tokio::spawn(async move {
            let msg = match service.done(request).await {
                Ok(()) => Msg::Notice {
                    text: done,
                    error: false,
                },
                Err(error) => Msg::Notice {
                    text: error.message,
                    error: true,
                },
            };
            let _ = tx.send(msg);
        });
    }

    /// Saves the selected track to Liked Songs.
    fn like_selected(&mut self) {
        let Some(track) = self.selected() else {
            self.inform("Select a track to save it to Liked Songs.");
            return;
        };
        let (tx, service, name) = (self.tx.clone(), self.service.clone(), track.name.clone());
        let uri = track.uri.clone();
        let saved = Track::clone(&track);
        tokio::spawn(async move {
            let msg = match service.like(uri).await {
                Ok(true) => {
                    let _ = tx.send(Msg::Liked(saved));
                    Msg::Notice {
                        text: format!("Saved “{name}” to Liked Songs."),
                        error: false,
                    }
                }
                Ok(false) => Msg::Notice {
                    text: format!("“{name}” is already in Liked Songs."),
                    error: false,
                },
                Err(error) => Msg::Notice {
                    text: error.message,
                    error: true,
                },
            };
            let _ = tx.send(msg);
        });
    }

    // ── Playback ────────────────────────────────────────────────────────

    /// Sends a request whose only answer is success; failures show as a notice.
    fn send(&self, request: Request) {
        let (tx, service) = (self.tx.clone(), self.service.clone());
        tokio::spawn(async move {
            if let Err(failure) = service.done(request).await {
                let _ = tx.send(Msg::Notice {
                    text: failure.message,
                    error: true,
                });
            }
        });
    }

    fn command(&mut self, command: Command) {
        self.send(Request::Command(command));
    }

    fn toggle_play(&mut self) {
        if self.now.track.is_none() {
            self.play_selected();
        } else if self.now.playing() {
            self.command(Command::Pause);
        } else {
            self.command(Command::Resume);
        }
    }

    fn seek_by(&mut self, forward: bool) {
        let elapsed = self.now.elapsed();
        let target = if forward {
            elapsed + SEEK_STEP
        } else {
            elapsed.saturating_sub(SEEK_STEP)
        };
        self.seek_to(target.as_millis().try_into().unwrap_or(u64::MAX));
    }

    fn seek_to(&mut self, position_ms: u64) {
        if self.now.track.is_none() {
            return;
        }
        let length = self.now.length().as_millis().try_into().unwrap_or(u64::MAX);
        // Positions past u32 milliseconds (49 days) clamp to the end of the track.
        let position: u32 = position_ms.min(length).try_into().unwrap_or(u32::MAX);
        log::debug!("seek from {}ms to {position}ms", self.now.elapsed_ms());
        self.command(Command::Seek(position));
        // Show the new position now, so quick repeated seeks add up before the speaker reports.
        self.now.seek(position);
    }

    fn change_volume(&mut self, up: bool) {
        let volume = playback::volume_step(self.now.volume, up);
        // The service keeps it for when the speaker becomes active, and saves it.
        self.command(Command::Volume(volume));
        // As with seeking, quick repeated presses build on the volume just asked for.
        self.now.set_volume(volume);
    }

    /// Mutes or unmutes; the volume level stays as it is.
    fn toggle_mute(&mut self) {
        self.now.muted = !self.now.muted;
        self.send(Request::ToggleMute);
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::TogglePlay => self.toggle_play(),
            Action::Next => self.command(Command::Next),
            Action::Previous => self.command(Command::Previous),
            Action::Shuffle => {
                let shuffle = !self.now.shuffle;
                self.command(Command::Shuffle(shuffle));
            }
            Action::Repeat => {
                let repeat = self.now.repeat.next();
                self.command(Command::Repeat(repeat));
            }
            Action::Search => self.start_search(),
            Action::Lyrics => self.toggle_lyrics(),
            Action::Help => self.help = true,
            Action::CloseHelp => self.help = false,
            Action::ClosePicker => self.close_picker(),
            Action::CloseFinder => self.close_finder(),
            Action::LogIn => self.log_in(),
        }
    }

    // ── Lyrics ──────────────────────────────────────────────────────────

    fn toggle_lyrics(&mut self) {
        self.view = match self.view {
            View::Tracks => View::Lyrics,
            View::Lyrics => View::Tracks,
        };
        if self.view == View::Lyrics {
            if self.focus == Focus::Search {
                self.focus = Focus::Tracks;
            }
            self.lyrics_offset = 0;
            self.ensure_lyrics();
        }
    }

    /// Fetches the playing track's lyrics unless they are loaded or loading.
    fn ensure_lyrics(&mut self) {
        let Some(track) = self.now.track.clone() else {
            return;
        };
        let current = self.lyrics.as_ref().is_some_and(|cache| {
            cache.uri == track.uri && !matches!(cache.status, LyricsStatus::Failed(_))
        });
        if current {
            return;
        }
        self.lyrics = Some(LyricsCache::new(track.uri.clone(), LyricsStatus::Loading));
        let (service, tx) = (self.service.clone(), self.tx.clone());
        tokio::spawn(async move {
            let uri = track.uri.clone();
            let result = service.lyrics(track).await;
            let _ = tx.send(Msg::Lyrics { uri, result });
        });
    }

    fn lyrics_arrived(&mut self, uri: String, result: Result<Option<Lyrics>, Failure>) {
        let status = match result {
            Ok(Some(lyrics)) => LyricsStatus::Ready(Rc::new(lyrics)),
            Ok(None) => LyricsStatus::Missing,
            Err(error) => LyricsStatus::Failed(error),
        };
        if self.lyrics.as_ref().is_some_and(|cache| cache.uri == uri) {
            self.lyrics = Some(LyricsCache::new(uri, status));
        }
    }

    /// The loaded lyrics of the playing track.
    pub fn current_lyrics(&self) -> Option<Rc<Lyrics>> {
        let cache = self.lyrics.as_ref()?;
        match &cache.status {
            LyricsStatus::Ready(lyrics) if Some(cache.uri.as_str()) == self.now.uri() => {
                Some(lyrics.clone())
            }
            _ => None,
        }
    }

    /// The line the lyrics view points at: the sung line, moved by manual scrolling.
    pub fn lyrics_cursor(&self) -> Option<usize> {
        let lyrics = self.current_lyrics()?;
        let base = if lyrics.synced {
            lyrics.current(self.now.elapsed_ms()).unwrap_or(0)
        } else {
            0
        };
        let last = lyrics.lines.len().checked_sub(1)?;
        Some(base.saturating_add_signed(self.lyrics_offset).min(last))
    }

    fn scroll_lyrics(&mut self, rows: isize) {
        let Some(lyrics) = self.current_lyrics() else {
            return;
        };
        let base = if lyrics.synced {
            lyrics.current(self.now.elapsed_ms()).unwrap_or(0)
        } else {
            0
        };
        let last = lyrics.lines.len().saturating_sub(1);
        let target = base
            .saturating_add_signed(self.lyrics_offset + rows)
            .min(last);
        self.lyrics_offset = target as isize - base as isize;
    }

    /// Jumps playback to a synced lyrics line.
    fn seek_to_lyric(&mut self, index: usize) {
        let Some(start) = self
            .current_lyrics()
            .and_then(|lyrics| lyrics.lines.get(index)?.start_ms)
        else {
            return;
        };
        self.lyrics_offset = 0;
        self.seek_to(start);
    }
}

/// Where a list's track URIs come from, in a form background tasks can own.
enum Target {
    Nothing,
    Recent,
    Context(String),
    /// Liked Songs before the service said where they are.
    Unknown,
}

impl Target {
    fn of(liked_uri: Option<&str>, source: &Source) -> Self {
        match source {
            Source::Search(text) if text.is_empty() => Self::Nothing,
            Source::Recent => Self::Recent,
            Source::Search(text) => Self::Context(spotify::search_uri(text)),
            Source::Liked => liked_uri.map_or(Self::Unknown, |uri| Self::Context(uri.to_owned())),
            Source::Playlist(playlist) => Self::Context(playlist.uri.clone()),
            Source::Album(album) => Self::Context(album.uri.clone()),
            Source::Artist(artist) => Self::Context(artist.uri.clone()),
        }
    }

    async fn resolve(self, service: &Service) -> Result<Vec<String>, Failure> {
        match self {
            Self::Nothing => Ok(Vec::new()),
            Self::Recent => service.uris(Request::RecentTracks).await,
            Self::Context(uri) => service.uris(Request::ContextTracks(uri)).await,
            Self::Unknown => Err(Failure::new(
                Kind::NotConnected,
                "Not connected to Spotify yet",
            )),
        }
    }
}

/// The page the browser shows after login, in the theme's colors.
fn callback_page(theme: &Theme) -> String {
    let hex = |color| match color {
        ratatui::style::Color::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        _ => "#888888".into(),
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>spotitui</title>\
         <style>body{{margin:0;height:100vh;display:grid;place-items:center;\
         background:{};color:{};font:16px ui-monospace,monospace}}strong{{color:{}}}</style>\
         </head><body><p><strong>You're logged in.</strong> Return to your terminal.</p></body></html>",
        hex(theme.background),
        hex(theme.text),
        hex(theme.accent),
    )
}

#[cfg(test)]
mod tests {
    use super::{App, LYRICS_RETRY, LyricsCache, LyricsStatus, View};
    use crate::{
        client::Service,
        protocol::{Failure, Kind},
        spotify::NowTrack,
        theme::ThemeSource,
    };
    use std::time::Instant;

    fn showing_failed_lyrics(kind: Kind, view: View) -> App {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, crate::spotify::http(), ThemeSource::new(), Service::detached());
        app.now.change_track(NowTrack {
            uri: "spotify:track:t".into(),
            name: "Track".into(),
            artists: Vec::new(),
            album: String::new(),
            album_artists: Vec::new(),
            duration_ms: 1,
            art_url: None,
        });
        app.view = view;
        app.lyrics = Some(LyricsCache {
            uri: "spotify:track:t".into(),
            status: LyricsStatus::Failed(Failure::new(kind, "no")),
            since: Instant::now() - LYRICS_RETRY,
        });
        app
    }

    fn loading(app: &App) -> bool {
        matches!(app.lyrics.as_ref().map(|cache| &cache.status), Some(LyricsStatus::Loading))
    }

    #[tokio::test]
    async fn lyrics_that_failed_for_a_passing_reason_load_again_while_shown() {
        let mut app = showing_failed_lyrics(Kind::Network, View::Lyrics);
        app.tick();
        assert!(loading(&app), "tried again");

        let mut hidden = showing_failed_lyrics(Kind::Network, View::Tracks);
        hidden.tick();
        assert!(!loading(&hidden), "not while the lyrics aren't shown");

        let mut lasting = showing_failed_lyrics(Kind::Other, View::Lyrics);
        lasting.tick();
        assert!(!loading(&lasting), "a lasting failure waits for the user");
    }
}
