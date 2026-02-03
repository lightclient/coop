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

/// Fixed viewport height: input (1 line) + 1 status bar.
pub const VIEWPORT_HEIGHT: u16 = 1 + 1;

/// Render the fixed viewport: input + status bar only.
///
/// Messages are not rendered in the viewport — they live in terminal
/// scrollback via `insert_before`.
pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // input
            Constraint::Length(1), // status bar
        ])
        .split(frame.area());

    draw_input(frame, app, chunks[0]);
    draw_status_bar(frame, app, chunks[1]);
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
                    Span::styled("❯ ", prompt_style),
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

    // Position cursor: +2 for the "❯ " prefix
    #[allow(clippy::cast_possible_truncation)]
    let cursor_x = area.x + 2 + cursor_col as u16;
    #[allow(clippy::cast_possible_truncation)]
    let cursor_y = area.y + (cursor_row as u16).saturating_sub(input_scroll);
    frame.set_cursor_position((cursor_x, cursor_y));
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let style = Style::default().fg(Color::Gray);
    let sep = Span::styled(" | ", style);

    let mut spans: Vec<Span> = Vec::new();

    // Model name
    spans.push(Span::styled(format!(" {}", app.model_name), style));

    // Folder name
    let folder = app
        .working_dir
        .rsplit('/')
        .next()
        .unwrap_or(&app.working_dir);
    spans.push(sep.clone());
    spans.push(Span::styled(format!("\u{1F4C1}{folder}"), style));

    // Git branch + uncommitted count
    if !app.git_branch.is_empty() {
        spans.push(sep.clone());
        spans.push(Span::styled(
            format!(
                "\u{1F500}{} ({} files uncommitted)",
                app.git_branch, app.git_uncommitted
            ),
            style,
        ));
    }

    // Token progress bar
    let pct = app.token_percent();
    let token_k = app.context_limit / 1000;
    let bar_width: usize = 10;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((pct / 100.0) * bar_width as f64).round() as usize;
    let empty = bar_width.saturating_sub(filled);
    let bar_filled = "\u{2588}".repeat(filled);
    let bar_empty = "\u{2591}".repeat(empty);

    spans.push(sep);
    spans.push(Span::styled(
        bar_filled,
        Style::default().fg(Color::LightBlue),
    ));
    spans.push(Span::styled(
        bar_empty,
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(
        format!(" ~{pct:.0}% of {token_k}k tokens"),
        style,
    ));

    let line = Line::from(spans);
    let status = Paragraph::new(line).style(Style::default().bg(Color::DarkGray));
    frame.render_widget(status, area);
}

/// Format the welcome header as styled lines for insert_before scrollback output.
pub fn format_welcome_header(
    version: &str,
    model_name: &str,
    working_dir: &str,
) -> Vec<Line<'static>> {
    let pink = Style::default().fg(Color::LightYellow);
    let bold_white = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let gray = Style::default().fg(Color::Gray);

    // ASCII pixel-art robot face (Coop mascot)
    let art = [
        "    ████████       ",
        "  ██▓▓▓▓▓▓▓▓██     ",
        "  ██▓▓▓▓▓▓▓▓██     ",
        "  ██▓▓██▓▓██▓▓██   ",
        "  ██▓▓▓▓▓▓▓▓██     ",
        "    ████████       ",
    ];

    let text_line0 = format!("Coop v{version}");
    let text_line1 = model_name.to_string();
    let text_line2 = working_dir.to_string();

    let mut lines = Vec::new();
    lines.push(Line::raw(""));

    for (i, art_line) in art.iter().enumerate() {
        let mut spans = vec![Span::styled((*art_line).to_string(), pink)];
        match i {
            1 => spans.push(Span::styled(text_line0.clone(), bold_white)),
            2 => spans.push(Span::styled(text_line1.clone(), gray)),
            3 => spans.push(Span::styled(text_line2.clone(), gray)),
            _ => {}
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::raw(""));
    lines
}
