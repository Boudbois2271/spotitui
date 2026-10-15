//! Spotify's playback as the account sees it, and the tracks around the playing
//! one (one before, three after) for the Omarchy bar widget, published to
//! `$XDG_RUNTIME_DIR/spotitui/queue.json`.
//!
//! Spotify's connect-state "cluster" carries the real play queue: previous
//! tracks, shuffle order and queued tracks, and which device plays. Spotify only
//! pushes it to devices that are *not* playing, so a second, hidden session
//! registers as an observer the way the web player does ("hobs_" devices never
//! show in device lists).

use super::{Catalog, Error, Repeat};
use futures_util::StreamExt;
use librespot_core::{
    Session, SessionConfig, authentication::Credentials, dealer::protocol::Message,
};
use librespot_protocol::authentication::AuthenticationType;
use librespot_protocol::{
    connect::{
        Capabilities, Cluster, ClusterUpdate, Device, DeviceInfo, MemberType, PutStateReason,
        PutStateRequest,
    },
    player::{PlayerState, ProvidedTrack},
};
use protobuf::{EnumOrUnknown, Message as _, MessageField};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::watch;

const CLUSTER_URI: &str = "hm://connect-state/v1/cluster";
const CONNECTION_URI: &str = "hm://pusher/v1/connections/";
const PREVIOUS: usize = 1;
const UPCOMING: usize = 3;
/// Waits before restarting the observer, growing to the last.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
];
/// An observer that ran this long without trouble starts over at the shortest wait.
const HEALTHY_RUN: Duration = Duration::from_secs(300);
/// How long the observer may stay silent after the speaker changes track.
const FOLLOW_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the observer checks that its session is still alive.
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// For logging in, starting the dealer and registering as an observer.
const SETUP_TIMEOUT: Duration = Duration::from_secs(45);
/// How long [`Observer::stop`] waits for the cleanup before cutting it short.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// A running play queue observer.
pub struct Observer {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Observer {
    /// Stops the observer and lets it close its session. Aborting it instead
    /// would leak that session: librespot's dealer task keeps it alive, with its
    /// connections, for the life of the process.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let abort = self.task.abort_handle();
        if tokio::time::timeout(STOP_TIMEOUT, self.task).await.is_err() {
            log::warn!("the play queue observer didn't stop in time");
            abort.abort();
        }
    }
}

/// A track in the published list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Place relative to the playing track: -1 before it, 0 playing, 1.. after it.
    pub offset: i32,
    pub uri: String,
    pub title: String,
    pub artist: String,
}

/// Spotify's playback and the queue around the playing track, as Spotify
/// reports them. Without a device playing, they are what played last.
#[derive(Clone, Debug, PartialEq)]
pub struct Around {
    pub context_uri: String,
    pub context_name: String,
    pub tracks: Vec<Entry>,
    /// The whole play queue, for reordering it; `None` when Spotify has no track.
    pub queue: Option<LiveQueue>,
    /// The device that plays, or is paused; `None` when none is.
    pub device: Option<ActiveDevice>,
    pub paused: bool,
    /// Where the track is, as of the update.
    pub position_ms: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// The Spotify Connect device that has the playback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveDevice {
    pub id: String,
    pub name: String,
}

impl Around {
    /// Whether a device other than `own` (a device ID) is playing.
    pub fn plays_elsewhere(&self, own: &str) -> bool {
        !self.paused && self.device.as_ref().is_some_and(|device| device.id != own)
    }
}

/// The speaker's play queue as last reported: what `set_queue` needs to put a
/// track first without losing the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveQueue {
    /// What plays from: a playlist, album or artist; empty for a list of tracks.
    pub context_uri: String,
    pub current_uri: String,
    pub next: Vec<ProvidedTrack>,
    pub prev: Vec<ProvidedTrack>,
    pub revision: String,
}

/// What the bar widget reads: the list, and what the service has to say.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Feed {
    /// What plays from, such as a playlist's name.
    pub context: String,
    pub tracks: Vec<Entry>,
    /// Why nothing can play here right now, such as "offline"; empty while connected.
    pub status: String,
    /// The other device Spotify plays (or is paused) on, by name; empty when
    /// it's this one, or none.
    pub device: String,
    pub device_playing: bool,
}

/// Watches the play queue from a hidden observer session and calls `report`
/// whenever the tracks around the playing one change, until stopped.
///
/// The observer logs in with the main session's reusable credentials, so it
/// needs no token of its own. It starts over (after a pause) when it fails,
/// when its session dies, and when the speaker changes track and no update comes
/// within [`FOLLOW_TIMEOUT`]: librespot's dealer gives up reconnecting after
/// one failed attempt and then stays silent, with no error to notice.
/// `playing` is the track the speaker plays.
pub fn observe(
    catalog: Catalog,
    playing: watch::Receiver<Option<String>>,
    report: impl Fn(Around) + Send + Sync + 'static,
) -> Observer {
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(async move {
        let mut failures = 0;
        loop {
            let started = Instant::now();
            let result = run_observer(&catalog, playing.clone(), stopped.clone(), &report).await;
            if *stopped.borrow() {
                return;
            }
            if started.elapsed() >= HEALTHY_RUN {
                failures = 0;
            }
            let delay = BACKOFF[failures.min(BACKOFF.len() - 1)];
            failures += 1;
            match result {
                Ok(()) => log::info!("the play queue observer stopped; restarting in {delay:?}"),
                Err(error) => {
                    log::warn!("cannot follow the play queue ({error}); retrying in {delay:?}");
                }
            }
            let mut stopped = stopped.clone();
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = stopped.wait_for(|stop| *stop) => return,
            }
        }
    });
    Observer { stop, task }
}

/// The main session's login, reusable by another session.
fn reusable_credentials(main: &Session) -> Credentials {
    Credentials {
        username: Some(main.username()),
        auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
        auth_data: main.auth_data(),
    }
}

async fn run_observer(
    catalog: &Catalog,
    mut playing: watch::Receiver<Option<String>>,
    mut stopped: watch::Receiver<bool>,
    report: &impl Fn(Around),
) -> Result<(), Error> {
    let config = SessionConfig {
        device_id: observer_id(),
        ..SessionConfig::default()
    };
    let session = Session::new(config, None);
    let result = tokio::select! {
        result = follow(&session, catalog, &mut playing, report) => result,
        _ = stopped.wait_for(|stop| *stop) => Ok(()),
    };
    super::close_session(&session).await;
    result
}

async fn follow(
    session: &Session,
    catalog: &Catalog,
    playing: &mut watch::Receiver<Option<String>>,
    report: &impl Fn(Around),
) -> Result<(), Error> {
    // The dealer only takes subscriptions before it starts.
    let mut connections = session
        .dealer()
        .listen_for(CONNECTION_URI, |message: Message| {
            message
                .headers
                .get("Spotify-Connection-Id")
                .cloned()
                .ok_or_else(|| librespot_core::Error::not_found("no connection id"))
        })?;
    let mut clusters = session
        .dealer()
        .listen_for(CLUSTER_URI, Message::from_raw::<ClusterUpdate>)?;
    let setup = async {
        session
            .connect(reusable_credentials(catalog.session()), false)
            .await?;
        session.dealer().start().await
    };
    tokio::time::timeout(SETUP_TIMEOUT, setup)
        .await
        .map_err(|_| Error::Timeout("starting the play queue observer"))??;

    let mut names: HashMap<String, (String, String)> = HashMap::new();
    let mut last: Option<Around> = None;
    // Set when the speaker changed track and no update has come since.
    let mut behind_since: Option<Instant> = None;
    let mut check = tokio::time::interval(CHECK_INTERVAL);
    loop {
        let cluster = tokio::select! {
            _ = check.tick() => {
                if session.is_invalid() {
                    return Err(Error::Observer("its session ended".into()));
                }
                if behind_since.is_some_and(|since| since.elapsed() >= FOLLOW_TIMEOUT) {
                    return Err(Error::Observer(format!(
                        "it fell behind: no update for the playing track in {FOLLOW_TIMEOUT:?}"
                    )));
                }
                continue;
            }
            changed = playing.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                behind_since = (!shows(last.as_ref(), playing.borrow().as_deref()))
                    .then(Instant::now);
                continue;
            }
            // A new connection (also after a reconnect) needs the observer registered again.
            id = connections.next() => match id {
                Some(Ok(id)) => {
                    session.set_connection_id(&id);
                    let request = observer(session);
                    let register = session.spclient().put_connect_state_request(&request);
                    let reply = tokio::time::timeout(SETUP_TIMEOUT, register)
                        .await
                        .map_err(|_| Error::Timeout("registering the play queue observer"))??;
                    Cluster::parse_from_bytes(&reply)?
                }
                Some(Err(error)) => {
                    log::debug!("unreadable connection id: {error}");
                    continue;
                }
                None => return Ok(()),
            },
            update = clusters.next() => match update {
                Some(Ok(update)) => update.cluster.unwrap_or_default(),
                Some(Err(error)) => {
                    log::debug!("unreadable cluster update: {error}");
                    continue;
                }
                None => return Ok(()),
            },
        };
        // Any update after a track change shows the observer still hears Spotify.
        // (Its track may not match the speaker's URI exactly: Spotify relinks
        // tracks to other copies for some countries.)
        behind_since = None;
        let around = around(&cluster, catalog, &mut names).await;
        if last.as_ref() != Some(&around) {
            report(around.clone());
            last = Some(around);
        }
    }
}

/// Whether the observer's queue has `uri` as the playing track.
fn shows(around: Option<&Around>, uri: Option<&str>) -> bool {
    match uri {
        None => true,
        Some(uri) => around
            .and_then(|around| around.queue.as_ref())
            .is_some_and(|queue| queue.current_uri == uri),
    }
}

/// Registers as a hidden device that can neither play nor be chosen.
pub(super) fn observer(session: &Session) -> PutStateRequest {
    PutStateRequest {
        member_type: EnumOrUnknown::new(MemberType::CONNECT_STATE),
        put_state_reason: EnumOrUnknown::new(PutStateReason::NEW_DEVICE),
        device: MessageField::some(Device {
            device_info: MessageField::some(DeviceInfo {
                can_play: false,
                name: "spotitui observer".into(),
                device_id: session.device_id().to_owned(),
                client_id: session.client_id(),
                capabilities: MessageField::some(Capabilities {
                    hidden: true,
                    can_be_player: false,
                    is_observable: true,
                    needs_full_player_state: true,
                    ..Capabilities::default()
                }),
                ..DeviceInfo::default()
            }),
            player_state: MessageField::some(PlayerState {
                session_id: session.session_id(),
                ..PlayerState::default()
            }),
            ..Device::default()
        }),
        ..PutStateRequest::default()
    }
}

/// Web player observers are named "hobs_" and a hex id. Stable, so restarts
/// don't leave a trail of hidden devices behind.
pub(super) fn observer_id() -> String {
    format!("hobs_{}", super::device_id("observer", 32))
}

async fn around(
    cluster: &Cluster,
    catalog: &Catalog,
    names: &mut HashMap<String, (String, String)>,
) -> Around {
    let device = (!cluster.active_device_id.is_empty()).then(|| ActiveDevice {
        id: cluster.active_device_id.clone(),
        name: cluster
            .device
            .get(&cluster.active_device_id)
            .map(|device| device.name.clone())
            .unwrap_or_default(),
    });
    let Some(state) = cluster.player_state.as_ref() else {
        return Around {
            context_uri: String::new(),
            context_name: String::new(),
            tracks: Vec::new(),
            queue: None,
            device,
            paused: true,
            position_ms: 0,
            shuffle: false,
            repeat: Repeat::Off,
        };
    };
    let uris = neighbors(&state.prev_tracks, &state.track, &state.next_tracks);
    let missing: Vec<String> = uris
        .iter()
        .map(|(_, uri)| uri.clone())
        .filter(|uri| !names.contains_key(uri) && uri.starts_with("spotify:track:"))
        .collect();
    if !missing.is_empty() {
        match catalog.tracks(missing).await {
            Ok(tracks) => {
                for track in tracks {
                    names.insert(
                        track.uri.clone(),
                        (track.name.clone(), track.artist_names()),
                    );
                }
            }
            Err(error) => log::info!("cannot name queued tracks: {error}"),
        }
    }
    Around {
        context_uri: state.context_uri.clone(),
        context_name: state
            .context_metadata
            .get("context_description")
            .cloned()
            .unwrap_or_default(),
        tracks: uris
            .into_iter()
            .map(|(offset, uri)| {
                let (title, artist) = names.get(&uri).cloned().unwrap_or_default();
                Entry {
                    offset,
                    uri,
                    title,
                    artist,
                }
            })
            .collect(),
        queue: state.track.as_ref().map(|track| LiveQueue {
            context_uri: state.context_uri.clone(),
            current_uri: track.uri.clone(),
            next: state.next_tracks.clone(),
            prev: state.prev_tracks.clone(),
            revision: state.queue_revision.clone(),
        }),
        device,
        // Spotify apps say "playing" for a paused track too, as having one.
        paused: state.is_paused || !state.is_playing,
        position_ms: position(state, cluster.server_timestamp_ms),
        shuffle: state
            .options
            .as_ref()
            .is_some_and(|options| options.shuffling_context),
        repeat: state.options.as_ref().map_or(Repeat::Off, |options| {
            Repeat::from_flags(options.repeating_context, options.repeating_track)
        }),
    }
}

/// Where the track is at `now` (Spotify's clock, in ms): the last report, plus
/// the time since while it plays. Never past the end.
fn position(state: &PlayerState, now: i64) -> u32 {
    let mut position = state.position_as_of_timestamp;
    if state.is_playing && !state.is_paused {
        position += (now - state.timestamp).max(0);
    }
    if state.duration > 0 {
        position = position.min(state.duration);
    }
    u32::try_from(position.max(0)).unwrap_or(u32::MAX)
}

/// The playable tracks around the playing one, with their offsets.
fn neighbors(
    previous: &[ProvidedTrack],
    current: &MessageField<ProvidedTrack>,
    next: &[ProvidedTrack],
) -> Vec<(i32, String)> {
    let Some(current) = current.as_ref().filter(|track| playable(track)) else {
        return Vec::new();
    };
    let mut tracks: Vec<(i32, String)> = previous
        .iter()
        .rev()
        .filter(|track| playable(track))
        .take(PREVIOUS)
        .enumerate()
        .map(|(index, track)| (-(index as i32) - 1, track.uri.clone()))
        .collect();
    tracks.reverse();
    tracks.push((0, current.uri.clone()));
    tracks.extend(
        next.iter()
            .filter(|track| playable(track))
            .take(UPCOMING)
            .enumerate()
            .map(|(index, track)| (index as i32 + 1, track.uri.clone())),
    );
    tracks
}

/// The track `count` places after `playing` in the queue, if the queue knows it.
/// The queue may still describe a track played before `playing` (the observer
/// hears of a skip a moment after it happens); `playing` is then found in it.
pub fn skip_target<'a>(
    queue: &'a LiveQueue,
    playing: &str,
    count: usize,
) -> Option<&'a ProvidedTrack> {
    let next: Vec<&ProvidedTrack> = queue.next.iter().filter(|track| playable(track)).collect();
    let after = if queue.current_uri == playing {
        0
    } else {
        next.iter().position(|track| track.uri == playing)? + 1
    };
    next.get(after + count.checked_sub(1)?).copied()
}

/// Skips the markers Spotify mixes into the queue, such as `spotify:delimiter`.
fn playable(track: &ProvidedTrack) -> bool {
    track.uri.starts_with("spotify:track:") || track.uri.starts_with("spotify:episode:")
}

/// Publishes what the bar widget shows.
pub fn write(feed: &Feed) {
    let Some(path) = file() else {
        return;
    };
    let Ok(json) = serde_json::to_vec_pretty(feed) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Written aside and renamed, so the widget never reads half a file.
    let partial = path.with_extension("json.partial");
    if std::fs::write(&partial, json).is_ok() {
        let _ = std::fs::rename(partial, path);
    }
}

/// Removes the published list, as the service stops.
pub fn clear() {
    if let Some(path) = file() {
        let _ = std::fs::remove_file(path);
    }
}

fn file() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(runtime).join("spotitui/queue.json"))
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveDevice, Around, LiveQueue, MessageField, PlayerState, ProvidedTrack, Repeat,
        neighbors, observer_id, position, shows,
    };

    fn around(current: &str) -> Around {
        Around {
            context_uri: String::new(),
            context_name: String::new(),
            tracks: Vec::new(),
            queue: Some(LiveQueue {
                context_uri: String::new(),
                current_uri: current.into(),
                next: Vec::new(),
                prev: Vec::new(),
                revision: String::new(),
            }),
            device: None,
            paused: true,
            position_ms: 0,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }

    fn track(uri: &str) -> ProvidedTrack {
        ProvidedTrack {
            uri: uri.into(),
            ..ProvidedTrack::default()
        }
    }

    #[test]
    fn keeps_one_before_and_three_after_skipping_markers() {
        let previous = [track("spotify:track:a"), track("spotify:track:b")];
        let next = [
            track("spotify:delimiter"),
            track("spotify:track:d"),
            track("spotify:track:e"),
            track("spotify:track:f"),
            track("spotify:track:g"),
        ];
        let current = MessageField::some(track("spotify:track:c"));
        let offsets: Vec<_> = neighbors(&previous, &current, &next)
            .into_iter()
            .map(|(offset, uri)| format!("{offset}{}", &uri[14..]))
            .collect();
        assert_eq!(offsets, ["-1b", "0c", "1d", "2e", "3f"]);
        assert!(neighbors(&previous, &MessageField::none(), &next).is_empty());
    }

    #[test]
    fn the_observer_is_behind_until_its_queue_shows_the_playing_track() {
        assert!(shows(None, None), "nothing plays, nothing to show");
        assert!(!shows(None, Some("spotify:track:a")));
        assert!(!shows(Some(&around("spotify:track:a")), Some("spotify:track:b")));
        assert!(shows(Some(&around("spotify:track:b")), Some("spotify:track:b")));
    }

    #[test]
    fn skip_targets_count_from_the_playing_track() {
        let queue = LiveQueue {
            context_uri: String::new(),
            current_uri: "spotify:track:a".into(),
            next: vec![
                track("spotify:track:b"),
                track("spotify:delimiter"),
                track("spotify:track:c"),
                track("spotify:track:d"),
            ],
            prev: Vec::new(),
            revision: String::new(),
        };
        let target = |playing: &str, count| {
            super::skip_target(&queue, playing, count).map(|track| track.uri.as_str())
        };
        assert_eq!(target("spotify:track:a", 1), Some("spotify:track:b"));
        assert_eq!(target("spotify:track:a", 2), Some("spotify:track:c"), "past the marker");
        // The queue still shows a, but b already plays.
        assert_eq!(target("spotify:track:b", 2), Some("spotify:track:d"));
        assert_eq!(target("spotify:track:a", 9), None, "beyond the known queue");
        assert_eq!(target("spotify:track:z", 1), None, "a queue about something else");
        assert_eq!(target("spotify:track:a", 0), None);
    }

    #[test]
    fn positions_move_on_only_while_playing_and_stay_in_the_track() {
        let state = |playing: bool, paused: bool| PlayerState {
            timestamp: 10_000,
            position_as_of_timestamp: 30_000,
            duration: 200_000,
            is_playing: playing,
            is_paused: paused,
            ..PlayerState::default()
        };
        assert_eq!(position(&state(true, false), 15_000), 35_000);
        // Spotify apps report a paused track as playing and paused.
        assert_eq!(position(&state(true, true), 15_000), 30_000);
        assert_eq!(position(&state(true, false), 10_000_000), 200_000, "never past the end");
    }

    #[test]
    fn only_another_device_playing_is_elsewhere() {
        let mut playback = around("spotify:track:a");
        assert!(!playback.plays_elsewhere("me"), "no device has it");
        playback.device = Some(ActiveDevice {
            id: "phone".into(),
            name: "Pixel".into(),
        });
        assert!(!playback.plays_elsewhere("me"), "paused there");
        playback.paused = false;
        assert!(playback.plays_elsewhere("me"));
        assert!(!playback.plays_elsewhere("phone"), "it's this one");
    }

    #[test]
    fn observer_ids_look_like_the_web_players() {
        let id = observer_id();
        assert!(id.starts_with("hobs_"));
        assert_eq!(id.len(), 5 + 32);
    }
}
