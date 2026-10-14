//! The now-playing bar.

use super::{button, ellipsize};
use crate::{
    app::{Action, App, Hit, listing::duration},
    spotify::{DEVICE_NAME, Repeat},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
};
use unicode_width::UnicodeWidthStr;

const VOLUME_CELLS: u32 = 10;

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [first, second, third] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);

    let controls = controls(app);
    let controls_width: u16 = controls
        .iter()
        .map(|(label, ..)| label.width() as u16 + 2)
        .sum();
    let [title_area, controls_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(controls_width)]).areas(first);
    let (title, details) = match &app.now.track {
        Some(track) => (
            track.name.clone(),
            format!("{} · {}", track.artist_names(), track.album),
        ),
        None => (
            "Nothing playing".into(),
            format!("Choose a track and press l · plays here as “{DEVICE_NAME}”"),
        ),
    };
    let title = ellipsize(&title, title_area.width.saturating_sub(1).into());
    frame.render_widget(
        Paragraph::new(title).style(Style::new().fg(theme.text).add_modifier(Modifier::BOLD)),
        title_area,
    );
    let mut x = controls_area.x + 2;
    for (label, action, style) in controls {
        let width = label.width() as u16;
        button(
            frame,
            app,
            Rect::new(x, controls_area.y, width, 1),
            &label,
            style,
            action,
        );
        x += width + 2;
    }
    let details = ellipsize(&details, second.width.into());
    frame.render_widget(
        Paragraph::new(details).style(Style::new().fg(theme.subtext)),
        second,
    );

    progress_line(frame, app, third);
}

fn controls(app: &App) -> Vec<(String, Action, Style)> {
    let theme = app.theme;
    let plain = Style::new().fg(theme.subtext);
    let on = Style::new().fg(theme.accent);
    let play = if app.now.playing() {
        "‖ Pause"
    } else {
        "▶ Play"
    };
    let repeat = match app.now.repeat {
        Repeat::Off | Repeat::Context => "↻ Repeat",
        Repeat::Track => "↻ Repeat one",
    };
    vec![
        (
            play.into(),
            Action::TogglePlay,
            Style::new().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        (
            "⇄ Shuffle".into(),
            Action::Shuffle,
            if app.now.shuffle { on } else { plain },
        ),
        (
            repeat.into(),
            Action::Repeat,
            if app.now.repeat == Repeat::Off {
                plain
            } else {
                on
            },
        ),
    ]
}

fn progress_line(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let volume = volume_label(app.now.volume_percent(), app.now.muted);
    let [elapsed, bar, length, _, volume_area] = Layout::horizontal([
        Constraint::Length(7),
        Constraint::Fill(1),
        Constraint::Length(8),
        Constraint::Length(1),
        Constraint::Length(volume.width() as u16),
    ])
    .areas(area);
    let label = |ms: u64| duration(ms);
    let subtext = Style::new().fg(theme.subtext);
    frame.render_widget(
        Paragraph::new(label(app.now.elapsed_ms())).style(subtext),
        elapsed,
    );
    let total = app.now.length().as_millis().try_into().unwrap_or(u64::MAX);
    frame.render_widget(
        Paragraph::new(label(total))
            .style(subtext)
            .alignment(ratatui::layout::Alignment::Right),
        length,
    );
    let width = usize::from(bar.width);
    let played = if app.now.track.is_some() && width > 0 {
        ((app.now.progress() * width as f64).round() as usize).clamp(1, width)
    } else {
        0
    };
    let mut spans = Vec::new();
    if played > 0 {
        spans.push(Span::styled(
            format!("{}●", "━".repeat(played - 1)),
            Style::new().fg(theme.accent),
        ));
    }
    spans.push(Span::styled(
        "─".repeat(width - played),
        Style::new().fg(theme.track),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), bar);
    app.hits.push((bar, Hit::Progress));
    // Muted: the level shows dimmed beside the status, ready to come back.
    let style = if app.now.muted {
        Style::new().fg(theme.dim)
    } else {
        subtext
    };
    frame.render_widget(Paragraph::new(volume).style(style), volume_area);
    app.hits.push((volume_area, Hit::Volume));
}

fn volume_label(percent: u32, muted: bool) -> String {
    let filled = percent.div_ceil(100 / VOLUME_CELLS).min(VOLUME_CELLS);
    let label = format!(
        "{}{} {percent:>3}%",
        "▮".repeat(filled as usize),
        "▯".repeat((VOLUME_CELLS - filled) as usize)
    );
    if muted {
        format!("muted {label}")
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use super::volume_label;

    #[test]
    fn volume_fills_a_cell_per_ten_percent() {
        assert_eq!(volume_label(0, false), "▯▯▯▯▯▯▯▯▯▯   0%");
        assert_eq!(volume_label(49, false), "▮▮▮▮▮▯▯▯▯▯  49%");
        assert_eq!(volume_label(100, false), "▮▮▮▮▮▮▮▮▮▮ 100%");
        assert_eq!(volume_label(65, true), "muted ▮▮▮▮▮▮▮▯▯▯  65%");
    }
}
