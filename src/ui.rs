use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph},
    Frame,
};

use crate::app::{App, Filter};
use crate::queue::StopReason;

const SELECTED_BG: Color = Color::Rgb(40, 44, 48);
const BORDER_COLOR: Color = Color::Rgb(88, 110, 117);

/// Formats relative age as `Xs`, `Xm`, `Xh`, or `Xd` (right-aligned to 4 chars by caller).
pub fn format_relative_age(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(timestamp).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Truncates a string to at most `max_chars` Unicode characters (using `…` if truncated)
/// and pads with trailing spaces to exactly `width_chars` characters.
pub fn truncate_and_pad(text: &str, width_chars: usize) -> String {
    if width_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > width_chars {
        if width_chars == 1 {
            "…".to_string()
        } else {
            let mut s: String = chars[..width_chars - 1].iter().collect();
            s.push('…');
            s
        }
    } else {
        let mut s: String = chars.iter().collect();
        for _ in chars.len()..width_chars {
            s.push(' ');
        }
        s
    }
}

/// Splits `text` into `(substring, matched)` pairs for case-insensitive highlighting of `query`.
pub fn split_highlight_segments(text: &str, query: Option<&str>) -> Vec<(String, bool)> {
    let Some(q) = query.filter(|q| !q.is_empty()) else {
        return vec![(text.to_string(), false)];
    };

    let text_chars: Vec<char> = text.chars().collect();
    let query_lower: Vec<String> = q.chars().map(|c| c.to_lowercase().to_string()).collect();
    let q_len = query_lower.len();

    if q_len == 0 || text_chars.len() < q_len {
        return vec![(text.to_string(), false)];
    }

    let mut segments: Vec<(String, bool)> = Vec::new();
    let mut current = String::new();
    let mut i = 0;

    while i < text_chars.len() {
        let matches = if i + q_len <= text_chars.len() {
            (0..q_len).all(|k| text_chars[i + k].to_lowercase().to_string() == query_lower[k])
        } else {
            false
        };

        if matches {
            if !current.is_empty() {
                segments.push((std::mem::take(&mut current), false));
            }
            let matched_str: String = text_chars[i..i + q_len].iter().collect();
            segments.push((matched_str, true));
            i += q_len;
        } else {
            current.push(text_chars[i]);
            i += 1;
        }
    }

    if !current.is_empty() {
        segments.push((current, false));
    }

    if segments.is_empty() {
        vec![(String::new(), false)]
    } else {
        segments
    }
}

fn highlighted_spans(
    text: &str,
    query: Option<&str>,
    base_style: Style,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    split_highlight_segments(text, query)
        .into_iter()
        .map(|(seg, is_match)| {
            if is_match {
                Span::styled(seg, highlight_style)
            } else {
                Span::styled(seg, base_style)
            }
        })
        .collect()
}

pub fn badge_for_entry(stop_reason: StopReason, filter: Filter) -> (&'static str, Color) {
    match (stop_reason, filter) {
        (StopReason::Running, _) => ("running", Color::Cyan),
        (StopReason::Visited, _) => ("visited", Color::Green),
        (StopReason::Error, _) => ("error  ", Color::Red),
        (StopReason::NeedsInput, Filter::Waiting) => ("waiting", Color::Yellow),
        (StopReason::NeedsInput, Filter::History) => ("idle   ", Color::Gray),
    }
}

pub fn format_header_labels(app: &App) -> (String, String) {
    let (unvisited, visited) = app.waiting_counts();
    let waiting_label = if visited > 0 {
        format!("Waiting ({unvisited} · {visited} visited)")
    } else {
        format!("Waiting ({})", app.entries.len())
    };

    let (history_total, running) = app.history_counts();
    let history_label = if running > 0 {
        format!("History ({history_total} · {running} running)")
    } else {
        format!("History ({history_total})")
    };

    (waiting_label, history_label)
}

pub fn format_footer_text(app: &App) -> String {
    if app.search_active {
        let q = app.search.as_deref().unwrap_or("");
        format!("search {q}  (enter to filter · esc to cancel)")
    } else if let Some(ref q) = app.search {
        format!("filter: {q} (j/k navigate · enter jump · / edit · esc to clear)")
    } else {
        "j/k navigate h/l waiting/history enter jump  o peek  d dismiss  / search  q quit"
            .to_string()
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    let outer = frame.area().inner(Margin {
        vertical: 1,
        horizontal: 2,
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(outer);

    draw_header(frame, app, chunks[0]);
    draw_list(frame, app, chunks[1]);
    draw_footer(frame, app, chunks[2]);
}

fn draw_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let (waiting_label, history_label) = format_header_labels(app);

    let active_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let inactive_style = Style::default().fg(Color::Gray);

    let (waiting_style, history_style) = match app.filter {
        Filter::Waiting => (active_style, inactive_style),
        Filter::History => (inactive_style, active_style),
    };

    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(waiting_label, waiting_style),
        Span::styled(" · ", inactive_style),
        Span::styled(history_label, history_style),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_COLOR))
        .title(Span::styled(
            " agent-queue ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(line).block(block);
    frame.render_widget(paragraph, area);
}

fn draw_list(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER_COLOR))
        .padding(Padding::horizontal(1));
    let inner_width = block.inner(area).width as usize;

    let now = Utc::now();
    let filtered = app.filtered_entries();
    let query = app.search.as_deref().filter(|s| !s.is_empty());

    // Fixed column widths:
    // indicator (2) + target (20) + gap (2) + badge (7) + gap (3) + age (4) = 38 chars
    let fixed_cols = 2 + 20 + 2 + 7 + 3 + 4;
    let summary_width = inner_width.saturating_sub(fixed_cols).max(1);

    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let is_selected = idx == app.selected;
            let row_base_style = if is_selected {
                Style::default()
                    .bg(SELECTED_BG)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let highlight_style = row_base_style
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);

            let indicator = if is_selected { "▸ " } else { "  " };
            let target_col = truncate_and_pad(&entry.tmux_target(), 20);
            let (badge_text, badge_color) = badge_for_entry(entry.stop_reason, app.filter);
            let badge_style = row_base_style.fg(badge_color);
            let summary_col = truncate_and_pad(&entry.summary, summary_width);
            let age_col = format!("{:>4}", format_relative_age(entry.timestamp, now));

            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled(indicator.to_string(), row_base_style));
            spans.extend(highlighted_spans(
                &target_col,
                query,
                row_base_style,
                highlight_style,
            ));
            spans.push(Span::styled("  ".to_string(), row_base_style));
            spans.push(Span::styled(badge_text.to_string(), badge_style));
            spans.push(Span::styled("   ".to_string(), row_base_style));
            spans.extend(highlighted_spans(
                &summary_col,
                query,
                row_base_style,
                highlight_style,
            ));
            spans.push(Span::styled(age_col, row_base_style.fg(Color::DarkGray)));

            ListItem::new(Line::from(spans)).style(row_base_style)
        })
        .collect();

    let mut list_state = ListState::default();
    if !filtered.is_empty() {
        list_state.select(Some(app.selected));
    }

    let list = List::new(items).block(block);
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let text = format_footer_text(app);
    let style = if app.search_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };
    let footer = Paragraph::new(Line::from(Span::styled(text, style)));
    frame.render_widget(footer, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_format_relative_age() {
        let base = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        assert_eq!(
            format_relative_age(base, base + chrono::Duration::seconds(42)),
            "42s"
        );
        assert_eq!(
            format_relative_age(base, base + chrono::Duration::minutes(15)),
            "15m"
        );
        assert_eq!(
            format_relative_age(base, base + chrono::Duration::hours(3)),
            "3h"
        );
        assert_eq!(
            format_relative_age(base, base + chrono::Duration::days(5)),
            "5d"
        );
    }

    #[test]
    fn test_truncate_and_pad_utf8() {
        assert_eq!(truncate_and_pad("abc", 5), "abc  ");
        assert_eq!(truncate_and_pad("abcdef", 5), "abcd…");
        assert_eq!(truncate_and_pad("🦀🐍🚀🎉🔥✨", 4), "🦀🐍🚀…");
    }

    #[test]
    fn test_split_highlight_segments_case_insensitive() {
        let segs = split_highlight_segments("Fix Auth Bug in auth.rs", Some("AUTH"));
        assert_eq!(
            segs,
            vec![
                ("Fix ".to_string(), false),
                ("Auth".to_string(), true),
                (" Bug in ".to_string(), false),
                ("auth".to_string(), true),
                (".rs".to_string(), false),
            ]
        );
    }

    #[test]
    fn test_badges_for_waiting_and_history() {
        assert_eq!(
            badge_for_entry(StopReason::Running, Filter::History),
            ("running", Color::Cyan)
        );
        assert_eq!(
            badge_for_entry(StopReason::Visited, Filter::Waiting),
            ("visited", Color::Green)
        );
        assert_eq!(
            badge_for_entry(StopReason::NeedsInput, Filter::Waiting),
            ("waiting", Color::Yellow)
        );
        assert_eq!(
            badge_for_entry(StopReason::NeedsInput, Filter::History),
            ("idle   ", Color::Gray)
        );
        assert_eq!(
            badge_for_entry(StopReason::Error, Filter::Waiting),
            ("error  ", Color::Red)
        );
    }

    #[test]
    fn test_draw_with_test_backend() {
        use crate::queue::{upsert_entry_in, QueueEntry};
        use ratatui::{backend::TestBackend, Terminal};
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().to_path_buf();

        let entry = QueueEntry {
            pane_id: "%1".to_string(),
            session_name: "work".to_string(),
            window_index: 2,
            window_name: "code".to_string(),
            stop_reason: StopReason::NeedsInput,
            summary: "Ready for review".to_string(),
            timestamp: Utc::now() - chrono::Duration::seconds(10),
            cwd: "/tmp".to_string(),
        };
        upsert_entry_in(&dir, &entry).unwrap();

        let app = App::new(dir).unwrap();
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();

        let buf = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..20 {
            for x in 0..100 {
                rendered.push_str(buf[(x, y)].symbol());
            }
            rendered.push('\n');
        }
        assert!(rendered.contains("agent-queue"));
        assert!(rendered.contains("Waiting (1)"));
        assert!(rendered.contains("History (0)"));
        assert!(rendered.contains("work:2:code"));
        assert!(rendered.contains("waiting"));
        assert!(rendered.contains("Ready for review"));
    }
}
