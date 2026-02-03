use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Widget, Wrap},
};

use crate::app::{App, DisplayMessage, DisplayRole};

/// Human-readable label for a tool: (icon, verb).
fn tool_label(name: &str) -> (&'static str, &'static str) {
    match name {
        "bash" => ("⚡", "Execute"),
        "read_file" => ("📄", "Read"),
        "write_file" => ("✏️", "Write"),
        "list_directory" => ("📂", "List"),
        _ => ("🔧", "Run"),
    }
}

/// Maximum lines the input area can grow to before it stops expanding.
const MAX_INPUT_HEIGHT: u16 = 10;

/// Fixed viewport height: input + 2 status bars.
pub const VIEWPORT_HEIGHT: u16 = 3;

/// Render the inline viewport: input + status bars only.
/// All message content is printed above the viewport via `insert_before`.
pub fn draw(frame: &mut Frame, app: &App) {
    #[allow(clippy::cast_possible_truncation)]
    let input_lines = app.input_line_count() as u16;
    let input_height = input_lines.clamp(1, MAX_INPUT_HEIGHT);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),               // padding (absorbs unused space)
            Constraint::Length(input_height), // input
            Constraint::Length(1),            // status line 1
            Constraint::Length(1),            // status line 2
        ])
        .split(frame.area());

    draw_input(frame, app, chunks[1]);
    draw_status_line1(frame, app, chunks[2]);
    draw_status_line2(frame, app, chunks[3]);
}

/// Format a slice of display messages into styled lines for rendering.
#[allow(clippy::too_many_lines)]
pub fn format_messages(
    messages: &[DisplayMessage],
    agent_name: &str,
    verbose: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    for msg in messages {
        if msg.is_tool && !verbose {
            continue;
        }

        match &msg.role {
            DisplayRole::ToolCall { name, .. } => {
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                let (icon, verb) = tool_label(name);
                let header = if verb == "Run" {
                    format!("  {icon} {verb} {name}")
                } else {
                    format!("  {icon} {verb}")
                };
                let mut spans = vec![Span::styled(
                    header,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )];
                if !msg.content.is_empty() {
                    spans.push(Span::styled(
                        format!("  {}", msg.content),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                lines.push(Line::from(spans));
            }
            DisplayRole::ToolOutput { is_error, .. } => {
                let style = if *is_error {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
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
                        format!("{} {agent_name}: ", msg.local_time()),
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

    lines
}

/// Render lines into a buffer for `Terminal::insert_before` scrollback output.
pub fn render_scrollback(lines: &[Line<'_>], width: u16, buf: &mut Buffer) {
    let area = Rect::new(0, 0, width, buf.area.height);
    let paragraph = Paragraph::new(Text::from(lines.to_vec())).wrap(Wrap { trim: false });
    paragraph.render(area, buf);
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let prompt_style = Style::default().fg(Color::Cyan);
    let text_style = Style::default().fg(Color::White);

    let input_lines: Vec<&str> = if app.input.is_empty() {
        vec![""]
    } else {
        app.input.split('\n').collect()
    };

    let lines: Vec<Line> = input_lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                Line::from(vec![
                    Span::styled("> ", prompt_style),
                    Span::styled(*line, text_style),
                ])
            } else {
                Line::from(vec![
                    Span::styled("  ", prompt_style),
                    Span::styled(*line, text_style),
                ])
            }
        })
        .collect();

    // Scroll the input if it exceeds the visible area
    #[allow(clippy::cast_possible_truncation)] // line counts fit in u16 for any terminal
    let total_lines = lines.len() as u16;
    let (cursor_row, cursor_col) = app.cursor_row_col();
    #[allow(clippy::cast_possible_truncation)]
    let input_scroll = if total_lines > area.height {
        (cursor_row as u16).saturating_sub(area.height.saturating_sub(1))
    } else {
        0
    };

    let input = Paragraph::new(Text::from(lines))
        .style(text_style)
        .scroll((input_scroll, 0));

    frame.render_widget(input, area);

    // Position cursor: +2 for the "> " prefix
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = area.x + 2 + cursor_col as u16;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_y = area.y + (cursor_row as u16).saturating_sub(input_scroll);
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
