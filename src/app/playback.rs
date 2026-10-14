//! What the speaker reports it is playing.

use crate::spotify::{NowTrack, Repeat};
use std::time::{Duration, Instant};

const DEFAULT_VOLUME: u16 = u16::MAX / 2;
/// Without word from the speaker for this long, a seek is taken as done.
const SEEK_SETTLE: Duration = Duration::from_secs(2);
/// How close a report must be to the seek target to count as its arrival.
const SEEK_TOLERANCE_MS: u32 = 1500;
/// How long after the user's last volume change reports of it are held back.
const VOLUME_SETTLE: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug)]
struct Seeking {
    /// The latest target.
    target: u32,
    /// Seeks sent but not yet reported.
    pending: u32,
    /// When a seek was last sent or reported.
    activity: Instant,
}

impl Seeking {
    fn reached(&self, position_ms: u32) -> bool {
        position_ms.abs_diff(self.target) <= SEEK_TOLERANCE_MS
    }

    fn settled(&self) -> bool {
        self.pending == 0 || self.activity.elapsed() >= SEEK_SETTLE
    }
}

#[derive(Clone, Debug)]
pub struct NowPlaying {
    pub track: Option<NowTrack>,
    pub playing: bool,
    position: Duration,
    reported: Instant,
    pub volume: u16,
    /// Muted output; the volume keeps its level underneath.
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Seeks the speaker has yet to report, while any are on their way.
    seeking: Option<Seeking>,
    /// The volume last asked for here, and when, while reports are held back.
    volume_target: Option<(u16, Instant)>,
    /// The latest volume reported while held back, taken once things settle.
    held_volume: Option<u16>,
}

impl Default for NowPlaying {
    fn default() -> Self {
        Self {
            track: None,
            playing: false,
            position: Duration::ZERO,
            reported: Instant::now(),
            volume: DEFAULT_VOLUME,
            muted: false,
            shuffle: false,
            repeat: Repeat::Off,
            seeking: None,
            volume_target: None,
            held_volume: None,
        }
    }
}

impl NowPlaying {
    #[cfg(test)]
    pub fn with_volume(volume: u16) -> Self {
        Self {
            volume,
            ..Self::default()
        }
    }

    /// Takes a position the speaker reports. While seeks are on their way, reports of
    /// earlier positions are stale and only the playing state is taken.
    pub fn report(&mut self, position_ms: u32, playing: bool) {
        if let Some(seeking) = self.seeking {
            if !seeking.reached(position_ms) && !seeking.settled() {
                self.set_playing(playing);
                return;
            }
            self.seeking = None;
        }
        self.position = Duration::from_millis(position_ms.into());
        self.reported = Instant::now();
        self.playing = playing;
    }

    /// The speaker finished one seek.
    pub fn seeked(&mut self, position_ms: u32) {
        if let Some(seeking) = &mut self.seeking {
            seeking.pending = seeking.pending.saturating_sub(1);
            seeking.activity = Instant::now();
        }
        let playing = self.playing;
        self.report(position_ms, playing);
    }

    /// Moves to `position_ms` right away, ahead of the speaker's report.
    pub fn seek(&mut self, position_ms: u32) {
        self.position = Duration::from_millis(position_ms.into());
        self.reported = Instant::now();
        let pending = self.seeking.map_or(0, |seeking| seeking.pending);
        self.seeking = Some(Seeking {
            target: position_ms,
            pending: pending + 1,
            activity: Instant::now(),
        });
    }

    /// Shows a volume right away, ahead of the speaker's report.
    pub fn set_volume(&mut self, volume: u16) {
        self.volume = volume;
        self.volume_target = Some((volume, Instant::now()));
        self.held_volume = None;
    }

    /// Takes a volume the service or speaker reports. While a key is held, reports
    /// of earlier presses keep arriving: the service confirms each at once, the
    /// speaker later. Taking one would make the next step start from an old value,
    /// so the volume would bounce back. Until the presses settle, reports only
    /// wait; the last one counts afterwards, in case the volume changed elsewhere.
    pub fn report_volume(&mut self, volume: u16) {
        if self.volume_target.is_some() {
            self.held_volume = Some(volume);
            self.settle_volume();
            return;
        }
        self.volume = volume;
    }

    /// Ends the hold once the presses have settled; called on every tick too.
    pub fn settle_volume(&mut self) {
        let Some((target, at)) = self.volume_target else {
            return;
        };
        if at.elapsed() < VOLUME_SETTLE {
            return;
        }
        self.volume_target = None;
        self.volume = self.held_volume.take().unwrap_or(target);
    }

    fn set_playing(&mut self, playing: bool) {
        if playing != self.playing {
            // Pausing or resuming keeps the position reached so far.
            self.position = self.elapsed();
            self.reported = Instant::now();
            self.playing = playing;
        }
    }

    pub fn change_track(&mut self, track: NowTrack) {
        self.track = Some(track);
        self.seeking = None;
        self.position = Duration::ZERO;
        self.reported = Instant::now();
    }

    pub fn uri(&self) -> Option<&str> {
        self.track.as_ref().map(|track| track.uri.as_str())
    }

    pub fn length(&self) -> Duration {
        self.track.as_ref().map_or(Duration::ZERO, |track| {
            Duration::from_millis(track.duration_ms)
        })
    }

    pub fn elapsed(&self) -> Duration {
        let since = if self.playing {
            self.reported.elapsed()
        } else {
            Duration::ZERO
        };
        let elapsed = self.position + since;
        match self.track {
            Some(_) => elapsed.min(self.length()),
            None => elapsed,
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
    }

    /// Fraction of the track played, from 0 to 1.
    pub fn progress(&self) -> f64 {
        let length = self.length();
        if length.is_zero() {
            0.0
        } else {
            (self.elapsed().as_secs_f64() / length.as_secs_f64()).clamp(0.0, 1.0)
        }
    }

    /// The volume in whole percent, rounded (half of the range shows as 50%).
    pub fn volume_percent(&self) -> u32 {
        (u32::from(self.volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX)
    }
}

/// The volume one 5% step up or down from `volume`, snapped to a multiple of 5%.
pub fn volume_step(volume: u16, up: bool) -> u16 {
    let percent = (u32::from(volume) * 100 + u32::from(u16::MAX) / 2) / u32::from(u16::MAX);
    let target = if up {
        (percent / 5 + 1) * 5
    } else {
        (percent.div_ceil(5)).saturating_sub(1) * 5
    }
    .min(100);
    // At most 100% of u16::MAX, so it fits.
    u16::try_from(target * u32::from(u16::MAX) / 100).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{NowPlaying, volume_step};

    #[test]
    fn volume_moves_in_snapped_five_percent_steps() {
        let percent = |volume| NowPlaying::with_volume(volume).volume_percent();
        assert_eq!(percent(u16::MAX / 2), 50);
        assert_eq!(percent(volume_step(u16::MAX / 2, true)), 55);
        assert_eq!(percent(volume_step(u16::MAX / 2, false)), 45);
        // 49% (an older default) snaps to the neighbouring multiples of 5.
        let forty_nine = u16::try_from(49 * u32::from(u16::MAX) / 100).unwrap_or(0);
        assert_eq!(percent(volume_step(forty_nine, true)), 50);
        assert_eq!(percent(volume_step(forty_nine, false)), 45);
        assert_eq!(volume_step(u16::MAX, true), u16::MAX);
        assert_eq!(volume_step(0, false), 0);
    }

    #[test]
    fn instant_confirmations_do_not_end_the_hold_early() {
        let mut now = NowPlaying::with_volume(u16::MAX / 2);
        let first = volume_step(now.volume, true);
        now.set_volume(first);
        let second = volume_step(now.volume, true);
        now.set_volume(second);
        // The service confirms the latest press at once...
        now.report_volume(second);
        // ...then the speaker's reports of both presses trail in.
        now.report_volume(first);
        assert_eq!(
            now.volume_percent(),
            60,
            "a trailing report must not undo a step"
        );
        now.report_volume(second);
        let third = volume_step(now.volume, true);
        assert_eq!(NowPlaying::with_volume(third).volume_percent(), 65);
    }

    #[test]
    fn a_change_from_elsewhere_shows_once_presses_settle() {
        let mut now = NowPlaying::with_volume(u16::MAX / 2);
        now.set_volume(volume_step(now.volume, true));
        now.volume_target = now
            .volume_target
            .map(|(target, at)| (target, at - super::VOLUME_SETTLE));
        now.report_volume(u16::MAX);
        assert_eq!(now.volume_percent(), 100);
    }

    #[test]
    fn late_volume_reports_do_not_undo_newer_steps() {
        let mut now = NowPlaying::with_volume(u16::MAX / 2);
        // Three quick presses down: 45%, 40%, 35%.
        let mut asked = Vec::new();
        for _ in 0..3 {
            let volume = volume_step(now.volume, false);
            now.set_volume(volume);
            asked.push(volume);
        }
        // The speaker confirms them one by one, after the presses.
        now.report_volume(asked[0]);
        assert_eq!(now.volume_percent(), 35);
        now.report_volume(asked[1]);
        assert_eq!(now.volume_percent(), 35);
        now.report_volume(asked[2]);
        assert_eq!(now.volume_percent(), 35);
    }

    #[test]
    fn reports_of_earlier_seeks_are_ignored_until_the_last_arrives() {
        let mut now = NowPlaying::default();
        now.report(0, false);
        now.seek(10_000);
        now.seek(20_000);
        now.seek(30_000);
        now.seeked(10_000);
        now.report(10_000, false);
        assert_eq!(now.elapsed_ms(), 30_000);
        now.seeked(20_000);
        assert_eq!(now.elapsed_ms(), 30_000);
        now.seeked(29_990);
        assert_eq!(now.elapsed_ms(), 29_990);
        // Once the seek has arrived, any report counts again.
        now.report(5_000, false);
        assert_eq!(now.elapsed_ms(), 5_000);
    }
}
