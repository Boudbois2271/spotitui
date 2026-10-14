//! The "add to playlist" picker (`A`): the playlists the selected track can go in.

use super::{App, Msg, listing::Step, listing::Window};
use crate::{
    protocol::Request,
    spotify::{Playlist, Track},
};
use std::rc::Rc;

pub struct Picker {
    pub track: Rc<Track>,
    /// The playlists tracks can be added to: the user's own and collaborative ones.
    pub playlists: Vec<Rc<Playlist>>,
    pub window: Window,
    /// Playlists ticked with Space.
    pub marked: Vec<bool>,
}

impl Picker {
    pub fn marked_count(&self) -> usize {
        self.marked.iter().filter(|marked| **marked).count()
    }

    /// The ticked playlists, or the highlighted one when none is ticked.
    fn targets(&self) -> Vec<Rc<Playlist>> {
        let ticked: Vec<Rc<Playlist>> = self
            .playlists
            .iter()
            .zip(&self.marked)
            .filter(|(_, marked)| **marked)
            .map(|(playlist, _)| playlist.clone())
            .collect();
        if ticked.is_empty() {
            self.playlists
                .get(self.window.selected)
                .cloned()
                .into_iter()
                .collect()
        } else {
            ticked
        }
    }

    pub fn step(&mut self, step: Step) {
        self.window.step(step, self.playlists.len());
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(marked) = self.marked.get_mut(index) {
            *marked = !*marked;
        }
    }
}

impl App {
    pub(super) fn open_picker(&mut self) {
        let Some(track) = self.selected() else {
            self.inform("Select a track to add it to a playlist.");
            return;
        };
        if self.playlists.is_empty() {
            self.inform("Your playlists are still loading.");
            return;
        }
        let playlists: Vec<Rc<Playlist>> = self
            .playlists
            .iter()
            .filter(|playlist| playlist.editable)
            .cloned()
            .collect();
        if playlists.is_empty() {
            self.inform("None of your playlists can take tracks; Spotify only allows your own.");
            return;
        }
        // Start on the playlist on screen, if it is one of them.
        let mut window = Window::default();
        if let Some(index) = playlists.iter().position(|playlist| {
            matches!(&self.listing.source, super::Source::Playlist(shown) if shown.uri == playlist.uri)
        }) {
            window.select(index, playlists.len());
        }
        self.picker = Some(Picker {
            marked: vec![false; playlists.len()],
            track,
            playlists,
            window,
        });
    }

    pub(super) fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Adds the track to the ticked playlists (or the highlighted one) and closes.
    pub(super) fn submit_picker(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let targets = picker.targets();
        if targets.is_empty() {
            return;
        }
        let names = targets
            .iter()
            .map(|playlist| playlist.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let request = Request::AddToPlaylists {
            track: picker.track.uri.clone(),
            playlists: targets
                .iter()
                .map(|playlist| playlist.uri.clone())
                .collect(),
        };
        let (tx, service, title) = (
            self.tx.clone(),
            self.service.clone(),
            picker.track.name.clone(),
        );
        tokio::spawn(async move {
            let msg = match service.done(request).await {
                Ok(()) => Msg::Notice {
                    text: format!("Added “{title}” to {names}."),
                    error: false,
                },
                Err(error) => Msg::Notice {
                    text: format!("Couldn't add “{title}”: {error}"),
                    error: true,
                },
            };
            let _ = tx.send(msg);
        });
    }
}
