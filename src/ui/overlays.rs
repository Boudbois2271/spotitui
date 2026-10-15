//! The welcome screen and the keys dialog.

use super::{button, ellipsize, panel, wrap};
use crate::app::{Action, App, Hit, Screen};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph},
};
use unicode_width::UnicodeWidthStr;

const CARD_WIDTH: u16 = 60;
/// Heights, in half rows, of the logo's bars.
const LOGO_BARS: [usize; 7] = [2, 4, 6, 3, 5, 2, 4];
const LOGO_PATTERN: [usize; 10] = [1, 3, 6, 4, 2, 5, 6, 3, 1, 4];
const LOGIN_LABEL: &str = "  Log in with Spotify  ↵  ";

/// One line of the keys dialog.
enum HelpRow {
    Section(&'static str),
    /// Two key/description pairs side by side; an empty key leaves a pair out.
    Keys(&'static str, &'static str, &'static str, &'static str),
    Note(&'static str),
    Blank,
}

const HELP: &[HelpRow] = &[
    HelpRow::Section("PLAYBACK"),
    HelpRow::Keys("Space", "play / pause", "n / p", "next / previous"),
    HelpRow::Keys("b / w", "seek 10 seconds", "s / r", "shuffle / repeat"),
    HelpRow::Keys("[ / ]", "volume down / up", "m", "mute"),
    HelpRow::Blank,
    HelpRow::Section("MOVING AROUND"),
    HelpRow::Keys("j / k", "down / up a row", "l", "open"),
    HelpRow::Keys("gg / G", "first / last row", "h", "back"),
    HelpRow::Keys("^d / ^u", "half a screen down / up", "L", "lyrics"),
    HelpRow::Keys("/", "search Spotify", "^f", "find in this list"),
    HelpRow::Blank,
    HelpRow::Section("THE HIGHLIGHTED TRACK"),
    HelpRow::Keys("l", "play", "o / a", "open album / artist"),
    HelpRow::Keys("z", "add to queue", "Z", "play next"),
    HelpRow::Keys("S", "save to Liked Songs", "A", "add to playlists"),
    HelpRow::Blank,
    HelpRow::Note("In lyrics, j / k pick a line and l jumps there."),
    HelpRow::Note("In ^f, Enter goes from typing to the matches; track keys work there."),
    HelpRow::Note("q quits · Other Spotify apps see this computer as “spotitui”."),
];

pub fn welcome(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let width = CARD_WIDTH.min(area.width);
    let x = area.x + (area.width - width) / 2;
    let mut lines: Vec<(Line, Option<Action>)> = Vec::new();
    let animate = matches!(app.screen, Screen::Connecting | Screen::Waiting(_));
    for row in logo(animate.then_some(app.ticks)) {
        lines.push((Line::styled(row, Style::new().fg(theme.accent)), None));
    }
    lines.push((Line::raw(""), None));
    lines.push((
        Line::styled(
            "spotitui",
            Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        None,
    ));
    lines.push((
        Line::styled(
            "Your music, in the terminal.",
            Style::new().fg(theme.subtext),
        ),
        None,
    ));
    lines.push((Line::raw(""), None));
    if app.needs_login() {
        let style = Style::new()
            .fg(theme.on_accent)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD);
        lines.push((Line::styled(LOGIN_LABEL, style), Some(Action::LogIn)));
        lines.push((Line::raw(""), None));
    }
    let (status, status_style) = match &app.screen {
        Screen::Connecting => (
            app.service_note
                .clone()
                .unwrap_or_else(|| "Connecting to Spotify…".to_owned()),
            Style::new().fg(theme.subtext),
        ),
        Screen::LoggedOut => (
            "Log in once; your login is kept in the system keyring.".to_owned(),
            Style::new().fg(theme.subtext),
        ),
        Screen::Waiting(_) => (
            "Approve spotitui in your browser. This screen continues by itself.".to_owned(),
            Style::new().fg(theme.subtext),
        ),
        Screen::Failed(error) if app.needs_login() => {
            (error.message.clone(), Style::new().fg(theme.error))
        }
        // The service keeps trying by itself; say so rather than ask for a login.
        Screen::Failed(error) => (
            format!("{} spotitui keeps trying by itself.", error.message),
            Style::new().fg(theme.error),
        ),
        Screen::Library => (String::new(), Style::new()),
    };
    for row in wrap(&status, width.into()) {
        lines.push((Line::styled(row, status_style), None));
    }
    if let Screen::Waiting(url) = &app.screen {
        lines.push((Line::raw(""), None));
        lines.push((
            Line::styled(
                "Browser didn't open? Copy this link:",
                Style::new().fg(theme.subtext),
            ),
            None,
        ));
        for row in wrap(url, width.into()) {
            lines.push((Line::styled(row, Style::new().fg(theme.dim)), None));
        }
    }
    lines.push((Line::raw(""), None));
    lines.push((
        Line::styled(
            "Playback needs Spotify Premium · Ctrl+C quits",
            Style::new().fg(theme.dim),
        ),
        None,
    ));
    let height = lines.len() as u16;
    let first = area.y + area.height.saturating_sub(height) / 2;
    for (y, (line, action)) in (first..).zip(lines) {
        if y >= area.bottom() {
            break;
        }
        let row = Rect::new(x, y, width, 1);
        if let Some(action) = action {
            let label_width = line.width() as u16;
            let button_area = Rect::new(
                x + (width.saturating_sub(label_width)) / 2,
                y,
                label_width,
                1,
            );
            let style = line.style;
            button(frame, app, button_area, LOGIN_LABEL, style, action);
        } else {
            frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), row);
        }
    }
}

/// The logo: equalizer bars, dancing while `tick` is given.
fn logo(tick: Option<u64>) -> Vec<String> {
    let heights: Vec<usize> = LOGO_BARS
        .iter()
        .enumerate()
        .map(|(bar, height)| match tick {
            Some(tick) => LOGO_PATTERN[(tick as usize + bar * 3) % LOGO_PATTERN.len()],
            None => *height,
        })
        .collect();
    (0..3)
        .map(|row| {
            let floor = (2 - row) * 2;
            heights
                .iter()
                .map(|height| match height.saturating_sub(floor) {
                    0 => "  ",
                    1 => "▄ ",
                    _ => "█ ",
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .map(|row| format!("{row:<13}"))
        .collect()
}

pub fn help(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let width = 72.min(area.width);
    // The title, a blank line, the rows, and the border.
    let height = (u16::try_from(HELP.len()).unwrap_or(u16::MAX) + 4).min(area.height);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, dialog);
    let block = panel(app, true).padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    app.hits.push((area, Hit::Action(Action::CloseHelp)));
    let key = Style::new().fg(theme.accent);
    let text = Style::new().fg(theme.text);
    let dim = Style::new().fg(theme.dim);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Keys", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled("   ? or h closes", dim),
        ]),
        Line::raw(""),
    ];
    for row in HELP {
        lines.push(match row {
            HelpRow::Section(title) => Line::styled(*title, dim),
            HelpRow::Keys(left_key, left, right_key, right) => Line::from(vec![
                Span::styled(format!("{left_key:<9}"), key),
                Span::styled(format!("{left:<25}"), text),
                Span::styled(format!("{right_key:<9}"), key),
                Span::styled(*right, text),
            ]),
            HelpRow::Note(note) => Line::styled(*note, Style::new().fg(theme.subtext)),
            HelpRow::Blank => Line::raw(""),
        });
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The "add to playlist" picker, over the main area.
pub fn picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let Some(count) = app.picker.as_ref().map(|picker| picker.playlists.len()) else {
        return;
    };
    let width = 64.min(area.width);
    // Title, blank, the rows, blank, hint, and the border.
    let height = (u16::try_from(count).unwrap_or(u16::MAX).saturating_add(6)).min(area.height);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    // A click outside closes it, as with the keys dialog.
    app.hits.push((area, Hit::Action(Action::ClosePicker)));
    frame.render_widget(Clear, dialog);
    let block = panel(app, true).padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    app.hits.push((dialog, Hit::PickerPane));
    let rows_height = inner.height.saturating_sub(4);
    let Some(picker) = app.picker.as_mut() else {
        return;
    };
    picker.window.resize(rows_height.into());
    let title = format!("Add “{}” to", picker.track.name);
    frame.render_widget(
        Paragraph::new(ellipsize(&title, inner.width.into()))
            .style(Style::new().fg(theme.text).add_modifier(Modifier::BOLD)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let mut rows = Vec::new();
    for (row, index) in picker.window.visible(count).enumerate() {
        let marked = picker.marked[index];
        let mut style = if marked {
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.subtext)
        };
        if index == picker.window.selected {
            style = style.bg(theme.selection);
        }
        let check = if marked { "✓" } else { " " };
        let name = ellipsize(
            &picker.playlists[index].name,
            usize::from(inner.width).saturating_sub(5),
        );
        let area = Rect::new(inner.x, inner.y + 2 + row as u16, inner.width, 1);
        frame.render_widget(
            Paragraph::new(format!(" {check}  {name}")).style(style),
            area,
        );
        rows.push((area, Hit::PickerRow(index)));
    }
    let ticked = picker.marked_count();
    let hint = match ticked {
        0 => "Space select · l add here · h cancel".to_owned(),
        1 => "Space select · l add to 1 playlist · h cancel".to_owned(),
        n => format!("Space select · l add to {n} playlists · h cancel"),
    };
    frame.render_widget(
        Paragraph::new(ellipsize(&hint, inner.width.into())).style(Style::new().fg(theme.dim)),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
    app.hits.extend(rows);
}

/// "Find in this list" (`^f`), over the main area.
pub fn finder(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let progress = app.finder_progress();
    let source = app.listing.source.title();
    let total = app.listing.uris.len();
    // Borrowed side by side: the finder changes, the list's tracks are only read.
    let App {
        finder, listing, ..
    } = app;
    let tracks = &listing.tracks;
    let Some(finder) = finder.as_mut() else {
        return;
    };
    let width = 76.min(area.width);
    // A steady size, so the window doesn't jump while typing.
    let height = 24.min(area.height);
    let dialog = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let mut hits = vec![(area, Hit::Action(Action::CloseFinder))];
    frame.render_widget(Clear, dialog);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);
    hits.push((dialog, Hit::FinderPane));
    let line = |offset: u16| Rect::new(inner.x, inner.y + offset, inner.width, 1);

    // Title, and how many match.
    let mut count = format!("{} of {total}", finder.matches.len());
    if let Some((loaded, total)) = progress {
        count = format!("{count} · loading {loaded}/{total}");
    }
    let title_width = usize::from(inner.width).saturating_sub(count.width() + 2);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            ellipsize(&format!("Find in {source}"), title_width),
            Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
        )])),
        line(0),
    );
    frame.render_widget(
        Paragraph::new(count)
            .style(Style::new().fg(theme.subtext))
            .alignment(Alignment::Right),
        line(0),
    );

    // The query.
    let query_style = if finder.typing {
        Style::new().fg(theme.text)
    } else {
        Style::new().fg(theme.subtext)
    };
    let mut query = vec![
        Span::styled("⌕ ", Style::new().fg(theme.accent)),
        Span::styled(finder.query.clone(), query_style),
    ];
    if finder.typing {
        query.push(Span::styled("▏", Style::new().fg(theme.accent)));
    }
    if finder.query.is_empty() {
        query.push(Span::styled(
            "title, artist or album",
            Style::new().fg(theme.dim).add_modifier(Modifier::ITALIC),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(query)), line(1));

    // The matches.
    let rows_height = inner.height.saturating_sub(5);
    finder.window.resize(rows_height.into());
    if finder.matches.is_empty() {
        let message = if progress.is_some() {
            "Searching while the list loads…"
        } else {
            "No matches."
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::new().fg(theme.subtext)),
            line(3),
        );
    }
    let text_width = usize::from(inner.width).saturating_sub(2);
    for (row, at) in finder.window.visible(finder.matches.len()).enumerate() {
        let Some(track) = tracks.get(finder.matches[at]) else {
            continue;
        };
        let title_width = (text_width * 3 / 5).min(track.name.width());
        let title = ellipsize(&track.name, title_width);
        let artist = ellipsize(
            &track.artist_names(),
            text_width.saturating_sub(title.width() + 2),
        );
        let mut title_style = Style::new().fg(theme.text);
        let mut artist_style = Style::new().fg(theme.subtext);
        if at == finder.window.selected {
            title_style = title_style.bg(theme.selection).add_modifier(Modifier::BOLD);
            artist_style = artist_style.bg(theme.selection);
        }
        let area = line(3 + row as u16);
        let pad = text_width.saturating_sub(title.width() + 2 + artist.width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {title}  "), title_style),
                Span::styled(format!("{artist}{} ", " ".repeat(pad)), artist_style),
            ])),
            area,
        );
        hits.push((area, Hit::FinderRow(at)));
    }

    // What the keys do now.
    let hint = if finder.typing {
        "Enter browse the matches · Esc close"
    } else {
        "l play · z/Z queue/next · S save · A playlists · o/a open · ^f type · h close"
    };
    frame.render_widget(
        Paragraph::new(ellipsize(hint, inner.width.into())).style(Style::new().fg(theme.dim)),
        line(inner.height.saturating_sub(1)),
    );
    app.hits.extend(hits);
}

#[cfg(test)]
mod tests {
    use super::logo;

    #[test]
    fn the_logo_stacks_half_blocks() {
        let rows = logo(None);
        assert_eq!(rows.len(), 3);
        assert!(rows[2].starts_with("█ █ █"));
        assert!(rows.iter().all(|row| row.chars().count() == 13));
    }
}
