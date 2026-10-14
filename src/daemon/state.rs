//! What the service knows: its connection to Spotify and what the speaker plays.

use crate::{
    protocol::{self, PlayerUpdate, Snapshot, Status},
    spotify::{self, Catalog, Play, Repeat, Speaker},
};
use librespot_playback::player::PlayerEvent;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

const DEFAULT_VOLUME: u16 = u16::MAX / 2;

/// A connected session.
pub struct Live {
    pub speaker: Arc<Speaker>,
    pub catalog: Catalog,
}

pub struct State {
    pub status: Status,
    pub live: Option<Live>,
    pub player: PlayerState,
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

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub track: Option<spotify::NowTrack>,
    pub playing: bool,
    position_ms: u32,
    reported: Instant,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl PlayerState {
    fn new(volume: u16) -> Self {
        Self {
            track: None,
            playing: false,
            position_ms: 0,
            reported: Instant::now(),
            volume,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }

    fn report(&mut self, position_ms: u32) {
        self.position_ms = position_ms;
        self.reported = Instant::now();
    }

    fn elapsed_ms(&self) -> u32 {
        let since = if self.playing {
            u32::try_from(self.reported.elapsed().as_millis()).unwrap_or(u32::MAX)
        } else {
            0
        };
        let elapsed = self.position_ms.saturating_add(since);
        match &self.track {
            Some(track) => elapsed.min(u32::try_from(track.duration_ms).unwrap_or(u32::MAX)),
            None => elapsed,
        }
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            status: Status::Connecting,
            live: None,
            player: PlayerState::new(saved_volume()),
            wanted_volume: None,
            context_names: HashMap::new(),
            tracks_name: None,
            live_queue: None,
            muted: saved_muted(),
            liked: None,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            protocol: protocol::VERSION,
            status: self.status.clone(),
            track: self.player.track.clone(),
            playing: self.player.playing,
            position_ms: self.player.elapsed_ms(),
            volume: self.wanted_volume.unwrap_or(self.player.volume),
            muted: self.muted,
            shuffle: self.player.shuffle,
            repeat: self.player.repeat,
        }
    }

    /// Ends the session's playback state; the volume is kept.
    pub fn disconnect(&mut self) -> Option<Live> {
        let volume = self.player.volume;
        self.player = PlayerState::new(volume);
        self.live_queue = None;
        self.liked = None;
        self.live.take()
    }

    /// Takes a speaker event; returns what windows need to hear, if anything.
    pub fn apply(&mut self, event: &PlayerEvent) -> Option<PlayerUpdate> {
        let player = &mut self.player;
        Some(match event {
            PlayerEvent::TrackChanged { audio_item } => {
                let track = spotify::now_track(audio_item);
                player.track = Some(track.clone());
                player.report(0);
                PlayerUpdate::Track(track)
            }
            PlayerEvent::Playing { position_ms, .. } => {
                player.playing = true;
                player.report(*position_ms);
                PlayerUpdate::Playing {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::Paused { position_ms, .. } => {
                player.playing = false;
                player.report(*position_ms);
                PlayerUpdate::Paused {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::Seeked { position_ms, .. } => {
                player.report(*position_ms);
                PlayerUpdate::Seeked {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                player.report(*position_ms);
                PlayerUpdate::Position {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::Stopped { .. } => {
                player.playing = false;
                PlayerUpdate::Stopped
            }
            PlayerEvent::VolumeChanged { volume } => {
                player.volume = *volume;
                if self.wanted_volume == Some(*volume) {
                    self.wanted_volume = None;
                }
                save_volume(*volume);
                PlayerUpdate::Volume(*volume)
            }
            PlayerEvent::ShuffleChanged { shuffle } => {
                player.shuffle = *shuffle;
                PlayerUpdate::Shuffle(*shuffle)
            }
            PlayerEvent::RepeatChanged { context, track } => {
                player.repeat = Repeat::from_flags(*context, *track);
                PlayerUpdate::Repeat(player.repeat)
            }
            PlayerEvent::Unavailable { .. } => PlayerUpdate::Unavailable,
            _ => return None,
        })
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
    use crate::spotify::Play;

    #[test]
    fn contexts_are_named_from_the_best_source() {
        let mut state = State::new();
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
