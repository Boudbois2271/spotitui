//! Seeks and volume changes, let through to the speaker at a steady pace.
//! Each one makes the speaker update Spotify's record of the playback. A held
//! key asks for a dozen a second; Spotify then refuses those updates for a
//! while (429), and the play queue observer falls behind.

use tokio::time::{Duration, Instant};

/// The least time between two seeks, or two volume changes, sent on.
pub const PACE: Duration = Duration::from_millis(300);

/// The latest value asked for: let through at once after a quiet spell, else
/// when its turn comes. Values asked for in between are skipped.
#[derive(Debug)]
pub struct Paced<T> {
    waiting: Option<T>,
    /// When the next value may go.
    turn: Option<Instant>,
}

impl<T> Default for Paced<T> {
    fn default() -> Self {
        Self {
            waiting: None,
            turn: None,
        }
    }
}

impl<T> Paced<T> {
    pub fn ask(&mut self, value: T) {
        self.waiting = Some(value);
    }

    pub fn clear(&mut self) {
        self.waiting = None;
    }

    /// When the value waiting may go; `None` with nothing waiting.
    pub fn due(&self) -> Option<Instant> {
        self.waiting.as_ref()?;
        Some(self.turn.unwrap_or_else(Instant::now))
    }

    /// The value waiting, if its turn has come at `now`.
    pub fn take(&mut self, now: Instant) -> Option<T> {
        if self.turn.is_some_and(|turn| now < turn) {
            return None;
        }
        let value = self.waiting.take()?;
        self.turn = Some(now + PACE);
        Some(value)
    }
}

/// The settings waiting for their turn.
#[derive(Debug, Default)]
pub struct Settings {
    /// A position within a track, by URI: dropped once another track plays.
    pub seek: Paced<(String, u32)>,
    pub volume: Paced<u16>,
}

impl Settings {
    /// When the next one may go.
    pub fn due(&self) -> Option<Instant> {
        self.seek.due().into_iter().chain(self.volume.due()).min()
    }
}

#[cfg(test)]
mod tests {
    use super::{PACE, Paced, Settings};
    use tokio::time::Instant;

    #[test]
    fn the_first_goes_at_once_then_the_latest_on_its_turn() {
        let start = Instant::now();
        let mut paced = Paced::default();
        assert_eq!(paced.due(), None, "nothing waits");
        paced.ask(1);
        assert_eq!(paced.take(start), Some(1));
        paced.ask(2);
        paced.ask(3);
        assert_eq!(paced.take(start + PACE / 2), None, "not its turn yet");
        assert_eq!(paced.due(), Some(start + PACE));
        assert_eq!(paced.take(start + PACE), Some(3), "only the latest");
        assert_eq!(paced.take(start + PACE * 3), None, "nothing waits");
        paced.ask(4);
        assert_eq!(paced.take(start + PACE * 3), Some(4), "at once after a quiet spell");
        paced.ask(5);
        paced.clear();
        assert_eq!(paced.take(start + PACE * 9), None, "a cleared value never goes");
    }

    #[test]
    fn seeks_and_volumes_take_turns_of_their_own() {
        let start = Instant::now();
        let mut settings = Settings::default();
        settings.seek.ask(("spotify:track:a".into(), 10_000));
        assert!(settings.seek.take(start).is_some());
        settings.volume.ask(7);
        assert_eq!(settings.volume.take(start), Some(7), "a seek doesn't hold a volume up");
        settings.seek.ask(("spotify:track:a".into(), 20_000));
        assert_eq!(settings.due(), Some(start + PACE));
    }
}
