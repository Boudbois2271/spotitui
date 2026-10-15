//! Tests against the real Spotify account, using the login in the keyring:
//! `cargo test live_ -- --ignored --nocapture --test-threads=1`. They log in a
//! plain session (no speaker), so a running spotitui service isn't disturbed.

use super::{Catalog, Token, queue, stored_refresh_token};
use futures_util::StreamExt;
use librespot_core::{Session, SessionConfig, authentication::Credentials};
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::Message;

async fn catalog() -> Catalog {
    let refresh = stored_refresh_token(super::secrets::Unlock::Ask)
        .await
        .expect("keyring readable")
        .expect("logged in");
    let token = Token::refresh(&reqwest::Client::new(), &refresh)
        .await
        .expect("token refreshes");
    token.store(super::secrets::Unlock::Ask).await.expect("token stored");
    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(token.access), false)
        .await
        .expect("session connects");
    Catalog::new(session)
}

#[tokio::test]
#[ignore = "talks to Spotify"]
async fn live_rootlist_ownership() {
    let catalog = catalog().await;
    let session = catalog.session().clone();
    let bytes = session
        .spclient()
        .get_rootlist(0, Some(1000))
        .await
        .expect("rootlist");
    let rootlist = SelectedListContent::parse_from_bytes(&bytes).expect("parses");
    println!("me: {}", session.username());
    for (item, meta) in rootlist
        .contents
        .items
        .iter()
        .zip(&rootlist.contents.meta_items)
        .take(12)
    {
        println!(
            "{:<45} owner={:<28} caps={} edit_items={:?} revision={}",
            meta.attributes.name(),
            meta.owner_username(),
            meta.capabilities.is_some(),
            meta.capabilities.as_ref().map(|caps| caps.can_edit_items()),
            meta.revision().len(),
        );
        let _ = item;
    }
}

/// Polls until `check` holds, since writes can take a moment to show in reads.
async fn eventually(mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..10 {
        if check().await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

#[tokio::test]
#[ignore = "changes the library (and changes it back)"]
async fn live_like_and_unlike() {
    let catalog = catalog().await;
    let liked = catalog
        .context_tracks(catalog.liked_uri())
        .await
        .expect("liked songs");
    let candidates = catalog
        .context_tracks(super::search_uri("daft punk"))
        .await
        .expect("search");
    let track = candidates
        .into_iter()
        .find(|uri| !liked.contains(uri))
        .expect("a track not in Liked Songs");
    println!("liking {track}");
    catalog.set_liked(track.clone(), true).await.expect("like");
    assert!(
        eventually(async || catalog.is_liked(&track).await.unwrap_or(false)).await,
        "now liked"
    );
    catalog
        .set_liked(track.clone(), false)
        .await
        .expect("unlike");
    assert!(
        eventually(async || !catalog.is_liked(&track).await.unwrap_or(true)).await,
        "unliked again"
    );
    println!("liked and unliked {track}");
}

#[tokio::test]
#[ignore = "changes a playlist (and changes it back)"]
async fn live_add_to_playlist_and_remove() {
    let catalog = catalog().await;
    let playlists = catalog.playlists().await.expect("playlists");
    let playlist = playlists
        .iter()
        .find(|playlist| playlist.name == "Hell yeah")
        .expect("the Hell yeah playlist")
        .uri
        .clone();
    let before = catalog
        .playlist_items(playlist.clone())
        .await
        .expect("items");
    let liked = catalog
        .context_tracks(catalog.liked_uri())
        .await
        .expect("liked songs");
    let track = liked
        .into_iter()
        .find(|uri| !before.contains(uri))
        .expect("a liked track not in the playlist");
    println!("adding {track} to {playlist} ({} items)", before.len());
    catalog
        .add_to_playlist(playlist.clone(), track.clone())
        .await
        .expect("add");
    let items = catalog
        .playlist_items(playlist.clone())
        .await
        .expect("items");
    assert_eq!(items.last(), Some(&track), "added at the end");
    catalog
        .remove_from_playlist(playlist.clone(), track.clone())
        .await
        .expect("remove");
    let after = catalog
        .playlist_items(playlist.clone())
        .await
        .expect("items");
    assert_eq!(after, before, "back as it was");
    println!("added and removed again; {} items", after.len());
}

/// Undoes a test change: `SPOTITUI_UNLIKE=<uri>` removes a track from Liked Songs,
/// `SPOTITUI_UNADD=<playlist uri> <track uri>` removes a track from a playlist.
#[tokio::test]
#[ignore = "changes the library"]
async fn live_undo() {
    let catalog = catalog().await;
    if let Ok(track) = std::env::var("SPOTITUI_UNLIKE") {
        catalog
            .set_liked(track.clone(), false)
            .await
            .expect("unlike");
        println!("unliked {track}");
    }
    if let Ok(pair) = std::env::var("SPOTITUI_UNADD") {
        let (playlist, track) = pair.split_once(' ').expect("playlist and track");
        catalog
            .remove_from_playlist(playlist.into(), track.into())
            .await
            .expect("remove");
        println!("removed {track} from {playlist}");
    }
}

/// Prints Spotify's device list as a hidden observer sees it: which device is
/// active and whether it plays. Shows whether a stopped service left the list.
#[tokio::test]
#[ignore = "talks to Spotify"]
async fn live_cluster() {
    let refresh = stored_refresh_token(super::secrets::Unlock::Ask)
        .await
        .expect("keyring readable")
        .expect("logged in");
    let token = Token::refresh(&reqwest::Client::new(), &refresh)
        .await
        .expect("token refreshes");
    token.store(super::secrets::Unlock::Ask).await.expect("token stored");
    let config = SessionConfig {
        device_id: format!("hobs_{}", super::device_id("live test", 32)),
        ..SessionConfig::default()
    };
    let session = Session::new(config, None);
    let mut connections = session
        .dealer()
        .listen_for("hm://pusher/v1/connections/", |message| {
            message
                .headers
                .get("Spotify-Connection-Id")
                .cloned()
                .ok_or_else(|| librespot_core::Error::not_found("no connection id"))
        })
        .expect("subscribes");
    session
        .connect(Credentials::with_access_token(token.access), false)
        .await
        .expect("session connects");
    session.dealer().start().await.expect("dealer starts");
    let id = connections
        .next()
        .await
        .expect("a connection")
        .expect("an id");
    session.set_connection_id(&id);
    let reply = session
        .spclient()
        .put_connect_state_request(&queue::observer(&session))
        .await
        .expect("registers");
    let cluster =
        librespot_protocol::connect::Cluster::parse_from_bytes(&reply).expect("a cluster");
    let state = cluster.player_state.as_ref();
    println!(
        "active: {:?} playing: {} paused: {}",
        cluster.active_device_id,
        state.is_some_and(|state| state.is_playing),
        state.is_some_and(|state| state.is_paused)
    );
    if let Some(state) = state {
        println!(
            "track: {:?} in {:?} at {} of {} ms, {} s ago; shuffle {:?}; {} next",
            state.track.as_ref().map(|track| &track.uri),
            state.context_uri,
            state.position_as_of_timestamp,
            state.duration,
            (cluster.server_timestamp_ms - state.timestamp) / 1000,
            state.options.as_ref().map(|options| options.shuffling_context),
            state.next_tracks.len(),
        );
    }
    for (id, device) in &cluster.device {
        println!(
            "device {} {:?} hidden={}",
            id,
            device.name,
            device.capabilities.as_ref().is_some_and(|caps| caps.hidden)
        );
    }
}

/// A second session can log in with the first one's reusable credentials,
/// under another device ID, without a fresh token.
#[tokio::test]
#[ignore = "talks to Spotify"]
async fn live_reusable_credentials() {
    let first = catalog().await.session().clone();
    let credentials = Credentials {
        username: Some(first.username()),
        auth_type:
            librespot_protocol::authentication::AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
        auth_data: first.auth_data(),
    };
    let config = SessionConfig {
        device_id: format!("hobs_{}", super::device_id("live test 2", 32)),
        ..SessionConfig::default()
    };
    let second = Session::new(config, None);
    second
        .connect(credentials, false)
        .await
        .expect("reusable credentials log in");
    println!("second session: {} ok", second.username());
}

/// Takes playback over from the running service with a second, silent speaker,
/// as another Spotify app would: the service then shows it as playing on
/// another device. Plays it with `SPOTITUI_TAKEOVER_PLAY=1`. Holds for
/// `SPOTITUI_TAKEOVER_SECS` (default 8) before leaving.
#[tokio::test]
#[ignore = "takes playback over from the running service"]
async fn live_take_over() {
    let refresh = stored_refresh_token(super::secrets::Unlock::Ask)
        .await
        .expect("keyring readable")
        .expect("logged in");
    let token = Token::refresh(&reqwest::Client::new(), &refresh)
        .await
        .expect("token refreshes");
    token.store(super::secrets::Unlock::Ask).await.expect("token stored");
    let device = super::device_id("live test speaker", 40);
    let connection = super::Speaker::connect_as(device, token.access.clone(), 0)
        .await
        .expect("a second speaker");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    connection.speaker.take_over().await.expect("transfer");
    println!("took playback over");
    if std::env::var("SPOTITUI_TAKEOVER_PLAY").is_ok_and(|play| play == "1") {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        connection
            .speaker
            .command(super::Command::Resume)
            .expect("plays");
        println!("playing");
    }
    let hold = std::env::var("SPOTITUI_TAKEOVER_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok())
        .unwrap_or(8);
    tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
}
