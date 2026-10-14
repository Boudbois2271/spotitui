use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub uri: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub uri: String,
    pub name: String,
}

/// A catalog track, as listed in the track table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub uri: String,
    pub name: String,
    pub artists: Vec<Artist>,
    pub album: Album,
    pub duration_ms: u64,
}

/// The track the speaker reports it is playing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowTrack {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    /// For MPRIS; absent from services older than it.
    #[serde(default)]
    pub album_artists: Vec<String>,
    pub duration_ms: u64,
    pub art_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub uri: String,
    pub name: String,
    /// Whether tracks can be added: the user's own and collaborative playlists.
    #[serde(default)]
    pub editable: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

impl Track {
    pub fn artist_names(&self) -> String {
        join_names(self.artists.iter().map(|artist| artist.name.as_str()))
    }
}

impl NowTrack {
    pub fn artist_names(&self) -> String {
        join_names(self.artists.iter().map(String::as_str))
    }

    /// The base62 ID of a `spotify:track:` URI.
    pub fn track_id(&self) -> Option<&str> {
        self.uri.strip_prefix("spotify:track:")
    }
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::Context,
            Self::Context => Self::Track,
            Self::Track => Self::Off,
        }
    }

    pub fn from_flags(context: bool, track: bool) -> Self {
        match (context, track) {
            (_, true) => Self::Track,
            (true, false) => Self::Context,
            (false, false) => Self::Off,
        }
    }
}

fn join_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    names.collect::<Vec<_>>().join(", ")
}
