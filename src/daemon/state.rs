//! What the service knows: its connection to Spotify and what the speaker plays.

use crate::{
    protocol::{self, PlayerUpdate, Snapshot, Status},
    spotify::{
        self, Catalog, NowTrack, Play, Repeat, Speaker,
        playing::{self, Playing},
        queue::{Entry, Feed},
    },
};
use librespot_playback::player::PlayerEvent;
use serde::{Deserialize, Serialize};
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
    /// When the speaker last reported tracks it couldn't play.
    pub refusals: Vec<std::time::Instant>,
    /// What the bar widget shows besides MPRIS: the list and the service's status.
    pub feed: Feed,
    /// What the listed tracks play from, such as a playlist.
    pub context_uri: String,
    /// When Play was asked for while the playback wasn't here yet; it plays
    /// once it's here, if that's soon.
    pub pending_play: Option<Instant>,
    /// When the speaker last asked to take Spotify's playback over.
    pub taking_over: Option<Instant>,
    /// Where to go once the playback taken over is here, if not where Spotify says.
    pub restore_seek: Option<u32>,
    /// The play queue Spotify had before a take-over, put back once the
    /// playback is here if librespot's differs (it shuffles anew).
    pub restore_queue: Option<spotify::queue::LiveQueue>,
    /// Set when the speaker takes Spotify's playback over: its shuffle and
    /// repeat are then read from Spotify.
    pub adopt_options: bool,
    /// The playback last saved, so the same isn't written again.
    last_saved: Option<Last>,
}

/// What played last, kept in `~/.local/state/spotitui/last.json`: shown as soon
/// as the service starts, before Spotify answers, and played again when
/// Spotify has no playback to hand over.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Last {
    pub track: NowTrack,
    pub position_ms: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// What it played from, such as a playlist; empty for a list of tracks.
    pub context_uri: String,
    /// The name of what it played from, and the tracks around it, for the widget.
    pub context: String,
    pub tracks: Vec<Entry>,
}

impl Last {
    /// The track within what it played from, to load it again.
    pub fn play(&self) -> Play {
        let track = self.track.uri.clone();
        // Lists of tracks get a made-up context that can't be loaded again.
        let loadable = !self.context_uri.is_empty()
            && !["spotify:web-api", "spotify:unknown"].contains(&self.context_uri.as_str());
        if loadable {
            Play::Context {
                uri: self.context_uri.clone(),
                track,
            }
        } else {
            Play::Tracks {
                uris: vec![track.clone()],
                track,
            }
        }
    }
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
            refusals: Vec::new(),
            feed: Feed::default(),
            context_uri: String::new(),
            pending_play: None,
            taking_over: None,
            restore_seek: None,
            restore_queue: None,
            adopt_options: false,
            last_saved: None,
        }
    }

    /// Takes what played last, as the service starts: the record already
    /// shows its track; this is the widget's list.
    pub fn show_last(&mut self, last: Last) {
        self.feed.context = last.context.clone();
        self.feed.tracks = last.tracks.clone();
        self.context_uri = last.context_uri.clone();
        self.last_saved = Some(last);
    }

    /// The playback to save, if there is a track and it changed since last saved.
    pub fn unsaved_last(&mut self) -> Option<Last> {
        let last = {
            let playing = self.playing();
            Last {
                track: playing.track.clone()?,
                position_ms: playing.clock.elapsed_ms(),
                shuffle: playing.shuffle,
                repeat: playing.repeat,
                context_uri: self.context_uri.clone(),
                context: self.feed.context.clone(),
                tracks: self.feed.tracks.clone(),
            }
        };
        if self.last_saved.as_ref() == Some(&last) {
            return None;
        }
        self.last_saved = Some(last.clone());
        Some(last)
    }

    /// Forgets what played, as after logging out.
    pub fn forget_playback(&mut self) {
        {
            let mut playing = self.playing();
            *playing = Playing::new(playing.volume);
        }
        self.feed = Feed::default();
        self.context_uri.clear();
        self.pending_play = None;
        self.taking_over = None;
        self.last_saved = None;
    }

    /// Takes a Play asked for before the playback got here, if still wanted.
    pub fn take_pending_play(&mut self, wait: std::time::Duration) -> bool {
        self.pending_play
            .take()
            .is_some_and(|asked| asked.elapsed() < wait)
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

    /// Ends the session's playback state. The track stays shown, paused, until
    /// the next session brings it back.
    pub fn disconnect(&mut self) -> Option<Live> {
        self.playing().leave();
        self.taking_over = None;
        self.restore_seek = None;
        self.restore_queue = None;
        self.live_queue = None;
        self.liked = None;
        self.refusals.clear();
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

/// What played when the service last ran, if anything did.
pub fn saved_last() -> Option<Last> {
    let text = std::fs::read_to_string(state_file("last.json")?).ok()?;
    serde_json::from_str(&text)
        .inspect_err(|error| log::warn!("cannot read what played last: {error}"))
        .ok()
}

pub fn save_last(last: &Last) {
    let Some(path) = state_file("last.json") else {
        return;
    };
    let Ok(json) = serde_json::to_vec_pretty(last) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Written aside and renamed, so a crash never leaves half a file.
    let partial = path.with_extension("json.partial");
    if std::fs::write(&partial, json).is_ok() {
        let _ = std::fs::rename(partial, path);
    }
}

pub fn forget_last() {
    if let Some(path) = state_file("last.json") {
        let _ = std::fs::remove_file(path);
    }
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
    use super::{Last, State};
    use crate::spotify::{NowTrack, Play, Repeat, playing};

    fn last(context_uri: &str) -> Last {
        Last {
            track: NowTrack {
                uri: "spotify:track:t".into(),
                name: "Big Iron".into(),
                artists: Vec::new(),
                album: String::new(),
                album_artists: Vec::new(),
                duration_ms: 235_733,
                art_url: None,
            },
            position_ms: 76_000,
            shuffle: false,
            repeat: Repeat::Off,
            context_uri: context_uri.into(),
            context: String::new(),
            tracks: Vec::new(),
        }
    }

    #[test]
    fn the_last_track_loads_again_within_what_it_played_from() {
        assert!(matches!(
            last("spotify:album:a").play(),
            Play::Context { uri, track } if uri == "spotify:album:a" && track == "spotify:track:t"
        ));
        // A list of tracks played from a window has no context to load.
        for context in ["", "spotify:web-api"] {
            assert!(matches!(
                last(context).play(),
                Play::Tracks { uris, .. } if uris == ["spotify:track:t"]
            ));
        }
    }

    #[test]
    fn what_plays_is_saved_once_per_change() {
        let mut state = State::new(playing::shared(0));
        assert!(state.unsaved_last().is_none(), "nothing to save");
        let saved = last("spotify:album:a");
        state.playing().show(saved.track.clone(), saved.position_ms, false, Repeat::Off);
        state.show_last(saved);
        state.context_uri = "spotify:album:a".into();
        assert!(state.unsaved_last().is_none(), "as it was saved");
        state.playing().show(last("").track, 80_000, false, Repeat::Off);
        assert_eq!(state.unsaved_last().map(|last| last.position_ms), Some(80_000));
        assert!(state.unsaved_last().is_none());
    }

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
