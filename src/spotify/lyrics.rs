//! Lyrics: Spotify's own (synced when available), falling back to LRCLIB.

use super::{Error, NowTrack, catalog::run};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use librespot_core::Session;
use serde::{Deserialize, Serialize};

const LRCLIB_GET: &str = "https://lrclib.net/api/get";
const LRCLIB_SEARCH: &str = "https://lrclib.net/api/search";
const USER_AGENT: &str = concat!("spotitui/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricsLine {
    /// When the line starts; `None` for unsynced lyrics.
    pub start_ms: Option<u64>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    pub lines: Vec<LyricsLine>,
    pub synced: bool,
    pub provider: String,
}

impl Lyrics {
    /// The line being sung at `position_ms`, for synced lyrics.
    pub fn current(&self, position_ms: u64) -> Option<usize> {
        if !self.synced {
            return None;
        }
        self.lines
            .iter()
            .rposition(|line| line.start_ms.is_some_and(|start| start <= position_ms))
    }

    /// Looks up lyrics for `track`. `Ok(None)` means no provider has any.
    pub async fn fetch(
        session: Session,
        http: reqwest::Client,
        track: NowTrack,
    ) -> Result<Option<Self>, Error> {
        if let Some(id) = track.track_id().map(str::to_owned) {
            match run(spotify(session, id)).await {
                Ok(Some(lyrics)) => return Ok(Some(lyrics)),
                Ok(None) => {}
                Err(error) => log::info!("Spotify lyrics failed, trying LRCLIB: {error}"),
            }
        }
        lrclib(&http, &track).await
    }
}

#[derive(Deserialize)]
struct SpotifyResponse {
    lyrics: SpotifyLyrics,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpotifyLyrics {
    sync_type: String,
    lines: Vec<SpotifyLine>,
    #[serde(default)]
    provider_display_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpotifyLine {
    start_time_ms: String,
    words: String,
}

async fn spotify(session: Session, id: String) -> Result<Option<Lyrics>, Error> {
    let endpoint = format!("/color-lyrics/v2/track/{id}?format=json&vocalRemoval=false");
    let mut headers = HeaderMap::new();
    headers.insert("app-platform", HeaderValue::from_static("WebPlayer"));
    headers.insert("accept", HeaderValue::from_static("application/json"));
    let body = match session
        .spclient()
        .request(&Method::GET, &endpoint, Some(headers), None)
        .await
    {
        Ok(body) => body,
        // Spotify answers 404 for tracks without lyrics.
        Err(error) if error.to_string().contains("404") => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if body.is_empty() {
        return Ok(None);
    }
    let response: SpotifyResponse = serde_json::from_slice(&body)?;
    let synced = response.lyrics.sync_type == "LINE_SYNCED";
    let lines: Vec<LyricsLine> = response
        .lyrics
        .lines
        .into_iter()
        .map(|line| LyricsLine {
            start_ms: synced.then(|| line.start_time_ms.parse().ok()).flatten(),
            text: clean(&line.words),
        })
        .collect();
    let provider = match response.lyrics.provider_display_name.as_str() {
        "" => "Spotify".to_owned(),
        name => name.to_owned(),
    };
    Ok(non_empty(Lyrics {
        lines,
        synced,
        provider,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibRecord {
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}

async fn lrclib(http: &reqwest::Client, track: &NowTrack) -> Result<Option<Lyrics>, Error> {
    let artist = track.artists.first().cloned().unwrap_or_default();
    let duration = (track.duration_ms / 1000).to_string();
    let exact = http
        .get(LRCLIB_GET)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .query(&[
            ("track_name", track.name.as_str()),
            ("artist_name", artist.as_str()),
            ("album_name", track.album.as_str()),
            ("duration", duration.as_str()),
        ])
        .send()
        .await?;
    let record = if exact.status() == StatusCode::NOT_FOUND {
        // The exact lookup needs matching metadata; a search tolerates small differences.
        let mut found: Vec<LrclibRecord> = http
            .get(LRCLIB_SEARCH)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .query(&[
                ("track_name", track.name.as_str()),
                ("artist_name", &artist),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let synced = |record: &LrclibRecord| {
            record
                .synced_lyrics
                .as_deref()
                .is_some_and(|text| !text.is_empty())
        };
        let best = found.iter().position(synced).or_else(|| {
            found
                .iter()
                .position(|record| record.plain_lyrics.is_some())
        });
        match best {
            Some(index) => found.swap_remove(index),
            None => return Ok(None),
        }
    } else {
        exact.error_for_status()?.json().await?
    };
    Ok(from_lrclib(record))
}

fn from_lrclib(record: LrclibRecord) -> Option<Lyrics> {
    let provider = "LRCLIB".to_owned();
    if record.instrumental {
        return Some(Lyrics {
            lines: vec![LyricsLine {
                start_ms: None,
                text: "♪ Instrumental ♪".into(),
            }],
            synced: false,
            provider,
        });
    }
    if let Some(lines) = record.synced_lyrics.as_deref().map(parse_lrc)
        && !lines.is_empty()
    {
        return non_empty(Lyrics {
            lines,
            synced: true,
            provider,
        });
    }
    let lines = record
        .plain_lyrics?
        .lines()
        .map(|text| LyricsLine {
            start_ms: None,
            text: text.trim().to_owned(),
        })
        .collect();
    non_empty(Lyrics {
        lines,
        synced: false,
        provider,
    })
}

/// Parses LRC text: `[mm:ss.xx] words`, possibly with several stamps per line.
pub fn parse_lrc(text: &str) -> Vec<LyricsLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some((stamp, after)) = stripped.split_once(']') else {
                break;
            };
            // Tags such as [ar:Artist] carry no timing.
            if let Some(ms) = parse_stamp(stamp) {
                stamps.push(ms);
            }
            rest = after;
        }
        let words = clean(rest);
        lines.extend(stamps.into_iter().map(|start| LyricsLine {
            start_ms: Some(start),
            text: words.clone(),
        }));
    }
    lines.sort_by_key(|line| line.start_ms);
    lines
}

fn parse_stamp(stamp: &str) -> Option<u64> {
    let (minutes, seconds) = stamp.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;
    let (whole, fraction) = seconds.split_once(['.', ':']).unwrap_or((seconds, "0"));
    let whole: u64 = whole.trim().parse().ok()?;
    let fraction_ms = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.get(..3)?.parse().ok()?,
    };
    Some(minutes * 60_000 + whole * 1000 + fraction_ms)
}

/// Spotify marks instrumental breaks with "♪"; keep those, trim everything else.
fn clean(words: &str) -> String {
    words.trim().to_owned()
}

fn non_empty(lyrics: Lyrics) -> Option<Lyrics> {
    lyrics
        .lines
        .iter()
        .any(|line| !line.text.is_empty())
        .then_some(lyrics)
}

#[cfg(test)]
mod tests {
    use super::{Lyrics, LyricsLine, lrclib, parse_lrc};
    use crate::spotify::NowTrack;

    /// Talks to lrclib.net; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn lrclib_finds_synced_lyrics() {
        let track = NowTrack {
            uri: "spotify:track:5W3cjX2J3tjhG8zb6u0qHn".into(),
            name: "Harder, Better, Faster, Stronger".into(),
            artists: vec!["Daft Punk".into()],
            album: "Discovery".into(),
            duration_ms: 224_000,
            art_url: None,
        };
        let lyrics = lrclib(&reqwest::Client::new(), &track)
            .await
            .expect("LRCLIB answers")
            .expect("LRCLIB has these lyrics");
        assert_eq!(lyrics.provider, "LRCLIB");
        assert!(
            lyrics
                .lines
                .iter()
                .any(|line| line.text == "Work it harder")
        );
        println!("synced: {}, lines: {}", lyrics.synced, lyrics.lines.len());
    }

    #[test]
    fn lrc_lines_are_timed_and_sorted() {
        let lines = parse_lrc("[ar:Someone]\n[00:12.50]Hello\n[01:02.123][00:05.00]Again\n[00:20]");
        let starts: Vec<_> = lines.iter().map(|line| line.start_ms).collect();
        assert_eq!(
            starts,
            [Some(5_000), Some(12_500), Some(20_000), Some(62_123)]
        );
        assert_eq!(lines[0].text, "Again");
        assert_eq!(lines[2].text, "");
    }

    #[test]
    fn the_current_line_is_the_last_one_started() {
        let lyrics = Lyrics {
            lines: [1_000, 5_000, 9_000]
                .map(|start| LyricsLine {
                    start_ms: Some(start),
                    text: "la".into(),
                })
                .to_vec(),
            synced: true,
            provider: "test".into(),
        };
        assert_eq!(lyrics.current(0), None);
        assert_eq!(lyrics.current(5_000), Some(1));
        assert_eq!(lyrics.current(60_000), Some(2));
    }
}
