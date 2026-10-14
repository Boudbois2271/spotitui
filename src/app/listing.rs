//! What the main pane lists, and the scrolled window over it.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use crate::{
    protocol::Failure,
    spotify::{Album, Artist, Playlist, Track},
};
use std::{
    collections::HashSet,
    ops::Range,
    rc::Rc,
    time::{Duration, Instant},
};

/// The first wait before loading again after a temporary failure; it doubles
/// with each failure in a row, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// Something the main pane can list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Search(String),
    Liked,
    Recent,
    Playlist(Rc<Playlist>),
    Album(Rc<Album>),
    Artist(Rc<Artist>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loading {
    Idle,
    Busy,
    Failed(Failure),
}

/// The tracks of a source. Every track URI is known up front; details load as rows come into view.
#[derive(Clone, Debug)]
pub struct Listing {
    pub source: Source,
    pub uris: Rc<[String]>,
    pub tracks: Vec<Rc<Track>>,
    pub requested: usize,
    pub loading: Loading,
    /// When the last failure happened, and how many failed in a row.
    failed_at: Option<Instant>,
    failures: u32,
}

/// How a key or the wheel moves a selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
}

/// The visible slice of a list and its selected entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub selected: usize,
    top: usize,
    rows: usize,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            selected: 0,
            top: 0,
            rows: 1,
        }
    }
}

impl Window {
    pub fn resize(&mut self, rows: usize) {
        self.rows = rows.max(1);
        self.reveal();
    }

    pub fn select(&mut self, index: usize, length: usize) {
        self.selected = index.min(length.saturating_sub(1));
        self.reveal();
    }

    pub fn step(&mut self, step: Step, length: usize) {
        let half = (self.rows / 2).max(1);
        let target = match step {
            Step::Up => self.selected.saturating_sub(1),
            Step::Down => self.selected + 1,
            Step::PageUp => self.selected.saturating_sub(self.rows),
            Step::PageDown => self.selected + self.rows,
            Step::HalfUp => self.selected.saturating_sub(half),
            Step::HalfDown => self.selected + half,
            Step::Top => 0,
            Step::Bottom => length,
        };
        self.select(target, length);
    }

    pub fn visible(&self, length: usize) -> Range<usize> {
        self.top.min(length)..(self.top + self.rows).min(length)
    }

    /// The first row past the window, which should be loaded before it scrolls into view.
    pub fn end(&self) -> usize {
        self.top + self.rows
    }

    fn reveal(&mut self) {
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.rows {
            self.top = self.selected + 1 - self.rows;
        }
    }
}

impl Source {
    pub fn title(&self) -> String {
        match self {
            Self::Search(text) if text.is_empty() => "Search".into(),
            Self::Search(text) => format!("Search · “{text}”"),
            Self::Liked => "Liked Songs".into(),
            Self::Recent => "Recently played".into(),
            Self::Playlist(playlist) => playlist.name.clone(),
            Self::Album(album) => format!("Album · {}", album.name),
            Self::Artist(artist) => format!("Artist · {}", artist.name),
        }
    }

    pub fn empty_message(&self) -> &'static str {
        match self {
            Self::Search(text) if text.is_empty() => "Type what you want to hear and press Enter.",
            Self::Search(_) => "Nothing matched. Try other words.",
            Self::Liked => "Songs you like in Spotify appear here.",
            Self::Recent => "Nothing played recently.",
            Self::Playlist(_) | Self::Album(_) | Self::Artist(_) => "No playable tracks here.",
        }
    }
}

impl Listing {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            uris: Rc::from([]),
            tracks: Vec::new(),
            requested: 0,
            loading: Loading::Busy,
            failed_at: None,
            failures: 0,
        }
    }

    /// Puts a track first, as Liked Songs lists the latest saved first. Returns
    /// whether it was added (it wasn't listed yet).
    pub fn insert_first(&mut self, track: Rc<Track>) -> bool {
        if self.uris.iter().any(|uri| *uri == track.uri) {
            return false;
        }
        let uris: Vec<String> = std::iter::once(track.uri.clone())
            .chain(self.uris.iter().cloned())
            .collect();
        self.uris = uris.into();
        self.tracks.insert(0, track);
        self.requested += 1;
        true
    }

    pub fn fail(&mut self, failure: Failure) {
        self.loading = Loading::Failed(failure);
        self.failed_at = Some(Instant::now());
        self.failures = self.failures.saturating_add(1);
    }

    /// Whether a failure is temporary and has waited long enough to try again.
    pub fn retry_due(&self) -> bool {
        let Loading::Failed(failure) = &self.loading else {
            return false;
        };
        let wait = RETRY_FIRST
            .saturating_mul(2u32.saturating_pow(self.failures.saturating_sub(1)))
            .min(RETRY_MAX);
        failure.is_transient() && self.failed_at.is_some_and(|at| at.elapsed() >= wait)
    }

    /// Takes the details of the URIs `chunk`, asked for from row `start`. Tracks
    /// Spotify couldn't describe are left out of the list too, so every row has
    /// its track and later loads ask for the right ones.
    pub fn take_chunk(&mut self, start: usize, chunk: &[String], found: Vec<Track>) {
        if start != self.tracks.len() {
            return;
        }
        let asked: HashSet<&str> = chunk.iter().map(String::as_str).collect();
        let found: Vec<Track> = found
            .into_iter()
            .filter(|track| asked.contains(track.uri.as_str()))
            .collect();
        if found.len() != chunk.len() {
            let kept: HashSet<&str> = found.iter().map(|track| track.uri.as_str()).collect();
            let after = (start + chunk.len()).min(self.uris.len());
            let uris: Vec<String> = self.uris[..start]
                .iter()
                .chain(chunk.iter().filter(|uri| kept.contains(uri.as_str())))
                .chain(&self.uris[after..])
                .cloned()
                .collect();
            log::info!(
                "{} of {} tracks have no details; left out",
                chunk.len() - found.len(),
                chunk.len()
            );
            self.uris = uris.into();
        }
        self.tracks.extend(found.into_iter().map(Rc::new));
        self.requested = self.tracks.len();
        self.loading = Loading::Idle;
        self.failed_at = None;
        self.failures = 0;
    }

    pub fn summary(&self) -> String {
        match &self.loading {
            Loading::Busy if self.tracks.is_empty() => "Loading…".into(),
            Loading::Failed(error) => error.message.clone(),
            _ if self.uris.len() == 1 => "1 song".into(),
            _ => format!("{} songs", self.uris.len()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.uris.is_empty() && self.loading == Loading::Idle
    }
}

/// Formats milliseconds as `m:ss`, or `h:mm:ss` for an hour or more.
pub fn duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::{Listing, Loading, Source, Step, Window, duration};
    use crate::{
        protocol::{Failure, Kind},
        spotify::{Album, Track},
    };
    use std::time::{Duration, Instant};

    fn track(uri: &str) -> Track {
        Track {
            uri: uri.into(),
            name: uri.into(),
            artists: Vec::new(),
            album: Album {
                uri: "spotify:album:a".into(),
                name: "A".into(),
            },
            duration_ms: 1,
        }
    }

    fn listing(count: usize) -> Listing {
        let mut listing = Listing::new(Source::Liked);
        listing.uris = (0..count).map(|i| format!("spotify:track:{i}")).collect();
        listing.loading = Loading::Idle;
        listing
    }

    #[test]
    fn rows_stay_with_their_tracks_when_some_have_no_details() {
        let mut listing = listing(6);
        let chunk: Vec<String> = listing.uris[0..3].to_vec();
        listing.requested = 3;
        // Spotify knows nothing about track 1.
        listing.take_chunk(0, &chunk, vec![track("spotify:track:0"), track("spotify:track:2")]);
        assert_eq!(listing.tracks.len(), 2);
        assert_eq!(listing.requested, 2);
        assert_eq!(
            listing.uris.iter().map(String::as_str).collect::<Vec<_>>(),
            ["spotify:track:0", "spotify:track:2", "spotify:track:3", "spotify:track:4", "spotify:track:5"]
        );
        // The next chunk starts at the right row: nothing loads twice.
        let next: Vec<String> = listing.uris[2..5].to_vec();
        assert_eq!(next[0], "spotify:track:3");
        listing.take_chunk(2, &next, next.iter().map(|uri| track(uri)).collect());
        let rows: Vec<&str> = listing.tracks.iter().map(|track| track.uri.as_str()).collect();
        assert_eq!(&rows[..], &listing.uris[..5]);
        // A late answer for an older position is ignored.
        listing.take_chunk(0, &chunk, vec![track("spotify:track:0")]);
        assert_eq!(listing.tracks.len(), 5);
    }

    #[test]
    fn a_saved_track_goes_first_once() {
        let mut listing = listing(3);
        let chunk: Vec<String> = listing.uris[..2].to_vec();
        listing.requested = 2;
        listing.take_chunk(0, &chunk, chunk.iter().map(|uri| track(uri)).collect());
        assert!(listing.insert_first(std::rc::Rc::new(track("spotify:track:new"))));
        assert_eq!(listing.uris[0], "spotify:track:new");
        assert_eq!(listing.tracks[0].uri, "spotify:track:new");
        assert_eq!((listing.uris.len(), listing.tracks.len(), listing.requested), (4, 3, 3));
        assert!(!listing.insert_first(std::rc::Rc::new(track("spotify:track:new"))));
    }

    #[test]
    fn only_temporary_failures_retry_and_they_wait_longer_each_time() {
        let mut listing = listing(3);
        listing.fail(Failure::new(Kind::Network, "offline"));
        assert!(!listing.retry_due(), "not right away");
        listing.failed_at = Some(Instant::now() - Duration::from_secs(5));
        assert!(listing.retry_due());
        listing.fail(Failure::new(Kind::Network, "offline"));
        listing.failed_at = Some(Instant::now() - Duration::from_secs(6));
        assert!(!listing.retry_due(), "the second wait is 10 s");
        listing.failed_at = Some(Instant::now() - Duration::from_secs(10));
        assert!(listing.retry_due());
        listing.fail(Failure::new(Kind::Other, "broken"));
        listing.failed_at = Some(Instant::now() - Duration::from_secs(600));
        assert!(!listing.retry_due(), "a lasting failure waits for the user");
    }

    #[test]
    fn durations_use_minutes_and_hours() {
        assert_eq!(duration(0), "0:00");
        assert_eq!(duration(65_400), "1:05");
        assert_eq!(duration(3_723_000), "1:02:03");
    }

    #[test]
    fn window_keeps_the_selection_visible() {
        let mut window = Window::default();
        window.resize(3);
        for _ in 0..4 {
            window.step(Step::Down, 10);
        }
        assert_eq!(window.selected, 4);
        assert_eq!(window.visible(10), 2..5);
        window.step(Step::Bottom, 10);
        assert_eq!(window.visible(10), 7..10);
        window.step(Step::PageUp, 10);
        assert_eq!((window.selected, window.visible(10)), (6, 6..9));
        window.step(Step::Top, 10);
        assert_eq!(window.visible(10), 0..3);
    }

    #[test]
    fn half_pages_and_short_lists() {
        let mut window = Window::default();
        window.resize(10);
        window.step(Step::HalfDown, 100);
        assert_eq!(window.selected, 5);
        window.step(Step::PageDown, 2);
        assert_eq!((window.selected, window.visible(2)), (1, 0..2));
        window.select(9, 0);
        assert_eq!((window.selected, window.visible(0)), (0, 0..0));
    }
}
