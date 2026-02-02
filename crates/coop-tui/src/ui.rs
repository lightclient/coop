use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

use crate::app::{App, DisplayRole};

/// Render the entire TUI.
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // header
            Constraint::Min(1),    // messages
            Constraint::Length(3), // input
        ])
        .split(frame.area());

    draw_header(frame, app, chunks[0]);
    draw_messages(frame, app, chunks[1]);
    draw_input(frame, app, chunks[2]);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let status = if app.is_loading {
        format!(" {} thinking...", app.loading_text())
    } else {
        String::new()
    };

    let header = Line::from(vec![
        Span::styled("🐔 Coop", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw(" │ "),
        Span::styled(&app.agent_name, Style::default().fg(Color::Cyan)),
        Span::raw(" │ "),
        Span::styled(&app.model_name, Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]);

    frame.render_widget(Paragraph::new(header), area);
}

fn draw_messages(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        let (prefix, style) = match msg.role {
            DisplayRole::User => (
                format!("{} you: ", msg.local_time()),
                Style::default().fg(Color::Cyan),
            ),
            DisplayRole::Assistant => (
                format!("{} {}: ", msg.local_time(), app.agent_name),
                Style::default().fg(Color::White),
            ),
            DisplayRole::System => (
                format!("{} ", msg.local_time()),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ),
        };

        // Add blank line between messages
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }

        // Render message with prefix on first line
        let content_lines: Vec<&str> = msg.content.lines().collect();
        let prefix_len = prefix.len();
        if content_lines.is_empty() {
            lines.push(Line::from(Span::styled(prefix, style)));
        } else {
            for (i, content_line) in content_lines.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(prefix.clone(), style),
                        Span::styled(content_line.to_string(), style),
                    ]));
                } else {
                    let indent = " ".repeat(prefix_len);
                    lines.push(Line::from(vec![
                        Span::raw(indent),
                        Span::styled(content_line.to_string(), style),
                    ]));
                }
            }
        }
    }

    // If loading, show spinner at the end
    if app.is_loading {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("  {} thinking...", app.loading_text()),
            Style::default().fg(Color::Yellow),
        )));
    }

    // Calculate scroll position
    let visible_height = area.height.saturating_sub(2) as usize; // account for borders
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = (app.scroll as usize).min(max_scroll);

    let messages = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title("Messages"))
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0));

    frame.render_widget(messages, area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let input = Paragraph::new(app.input.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Input (Enter to send, Ctrl-C to quit)"),
        )
        .style(Style::default().fg(Color::White));

    frame.render_widget(input, area);

    // Position cursor
    let cursor_x = area.x + 1 + app.cursor_pos as u16;
    let cursor_y = area.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}
