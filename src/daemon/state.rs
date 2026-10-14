//! What the service knows: its connection to Spotify and what the speaker plays.

use crate::{
    protocol::{self, PlayerUpdate, Snapshot, Status},
    spotify::{
        self, Catalog, Play, Speaker,
        playing::{self, Playing},
    },
};
use librespot_playback::player::PlayerEvent;
use std::{collections::HashMap, path::PathBuf, sync::Arc};

const DEFAULT_VOLUME: u16 = u16::MAX / 2;

/// A connected session.
pub struct Live {
    pub speaker: Arc<Speaker>,
    pub catalog: Catalog,
}

pub struct State {
    pub status: Status,
    pub live: Option<Live>,
    /// What the speaker plays; MPRIS reads the same record.
    pub playing: playing::Shared,
    /// A volume asked for that the speaker hasn't confirmed. Spirc ignores volume
    /// changes until it is the active device, so this is sent again when playback starts.
    pub wanted_volume: Option<u16>,
    /// Names of contexts by URI, for the bar widget's queue header.
    context_names: HashMap<String, String>,
    /// The name of the last list of tracks played from a window (a search, recent plays),
    /// which has no context of its own.
    tracks_name: Option<String>,
    /// The play queue as the hidden observer last saw it.
    pub live_queue: Option<spotify::queue::LiveQueue>,
    /// Whether the output is muted; separate from the volume, and kept across runs.
    pub muted: bool,
    /// Liked Songs, kept up to date through the collection's changes, so saving a
    /// track needs no full read to tell whether it is already there.
    pub liked: Option<spotify::Liked>,
}

impl State {
    pub fn new(playing: playing::Shared) -> Self {
        Self {
            status: Status::Connecting,
            live: None,
            playing,
            wanted_volume: None,
            context_names: HashMap::new(),
            tracks_name: None,
            live_queue: None,
            muted: saved_muted(),
            liked: None,
        }
    }

    pub fn playing(&self) -> std::sync::MutexGuard<'_, Playing> {
        playing::lock(&self.playing)
    }

    pub fn snapshot(&self) -> Snapshot {
        let playing = self.playing();
        Snapshot {
            protocol: protocol::VERSION,
            status: self.status.clone(),
            track: playing.track.clone(),
            playing: playing.playing(),
            position_ms: playing.clock.elapsed_ms(),
            volume: self.wanted_volume.unwrap_or(playing.volume),
            muted: self.muted,
            shuffle: playing.shuffle,
            repeat: playing.repeat,
        }
    }

    /// Ends the session's playback state; the volume is kept.
    pub fn disconnect(&mut self) -> Option<Live> {
        {
            let mut playing = self.playing();
            *playing = Playing::new(playing.volume);
        }
        self.live_queue = None;
        self.liked = None;
        self.live.take()
    }

    /// Takes a speaker event; returns what windows need to hear, if anything.
    pub fn apply(&mut self, event: &PlayerEvent) -> Option<PlayerUpdate> {
        let update = self.playing().apply(event);
        if let Some(PlayerUpdate::Volume(volume)) = update {
            if self.wanted_volume == Some(volume) {
                self.wanted_volume = None;
            }
            save_volume(volume);
        }
        update
    }

    /// Remembers what a window named the list it started playing.
    pub fn remember_play(&mut self, play: &Play, name: &str) {
        match play {
            Play::Context { uri, .. } => {
                self.context_names.insert(uri.clone(), name.to_owned());
                self.tracks_name = None;
            }
            Play::Tracks { .. } => self.tracks_name = Some(name.to_owned()),
        }
    }

    pub fn remember_playlists(&mut self, playlists: &[spotify::Playlist]) {
        for playlist in playlists {
            self.context_names
                .insert(playlist.uri.clone(), playlist.name.clone());
        }
    }

    /// A name for what plays: Spotify's own, else one a window or the library gave.
    pub fn context_name(&self, uri: &str, spotify_name: &str) -> String {
        if !spotify_name.is_empty() {
            spotify_name.to_owned()
        } else if let Some(name) = self.context_names.get(uri) {
            name.clone()
        } else if uri.ends_with(":collection") {
            "Liked Songs".into()
        } else {
            self.tracks_name.clone().unwrap_or_default()
        }
    }
}

/// The volume saved between runs, in `~/.local/state/spotitui/volume`.
pub fn saved_volume() -> u16 {
    volume_file()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(DEFAULT_VOLUME)
}

pub fn save_volume(volume: u16) {
    let Some(path) = volume_file() else {
        return;
    };
    // Losing the saved volume only means the next run starts at the default.
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, volume.to_string());
}

/// Whether the output was muted when the service last ran.
pub fn saved_muted() -> bool {
    state_file("muted")
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|text| text.trim() == "1")
}

pub fn save_muted(muted: bool) {
    let Some(path) = state_file("muted") else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, if muted { "1" } else { "0" });
}

fn volume_file() -> Option<PathBuf> {
    state_file("volume")
}

fn state_file(name: &str) -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/state")))?;
    Some(state.join("spotitui").join(name))
}

#[cfg(test)]
mod tests {
    use super::State;
    use crate::spotify::{Play, playing};

    #[test]
    fn contexts_are_named_from_the_best_source() {
        let mut state = State::new(playing::shared(0));
        let album = Play::Context {
            uri: "spotify:album:a".into(),
            track: "spotify:track:t".into(),
        };
        state.remember_play(&album, "Album · Discovery");
        assert_eq!(
            state.context_name("spotify:album:a", ""),
            "Album · Discovery"
        );
        assert_eq!(
            state.context_name("spotify:album:a", "From Spotify"),
            "From Spotify"
        );
        assert_eq!(
            state.context_name("spotify:user:me:collection", ""),
            "Liked Songs"
        );
        let search = Play::Tracks {
            uris: vec!["spotify:track:t".into()],
            track: "spotify:track:t".into(),
        };
        state.remember_play(&search, "Search · “daft punk”");
        assert_eq!(state.context_name("", ""), "Search · “daft punk”");
    }
}
