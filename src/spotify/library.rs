//! Changes to the user's library: Liked Songs and playlists, through the same
//! internal services the official apps write to.

use super::{Catalog, Error, catalog::run};
use http::{HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use librespot_core::{Session, SpotifyId};
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, Delta, Item, ItemAttributes, ListChanges, Op, SelectedListContent, op::Kind,
};
use protobuf::{CodedOutputStream, EnumOrUnknown, Message, MessageField};
use rand::Rng;
use std::time::{SystemTime, UNIX_EPOCH};

const PLAYLIST_PREFIX: &str = "spotify:playlist:";
const COLLECTION_CONTENT_TYPE: &str = "application/vnd.collection-v2.spotify.proto";

impl Catalog {
    /// Whether a track is in Liked Songs.
    pub async fn is_liked(&self, track: &str) -> Result<bool, Error> {
        let liked = self.context_tracks(self.liked_uri()).await?;
        Ok(liked.iter().any(|uri| uri == track))
    }

    /// Adds a track to Liked Songs, or with `liked` false, removes it.
    pub async fn set_liked(&self, track: String, liked: bool) -> Result<(), Error> {
        let session = self.session().clone();
        run(async move {
            let added_at = i32::try_from(now_ms() / 1000).unwrap_or(i32::MAX);
            let request =
                collection_write(&session.username(), &track, added_at, !liked, &random_hex())?;
            let mut headers = HeaderMap::new();
            headers.insert(
                CONTENT_TYPE,
                HeaderValue::from_static(COLLECTION_CONTENT_TYPE),
            );
            session
                .spclient()
                .request(
                    &Method::POST,
                    "/collection/v2/write",
                    Some(headers),
                    Some(&request),
                )
                .await?;
            Ok(())
        })
        .await
    }

    /// Adds a track to the end of a playlist.
    pub async fn add_to_playlist(&self, playlist: String, track: String) -> Result<(), Error> {
        let session = self.session().clone();
        run(async move {
            let (id, current) = playlist_contents(&session, &playlist).await?;
            let item = Item {
                uri: Some(track),
                attributes: MessageField::some(ItemAttributes {
                    added_by: Some(session.username()),
                    timestamp: Some(now_ms()),
                    ..ItemAttributes::default()
                }),
                ..Item::default()
            };
            let op = Op {
                kind: Some(EnumOrUnknown::new(Kind::ADD)),
                add: MessageField::some(Add {
                    items: vec![item],
                    add_last: Some(true),
                    ..Add::default()
                }),
                ..Op::default()
            };
            send_changes(&session, &id, current.revision.clone(), op).await
        })
        .await
    }

    /// Removes the last occurrence of a track from a playlist.
    #[cfg(test)]
    pub async fn remove_from_playlist(&self, playlist: String, track: String) -> Result<(), Error> {
        let session = self.session().clone();
        run(async move {
            let (id, current) = playlist_contents(&session, &playlist).await?;
            let items = &current.contents.items;
            let index = items
                .iter()
                .rposition(|item| item.uri() == track)
                .ok_or_else(|| Error::Response(format!("{track} isn't in {playlist}")))?;
            let op = Op {
                kind: Some(EnumOrUnknown::new(Kind::REM)),
                rem: MessageField::some(librespot_protocol::playlist4_external::Rem {
                    from_index: Some(i32::try_from(index).unwrap_or(i32::MAX)),
                    length: Some(1),
                    items: vec![items[index].clone()],
                    ..Default::default()
                }),
                ..Op::default()
            };
            send_changes(&session, &id, current.revision.clone(), op).await
        })
        .await
    }

    /// The track URIs of a playlist as stored, without context resolution.
    #[cfg(test)]
    pub async fn playlist_items(&self, playlist: String) -> Result<Vec<String>, Error> {
        let session = self.session().clone();
        run(async move {
            let (_, current) = playlist_contents(&session, &playlist).await?;
            Ok(current
                .contents
                .items
                .iter()
                .map(|item| item.uri().to_owned())
                .collect())
        })
        .await
    }
}

/// Encodes a `spotify.collection.proto.v2.WriteRequest` for one item. librespot
/// doesn't compile collection2v2.proto, and the message is small:
///
/// ```proto
/// message WriteRequest { string username = 1; string set = 2;
///     repeated CollectionItem items = 3; string client_update_id = 4; }
/// message CollectionItem { string uri = 1; int32 added_at = 2; bool is_removed = 3; }
/// ```
fn collection_write(
    username: &str,
    uri: &str,
    added_at: i32,
    is_removed: bool,
    client_update_id: &str,
) -> Result<Vec<u8>, Error> {
    let mut item = Vec::new();
    {
        let mut out = CodedOutputStream::vec(&mut item);
        out.write_string(1, uri)?;
        out.write_int32(2, added_at)?;
        if is_removed {
            out.write_bool(3, true)?;
        }
        out.flush()?;
    }
    let mut request = Vec::new();
    {
        let mut out = CodedOutputStream::vec(&mut request);
        out.write_string(1, username)?;
        out.write_string(2, "collection")?;
        out.write_bytes(3, &item)?;
        out.write_string(4, client_update_id)?;
        out.flush()?;
    }
    Ok(request)
}

/// A playlist's ID and current state, whose revision changes are based on.
async fn playlist_contents(
    session: &Session,
    playlist: &str,
) -> Result<(SpotifyId, SelectedListContent), Error> {
    let base62 = playlist
        .strip_prefix(PLAYLIST_PREFIX)
        .ok_or_else(|| Error::Response(format!("{playlist} is not a playlist")))?;
    let id = SpotifyId::from_base62(base62)?;
    let bytes = session.spclient().get_playlist(&id).await?;
    Ok((id, SelectedListContent::parse_from_bytes(&bytes)?))
}

async fn send_changes(
    session: &Session,
    id: &SpotifyId,
    base_revision: Option<Vec<u8>>,
    op: Op,
) -> Result<(), Error> {
    let changes = ListChanges {
        base_revision: base_revision.clone(),
        deltas: vec![Delta {
            base_version: base_revision,
            ops: vec![op],
            info: MessageField::some(ChangeInfo {
                user: Some(session.username()),
                timestamp: Some(now_ms()),
                ..ChangeInfo::default()
            }),
            ..Delta::default()
        }],
        want_resulting_revisions: Some(true),
        ..ListChanges::default()
    };
    let endpoint = format!("/playlist/v2/playlist/{}/changes", id.to_base62()?);
    session
        .spclient()
        .request_with_protobuf(&Method::POST, &endpoint, None, &changes)
        .await?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn random_hex() -> String {
    let bytes: [u8; 16] = rand::rng().random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
