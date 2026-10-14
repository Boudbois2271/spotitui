//! What the player window and the background service say to each other.
//!
//! The service listens on `$XDG_RUNTIME_DIR/spotitui/daemon.sock`. Each message
//! is one line of JSON: the window sends [`ClientMessage`]s, the service answers
//! with [`ServerMessage::Reply`] and pushes [`Event`]s as playback changes.

use crate::spotify::{Command, Lyrics, Mode, NowTrack, Play, Playlist, Repeat, Track};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Where the service listens; `None` without `$XDG_RUNTIME_DIR`.
pub fn socket_path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("spotitui/daemon.sock"))
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ClientMessage {
    pub id: u64,
    pub request: Request,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Playlists,
    /// The track URIs of a playlist, album, artist, the liked songs or a search.
    ContextTracks(String),
    RecentTracks,
    Tracks(Vec<String>),
    Lyrics(NowTrack),
    /// `context_name` names what plays, for the bar widget (such as a search).
    Play {
        play: Play,
        mode: Mode,
        context_name: String,
    },
    Command(Command),
    AddToQueue(String),
    /// Queue a track to play right after the current one.
    PlayNext(String),
    /// Mute or unmute. Muting is separate from the volume, which it leaves alone.
    ToggleMute,
    /// Add a track to Liked Songs.
    Like(String),
    /// Add a track to the end of each playlist.
    AddToPlaylists {
        track: String,
        playlists: Vec<String>,
    },
    /// Read the login from the keyring again, after logging in.
    Reconnect,
    /// Forget the login and disconnect.
    Logout,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    Done,
    Playlists(Vec<Playlist>),
    Uris(Vec<String>),
    Tracks(Vec<Track>),
    Lyrics(Option<Lyrics>),
    /// After `Like`: false when the track was already in Liked Songs.
    Liked {
        added: bool,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMessage {
    Reply {
        id: u64,
        result: Result<Response, String>,
    },
    Event(Event),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Event {
    /// Everything at once: sent first to every window, and whenever events were missed.
    Snapshot(Snapshot),
    Status(Status),
    Player(PlayerUpdate),
}

/// The service's connection to Spotify.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Connecting,
    /// No login stored; the window shows its login screen.
    LoggedOut,
    /// The last attempt failed; the service tries again by itself.
    Failed(String),
    Connected {
        liked_uri: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub status: Status,
    pub track: Option<NowTrack>,
    pub playing: bool,
    pub position_ms: u32,
    pub volume: u16,
    #[serde(default)]
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// What the speaker reports, as the window needs it.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PlayerUpdate {
    Track(NowTrack),
    Playing { position_ms: u32 },
    Paused { position_ms: u32 },
    Seeked { position_ms: u32 },
    Position { position_ms: u32 },
    Stopped,
    Volume(u16),
    Muted(bool),
    Shuffle(bool),
    Repeat(Repeat),
    Unavailable,
}

/// Writes one message as a line of JSON.
pub async fn write_message<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line).await
}

/// Reads one message; `Ok(None)` at the end of the stream.
pub async fn read_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut String,
) -> std::io::Result<Option<T>> {
    line.clear();
    if reader.read_line(line).await? == 0 {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientMessage, Event, PlayerUpdate, Request, ServerMessage, read_message, write_message,
    };
    use tokio::io::BufReader;

    #[tokio::test]
    async fn messages_round_trip_as_json_lines() {
        let mut buffer = Vec::new();
        let sent = ClientMessage {
            id: 7,
            request: Request::ContextTracks("spotify:album:x".into()),
        };
        write_message(&mut buffer, &sent).await.expect("writes");
        write_message(
            &mut buffer,
            &ServerMessage::Event(Event::Player(PlayerUpdate::Stopped)),
        )
        .await
        .expect("writes");
        let mut reader = BufReader::new(buffer.as_slice());
        let mut line = String::new();
        let first: ClientMessage = read_message(&mut reader, &mut line)
            .await
            .expect("reads")
            .expect("a message");
        assert_eq!(first.id, 7);
        assert!(matches!(first.request, Request::ContextTracks(uri) if uri == "spotify:album:x"));
        let second: ServerMessage = read_message(&mut reader, &mut line)
            .await
            .expect("reads")
            .expect("a message");
        assert!(matches!(
            second,
            ServerMessage::Event(Event::Player(PlayerUpdate::Stopped))
        ));
        let end: Option<ServerMessage> = read_message(&mut reader, &mut line).await.expect("reads");
        assert!(end.is_none());
    }
}
