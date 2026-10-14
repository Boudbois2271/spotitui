//! "Find in this list" (`^f`): a filter over the list on screen, typed first,
//! then browsed with the usual keys. The highlighted match is also the main
//! list's selected row, so every track action works on it unchanged.

use super::{App, Focus, View, listing::Step, listing::Window};
use crate::spotify::Track;

pub struct Finder {
    pub query: String,
    /// Typing the query; otherwise browsing the matches.
    pub typing: bool,
    /// Indices into the list's loaded tracks, in list order.
    pub matches: Vec<usize>,
    pub window: Window,
}

impl Finder {
    /// The list index of the highlighted match.
    pub fn current(&self) -> Option<usize> {
        self.matches.get(self.window.selected).copied()
    }
}

/// Whether every word of the query appears in the title, artists or album,
/// ignoring case. An empty query matches everything.
pub fn matches(track: &Track, query: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        track.name,
        track.artist_names(),
        track.album.name
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

impl App {
    pub(super) fn open_finder(&mut self) {
        if self.listing.uris.is_empty() {
            self.inform("There is nothing to find in this list.");
            return;
        }
        self.view = View::Tracks;
        self.focus = Focus::Tracks;
        self.finder = Some(Finder {
            query: String::new(),
            typing: true,
            matches: Vec::new(),
            window: Window::default(),
        });
        self.refilter();
        // Every row must be loaded to be found; the rest loads in the background.
        self.load_more();
    }

    pub(super) fn close_finder(&mut self) {
        self.finder = None;
    }

    /// Recomputes the matches, keeping the highlight on the same track if it still matches.
    pub(super) fn refilter(&mut self) {
        let Some(finder) = self.finder.as_mut() else {
            return;
        };
        let keep = finder.current();
        finder.matches = self
            .listing
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| matches(track, &finder.query))
            .map(|(index, _)| index)
            .collect();
        let at = keep
            .and_then(|index| finder.matches.iter().position(|found| *found == index))
            .unwrap_or(0);
        finder.window.select(at, finder.matches.len());
        self.sync_finder();
    }

    pub(super) fn finder_step(&mut self, step: Step) {
        if let Some(finder) = self.finder.as_mut() {
            finder.window.step(step, finder.matches.len());
        }
        self.sync_finder();
    }

    pub(super) fn finder_select(&mut self, row: usize) {
        if let Some(finder) = self.finder.as_mut() {
            finder.window.select(row, finder.matches.len());
            finder.typing = false;
        }
        self.sync_finder();
    }

    /// Puts the main list's selection on the highlighted match.
    fn sync_finder(&mut self) {
        let Some(index) = self.finder.as_ref().and_then(Finder::current) else {
            return;
        };
        self.to_end = false;
        self.window.select(index, self.listing.tracks.len());
    }

    /// How much of the list is loaded, while more is on its way.
    pub fn finder_progress(&self) -> Option<(usize, usize)> {
        let listing = &self.listing;
        let total = listing.uris.len();
        let more = listing.requested < total || listing.loading == super::Loading::Busy;
        more.then_some((listing.tracks.len(), total))
    }
}

#[cfg(test)]
mod tests {
    use super::matches;
    use crate::spotify::{Album, Artist, Track};

    fn track() -> Track {
        Track {
            uri: "spotify:track:x".into(),
            name: "Harder, Better, Faster, Stronger".into(),
            artists: vec![Artist {
                uri: "spotify:artist:d".into(),
                name: "Daft Punk".into(),
            }],
            album: Album {
                uri: "spotify:album:a".into(),
                name: "Discovery".into(),
            },
            duration_ms: 224_000,
        }
    }

    #[test]
    fn every_word_must_match_somewhere_ignoring_case() {
        assert!(matches(&track(), ""));
        assert!(matches(&track(), "harder"));
        assert!(matches(&track(), "DAFT discovery"));
        assert!(matches(&track(), "punk faster"));
        assert!(!matches(&track(), "daft homework"));
    }
}
