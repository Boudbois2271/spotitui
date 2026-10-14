//! The lyrics view: the sung line stays centered and highlighted.

use super::{library::heading_line, panel, wrap};
use crate::app::{App, Focus, Hit, LyricsStatus};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Paragraph, Wrap},
};

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let [heading, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let title = match &app.now.track {
        Some(track) => format!("Lyrics · {}", track.name),
        None => "Lyrics".into(),
    };
    let lyrics = app.current_lyrics();
    let summary = match (&lyrics, app.lyrics.as_ref().map(|cache| &cache.status)) {
        (Some(lyrics), _) if lyrics.synced => format!("Synced · {}", lyrics.provider),
        (Some(lyrics), _) => format!("Not synced · {}", lyrics.provider),
        (None, Some(LyricsStatus::Loading)) => "Loading…".into(),
        _ => String::new(),
    };
    heading_line(
        frame,
        app,
        heading,
        &title,
        Span::styled(summary, Style::new().fg(theme.subtext)),
    );
    let block = panel(app, app.focus == Focus::Tracks);
    let inner = block.inner(body);
    frame.render_widget(block, body);
    app.hits.push((body, Hit::LyricsPane));
    let hint = if app.lyrics_offset != 0 && lyrics.as_ref().is_some_and(|lyrics| lyrics.synced) {
        " l jumps to this line · h closes"
    } else {
        " j/k scroll · h closes"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(theme.dim)),
        footer,
    );

    let Some(lyrics) = lyrics else {
        let message = match app.lyrics.as_ref().map(|cache| &cache.status) {
            _ if app.now.track.is_none() => "Play something to see its lyrics.".to_owned(),
            Some(LyricsStatus::Missing) => "No lyrics for this track.".to_owned(),
            Some(LyricsStatus::Failed(error)) => format!("Couldn't load lyrics: {error}"),
            _ => "Loading lyrics…".to_owned(),
        };
        let middle = Rect::new(
            inner.x,
            inner.y + inner.height / 2,
            inner.width,
            inner.height.min(2),
        );
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::new().fg(theme.subtext))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true }),
            middle,
        );
        return;
    };

    let sung = lyrics.current(app.now.elapsed_ms());
    let cursor = app.lyrics_cursor().unwrap_or(0);
    let width = usize::from(inner.width.saturating_sub(4));
    // Every lyric line, wrapped to the pane, tagged with its index.
    let rows: Vec<(usize, String)> = lyrics
        .lines
        .iter()
        .enumerate()
        .flat_map(|(index, line)| {
            let text = if line.text.is_empty() {
                "♪"
            } else {
                line.text.as_str()
            };
            wrap(text, width).into_iter().map(move |row| (index, row))
        })
        .collect();
    let first_row_of = |index: usize| {
        rows.iter()
            .position(|(line, _)| *line == index)
            .unwrap_or(0)
    };
    let height = usize::from(inner.height);
    // Synced lyrics keep the cursor line in the middle; plain lyrics scroll from the top.
    let top = if lyrics.synced {
        first_row_of(cursor).saturating_sub(height / 2)
    } else {
        first_row_of(cursor)
    };
    for (offset, (index, text)) in rows.iter().skip(top).take(height).enumerate() {
        let mut style = match sung {
            Some(sung) if *index == sung => {
                Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
            }
            Some(sung) if *index < sung => Style::new().fg(theme.dim),
            _ if lyrics.synced => Style::new().fg(theme.subtext),
            _ => Style::new().fg(theme.text),
        };
        if lyrics.synced && app.lyrics_offset != 0 && *index == cursor {
            style = style.bg(theme.selection);
        }
        let row = Rect::new(inner.x, inner.y + offset as u16, inner.width, 1);
        frame.render_widget(
            Paragraph::new(text.as_str())
                .style(style)
                .alignment(Alignment::Center),
            row,
        );
        if lyrics.synced {
            app.hits.push((row, Hit::Lyric(*index)));
        }
    }
}
