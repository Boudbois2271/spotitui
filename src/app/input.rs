//! Keyboard and mouse input.

use super::{Action, App, Focus, Hit, Screen, View, listing::Step};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Position;
use std::time::{Duration, Instant};

const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_ROWS: usize = 3;

impl App {
    pub fn input(&mut self, event: &Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.key(*key),
            Event::Mouse(mouse) => self.mouse(*mouse),
            Event::Paste(text) if self.focus == Focus::Search => {
                self.search
                    .push_str(text.lines().next().unwrap_or_default());
            }
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.screen != Screen::Library {
            match key.code {
                KeyCode::Enter => self.act(Action::LogIn),
                KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
                _ => {}
            }
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Char('?' | 'q' | 'h') => self.help = false,
                // Playback keys work over the help too.
                code if is_playback_key(code) => {
                    self.global_key(key);
                }
                _ => {}
            }
            return;
        }
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
        if self.finder.is_some() {
            self.finder_key(key);
            return;
        }
        // ^f finds in the list on screen; / searches Spotify.
        if control && key.code == KeyCode::Char('f') {
            self.open_finder();
            return;
        }
        if self.focus == Focus::Search {
            self.search_key(key);
            return;
        }
        let pending_g = std::mem::take(&mut self.pending_g);
        if self.global_key(key) {
            return;
        }
        match self.focus {
            Focus::Sidebar => self.sidebar_key(key, pending_g),
            Focus::Tracks if self.view == View::Lyrics => self.lyrics_key(key, pending_g),
            Focus::Tracks | Focus::Search => self.tracks_key(key, pending_g),
        }
    }

    /// Keys that work in every pane; returns whether `key` was one.
    fn global_key(&mut self, key: KeyEvent) -> bool {
        // Layouts that type brackets with AltGr report them with Ctrl+Alt held.
        match key.code {
            KeyCode::Char(']') => {
                self.change_volume(true);
                return true;
            }
            KeyCode::Char('[') => {
                self.change_volume(false);
                return true;
            }
            _ => {}
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            return false;
        }
        match key.code {
            KeyCode::Char(' ') => self.act(Action::TogglePlay),
            KeyCode::Char('n') => self.act(Action::Next),
            KeyCode::Char('p') => self.act(Action::Previous),
            KeyCode::Right => self.seek_by(true),
            KeyCode::Left => self.seek_by(false),
            KeyCode::Char('+' | '=') => self.change_volume(true),
            KeyCode::Char('-') => self.change_volume(false),
            KeyCode::Char('s') => self.act(Action::Shuffle),
            KeyCode::Char('m') => self.toggle_mute(),
            KeyCode::Char('r') => self.act(Action::Repeat),
            KeyCode::Char('/') => self.act(Action::Search),
            KeyCode::Char('L') => self.act(Action::Lyrics),
            KeyCode::Char('?') => self.act(Action::Help),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('h') => self.go_back(),
            KeyCode::Backspace => {
                if self.view == View::Lyrics {
                    self.view = View::Tracks;
                } else if !self.back() && self.focus == Focus::Sidebar {
                    self.focus = Focus::Tracks;
                }
            }
            _ => return false,
        }
        true
    }

    /// Keys while "find in this list" is open: typing the query, then browsing.
    fn finder_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let Some(finder) = self.finder.as_mut() else {
            return;
        };
        if finder.typing {
            // A text field: Enter and Esc are its keys, as in the search box.
            match key.code {
                KeyCode::Esc => self.close_finder(),
                KeyCode::Enter => finder.typing = false,
                KeyCode::Backspace => {
                    finder.query.pop();
                    self.refilter();
                }
                KeyCode::Char('u') if control => {
                    finder.query.clear();
                    self.refilter();
                }
                KeyCode::Char('w') if control => {
                    let kept = finder
                        .query
                        .trim_end()
                        .rfind(' ')
                        .map_or(0, |space| space + 1);
                    finder.query.truncate(kept);
                    self.refilter();
                }
                KeyCode::Char(character) if !control => {
                    finder.query.push(character);
                    self.refilter();
                }
                _ => {}
            }
            return;
        }
        let pending_g = std::mem::take(&mut self.pending_g);
        // Track actions need a match; with none, the main list's row isn't one.
        let found = finder.current().is_some();
        match key.code {
            KeyCode::Char('f') if control => finder.typing = true,
            KeyCode::Char('h') => self.close_finder(),
            KeyCode::Char('l') if found => {
                // Play it, and land on it in the list.
                self.close_finder();
                self.play_selected();
            }
            KeyCode::Char('o') if found => {
                self.close_finder();
                self.open_album();
            }
            KeyCode::Char('a') if found => {
                self.close_finder();
                self.open_artist();
            }
            KeyCode::Char('z') if found => self.queue_selected(false),
            KeyCode::Char('Z') if found => self.queue_selected(true),
            KeyCode::Char('S') if found => self.like_selected(),
            KeyCode::Char('A') if found => self.open_picker(),
            code => match step(code, key.modifiers, pending_g) {
                Some(step) => self.finder_step(step),
                None if code == KeyCode::Char('g') => self.pending_g = true,
                // Playback keys work here too, and ? and q as everywhere.
                None if is_playback_key(code) || matches!(code, KeyCode::Char('?' | 'q')) => {
                    self.global_key(key);
                }
                None => {}
            },
        }
    }

    /// Keys while the "add to playlist" picker is open.
    fn picker_key(&mut self, key: KeyEvent) {
        let pending_g = std::mem::take(&mut self.pending_g);
        match key.code {
            KeyCode::Char('h' | 'q' | 'A') => self.close_picker(),
            KeyCode::Char('l') => self.submit_picker(),
            KeyCode::Char(' ') => {
                if let Some(picker) = &mut self.picker {
                    let index = picker.window.selected;
                    picker.toggle(index);
                }
            }
            code => match step(code, key.modifiers, pending_g) {
                Some(step) => {
                    if let Some(picker) = &mut self.picker {
                        picker.step(step);
                    }
                }
                None => self.pending_g = code == KeyCode::Char('g'),
            },
        }
    }

    fn sidebar_key(&mut self, key: KeyEvent, pending_g: bool) {
        let count = self.shelf_count();
        match key.code {
            KeyCode::Char('l') => {
                let index = self.sidebar.selected;
                self.enter_shelf(index);
            }
            code => match step(code, key.modifiers, pending_g) {
                Some(step) => self.sidebar.step(step, count),
                None => self.pending_g = code == KeyCode::Char('g'),
            },
        }
    }

    fn tracks_key(&mut self, key: KeyEvent, pending_g: bool) {
        match key.code {
            KeyCode::Char('o') => self.open_album(),
            KeyCode::Char('a') => self.open_artist(),
            KeyCode::Char('z') => self.queue_selected(false),
            KeyCode::Char('Z') => self.queue_selected(true),
            KeyCode::Char('S') => self.like_selected(),
            KeyCode::Char('A') => self.open_picker(),
            // Opening a track plays it, as Enter does.
            KeyCode::Char('l') => self.play_selected(),
            code => match step(code, key.modifiers, pending_g) {
                Some(step) => self.move_selection(step),
                None => self.pending_g = code == KeyCode::Char('g'),
            },
        }
    }

    fn lyrics_key(&mut self, key: KeyEvent, pending_g: bool) {
        match key.code {
            KeyCode::Char('l') => {
                if let Some(index) = self.lyrics_cursor() {
                    self.seek_to_lyric(index);
                }
            }
            code => match step(code, key.modifiers, pending_g) {
                Some(Step::Up) => self.scroll_lyrics(-1),
                Some(Step::Down) => self.scroll_lyrics(1),
                Some(Step::HalfUp | Step::PageUp) => self.scroll_lyrics(-10),
                Some(Step::HalfDown | Step::PageDown) => self.scroll_lyrics(10),
                Some(Step::Top) => self.scroll_lyrics(isize::MIN / 2),
                Some(Step::Bottom) => self.scroll_lyrics(isize::MAX / 2),
                None => self.pending_g = code == KeyCode::Char('g'),
            },
        }
    }

    fn search_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.submit_search(),
            // Leaving the box drops what was typed and goes back to the sidebar's
            // Search entry.
            KeyCode::Esc => {
                self.search.clear();
                self.sidebar.select(0, self.shelf_count());
                self.focus = Focus::Sidebar;
            }
            KeyCode::Down => self.focus = Focus::Tracks,
            KeyCode::Backspace => {
                self.search.pop();
            }
            KeyCode::Char('u') if control => self.search.clear(),
            KeyCode::Char('w') if control => {
                let kept = self
                    .search
                    .trim_end()
                    .rfind(' ')
                    .map_or(0, |space| space + 1);
                self.search.truncate(kept);
            }
            KeyCode::Char(character) if !control => self.search.push(character),
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        let position = Position::new(mouse.column, mouse.row);
        let hits: Vec<Hit> = self
            .hits
            .iter()
            .rev()
            .filter(|(area, _)| area.contains(position))
            .map(|(_, hit)| *hit)
            .collect();
        let Some(&top) = hits.first() else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(top, position),
            MouseEventKind::ScrollDown => self.wheel(&hits, true),
            MouseEventKind::ScrollUp => self.wheel(&hits, false),
            _ => {}
        }
    }

    fn click(&mut self, hit: Hit, position: Position) {
        let double = self
            .last_click
            .is_some_and(|(at, last)| last == hit && at.elapsed() < DOUBLE_CLICK);
        self.last_click = Some((Instant::now(), hit));
        if self.help {
            self.help = false;
            return;
        }
        match hit {
            Hit::Action(action) => self.act(action),
            Hit::Shelf(index) => {
                self.focus = Focus::Sidebar;
                self.open_shelf(index);
            }
            Hit::Row(index) => {
                self.focus = Focus::Tracks;
                let already = self.window.selected == index;
                self.to_end = false;
                self.window.select(index, self.listing.tracks.len());
                if double || already {
                    self.play_selected();
                }
            }
            Hit::Lyric(index) => self.seek_to_lyric(index),
            // A click picks the row and ticks it, like moving there and pressing Space.
            Hit::PickerRow(index) => {
                if let Some(picker) = &mut self.picker {
                    picker.window.select(index, picker.playlists.len());
                    picker.toggle(index);
                }
            }
            Hit::PickerPane | Hit::FinderPane => {}
            Hit::FinderRow(row) => self.finder_select(row),
            Hit::Progress => self.click_progress(position),
            Hit::Volume => {}
            Hit::SearchInput => self.focus = Focus::Search,
            Hit::SidebarPane => self.focus = Focus::Sidebar,
            Hit::TracksPane | Hit::LyricsPane => self.focus = Focus::Tracks,
        }
    }

    fn click_progress(&mut self, position: Position) {
        let Some(area) = self
            .hits
            .iter()
            .find(|(_, hit)| *hit == Hit::Progress)
            .map(|(area, _)| *area)
        else {
            return;
        };
        let fraction = f64::from(position.x.saturating_sub(area.x)) / f64::from(area.width.max(1));
        let length = self.now.length().as_millis() as f64;
        self.seek_to((fraction * length) as u64);
    }

    fn wheel(&mut self, hits: &[Hit], down: bool) {
        if hits.contains(&Hit::FinderPane) && !hits.contains(&Hit::PickerPane) {
            for _ in 0..WHEEL_ROWS {
                self.finder_step(if down { Step::Down } else { Step::Up });
            }
            return;
        }
        if hits.contains(&Hit::PickerPane) {
            if let Some(picker) = &mut self.picker {
                picker.step(if down { Step::Down } else { Step::Up });
            }
            return;
        }
        // One notch is one 5% step.
        if hits.contains(&Hit::Volume) {
            self.change_volume(!down);
            return;
        }
        let pane = hits
            .iter()
            .find(|hit| matches!(hit, Hit::SidebarPane | Hit::TracksPane | Hit::LyricsPane));
        let step = if down { Step::Down } else { Step::Up };
        for _ in 0..WHEEL_ROWS {
            match pane {
                Some(Hit::SidebarPane) => self.sidebar.step(step, self.shelf_count()),
                Some(Hit::TracksPane) => self.move_selection(step),
                Some(Hit::LyricsPane) => self.scroll_lyrics(if down { 1 } else { -1 }),
                _ => return,
            }
        }
    }
}

/// Keys that control playback, which work over the help and the finder too.
fn is_playback_key(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char(' ' | 'n' | 'p' | '[' | ']' | '+' | '=' | '-' | 'm' | 's' | 'r')
            | KeyCode::Left
            | KeyCode::Right
    )
}

/// The movement a key asks for in a list: vim keys, arrows and paging.
fn step(code: KeyCode, modifiers: KeyModifiers, pending_g: bool) -> Option<Step> {
    let control = modifiers.contains(KeyModifiers::CONTROL);
    Some(match code {
        KeyCode::Char('j') if !control => Step::Down,
        KeyCode::Char('k') if !control => Step::Up,
        KeyCode::Char('d') if control => Step::HalfDown,
        KeyCode::Char('u') if control => Step::HalfUp,
        KeyCode::PageDown => Step::PageDown,
        KeyCode::PageUp => Step::PageUp,
        KeyCode::Home => Step::Top,
        KeyCode::End | KeyCode::Char('G') => Step::Bottom,
        KeyCode::Char('g') if pending_g => Step::Top,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{KeyCode, KeyModifiers, Step, step};

    #[test]
    fn vim_keys_move_and_arrows_do_not() {
        let none = KeyModifiers::NONE;
        assert_eq!(step(KeyCode::Char('j'), none, false), Some(Step::Down));
        // The arrow keys don't pick rows; j and k do.
        assert_eq!(step(KeyCode::Down, none, false), None);
        assert_eq!(step(KeyCode::Up, none, false), None);
        assert_eq!(step(KeyCode::Char('k'), none, false), Some(Step::Up));
        assert_eq!(step(KeyCode::Char('G'), none, false), Some(Step::Bottom));
        assert_eq!(step(KeyCode::Char('g'), none, false), None);
        assert_eq!(step(KeyCode::Char('g'), none, true), Some(Step::Top));
        assert_eq!(
            step(KeyCode::Char('d'), KeyModifiers::CONTROL, false),
            Some(Step::HalfDown)
        );
    }
}
