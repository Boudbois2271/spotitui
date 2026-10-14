//! The tracks around the playing one (one before, three after), for the
//! Omarchy bar widget, published to `$XDG_RUNTIME_DIR/spotitui/queue.json`.
//!
//! Spotify's connect-state "cluster" carries the real play queue: previous
//! tracks, shuffle order and queued tracks. Spotify only pushes it to devices
//! that are *not* playing, so a second, hidden session registers as an observer
//! the way the web player does ("hobs_" devices never show in device lists).

use super::{Catalog, Error};
use futures_util::StreamExt;
use librespot_core::{
    Session, SessionConfig, authentication::Credentials, dealer::protocol::Message,
};
use librespot_protocol::{
    connect::{
        Capabilities, Cluster, ClusterUpdate, Device, DeviceInfo, MemberType, PutStateReason,
        PutStateRequest,
    },
    player::{PlayerState, ProvidedTrack},
};
use protobuf::{EnumOrUnknown, Message as _, MessageField};
use rand::Rng;
use serde::Serialize;
use std::{collections::HashMap, path::PathBuf};

const CLUSTER_URI: &str = "hm://connect-state/v1/cluster";
const CONNECTION_URI: &str = "hm://pusher/v1/connections/";
const PREVIOUS: usize = 1;
const UPCOMING: usize = 3;

/// A track in the published list.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Place relative to the playing track: -1 before it, 0 playing, 1.. after it.
    pub offset: i32,
    pub uri: String,
    pub title: String,
    pub artist: String,
}

/// The queue around the playing track, as Spotify reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Around {
    pub context_uri: String,
    pub context_name: String,
    pub tracks: Vec<Entry>,
    /// The whole play queue, for reordering it.
    pub queue: Option<LiveQueue>,
}

/// The speaker's play queue as last reported: what `set_queue` needs to put a
/// track first without losing the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveQueue {
    pub current_uri: String,
    pub next: Vec<ProvidedTrack>,
    pub prev: Vec<ProvidedTrack>,
    pub revision: String,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    context: &'a str,
    tracks: &'a [Entry],
}

/// Watches the play queue from a hidden observer session and calls `report`
/// whenever the tracks around the playing one change, until aborted.
pub fn observe(
    access_token: String,
    catalog: Catalog,
    report: impl Fn(Around) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = run_observer(access_token, catalog, report).await {
            log::warn!("cannot follow the play queue: {error}");
        }
    })
}

async fn run_observer(
    access_token: String,
    catalog: Catalog,
    report: impl Fn(Around) + Send + 'static,
) -> Result<(), Error> {
    let config = SessionConfig {
        device_id: observer_id(),
        ..SessionConfig::default()
    };
    let session = Session::new(config, None);
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
    session
        .connect(Credentials::with_access_token(access_token), false)
        .await?;
    session.dealer().start().await?;

    let mut names: HashMap<String, (String, String)> = HashMap::new();
    let mut last: Option<Around> = None;
    loop {
        let cluster = tokio::select! {
            // A new connection (also after a reconnect) needs the observer registered again.
            id = connections.next() => match id {
                Some(Ok(id)) => {
                    session.set_connection_id(&id);
                    let reply = session.spclient().put_connect_state_request(&observer(&session)).await?;
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
        let around = around(&cluster.player_state, &catalog, &mut names).await;
        if last.as_ref() != Some(&around) {
            report(around.clone());
            last = Some(around);
        }
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

/// Web player observers are named "hobs_" and a random hex id.
pub(super) fn observer_id() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("hobs_{hex}")
}

async fn around(
    state: &MessageField<PlayerState>,
    catalog: &Catalog,
    names: &mut HashMap<String, (String, String)>,
) -> Around {
    let Some(state) = state.as_ref() else {
        return Around {
            context_uri: String::new(),
            context_name: String::new(),
            tracks: Vec::new(),
            queue: None,
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
            current_uri: track.uri.clone(),
            next: state.next_tracks.clone(),
            prev: state.prev_tracks.clone(),
            revision: state.queue_revision.clone(),
        }),
    }
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

/// Skips the markers Spotify mixes into the queue, such as `spotify:delimiter`.
fn playable(track: &ProvidedTrack) -> bool {
    track.uri.starts_with("spotify:track:") || track.uri.starts_with("spotify:episode:")
}

/// Publishes the list; `context` names what is playing, such as a playlist.
pub fn write(context: &str, tracks: &[Entry]) {
    let Some(path) = file() else {
        return;
    };
    let Ok(json) = serde_json::to_vec_pretty(&Snapshot { context, tracks }) else {
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

/// Removes the published list; the widget then knows nothing plays here.
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
    use super::{MessageField, ProvidedTrack, neighbors, observer_id};

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
    fn observer_ids_look_like_the_web_players() {
        let id = observer_id();
        assert!(id.starts_with("hobs_"));
        assert_eq!(id.len(), 5 + 32);
    }
}
