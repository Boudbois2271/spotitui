//! The library and catalog, read through Spotify's internal services on the
//! same librespot session that plays audio.
//!
//! Adapted from fusor-rs/spotify-tui (MIT).

use super::{Album, Artist, Error, Playlist, Track};
use http::Method;
use librespot_core::{Session, SpotifyUri};
use librespot_protocol::{
    context_page::ContextPage,
    extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery},
    extension_kind::ExtensionKind,
    metadata,
    playlist4_external::SelectedListContent,
};
use protobuf::{EnumOrUnknown, Message};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    fmt::Write,
};

const ROOTLIST_LENGTH: usize = 1000;
const TRACK_PREFIX: &str = "spotify:track:";
const PLAYLIST_PREFIX: &str = "spotify:playlist:";
const SEARCH_PREFIX: &str = "spotify:search:";
const STATUS_OK: i32 = 200;
/// Stops following pages past this many tracks; Liked Songs caps at 10k anyway.
const MAX_CONTEXT_TRACKS: usize = 20_000;

#[derive(Clone)]
pub struct Catalog {
    session: Session,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecentlyPlayed {
    play_contexts: Vec<PlayContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayContext {
    last_played_track_uri: Option<String>,
}

impl Catalog {
    pub fn new(session: Session) -> Self {
        Self { session }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn liked_uri(&self) -> String {
        format!("spotify:user:{}:collection", self.session.username())
    }

    pub async fn playlists(&self) -> Result<Vec<Playlist>, Error> {
        let session = self.session.clone();
        run(async move {
            let username = session.username();
            let bytes = session
                .spclient()
                .get_rootlist(0, Some(ROOTLIST_LENGTH))
                .await?;
            let rootlist = SelectedListContent::parse_from_bytes(&bytes)?;
            let contents = &rootlist.contents;
            Ok(contents
                .items
                .iter()
                .zip(&contents.meta_items)
                .filter(|(item, _)| item.uri().starts_with(PLAYLIST_PREFIX))
                .filter_map(|(item, meta)| {
                    // Spotify says per playlist whether its tracks can be changed.
                    let editable = match meta.capabilities.as_ref() {
                        Some(capabilities) => capabilities.can_edit_items(),
                        None => meta.owner_username() == username,
                    };
                    Some(Playlist {
                        uri: item.uri().to_owned(),
                        name: meta.attributes.name.clone()?,
                        editable,
                    })
                })
                .collect())
        })
        .await
    }

    /// The track URIs of a playlist, album, artist, the liked songs or a search.
    pub async fn context_tracks(&self, uri: String) -> Result<Vec<String>, Error> {
        let session = self.session.clone();
        run(async move {
            let context = session.spclient().get_context(&uri).await?;
            let mut uris = Vec::new();
            for page in context.pages {
                // A page either carries its tracks or points at them; long ones continue
                // through `next_page_url`.
                let mut page = match (page.tracks.is_empty(), page.page_url.clone()) {
                    (true, Some(url)) if !url.is_empty() => fetch_page(&session, &url).await?,
                    _ => page,
                };
                loop {
                    uris.extend(page.tracks.iter().map(|track| track.uri().to_owned()));
                    match page.next_page_url.take().filter(|url| !url.is_empty()) {
                        Some(url) if uris.len() < MAX_CONTEXT_TRACKS => {
                            page = fetch_page(&session, &url).await?;
                        }
                        _ => break,
                    }
                }
            }
            Ok(unique_tracks(uris.into_iter()))
        })
        .await
    }

    /// The last track played in each recently played album, playlist or artist.
    pub async fn recent_tracks(&self) -> Result<Vec<String>, Error> {
        let session = self.session.clone();
        run(async move {
            let path = format!(
                "/recently-played/v3/user/{}/recently-played?format=json&offset=0&limit=50&filter=default",
                session.username()
            );
            let body = session
                .spclient()
                .request_as_json(&Method::GET, &path, None, None)
                .await?;
            let recent: RecentlyPlayed = serde_json::from_slice(&body)?;
            let uris = recent
                .play_contexts
                .into_iter()
                .filter_map(|context| context.last_played_track_uri);
            Ok(unique_tracks(uris))
        })
        .await
    }

    /// Looks up many tracks in one request, in the order of `uris`; unavailable tracks are left out.
    pub async fn tracks(&self, uris: Vec<String>) -> Result<Vec<Track>, Error> {
        let session = self.session.clone();
        run(async move {
            let request = BatchedEntityRequest {
                entity_request: uris.iter().map(|uri| track_request(uri)).collect(),
                ..BatchedEntityRequest::default()
            };
            let response = session.spclient().get_extended_metadata(request).await?;
            let mut found = response
                .extended_metadata
                .iter()
                .flat_map(|array| &array.extension_data)
                .filter(|data| data.header.status_code == STATUS_OK)
                .map(|data| {
                    let message = metadata::Track::parse_from_bytes(&data.extension_data.value)?;
                    Ok((data.entity_uri.clone(), track(&data.entity_uri, &message)?))
                })
                .collect::<Result<HashMap<_, _>, Error>>()?;
            Ok(uris.iter().filter_map(|uri| found.remove(uri)).collect())
        })
        .await
    }
}

/// The context URI that searches the catalog for `text`.
pub fn search_uri(text: &str) -> String {
    let mut uri = String::from(SEARCH_PREFIX);
    for byte in text.trim().bytes() {
        match byte {
            b' ' => uri.push('+'),
            _ if byte.is_ascii_alphanumeric() => uri.push(char::from(byte)),
            _ => {
                write!(uri, "%{byte:02X}").expect("writing to a String cannot fail");
            }
        }
    }
    uri
}

/// Generous: resolving a long list follows many pages.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// librespot's requests are `Send`; they run on the Tokio runtime's workers,
/// with a time limit.
pub(super) async fn run<T: Send + 'static>(
    task: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> Result<T, Error> {
    super::bounded(REQUEST_TIMEOUT, "a library request", task).await
}

async fn fetch_page(session: &Session, url: &str) -> Result<ContextPage, Error> {
    let bytes = session.spclient().get_next_page(url).await?;
    let text = String::from_utf8_lossy(&bytes);
    protobuf_json_mapping::parse_from_str_with_options(
        &text,
        &protobuf_json_mapping::ParseOptions {
            ignore_unknown_fields: true,
            ..Default::default()
        },
    )
    .map_err(|error| Error::Response(error.to_string()))
}

fn unique_tracks(uris: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    uris.filter(|uri| uri.starts_with(TRACK_PREFIX) && seen.insert(uri.clone()))
        .collect()
}

fn track_request(uri: &str) -> EntityRequest {
    EntityRequest {
        entity_uri: uri.to_owned(),
        query: vec![ExtensionQuery {
            extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
            ..ExtensionQuery::default()
        }],
        ..EntityRequest::default()
    }
}

fn track(uri: &str, message: &metadata::Track) -> Result<Track, Error> {
    let artists = message
        .artist
        .iter()
        .map(|artist| {
            Ok(Artist {
                uri: SpotifyUri::try_from(artist)?.to_uri()?,
                name: artist.name().to_owned(),
            })
        })
        .collect::<Result<_, Error>>()?;
    Ok(Track {
        uri: uri.to_owned(),
        name: message.name().to_owned(),
        artists,
        album: Album {
            uri: SpotifyUri::try_from(&*message.album)?.to_uri()?,
            name: message.album.name().to_owned(),
        },
        duration_ms: u64::from(message.duration().unsigned_abs()),
    })
}

#[cfg(test)]
mod tests {
    use super::{search_uri, unique_tracks};

    #[test]
    fn search_uris_encode_the_query() {
        assert_eq!(search_uri(" daft punk "), "spotify:search:daft+punk");
        assert_eq!(search_uri("AC/DC & co"), "spotify:search:AC%2FDC+%26+co");
        assert_eq!(search_uri("beyoncé"), "spotify:search:beyonc%C3%A9");
    }

    #[test]
    fn contexts_keep_each_track_once_in_order() {
        let uris = [
            "spotify:track:b",
            "spotify:episode:x",
            "spotify:track:a",
            "spotify:track:b",
        ]
        .map(String::from);
        assert_eq!(
            unique_tracks(uris.into_iter()),
            ["spotify:track:b", "spotify:track:a"]
        );
    }
}
