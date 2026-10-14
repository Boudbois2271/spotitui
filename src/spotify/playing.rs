//! What the speaker plays, kept in one record. The service's state and MPRIS
//! both read it, and windows keep their own copy with the same [`Clock`], so
//! they can't disagree about the track, the position or whether it plays.

use super::{NowTrack, Repeat};
use crate::protocol::PlayerUpdate;
use librespot_metadata::audio::{AudioItem, UniqueFields};
use librespot_playback::player::PlayerEvent;
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// A playback position that moves on by itself while running.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    position: Duration,
    since: Instant,
    running: bool,
    /// The track's length, which the position never passes; zero when unknown.
    length: Duration,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            position: Duration::ZERO,
            since: Instant::now(),
            running: false,
            length: Duration::ZERO,
        }
    }
}

impl Clock {
    /// A new track: back to the start, keeping whether it runs.
    pub fn restart(&mut self, length: Duration) {
        self.length = length;
        self.position = Duration::ZERO;
        self.since = Instant::now();
    }

    /// The speaker says where it is and whether it plays.
    pub fn report(&mut self, position: Duration, running: bool) {
        self.position = position;
        self.since = Instant::now();
        self.running = running;
    }

    /// Starts or stops at the position reached so far.
    pub fn set_running(&mut self, running: bool) {
        if running != self.running {
            self.position = self.elapsed();
            self.since = Instant::now();
            self.running = running;
        }
    }

    pub fn running(&self) -> bool {
        self.running
    }

    pub fn length(&self) -> Duration {
        self.length
    }

    pub fn elapsed(&self) -> Duration {
        let since = if self.running {
            self.since.elapsed()
        } else {
            Duration::ZERO
        };
        let elapsed = self.position + since;
        if self.length.is_zero() {
            elapsed
        } else {
            elapsed.min(self.length)
        }
    }

    pub fn elapsed_ms(&self) -> u32 {
        u32::try_from(self.elapsed().as_millis()).unwrap_or(u32::MAX)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Stopped,
    Playing,
    Paused,
}

/// The speaker's playback, as its events describe it.
#[derive(Clone, Debug)]
pub struct Playing {
    pub track: Option<NowTrack>,
    pub status: Status,
    pub clock: Clock,
    pub volume: u16,
    pub shuffle: bool,
    pub repeat: Repeat,
}

/// The record the service and MPRIS share.
pub type Shared = Arc<Mutex<Playing>>;

pub fn shared(volume: u16) -> Shared {
    Arc::new(Mutex::new(Playing::new(volume)))
}

/// Locks the record; a panic while holding it leaves plain data behind.
pub fn lock(shared: &Shared) -> MutexGuard<'_, Playing> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Playing {
    pub fn new(volume: u16) -> Self {
        Self {
            track: None,
            status: Status::Stopped,
            clock: Clock::default(),
            volume,
            shuffle: false,
            repeat: Repeat::Off,
        }
    }

    pub fn playing(&self) -> bool {
        self.status == Status::Playing
    }

    /// Takes a speaker event; returns what changed, as windows hear it.
    pub fn apply(&mut self, event: &PlayerEvent) -> Option<PlayerUpdate> {
        let ms = |position_ms: u32| Duration::from_millis(position_ms.into());
        Some(match event {
            PlayerEvent::TrackChanged { audio_item } => {
                let track = now_track(audio_item);
                self.clock
                    .restart(Duration::from_millis(track.duration_ms));
                self.track = Some(track.clone());
                PlayerUpdate::Track(track)
            }
            PlayerEvent::Playing { position_ms, .. } => {
                self.status = Status::Playing;
                self.clock.report(ms(*position_ms), true);
                PlayerUpdate::Playing {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::Paused { position_ms, .. } => {
                self.status = Status::Paused;
                self.clock.report(ms(*position_ms), false);
                PlayerUpdate::Paused {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::Seeked { position_ms, .. } => {
                let running = self.clock.running();
                self.clock.report(ms(*position_ms), running);
                PlayerUpdate::Seeked {
                    position_ms: *position_ms,
                }
            }
            PlayerEvent::PositionCorrection { position_ms, .. }
            | PlayerEvent::PositionChanged { position_ms, .. } => {
                let running = self.clock.running();
                self.clock.report(ms(*position_ms), running);
                PlayerUpdate::Position {
                    position_ms: *position_ms,
                }
            }
            // The track stays shown, stopped where it got to.
            PlayerEvent::Stopped { .. } => {
                self.status = Status::Stopped;
                self.clock.set_running(false);
                PlayerUpdate::Stopped
            }
            PlayerEvent::VolumeChanged { volume } => {
                self.volume = *volume;
                PlayerUpdate::Volume(*volume)
            }
            PlayerEvent::ShuffleChanged { shuffle } => {
                self.shuffle = *shuffle;
                PlayerUpdate::Shuffle(*shuffle)
            }
            PlayerEvent::RepeatChanged { context, track } => {
                self.repeat = Repeat::from_flags(*context, *track);
                PlayerUpdate::Repeat(self.repeat)
            }
            PlayerEvent::Unavailable { .. } => PlayerUpdate::Unavailable,
            _ => return None,
        })
    }
}

/// The now-playing details librespot reports with a track change.
pub fn now_track(item: &AudioItem) -> NowTrack {
    let (artists, album, album_artists) = match &item.unique_fields {
        UniqueFields::Track {
            artists,
            album,
            album_artists,
            ..
        } => (
            artists.0.iter().map(|artist| artist.name.clone()).collect(),
            album.clone(),
            album_artists.clone(),
        ),
        UniqueFields::Local {
            artists,
            album,
            album_artists,
            ..
        } => (
            artists.iter().cloned().collect(),
            album.clone().unwrap_or_default(),
            album_artists.iter().cloned().collect(),
        ),
        UniqueFields::Episode { show_name, .. } => (Vec::new(), show_name.clone(), Vec::new()),
    };
    NowTrack {
        uri: item.uri.clone(),
        name: item.name.clone(),
        artists,
        album,
        album_artists,
        duration_ms: item.duration_ms.into(),
        art_url: item
            .covers
            .iter()
            .max_by_key(|cover| cover.width)
            .map(|cover| cover.url.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, Playing, Status};
    use crate::protocol::PlayerUpdate;
    use librespot_playback::player::PlayerEvent;
    use std::time::Duration;

    #[test]
    fn the_clock_runs_stops_where_it_got_to_and_stays_in_the_track() {
        let mut clock = Clock::default();
        clock.restart(Duration::from_secs(10));
        clock.report(Duration::from_secs(4), true);
        std::thread::sleep(Duration::from_millis(20));
        clock.set_running(false);
        let stopped = clock.elapsed();
        assert!(stopped >= Duration::from_millis(4020));
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(clock.elapsed(), stopped, "a stopped clock stands still");
        clock.report(Duration::from_secs(60), false);
        assert_eq!(clock.elapsed(), Duration::from_secs(10), "never past the end");
    }

    #[test]
    fn stopping_keeps_the_position_reached() {
        let mut playing = Playing::new(0);
        let track_id = librespot_core::SpotifyUri::from_uri("spotify:track:4uLU6hMCjMI75M1A2tKUQC")
            .expect("a valid URI");
        playing.apply(&PlayerEvent::Playing {
            play_request_id: 0,
            track_id: track_id.clone(),
            position_ms: 30_000,
        });
        std::thread::sleep(Duration::from_millis(20));
        let update = playing.apply(&PlayerEvent::Stopped {
            play_request_id: 0,
            track_id,
        });
        assert_eq!(update, Some(PlayerUpdate::Stopped));
        assert_eq!(playing.status, Status::Stopped);
        assert!(playing.clock.elapsed_ms() >= 30_020, "not back to the last report");
        let volume = playing.apply(&PlayerEvent::VolumeChanged { volume: 7 });
        assert_eq!((volume, playing.volume), (Some(PlayerUpdate::Volume(7)), 7));
    }
}
