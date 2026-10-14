//! What the main pane lists, and the scrolled window over it.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use crate::spotify::{Album, Artist, Playlist, Track};
use std::{ops::Range, rc::Rc};

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
    Failed(String),
}

/// The tracks of a source. Every track URI is known up front; details load as rows come into view.
#[derive(Clone, Debug)]
pub struct Listing {
    pub source: Source,
    pub uris: Rc<[String]>,
    pub tracks: Vec<Rc<Track>>,
    pub requested: usize,
    pub loading: Loading,
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
        }
    }

    pub fn summary(&self) -> String {
        match &self.loading {
            Loading::Busy if self.tracks.is_empty() => "Loading…".into(),
            Loading::Failed(error) => error.clone(),
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
    use super::{Step, Window, duration};

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
