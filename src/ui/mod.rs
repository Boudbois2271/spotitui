//! Drawing. Each frame also records clickable regions in `App::hits`.

mod library;
mod lyrics;
mod overlays;
mod player;

use crate::app::{Action, App, Hit, Screen, View};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthStr;

/// Below this width the sidebar folds away.
const SIDEBAR_MIN_WIDTH: u16 = 72;
const SIDEBAR_WIDTH: u16 = 28;
const PLAYER_HEIGHT: u16 = 5;
const EQUALIZER_LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const EQUALIZER_PATTERN: [usize; 12] = [2, 5, 7, 4, 6, 3, 1, 4, 7, 5, 2, 6];

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    let area = frame.area();
    frame.render_widget(Block::new().style(Style::new().fg(app.theme.text)), area);
    if app.screen != Screen::Library {
        overlays::welcome(frame, app, area);
        return;
    }
    let [top, body, player_area, notice] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(PLAYER_HEIGHT),
        Constraint::Length(1),
    ])
    .areas(area);
    top_bar(frame, app, top);
    if app.help {
        overlays::help(frame, app, body);
    } else {
        let main = if area.width >= SIDEBAR_MIN_WIDTH {
            let [side, main] =
                Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Fill(1)])
                    .areas(body);
            library::sidebar(frame, app, side);
            main
        } else {
            body
        };
        match app.view {
            View::Tracks => library::tracks(frame, app, main),
            View::Lyrics => lyrics::draw(frame, app, main),
        }
        // The picker can open from the finder, so it draws on top.
        if app.finder.is_some() {
            overlays::finder(frame, app, body);
        }
        if app.picker.is_some() {
            overlays::picker(frame, app, body);
        }
    }
    player::draw(frame, app, player_area);
    notice_line(frame, app, notice);
}

fn top_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(equalizer(app), Style::new().fg(theme.accent)),
        Span::styled(" spotitui", Style::new().add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(title), area);
    let buttons = [
        (
            "Search  /",
            Action::Search,
            app.searching() && app.view == View::Tracks,
        ),
        ("Lyrics  L", Action::Lyrics, app.view == View::Lyrics),
        ("Keys  ?", Action::Help, app.help),
    ];
    let mut right = area.right().saturating_sub(1);
    for (label, action, active) in buttons.into_iter().rev() {
        let width = label.width() as u16;
        let x = right.saturating_sub(width);
        let style = if active {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text)
        };
        button(
            frame,
            app,
            Rect::new(x, area.y, width, 1),
            label,
            style,
            action,
        );
        right = x.saturating_sub(3);
    }
}

fn notice_line(frame: &mut Frame, app: &App, area: Rect) {
    let Some(notice) = app.notice() else {
        return;
    };
    let color = if notice.error {
        app.theme.error
    } else {
        app.theme.accent
    };
    let text = Paragraph::new(format!(" {}", notice.text)).style(Style::new().fg(color));
    frame.render_widget(text, area);
}

/// Four bars that dance while music plays.
fn equalizer(app: &App) -> String {
    if !app.now.playing() {
        return EQUALIZER_LEVELS[0].to_string().repeat(4);
    }
    let tick = app.ticks as usize;
    (0..4)
        .map(|bar| EQUALIZER_LEVELS[EQUALIZER_PATTERN[(tick + bar * 5) % EQUALIZER_PATTERN.len()]])
        .collect()
}

/// Draws a one-line clickable label.
fn button(frame: &mut Frame, app: &mut App, area: Rect, label: &str, style: Style, action: Action) {
    let area = area.intersection(frame.area());
    frame.render_widget(Paragraph::new(label).style(style), area);
    app.hits.push((area, Hit::Action(action)));
}

/// A rounded box, in the accent color when focused.
fn panel<'a>(app: &App, focused: bool) -> Block<'a> {
    let color = if focused {
        app.theme.accent
    } else {
        app.theme.border
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
}

/// Cuts `text` to `width` columns, ending with "…" when shortened.
fn ellipsize(text: &str, width: usize) -> String {
    let text = displayable(text);
    if text.width() <= width {
        return text;
    }
    let mut kept = String::new();
    let mut used = 0;
    for character in text.chars() {
        let size = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + size + 1 > width {
            break;
        }
        kept.push(character);
        used += size;
    }
    kept.push('…');
    kept
}

/// Drops emoji modifiers (skin tones, joiners, variation selectors). Terminals disagree on
/// how wide such sequences are, and a wrong guess shifts the rest of the row.
fn displayable(text: &str) -> String {
    text.chars()
        .filter(|character| {
            !matches!(
                character,
                '\u{1F3FB}'..='\u{1F3FF}' | '\u{200D}' | '\u{FE0E}' | '\u{FE0F}'
            )
        })
        .collect()
}

/// Splits `text` into lines of at most `width` columns, breaking at spaces where possible.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let text = displayable(text);
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let candidate = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if candidate.width() <= width {
            line = candidate;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A single word wider than the line is split by columns.
        let mut rest = word.to_owned();
        while rest.width() > width {
            let mut cut = String::new();
            for character in rest.chars() {
                // A character wider than the whole line still goes, alone, or this
                // would never move on.
                if !cut.is_empty() && (cut.clone() + &character.to_string()).width() > width {
                    break;
                }
                cut.push(character);
            }
            rest = rest[cut.len()..].to_owned();
            lines.push(cut);
        }
        line = rest;
    }
    // A word split to its last character leaves nothing over for a line of its own.
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{draw, ellipsize, wrap};
    use crate::{
        app::{App, LyricsCache, LyricsStatus, Screen, listing::Loading},
        client::Service,
        protocol::{Failure, Kind},
        spotify::{Album, Artist, Lyrics, LyricsLine, NowTrack, Playlist, Track},
        theme::ThemeSource,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};
    use std::rc::Rc;

    /// A window with a list, playlists, a playing track and its lyrics, all with
    /// long and wide text.
    fn window() -> App {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(tx, crate::spotify::http(), ThemeSource::new(), Service::detached());
        let name = "A rather long title that goes on — 日本語のタイトル 🙏🏻 and more";
        let track = |index: usize| Track {
            uri: format!("spotify:track:{index}"),
            name: format!("{name} {index}"),
            artists: vec![Artist {
                uri: "spotify:artist:a".into(),
                name: "Someone & Someone Else feat. Another".into(),
            }],
            album: Album {
                uri: "spotify:album:a".into(),
                name: "An album with a very long name (Deluxe Edition) 「DIGITAL」".into(),
            },
            duration_ms: 3_725_000,
        };
        app.screen = Screen::Library;
        app.listing.uris = (0..60).map(|index| format!("spotify:track:{index}")).collect();
        app.listing.tracks = (0..60).map(|index| Rc::new(track(index))).collect();
        app.listing.requested = 60;
        app.listing.loading = Loading::Idle;
        app.playlists = (0..30)
            .map(|index| {
                Rc::new(Playlist {
                    uri: format!("spotify:playlist:{index}"),
                    name: format!("Playlist with a long name, number {index} 🪩"),
                    editable: true,
                })
            })
            .collect();
        app.now.change_track(NowTrack {
            uri: "spotify:track:3".into(),
            name: name.into(),
            artists: vec!["Someone".into(), "Someone Else".into()],
            album: "An album".into(),
            album_artists: Vec::new(),
            duration_ms: 3_725_000,
            art_url: None,
        });
        app.now.report(1_234_567, true);
        app.lyrics = Some(LyricsCache::new(
            "spotify:track:3".into(),
            LyricsStatus::Ready(Rc::new(Lyrics {
                lines: (0..40)
                    .map(|index| LyricsLine {
                        start_ms: Some(index * 60_000),
                        text: format!("{name} — line {index}"),
                    })
                    .collect(),
                synced: true,
                provider: "Musixmatch".into(),
            })),
        ));
        app
    }

    fn press(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.input(&Event::Key(KeyEvent::new(code, modifiers)));
    }

    /// Every screen, overlay and view draws at every size from 1×1, without a
    /// panic: a window can be squeezed to anything in a tiling layout.
    #[tokio::test]
    async fn every_screen_draws_at_every_size() {
        let none = KeyModifiers::NONE;
        let failure = Failure::new(Kind::Other, "Something went wrong in a long and winding way ".repeat(4));
        type Setup = Box<dyn Fn(&mut App)>;
        let setups: Vec<(&str, Setup)> = vec![
            ("connecting", Box::new(|app: &mut App| app.screen = Screen::Connecting)),
            ("logged out", Box::new(|app: &mut App| app.screen = Screen::LoggedOut)),
            ("failed", Box::new(move |app: &mut App| app.screen = Screen::Failed(failure.clone()))),
            (
                "waiting",
                Box::new(|app: &mut App| {
                    app.screen = Screen::Waiting(format!("https://accounts.spotify.com/{}", "x".repeat(300)));
                }),
            ),
            ("library", Box::new(|_: &mut App| {})),
            ("help", Box::new(move |app: &mut App| press(app, KeyCode::Char('?'), none))),
            ("picker", Box::new(move |app: &mut App| press(app, KeyCode::Char('A'), none))),
            (
                "finder",
                Box::new(move |app: &mut App| {
                    press(app, KeyCode::Char('f'), KeyModifiers::CONTROL);
                    press(app, KeyCode::Char('a'), none);
                }),
            ),
            ("lyrics", Box::new(move |app: &mut App| press(app, KeyCode::Char('L'), none))),
            ("search", Box::new(move |app: &mut App| press(app, KeyCode::Char('/'), none))),
            ("sidebar", Box::new(move |app: &mut App| press(app, KeyCode::Char('h'), none))),
        ];
        // Every small size, then a few at each step of the layout.
        let widths = (1..=8u16).chain([12, 20, 30, 40, 60, 71, 72, 90, 120, 200]);
        let heights = (1..=6u16).chain([8, 12, 24, 40]);
        let sizes = widths
            .flat_map(|width| heights.clone().map(move |height| (width, height)))
            .chain([(300, 3), (3, 100)]);
        let sizes: Vec<(u16, u16)> = sizes.collect();
        for (label, setup) in &setups {
            let mut app = window();
            setup(&mut app);
            for &(width, height) in &sizes {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("a test terminal");
                let drawn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    terminal.draw(|frame| draw(frame, &mut app)).map(|_| ())
                }));
                assert!(
                    matches!(drawn, Ok(Ok(()))),
                    "the {label} screen fails to draw at {width}×{height}"
                );
            }
        }
    }

    #[test]
    fn long_text_is_shortened_with_an_ellipsis() {
        assert_eq!(ellipsize("Harder, Better", 20), "Harder, Better");
        assert_eq!(ellipsize("Harder, Better", 8), "Harder,…");
        assert_eq!(ellipsize("日本語テキスト", 7), "日本語…");
        assert_eq!(ellipsize("Hits home 🙏🏻", 20), "Hits home 🙏");
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_splits_long_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
        // Wider characters than the line: one per line, rather than looping forever.
        assert_eq!(wrap("사랑해", 1), ["사", "랑", "해"]);
        assert_eq!(wrap("🙏", 1), ["🙏"]);
    }
}
