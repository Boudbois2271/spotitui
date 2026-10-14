//! Publishes the speaker on the session bus as an MPRIS player, so `playerctl`,
//! media keys and the Omarchy shell can control it. It reads what plays from
//! the service's own record ([`playing::Shared`]); the service says when a
//! property changed.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use super::{DEVICE_NAME, NowTrack, Repeat, playing};
use crate::protocol::PlayerUpdate;
use librespot_connect::Spirc;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use zbus::{
    Connection, connection,
    fdo::{self, RequestNameFlags},
    interface,
    object_server::{InterfaceRef, SignalEmitter},
    zvariant::{ObjectPath, OwnedValue, Value},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
const BUS_NAME: &str = "org.mpris.MediaPlayer2.spotitui";
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
const TRACK_URL: &str = "https://open.spotify.com/track/";
/// Each skipped track is a command to the speaker; the bar widget uses -1..3.
const MAX_SKIP: u32 = 20;

/// The speaker MPRIS controls. The service keeps one MPRIS player for its whole
/// life and points it at each new Spirc as it reconnects; `None` between sessions.
pub type SpircSlot = Arc<RwLock<Option<Arc<Spirc>>>>;

/// A published MPRIS player.
pub struct Mpris {
    player: InterfaceRef<Player>,
    http: reqwest::Client,
    _connection: Connection,
}

/// Publishes the MPRIS player on the session bus.
pub async fn serve(slot: SpircSlot, playing: playing::Shared) -> zbus::Result<Mpris> {
    let connection = connection::Builder::session()?
        .serve_at(PATH, Root)?
        .serve_at(
            PATH,
            Player {
                slot: slot.clone(),
                playing,
                art: None,
            },
        )?
        .serve_at(PATH, Queue { slot })?
        .build()
        .await?;
    // A second spotitui claims a name of its own, as MPRIS asks of multiple instances.
    let flags = RequestNameFlags::DoNotQueue.into();
    if connection
        .request_name_with_flags(BUS_NAME, flags)
        .await
        .is_err()
    {
        let instance = format!("{BUS_NAME}.instance{}", std::process::id());
        connection.request_name_with_flags(instance, flags).await?;
    }
    let player = connection
        .object_server()
        .interface::<_, Player>(PATH)
        .await?;
    Ok(Mpris {
        player,
        http: super::http(),
        _connection: connection,
    })
}

impl Mpris {
    /// Tells MPRIS clients what an update of the shared record changed.
    pub async fn changed(&self, update: &PlayerUpdate) {
        let change = match update {
            PlayerUpdate::Track(track) => {
                self.player.get_mut().await.art = None;
                tokio::spawn(publish_cover(
                    self.player.clone(),
                    self.http.clone(),
                    track.clone(),
                ));
                Change::Track
            }
            PlayerUpdate::Playing { .. } | PlayerUpdate::Paused { .. } | PlayerUpdate::Stopped => {
                Change::Status
            }
            PlayerUpdate::Seeked { .. } | PlayerUpdate::Position { .. } => Change::Position,
            PlayerUpdate::Volume(_) => Change::Volume,
            PlayerUpdate::Shuffle(_) => Change::Shuffle,
            PlayerUpdate::Repeat(_) => Change::Loop,
            PlayerUpdate::Muted(_) | PlayerUpdate::Unavailable => return,
        };
        // A listener that misses one update catches up on the next.
        let _ = announce(&self.player, change).await;
    }

    /// Announces everything again, as after the session ended and the record
    /// was cleared: clients show nothing playing.
    pub async fn refresh(&self) {
        self.player.get_mut().await.art = None;
        for change in [
            Change::Track,
            Change::Status,
            Change::Volume,
            Change::Shuffle,
            Change::Loop,
        ] {
            let _ = announce(&self.player, change).await;
        }
    }
}

/// Downloads the track's cover to `$XDG_RUNTIME_DIR/spotitui/covers/` and
/// publishes it as a `file://` URL. Files are named after Spotify's image ID,
/// so the tracks of an album share one download.
async fn publish_cover(player: InterfaceRef<Player>, http: reqwest::Client, track: NowTrack) {
    let (Some(remote), Some(dir)) = (track.art_url.clone(), covers_dir()) else {
        return;
    };
    let id = track_id(&track.uri);
    let path = dir.join(format!("{}.jpg", cover_name(&remote, id)));
    if !path.exists() {
        let Ok(bytes) = async {
            http.get(&remote)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        }
        .await
        else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        // Named per track: two tracks of an album may download the same image at once.
        let partial = dir.join(format!("{id}.partial"));
        if std::fs::write(&partial, &bytes).is_err() || std::fs::rename(&partial, &path).is_err() {
            let _ = std::fs::remove_file(&partial);
            return;
        }
    }
    let url = format!("file://{}", path.display());
    {
        let mut player = player.get_mut().await;
        let current = playing::lock(&player.playing)
            .track
            .as_ref()
            .is_some_and(|playing| playing.uri == track.uri);
        // Another track started meanwhile; its own download publishes, and cleans up.
        if !current {
            return;
        }
        player.art = Some((track.uri.clone(), url));
    }
    let _ = announce(&player, Change::Track).await;
    prune_covers(&dir, &path, std::time::SystemTime::now());
}

/// The file name for a cover: the last part of its URL (Spotify's image ID),
/// or the track's ID for a URL without a usable one.
fn cover_name(url: &str, track_id: &str) -> String {
    let name: String = url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    if name.is_empty() {
        track_id.to_owned()
    } else {
        name
    }
}

/// Removes old covers. Keeps `current`, downloads in progress, and covers from
/// the last minute: a newer track's may have just landed.
fn prune_covers(dir: &std::path::Path, current: &std::path::Path, now: std::time::SystemTime) {
    const RECENT: Duration = Duration::from_secs(60);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let recent = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() < RECENT);
        let partial = path.extension().is_some_and(|extension| extension == "partial");
        if path != current && !recent && !partial {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn covers_dir() -> Option<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(std::path::PathBuf::from(runtime).join("spotitui/covers"))
}

async fn announce(player: &InterfaceRef<Player>, change: Change) -> zbus::Result<()> {
    let emitter = player.signal_emitter();
    let player = player.get().await;
    match change {
        // Seeking becomes possible once there is a track; clients cache CanSeek.
        Change::Track => {
            player.metadata_changed(emitter).await?;
            player.can_seek_changed(emitter).await
        }
        Change::Status => player.playback_status_changed(emitter).await,
        Change::Volume => player.volume_changed(emitter).await,
        Change::Shuffle => player.shuffle_changed(emitter).await,
        Change::Loop => player.loop_status_changed(emitter).await,
        Change::Position => {
            let position = micros(playing::lock(&player.playing).clock.elapsed());
            Player::seeked(emitter, position).await
        }
    }
}

/// Which property an update changed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Change {
    Track,
    Status,
    Volume,
    Shuffle,
    Loop,
    Position,
}

/// The base62 ID of a Spotify URI: its last part.
fn track_id(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or_default()
}

/// An object path unique to the track; Spotify IDs are alphanumeric.
fn track_path(uri: &str) -> String {
    let id: String = track_id(uri)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect();
    format!("{PATH}/track/{id}")
}

/// The MPRIS metadata of a track; `art` is its local cover, once downloaded.
fn metadata(track: &NowTrack, art: Option<&str>) -> Vec<(&'static str, Value<'static>)> {
    let path = ObjectPath::try_from(track_path(&track.uri)).unwrap_or_else(|_| no_track());
    let mut fields = vec![
        ("mpris:trackid", Value::from(path.into_owned())),
        (
            "mpris:length",
            Value::from(micros(Duration::from_millis(track.duration_ms))),
        ),
        ("xesam:title", Value::from(track.name.clone())),
        ("xesam:artist", Value::from(track.artists.clone())),
        ("xesam:albumArtist", Value::from(track.album_artists.clone())),
    ];
    if !track.album.is_empty() {
        fields.push(("xesam:album", Value::from(track.album.clone())));
    }
    // Some MPRIS clients (Quickshell) crashed fetching https covers; only the
    // local copy is published.
    if let Some(art) = art {
        fields.push(("mpris:artUrl", Value::from(art.to_owned())));
    }
    if track.uri.starts_with("spotify:track:") {
        fields.push((
            "xesam:url",
            Value::from(format!("{TRACK_URL}{}", track_id(&track.uri))),
        ));
    }
    fields
}

/// spotitui's own additions, beside the standard MPRIS interfaces.
struct Queue {
    slot: SpircSlot,
}

#[interface(name = "io.spotitui.Queue")]
impl Queue {
    /// Plays the track `offset` places away in the play order: -1 is the previous
    /// track, 2 the one after next. Unlike MPRIS Previous, going back never just
    /// restarts the current track. Shuffle order and queued tracks are kept.
    fn skip(&self, offset: i32) -> fdo::Result<()> {
        if offset.unsigned_abs() > MAX_SKIP {
            return Err(fdo::Error::InvalidArgs(format!(
                "skip at most {MAX_SKIP} tracks either way, not {offset}"
            )));
        }
        let spirc = current(&self.slot)?;
        failed(skip(&spirc, offset))
    }
}

/// Spirc runs commands one at a time in the order sent, so a seek to 0:00 right
/// before "previous" is always seen by it: under 3 seconds in, previous goes back.
fn skip(spirc: &Spirc, offset: i32) -> Result<(), librespot_core::Error> {
    for _ in 0..offset.unsigned_abs() {
        if offset < 0 {
            spirc.set_position_ms(0)?;
            spirc.prev()?;
        } else {
            spirc.next()?;
        }
    }
    Ok(())
}

struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        DEVICE_NAME.into()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    slot: SpircSlot,
    playing: playing::Shared,
    /// The local cover file of a track, by URI.
    art: Option<(String, String)>,
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) -> fdo::Result<()> {
        failed(self.spirc()?.next())
    }

    fn previous(&self) -> fdo::Result<()> {
        failed(self.spirc()?.prev())
    }

    fn pause(&self) -> fdo::Result<()> {
        failed(self.spirc()?.pause())
    }

    fn play_pause(&self) -> fdo::Result<()> {
        failed(self.spirc()?.play_pause())
    }

    fn stop(&self) -> fdo::Result<()> {
        failed(self.spirc()?.pause())
    }

    fn play(&self) -> fdo::Result<()> {
        failed(self.spirc()?.play())
    }

    fn seek(&self, offset: i64) -> fdo::Result<()> {
        let elapsed = playing::lock(&self.playing).clock.elapsed();
        let target = micros(elapsed).saturating_add(offset).max(0);
        self.go_to(target)
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "zbus passes method arguments by value"
    )]
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        // MPRIS ignores positions for a track that is no longer playing, or out of range.
        let (current, length) = {
            let playing = playing::lock(&self.playing);
            let current = playing.track.as_ref().map(|track| track_path(&track.uri));
            (current, micros(playing.clock.length()))
        };
        if current.as_deref() != Some(track_id.as_str()) || !(0..=length).contains(&position) {
            return Ok(());
        }
        self.go_to(position)
    }

    fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "spotitui opens tracks from its own window".into(),
        ))
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        format!("{:?}", playing::lock(&self.playing).status)
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        match playing::lock(&self.playing).repeat {
            Repeat::Off => "None".into(),
            Repeat::Context => "Playlist".into(),
            Repeat::Track => "Track".into(),
        }
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "zbus passes property values by value"
    )]
    #[zbus(property)]
    fn set_loop_status(&self, status: String) -> fdo::Result<()> {
        let (context, track) = match status.as_str() {
            "None" => (false, false),
            "Playlist" => (true, false),
            "Track" => (true, true),
            _ => {
                return Err(fdo::Error::InvalidArgs(format!(
                    "unknown loop status {status}"
                )));
            }
        };
        let spirc = self.spirc()?;
        failed(
            spirc
                .repeat(context)
                .and_then(|()| spirc.repeat_track(track)),
        )
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        playing::lock(&self.playing).shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> fdo::Result<()> {
        failed(self.spirc()?.shuffle(shuffle))
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let playing = playing::lock(&self.playing);
        let Some(track) = &playing.track else {
            return [("mpris:trackid", Value::from(no_track()))]
                .into_iter()
                .filter_map(owned)
                .collect();
        };
        let art = self
            .art
            .as_ref()
            .filter(|(uri, _)| *uri == track.uri)
            .map(|(_, url)| url.as_str());
        metadata(track, art).into_iter().filter_map(owned).collect()
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        f64::from(playing::lock(&self.playing).volume) / f64::from(u16::MAX)
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) -> fdo::Result<()> {
        // Clamped to 0–1 first, so the conversion cannot overflow.
        let volume = (volume.clamp(0.0, 1.0) * f64::from(u16::MAX)).round() as u16;
        failed(self.spirc()?.set_volume(volume))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        micros(playing::lock(&self.playing).clock.elapsed())
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        playing::lock(&self.playing).track.is_some()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

impl Player {
    fn spirc(&self) -> fdo::Result<Arc<Spirc>> {
        current(&self.slot)
    }

    fn go_to(&self, position: i64) -> fdo::Result<()> {
        // Positions past u32 milliseconds (49 days) clamp to the end of the track.
        let millis = (position / MICROS_PER_MILLI).try_into().unwrap_or(u32::MAX);
        failed(self.spirc()?.set_position_ms(millis))
    }
}

/// The speaker of the current session.
fn current(slot: &SpircSlot) -> fdo::Result<Arc<Spirc>> {
    slot.read()
        .ok()
        .and_then(|spirc| spirc.clone())
        .ok_or_else(|| fdo::Error::Failed("spotitui is not connected to Spotify".into()))
}

const MICROS_PER_MILLI: i64 = 1000;

fn micros(duration: Duration) -> i64 {
    // Saturates only past 292 thousand years.
    duration.as_micros().try_into().unwrap_or(i64::MAX)
}

fn no_track() -> ObjectPath<'static> {
    ObjectPath::from_static_str_unchecked(NO_TRACK)
}

fn owned((key, value): (&str, Value<'_>)) -> Option<(String, OwnedValue)> {
    // Only values carrying file descriptors fail to convert; metadata has none.
    Some((key.to_owned(), value.try_into().ok()?))
}

fn failed(result: Result<(), librespot_core::Error>) -> fdo::Result<()> {
    result.map_err(|error| fdo::Error::Failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{cover_name, metadata, prune_covers, track_path};
    use crate::spotify::NowTrack;
    use std::time::Duration;

    fn track() -> NowTrack {
        NowTrack {
            uri: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
            name: "Never Gonna Give You Up".into(),
            artists: vec!["Rick Astley".into()],
            album: "Whenever You Need Somebody".into(),
            album_artists: vec!["Rick Astley".into()],
            duration_ms: 213_573,
            art_url: None,
        }
    }

    #[test]
    fn tracks_have_valid_object_paths() {
        assert_eq!(
            track_path(&track().uri),
            "/org/mpris/MediaPlayer2/track/4uLU6hMCjMI75M1A2tKUQC"
        );
        let track = track();
        let keys: Vec<_> = metadata(&track, Some("file:///x.jpg"))
            .iter()
            .map(|(key, _)| *key)
            .collect();
        assert!(keys.contains(&"mpris:trackid"));
        assert!(keys.contains(&"xesam:url"));
        assert!(keys.contains(&"mpris:artUrl"));
        assert!(!metadata(&track, None).iter().any(|(key, _)| *key == "mpris:artUrl"));
    }

    #[test]
    fn covers_are_named_after_the_image() {
        assert_eq!(
            cover_name("https://i.scdn.co/image/ab67616d0000b273abc", "t1"),
            "ab67616d0000b273abc"
        );
        assert_eq!(cover_name("https://example.com/", "t1"), "t1");
    }

    #[test]
    fn pruning_keeps_the_current_new_and_partial_covers() {
        let dir = std::env::temp_dir().join(format!("spotitui-covers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        let file = |name: &str, age: u64| {
            let path = dir.join(name);
            std::fs::write(&path, b"jpg").expect("writes");
            let modified = std::time::SystemTime::now() - Duration::from_secs(age);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(modified))
                .expect("sets the time");
            path
        };
        let current = file("current.jpg", 600);
        let old = file("old.jpg", 600);
        let fresh = file("fresh.jpg", 5);
        let partial = file("t2.partial", 600);
        prune_covers(&dir, &current, std::time::SystemTime::now());
        assert!(current.exists() && fresh.exists() && partial.exists());
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
