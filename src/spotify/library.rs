//! Changes to the user's library: Liked Songs and playlists, through the same
//! internal services the official apps write to.

use super::{Catalog, Error, catalog::run};
use http::{HeaderMap, HeaderValue, Method, header::CONTENT_TYPE};
use librespot_core::{Session, SpotifyId};
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, Delta, Item, ItemAttributes, ListChanges, Op, SelectedListContent, op::Kind,
};
use protobuf::{CodedInputStream, CodedOutputStream, EnumOrUnknown, Message, MessageField};
use rand::Rng;
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

const PLAYLIST_PREFIX: &str = "spotify:playlist:";
const COLLECTION_CONTENT_TYPE: &str = "application/vnd.collection-v2.spotify.proto";
const COLLECTION_SET: &str = "collection";
/// Items per page when reading Liked Songs; Spotify caps Liked Songs at 10 000.
const PAGE_LIMIT: i32 = 5000;

/// Liked Songs as a set, with the sync token that later changes are asked from.
#[derive(Clone, Debug, Default)]
pub struct Liked {
    uris: HashSet<String>,
    sync_token: String,
}

impl Liked {
    pub fn contains(&self, uri: &str) -> bool {
        self.uris.contains(uri)
    }

    pub fn insert(&mut self, uri: String) {
        self.uris.insert(uri);
    }

    pub fn len(&self) -> usize {
        self.uris.len()
    }

    fn apply(&mut self, items: Vec<CollectionItem>) {
        for item in items {
            if item.is_removed {
                self.uris.remove(&item.uri);
            } else {
                self.uris.insert(item.uri);
            }
        }
    }
}

impl Catalog {
    /// All of Liked Songs, read through the collection service (one request for
    /// up to 5000 songs).
    pub async fn liked(&self) -> Result<Liked, Error> {
        let session = self.session().clone();
        run(async move {
            let username = session.username();
            let mut liked = Liked::default();
            let mut token = String::new();
            loop {
                let mut request = Vec::new();
                {
                    let mut out = CodedOutputStream::vec(&mut request);
                    out.write_string(1, &username)?;
                    out.write_string(2, COLLECTION_SET)?;
                    if !token.is_empty() {
                        out.write_string(3, &token)?;
                    }
                    out.write_int32(4, PAGE_LIMIT)?;
                    out.flush()?;
                }
                let page = PageResponse::parse(&collection(&session, "paging", request).await?)?;
                liked.apply(page.items);
                liked.sync_token = page.sync_token;
                if page.next_page_token.is_empty() || page.next_page_token == token {
                    return Ok(liked);
                }
                token = page.next_page_token;
            }
        })
        .await
    }

    /// Brings `liked` up to date with one small request; false when Spotify can't
    /// say what changed, and the whole set must be read again.
    pub async fn update_liked(&self, liked: &mut Liked) -> Result<bool, Error> {
        let session = self.session().clone();
        let token = liked.sync_token.clone();
        let delta = run(async move {
            let mut request = Vec::new();
            {
                let mut out = CodedOutputStream::vec(&mut request);
                out.write_string(1, &session.username())?;
                out.write_string(2, COLLECTION_SET)?;
                out.write_string(3, &token)?;
                out.flush()?;
            }
            Ok(DeltaResponse::parse(
                &collection(&session, "delta", request).await?,
            )?)
        })
        .await?;
        if !delta.possible {
            return Ok(false);
        }
        liked.apply(delta.items);
        liked.sync_token = delta.sync_token;
        Ok(true)
    }

    /// Whether a track is in Liked Songs.
    #[cfg(test)]
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
            let (id, revision) = playlist_revision(&session, &playlist).await?;
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
            send_changes(&session, &id, revision, op).await
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
#[cfg(test)]
async fn playlist_contents(
    session: &Session,
    playlist: &str,
) -> Result<(SpotifyId, SelectedListContent), Error> {
    let id = playlist_id(playlist)?;
    let bytes = session.spclient().get_playlist(&id).await?;
    Ok((id, SelectedListContent::parse_from_bytes(&bytes)?))
}

/// A playlist's ID and revision, without its tracks (`length=0`): adding to a
/// long playlist doesn't download it first.
async fn playlist_revision(
    session: &Session,
    playlist: &str,
) -> Result<(SpotifyId, Option<Vec<u8>>), Error> {
    let id = playlist_id(playlist)?;
    let path = format!("/playlist/v2/playlist/{}?length=0", id.to_base62()?);
    let bytes = session
        .spclient()
        .request(&Method::GET, &path, None, None)
        .await?;
    let list = SelectedListContent::parse_from_bytes(&bytes)?;
    Ok((id, list.revision))
}

fn playlist_id(playlist: &str) -> Result<SpotifyId, Error> {
    let base62 = playlist
        .strip_prefix(PLAYLIST_PREFIX)
        .ok_or_else(|| Error::Response(format!("{playlist} is not a playlist")))?;
    Ok(SpotifyId::from_base62(base62)?)
}

/// POSTs a collection2v2 request to `/collection/v2/{method}`.
async fn collection(session: &Session, method: &str, request: Vec<u8>) -> Result<Vec<u8>, Error> {
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static(COLLECTION_CONTENT_TYPE),
    );
    let bytes = session
        .spclient()
        .request(
            &Method::POST,
            &format!("/collection/v2/{method}"),
            Some(headers),
            Some(&request),
        )
        .await?;
    Ok(bytes.to_vec())
}

/// `CollectionItem { string uri = 1; int32 added_at = 2; bool is_removed = 3; }`
#[derive(Debug, Default, PartialEq)]
struct CollectionItem {
    uri: String,
    is_removed: bool,
}

/// `PageResponse { repeated CollectionItem items = 1; string next_page_token = 2;
/// string sync_token = 3; }`
#[derive(Debug, Default)]
struct PageResponse {
    items: Vec<CollectionItem>,
    next_page_token: String,
    sync_token: String,
}

/// `DeltaResponse { bool delta_update_possible = 1; repeated CollectionItem items = 2;
/// string sync_token = 3; }`
#[derive(Debug, Default)]
struct DeltaResponse {
    possible: bool,
    items: Vec<CollectionItem>,
    sync_token: String,
}

/// Calls `field` for each field of a message: its number and a reader positioned
/// on its value. Unknown fields are skipped.
fn fields(
    bytes: &[u8],
    mut field: impl FnMut(u32, &mut CodedInputStream) -> protobuf::Result<bool>,
) -> protobuf::Result<()> {
    let mut input = CodedInputStream::from_bytes(bytes);
    while let Some(tag) = input.read_raw_tag_or_eof()? {
        if !field(tag >> 3, &mut input)? {
            let wire = protobuf::rt::WireType::new(tag & 7)
                .ok_or_else(|| protobuf::Error::from(std::io::Error::other("bad wire type")))?;
            input.skip_field(wire)?;
        }
    }
    Ok(())
}

impl CollectionItem {
    fn parse(bytes: &[u8]) -> protobuf::Result<Self> {
        let mut item = Self::default();
        fields(bytes, |number, input| {
            match number {
                1 => item.uri = input.read_string()?,
                3 => item.is_removed = input.read_bool()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(item)
    }
}

impl PageResponse {
    fn parse(bytes: &[u8]) -> protobuf::Result<Self> {
        let mut page = Self::default();
        fields(bytes, |number, input| {
            match number {
                1 => page.items.push(CollectionItem::parse(&input.read_bytes()?)?),
                2 => page.next_page_token = input.read_string()?,
                3 => page.sync_token = input.read_string()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(page)
    }
}

impl DeltaResponse {
    fn parse(bytes: &[u8]) -> protobuf::Result<Self> {
        let mut delta = Self::default();
        fields(bytes, |number, input| {
            match number {
                1 => delta.possible = input.read_bool()?,
                2 => delta.items.push(CollectionItem::parse(&input.read_bytes()?)?),
                3 => delta.sync_token = input.read_string()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(delta)
    }
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

#[cfg(test)]
mod tests {
    use super::{CollectionItem, DeltaResponse, Liked, PageResponse};
    use protobuf::CodedOutputStream;

    fn item(uri: &str, removed: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut out = CodedOutputStream::vec(&mut bytes);
        out.write_string(1, uri).expect("writes");
        out.write_int32(2, 1_700_000_000).expect("writes");
        if removed {
            out.write_bool(3, true).expect("writes");
        }
        out.flush().expect("writes");
        drop(out);
        bytes
    }

    #[test]
    fn collection_answers_parse_and_apply() {
        let mut page = Vec::new();
        {
            let mut out = CodedOutputStream::vec(&mut page);
            out.write_bytes(1, &item("spotify:track:a", false)).expect("writes");
            out.write_bytes(1, &item("spotify:track:b", false)).expect("writes");
            out.write_string(3, "sync-1").expect("writes");
            out.write_string(9, "a field from a newer Spotify").expect("writes");
            out.flush().expect("writes");
        }
        let page = PageResponse::parse(&page).expect("parses");
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.sync_token, "sync-1");
        assert!(page.next_page_token.is_empty());
        let mut liked = Liked::default();
        liked.apply(page.items);

        let mut delta = Vec::new();
        {
            let mut out = CodedOutputStream::vec(&mut delta);
            out.write_bool(1, true).expect("writes");
            out.write_bytes(2, &item("spotify:track:a", true)).expect("writes");
            out.write_bytes(2, &item("spotify:track:c", false)).expect("writes");
            out.write_string(3, "sync-2").expect("writes");
            out.flush().expect("writes");
        }
        let delta = DeltaResponse::parse(&delta).expect("parses");
        assert!(delta.possible);
        assert_eq!(
            delta.items[0],
            CollectionItem {
                uri: "spotify:track:a".into(),
                is_removed: true
            }
        );
        liked.apply(delta.items);
        assert!(!liked.contains("spotify:track:a"));
        assert!(liked.contains("spotify:track:b") && liked.contains("spotify:track:c"));
    }
}
