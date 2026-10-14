//! The built-in Spotify Connect speaker: a librespot session, player and Spirc.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use super::{Catalog, Error, NowTrack, Repeat, bounded, catalog::run};
use http::{HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::{Session, SessionConfig, authentication::Credentials, config::DeviceType};
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::{
    audio_backend,
    config::{AudioFormat, PlayerConfig},
    mixer::{Mixer, MixerConfig, VolumeGetter, softmixer::SoftMixer},
    player::{Player, PlayerEventChannel},
};
use librespot_protocol::player::ProvidedTrack;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// The name this computer shows in every Spotify Connect device list.
pub const DEVICE_NAME: &str = "spotitui";
/// Logging in and registering the speaker; librespot's own login has a shorter one.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
/// PulseAudio (PipeWire on Omarchy) names the stream in volume mixers; rodio/ALSA is the fallback.
const PREFERRED_BACKEND: &str = "pulseaudio";

/// A Spotify Connect speaker that plays audio on this computer.
pub struct Speaker {
    spirc: Arc<Spirc>,
    session: Session,
    mixer: Arc<MuteMixer>,
}

/// librespot's software volume with a mute switch of its own. Muting silences
/// the output but leaves the volume as librespot, Spotify Connect and MPRIS see
/// it, so unmuting returns to the same level, and volume changes while muted
/// set the level to come back to.
pub struct MuteMixer {
    inner: SoftMixer,
    /// The volume as set, and whether the output is muted.
    state: Mutex<(u16, bool)>,
}

impl MuteMixer {
    pub fn set_muted(&self, muted: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.1 = muted;
        self.inner.set_volume(if muted { 0 } else { state.0 });
    }
}

impl Mixer for MuteMixer {
    fn open(config: MixerConfig) -> Result<Self, librespot_core::Error> {
        let inner = SoftMixer::open(config)?;
        let volume = inner.volume();
        Ok(Self {
            inner,
            state: Mutex::new((volume, false)),
        })
    }

    fn volume(&self) -> u16 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
    }

    fn set_volume(&self, volume: u16) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.0 = volume;
        if !state.1 {
            self.inner.set_volume(volume);
        }
    }

    fn get_soft_volume(&self) -> Box<dyn VolumeGetter + Send> {
        self.inner.get_soft_volume()
    }
}

/// A logged-in session: the speaker, its player events and the catalog.
pub struct Connection {
    pub speaker: Speaker,
    pub events: PlayerEventChannel,
    pub catalog: Catalog,
    /// For the play-queue observer's own session.
    pub access_token: String,
    /// Finishes when the session ends, such as after losing the network.
    pub ended: tokio::task::JoinHandle<()>,
}

/// What to start playing: a track within a context, or within a list of tracks.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Play {
    Context { uri: String, track: String },
    Tracks { uris: Vec<String>, track: String },
}

/// The shuffle and repeat a newly played list starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mode {
    pub shuffle: bool,
    pub repeat: Repeat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Resume,
    Pause,
    Next,
    Previous,
    Seek(u32),
    Volume(u16),
    Shuffle(bool),
    Repeat(Repeat),
}

impl Speaker {
    /// Logs in and registers the speaker. Must run inside the Tokio runtime.
    pub async fn connect(access_token: String, initial_volume: u16) -> Result<Connection, Error> {
        bounded(
            CONNECT_TIMEOUT,
            "logging in",
            start(access_token, initial_volume),
        )
        .await
    }

    pub fn play(&self, play: Play, mode: Mode) -> Result<(), Error> {
        let options = |track| LoadRequestOptions {
            start_playing: true,
            playing_track: Some(PlayingTrack::Uri(track)),
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle: mode.shuffle,
                repeat: mode.repeat != Repeat::Off,
                repeat_track: mode.repeat == Repeat::Track,
            })),
            ..LoadRequestOptions::default()
        };
        let request = match play {
            Play::Context { uri, track } => LoadRequest::from_context_uri(uri, options(track)),
            Play::Tracks { uris, track } => LoadRequest::from_tracks(uris, options(track)),
        };
        // A speaker ignores commands until it is the active device; activating it again is harmless.
        self.spirc.activate()?;
        Ok(self.spirc.load(request)?)
    }

    pub fn command(&self, command: Command) -> Result<(), Error> {
        match command {
            Command::Resume => self.spirc.play(),
            Command::Pause => self.spirc.pause(),
            Command::Next => self.spirc.next(),
            Command::Previous => self.spirc.prev(),
            Command::Seek(position) => self.spirc.set_position_ms(position),
            Command::Volume(volume) => self.spirc.set_volume(volume),
            Command::Shuffle(enabled) => self.spirc.shuffle(enabled),
            Command::Repeat(mode) => self
                .spirc
                .repeat(mode != Repeat::Off)
                .and_then(|()| self.spirc.repeat_track(mode == Repeat::Track)),
        }?;
        Ok(())
    }

    pub fn spirc(&self) -> Arc<Spirc> {
        self.spirc.clone()
    }

    pub fn set_muted(&self, muted: bool) {
        self.mixer.set_muted(muted);
    }

    /// Plays a track right after the current one, ahead of tracks queued before.
    /// `queue` is the play queue as Spotify last reported it.
    pub fn play_next(
        &self,
        track: String,
        queue: Option<super::queue::LiveQueue>,
    ) -> impl Future<Output = Result<(), Error>> + Send + 'static {
        let session = self.session.clone();
        async move { run(play_next(session, track, queue)).await }
    }

    /// Adds a track after the playing one and any tracks queued before it.
    pub fn queue(&self, track: String) -> impl Future<Output = Result<(), Error>> + Send + 'static {
        let session = self.session.clone();
        async move { run(add_to_queue(session, track)).await }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        // The process is exiting; a speaker that already stopped needs no shutdown.
        let _ = self.spirc.shutdown();
    }
}

/// The now-playing details librespot reports with a track change.
pub fn now_track(item: &AudioItem) -> NowTrack {
    let (artists, album) = match &item.unique_fields {
        UniqueFields::Track { artists, album, .. } => (
            artists.0.iter().map(|artist| artist.name.clone()).collect(),
            album.clone(),
        ),
        UniqueFields::Local { artists, album, .. } => (
            artists.iter().cloned().collect(),
            album.clone().unwrap_or_default(),
        ),
        UniqueFields::Episode { show_name, .. } => (Vec::new(), show_name.clone()),
    };
    NowTrack {
        uri: item.uri.clone(),
        name: item.name.clone(),
        artists,
        album,
        duration_ms: item.duration_ms.into(),
        art_url: item
            .covers
            .iter()
            .max_by_key(|cover| cover.width)
            .map(|cover| cover.url.clone()),
    }
}

async fn start(access_token: String, initial_volume: u16) -> Result<Connection, Error> {
    let backend = audio_backend::find(Some(PREFERRED_BACKEND.into()))
        .or_else(|| audio_backend::find(None))
        .ok_or(Error::NoAudio)?;
    let mixer = Arc::new(MuteMixer::open(MixerConfig::default())?);
    let session = Session::new(SessionConfig::default(), None);
    let player = Player::new(
        PlayerConfig::default(),
        session.clone(),
        mixer.get_soft_volume(),
        move || backend(None, AudioFormat::default()),
    );
    let events = player.get_player_event_channel();
    let config = ConnectConfig {
        name: DEVICE_NAME.into(),
        device_type: DeviceType::Computer,
        initial_volume,
        ..ConnectConfig::default()
    };
    let credentials = Credentials::with_access_token(access_token.clone());
    let (spirc, task) =
        Spirc::new(config, session.clone(), credentials, player, mixer.clone()).await?;
    let ended = tokio::spawn(task);
    let spirc = Arc::new(spirc);
    let catalog = Catalog::new(session.clone());
    Ok(Connection {
        speaker: Speaker {
            spirc,
            session: session.clone(),
            mixer,
        },
        events,
        catalog,
        access_token,
        ended,
    })
}

/// A track as the queue holds it: Spotify plays queued tracks before the rest.
fn queued(track: &str) -> serde_json::Value {
    serde_json::json!({ "uri": track, "provider": "queue", "metadata": { "is_queued": "true" } })
}

/// Asks Spotify to send this speaker an add-to-queue command, as other Spotify apps do.
async fn add_to_queue(session: Session, track: String) -> Result<(), Error> {
    let command = serde_json::json!({
        "endpoint": "add_to_queue",
        "track": queued(&track),
        "logging_params": {},
    });
    send_command(&session, command).await
}

/// Puts a track first in the queue. Adding to the queue already does that when
/// nothing else is queued; otherwise the queue is sent back with the track in
/// front (`set_queue`, as the phone app reorders it).
async fn play_next(
    session: Session,
    track: String,
    queue: Option<super::queue::LiveQueue>,
) -> Result<(), Error> {
    let queued_ahead = queue.as_ref().is_some_and(|queue| {
        queue
            .next
            .first()
            .is_some_and(|next| next.provider == "queue")
    });
    let Some(queue) = queue.filter(|_| queued_ahead) else {
        return add_to_queue(session, track).await;
    };
    let as_json = |tracks: &[ProvidedTrack]| -> Result<Vec<serde_json::Value>, Error> {
        tracks
            .iter()
            .map(|track| {
                let text = protobuf_json_mapping::print_to_string(track)
                    .map_err(|error| Error::Response(error.to_string()))?;
                Ok(serde_json::from_str(&text)?)
            })
            .collect()
    };
    let mut next = vec![queued(&track)];
    next.extend(as_json(&queue.next)?);
    let command = serde_json::json!({
        "endpoint": "set_queue",
        "next_tracks": next,
        "prev_tracks": as_json(&queue.prev)?,
        "queue_revision": queue.revision,
        "logging_params": {},
    });
    send_command(&session, command).await
}

/// Sends a player command to this speaker through Spotify, as remote apps do.
async fn send_command(session: &Session, command: serde_json::Value) -> Result<(), Error> {
    let device = session.device_id();
    let endpoint = format!("/connect-state/v1/player/command/from/{device}/to/{device}");
    let body = serde_json::json!({ "command": command });
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    session
        .spclient()
        .request(
            &Method::POST,
            &endpoint,
            Some(headers),
            Some(body.to_string().as_bytes()),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::MuteMixer;
    use librespot_playback::mixer::{Mixer, MixerConfig};

    #[test]
    fn muting_silences_the_output_but_keeps_the_level() {
        let mixer = MuteMixer::open(MixerConfig::default()).expect("a soft mixer");
        let output = mixer.get_soft_volume();
        let sixty_five = u16::try_from(65 * u32::from(u16::MAX) / 100).unwrap_or(0);
        mixer.set_volume(sixty_five);
        let audible = output.attenuation_factor();
        assert!(audible > 0.0);

        mixer.set_muted(true);
        assert_eq!(output.attenuation_factor(), 0.0, "muted output is silent");
        assert_eq!(mixer.volume(), sixty_five, "librespot still sees the level");

        // A volume change while muted moves the level, not the output.
        mixer.set_volume(u16::MAX / 2);
        assert_eq!(output.attenuation_factor(), 0.0);
        mixer.set_volume(sixty_five);

        mixer.set_muted(false);
        assert_eq!(
            output.attenuation_factor(),
            audible,
            "back to the same level"
        );
    }
}
