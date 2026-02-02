use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};

use crate::app::{App, DisplayRole};

/// Icon for a tool name.
fn tool_icon(name: &str) -> &'static str {
    match name {
        "bash" => "⚡",
        "read_file" => "📄",
        "write_file" => "✏️",
        "list_directory" => "📂",
        _ => "🔧",
    }
}

/// Render the entire TUI.
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // messages
            Constraint::Length(1), // input
            Constraint::Length(1), // status line 1
            Constraint::Length(1), // status line 2
        ])
        .split(frame.area());

    draw_messages(frame, app, chunks[0]);
    draw_input(frame, app, chunks[1]);
    draw_status_line1(frame, app, chunks[2]);
    draw_status_line2(frame, app, chunks[3]);
}

#[allow(clippy::too_many_lines)]
fn draw_messages(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    for msg in &app.messages {
        if msg.is_tool && !app.verbose {
            continue;
        }

        match &msg.role {
            DisplayRole::ToolCall { name, .. } => {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                let icon = tool_icon(name);
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {icon} {name}"),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if msg.content.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", msg.content)
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            DisplayRole::ToolOutput { is_error, .. } => {
                let style = if *is_error {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                // Truncate long output, show first/last few lines
                let content_lines: Vec<&str> = msg.content.lines().collect();
                let max_lines = 20;
                if content_lines.len() > max_lines {
                    for line in &content_lines[..max_lines / 2] {
                        lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                    }
                    lines.push(Line::from(Span::styled(
                        format!("    … ({} lines hidden)", content_lines.len() - max_lines),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    for line in &content_lines[content_lines.len() - max_lines / 2..] {
                        lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                    }
                } else {
                    for line in &content_lines {
                        lines.push(Line::from(Span::styled(format!("    {line}"), style)));
                    }
                }
            }
            role => {
                let (prefix, style) = match role {
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
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ),
                    DisplayRole::ToolCall { .. } | DisplayRole::ToolOutput { .. } => {
                        unreachable!()
                    }
                };

                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }

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
        }
    }

    let visible_height = area.height as usize;
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_height);
    let scroll = (app.scroll as usize).min(max_scroll);

    let messages = Paragraph::new(Text::from(lines))
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));

    frame.render_widget(messages, area);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let input_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(&app.input),
    ]);

    let input = Paragraph::new(input_line).style(Style::default().fg(Color::White));

    frame.render_widget(input, area);

    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = area.x + 2 + app.cursor_pos as u16;
    let cursor_y = area.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn draw_status_line1(frame: &mut Frame, app: &App, area: Rect) {
    let style = Style::default().fg(Color::Yellow);

    let mut spans = Vec::new();

    if app.is_loading {
        spans.push(Span::styled(
            format!(" {} streaming", app.loading_text()),
            style,
        ));
        let elapsed = app.elapsed_text();
        if !elapsed.is_empty() {
            spans.push(Span::styled(format!(" {elapsed}"), style));
        }
    } else {
        spans.push(Span::styled(" idle", style));
    }

    if !app.connection_status.is_empty() {
        spans.push(Span::styled(format!(" | {}", app.connection_status), style));
    }

    let line = Line::from(spans);
    let status = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(status, area);
}

fn draw_status_line2(frame: &mut Frame, app: &App, area: Rect) {
    let style = Style::default().fg(Color::Gray);

    let token_k = app.context_limit / 1000;
    let pct = app.token_percent();

    let line = Line::from(vec![Span::styled(
        format!(
            " agent {} | session {} | {} | tokens {}/{token_k}k ({pct:.0}%)",
            app.agent_name, app.session_name, app.model_name, app.token_count,
        ),
        style,
    )]);

    let status = Paragraph::new(line).style(Style::default().bg(Color::Black));
    frame.render_widget(status, area);
}
