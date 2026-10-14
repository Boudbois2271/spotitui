//! Tests against the real Spotify account, using the login in the keyring:
//! `cargo test live_ -- --ignored --nocapture --test-threads=1`. They log in a
//! plain session (no speaker), so a running spotitui service isn't disturbed.

use super::{Catalog, Token, stored_refresh_token};
use librespot_core::{Session, SessionConfig, authentication::Credentials};
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::Message;

async fn catalog() -> Catalog {
    let refresh = stored_refresh_token()
        .expect("keyring readable")
        .expect("logged in");
    let token = Token::refresh(&reqwest::Client::new(), &refresh)
        .await
        .expect("token refreshes");
    token.store().expect("token stored");
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
