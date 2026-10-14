//! The sidebar and the track list.

use super::{ellipsize, panel};
use crate::app::{
    App, FIXED_SHELVES, Focus, Hit,
    listing::{Loading, duration},
};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Paragraph, Row, Table},
};

const FIXED_LABELS: [&str; FIXED_SHELVES] = ["⌕ Search", "♥ Liked Songs", "◷ Recently played"];
/// Main-pane width below which the album column is left out.
const ALBUM_MIN_WIDTH: u16 = 80;

pub fn sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = panel(app, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push((area, Hit::SidebarPane));
    let [heading, fixed, _, playlists_heading, playlists] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(FIXED_SHELVES as u16),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let dim = Style::new().fg(app.theme.dim);
    frame.render_widget(Paragraph::new(" YOUR LIBRARY").style(dim), heading);
    frame.render_widget(Paragraph::new(" PLAYLISTS").style(dim), playlists_heading);
    for (index, label) in FIXED_LABELS.iter().enumerate() {
        let row = Rect::new(fixed.x, fixed.y + index as u16, fixed.width, 1);
        shelf(frame, app, row, index, label);
    }
    app.playlist_window.resize(playlists.height.into());
    if let Some(index) = app.sidebar.selected.checked_sub(FIXED_SHELVES) {
        app.playlist_window.select(index, app.playlists.len());
    }
    let visible = app.playlist_window.visible(app.playlists.len());
    if app.playlists.is_empty() {
        let text = Paragraph::new(" Loading…").style(dim);
        frame.render_widget(text, playlists);
    }
    for (row_index, index) in visible.enumerate() {
        let row = Rect::new(
            playlists.x,
            playlists.y + row_index as u16,
            playlists.width,
            1,
        );
        let name = app.playlists[index].name.clone();
        shelf(frame, app, row, FIXED_SHELVES + index, &name);
    }
}

fn shelf(frame: &mut Frame, app: &mut App, area: Rect, index: usize, label: &str) {
    let theme = app.theme;
    let mut style = if app.shelf_is_current(index) {
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.subtext)
    };
    if app.focus == Focus::Sidebar && app.sidebar.selected == index {
        style = style.bg(theme.selection);
    }
    let text = format!(
        " {}",
        ellipsize(label, usize::from(area.width).saturating_sub(2))
    );
    frame.render_widget(Paragraph::new(text).style(style), area);
    app.hits.push((area, Hit::Shelf(index)));
}

pub fn tracks(frame: &mut Frame, app: &mut App, area: Rect) {
    let searching = app.searching();
    let [heading, search, list, position] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if searching { 3 } else { 0 }),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let title = app.listing.source.title();
    let summary = app.listing.summary();
    let summary_style = match app.listing.loading {
        Loading::Failed(_) => Style::new().fg(app.theme.error),
        _ => Style::new().fg(app.theme.subtext),
    };
    heading_line(
        frame,
        app,
        heading,
        &title,
        Span::styled(summary, summary_style),
    );
    if searching {
        search_input(frame, app, search);
    }
    track_table(frame, app, list);
    position_line(frame, app, position);
}

/// A bold title on the left and a short summary on the right.
pub fn heading_line(frame: &mut Frame, app: &App, area: Rect, title: &str, summary: Span) {
    let summary_width = summary.width() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(summary_width + 1)])
            .areas(area);
    let title = ellipsize(title, usize::from(left.width).saturating_sub(1));
    let title = Paragraph::new(format!(" {title}"))
        .style(Style::new().fg(app.theme.text).add_modifier(Modifier::BOLD));
    frame.render_widget(title, left);
    frame.render_widget(
        Paragraph::new(Line::from(summary)).alignment(Alignment::Right),
        right,
    );
}

fn search_input(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let focused = app.focus == Focus::Search;
    let block = panel(app, focused);
    let line = if app.search.is_empty() && !focused {
        Line::styled(
            "Songs or artists… then Enter",
            Style::new().fg(theme.dim).add_modifier(Modifier::ITALIC),
        )
    } else {
        let mut spans = vec![Span::raw(app.search.clone())];
        if focused {
            spans.push(Span::styled("▏", Style::new().fg(theme.accent)));
        }
        if app.search.is_empty() {
            spans.push(Span::styled(
                "Songs or artists… then Enter",
                Style::new().fg(theme.dim).add_modifier(Modifier::ITALIC),
            ));
        }
        Line::from(spans)
    };
    frame.render_widget(
        Paragraph::new(line).block(block.padding(ratatui::widgets::Padding::horizontal(1))),
        area,
    );
    app.hits.push((area, Hit::SearchInput));
}

fn track_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let focused = app.focus == Focus::Tracks;
    let block = panel(app, focused).padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push((area, Hit::TracksPane));
    // One row for the column headings.
    app.window
        .resize(usize::from(inner.height.saturating_sub(1)));
    if app.listing.tracks.is_empty() {
        let (text, style) = match &app.listing.loading {
            Loading::Failed(error) => (error.message.clone(), Style::new().fg(theme.error)),
            _ if app.listing.is_empty() => (
                app.listing.source.empty_message().into(),
                Style::new().fg(theme.subtext),
            ),
            _ => ("Loading…".into(), Style::new().fg(theme.dim)),
        };
        let message = Paragraph::new(text)
            .style(style)
            .wrap(ratatui::widgets::Wrap { trim: true });
        frame.render_widget(message, inner.inner(ratatui::layout::Margin::new(0, 1)));
        return;
    }
    let with_album = inner.width >= ALBUM_MIN_WIDTH;
    let mut widths = vec![
        Constraint::Length(5),
        Constraint::Fill(3),
        Constraint::Fill(2),
    ];
    if with_album {
        widths.push(Constraint::Fill(2));
    }
    widths.push(Constraint::Length(7));
    let mut heading = vec!["#", "Title", "Artist"];
    if with_album {
        heading.push("Album");
    }
    heading.push("Time");
    let header = Row::new(heading).style(Style::new().fg(theme.dim));
    let playing = app.now.uri().map(str::to_owned);
    let visible = app.window.visible(app.listing.tracks.len());
    let column_widths = column_widths(inner.width, with_album);
    let rows: Vec<Row> = app.listing.tracks[visible.clone()]
        .iter()
        .zip(visible.clone())
        .map(|(track, index)| {
            let is_playing = playing.as_deref() == Some(track.uri.as_str());
            let marker = if is_playing {
                "♫".to_owned()
            } else {
                (index + 1).to_string()
            };
            let secondary = Style::new().fg(if is_playing {
                theme.accent
            } else {
                theme.subtext
            });
            let mut cells = vec![
                Cell::from(marker).style(Style::new().fg(if is_playing {
                    theme.accent
                } else {
                    theme.dim
                })),
                Cell::from(ellipsize(&track.name, column_widths[0])),
                Cell::from(ellipsize(&track.artist_names(), column_widths[1])).style(secondary),
            ];
            if with_album {
                cells.push(
                    Cell::from(ellipsize(&track.album.name, column_widths[2])).style(secondary),
                );
            }
            cells.push(Cell::from(duration(track.duration_ms)).style(secondary));
            let mut style = Style::new();
            if is_playing {
                style = style.fg(theme.accent).add_modifier(Modifier::BOLD);
            }
            if index == app.window.selected {
                style = style.bg(theme.selection);
            }
            Row::new(cells).style(style)
        })
        .collect();
    let table = Table::new(rows, widths).header(header).column_spacing(1);
    frame.render_widget(table, inner);
    for (row_index, index) in visible.enumerate() {
        let row = Rect::new(inner.x, inner.y + 1 + row_index as u16, inner.width, 1);
        app.hits.push((row, Hit::Row(index)));
    }
}

/// The widths of the title, artist and album columns, matching the table's layout.
fn column_widths(width: u16, with_album: bool) -> [usize; 3] {
    let columns: u16 = if with_album { 5 } else { 4 };
    let fixed = 5 + 7 + (columns - 1);
    let flexible = usize::from(width.saturating_sub(fixed));
    let shares = if with_album { 7 } else { 5 };
    let unit = flexible as f64 / f64::from(shares);
    let title = (unit * 3.0) as usize;
    let other = (unit * 2.0) as usize;
    [title, other, if with_album { other } else { 0 }]
}

fn position_line(frame: &mut Frame, app: &App, area: Rect) {
    if app.listing.tracks.is_empty() {
        return;
    }
    let back = if app.has_history() { " · h back" } else { "" };
    let text = format!(
        " {} of {}{back}",
        app.window.selected + 1,
        app.listing.uris.len()
    );
    frame.render_widget(
        Paragraph::new(text).style(Style::new().fg(app.theme.dim)),
        area,
    );
}
