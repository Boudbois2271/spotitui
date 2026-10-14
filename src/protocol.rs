//! What the player window and the background service say to each other.
//!
//! The service listens on `$XDG_RUNTIME_DIR/spotitui/daemon.sock`. Each message
//! is one line of JSON: the window sends [`ClientMessage`]s, the service answers
//! with [`ServerMessage::Reply`] and pushes [`Event`]s as playback changes.

use crate::spotify::{Command, Lyrics, Mode, NowTrack, Play, Playlist, Repeat, Track};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped whenever a message changes shape, so a window and a service built from
/// different versions notice it instead of misreading each other.
pub const VERSION: u32 = 3;

/// What a window shows when it and the service speak different versions.
pub fn mismatch(service: u32) -> String {
    let side = if service < VERSION { "older" } else { "newer" };
    format!(
        "The spotitui service is {side} than this window (protocol {service}, window {VERSION}); \
         restart it: systemctl --user restart spotitui"
    )
}

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

/// Why a request failed, in a form a window can act on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: Kind,
    /// Shown to the user as is.
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The login is gone or was refused; only logging in again helps.
    NeedsLogin,
    /// The service has no session to Spotify right now; it reconnects by itself.
    NotConnected,
    /// The window can't reach the service.
    ServiceDown,
    /// The network or Spotify didn't answer; trying again may work.
    Network,
    /// Spotify asked to slow down.
    RateLimited,
    NotFound,
    /// The other side is a different version and doesn't know the message.
    Unsupported,
    Other,
}

impl Failure {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl Failure {
    /// Whether the same request may work later without the user doing anything.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind,
            Kind::NotConnected | Kind::ServiceDown | Kind::Network | Kind::RateLimited
        )
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerMessage {
    Reply {
        id: u64,
        result: Result<Response, Failure>,
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
    /// The last attempt failed; the service tries again by itself unless the
    /// failure's kind is [`Kind::NeedsLogin`].
    Failed(Failure),
    Connected {
        liked_uri: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Snapshot {
    /// The service's [`VERSION`]; 0 from services that predate it.
    #[serde(default)]
    pub protocol: u32,
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
    /// Spotify refused several tracks in a row, usually for a minute or two
    /// after many skips; the speaker keeps moving on until one plays.
    Refusing,
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

/// Reads one line; `Ok(false)` at the end of the stream.
pub async fn read_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut String,
) -> std::io::Result<bool> {
    line.clear();
    Ok(reader.read_line(line).await? != 0)
}

/// One line read and understood, or the reason it couldn't be.
pub enum Incoming<T> {
    Message(T),
    /// Valid JSON this build doesn't know, such as from a newer version. `id` is
    /// the request it answers or asks, when it has one.
    Unknown { id: Option<u64>, error: String },
}

/// Parses a line, telling apart messages from another version from garbage.
pub fn parse<T: DeserializeOwned>(line: &str) -> Result<Incoming<T>, serde_json::Error> {
    match serde_json::from_str(line) {
        Ok(message) => Ok(Incoming::Message(message)),
        Err(error) => {
            let value: serde_json::Value = serde_json::from_str(line)?;
            // A window's request carries `id`; the service's reply carries `Reply.id`.
            let id = value
                .get("id")
                .or_else(|| value.get("Reply")?.get("id"))
                .and_then(serde_json::Value::as_u64);
            Ok(Incoming::Unknown {
                id,
                error: error.to_string(),
            })
        }
    }
}

/// Reads one message; `Ok(None)` at the end of the stream.
#[cfg(test)]
pub async fn read_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncBufRead + Unpin),
    line: &mut String,
) -> std::io::Result<Option<T>> {
    if !read_line(reader, line).await? {
        return Ok(None);
    }
    serde_json::from_str(line)
        .map(Some)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::{
        ClientMessage, Event, Failure, Incoming, Kind, PlayerUpdate, Request, ServerMessage,
        mismatch, parse,
        read_message, write_message,
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

    #[test]
    fn messages_from_another_version_keep_their_id() {
        let request = parse::<ClientMessage>(r#"{"id":4,"request":"Teleport"}"#).expect("json");
        assert!(matches!(request, Incoming::Unknown { id: Some(4), .. }));
        let reply = parse::<ServerMessage>(r#"{"Reply":{"id":9,"result":{"Ok":{"Hologram":1}}}}"#)
            .expect("json");
        assert!(matches!(reply, Incoming::Unknown { id: Some(9), .. }));
        let event = parse::<ServerMessage>(r#"{"Event":{"Player":{"Warp":3}}}"#).expect("json");
        assert!(matches!(event, Incoming::Unknown { id: None, .. }));
        assert!(parse::<ServerMessage>("not json").is_err());
    }

    #[test]
    fn failures_cross_the_socket_with_their_kind() {
        let reply = ServerMessage::Reply {
            id: 3,
            result: Err(Failure::new(Kind::RateLimited, "slow down")),
        };
        let line = serde_json::to_string(&reply).expect("serializes");
        let Ok(Incoming::Message(ServerMessage::Reply { id: 3, result: Err(failure) })) =
            parse(&line)
        else {
            panic!("a failure parses back");
        };
        assert_eq!(failure.kind, Kind::RateLimited);
        assert_eq!(failure.to_string(), "slow down");
    }

    #[test]
    fn snapshots_from_older_services_have_protocol_zero() {
        let line = r#"{"Event":{"Snapshot":{"status":"Connecting","track":null,"playing":false,
            "position_ms":0,"volume":1,"shuffle":false,"repeat":"Off"}}}"#;
        let Ok(Incoming::Message(ServerMessage::Event(Event::Snapshot(snapshot)))) = parse(line)
        else {
            panic!("an old snapshot still parses");
        };
        assert_eq!(snapshot.protocol, 0);
        assert!(mismatch(0).contains("older"));
    }
}
