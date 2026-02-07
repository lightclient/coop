use std::fmt::Write as _;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::engine::{Component, StyledLine};
use crate::theme;
use crate::utils::{apply_bg_to_line, pad_to_width, wrap_text_with_ansi};

/// Markdown renderer component.
/// Simplified translation of pi's markdown.js using pulldown-cmark.
#[derive(Debug)]
pub struct MarkdownComponent {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg_color: Option<(u8, u8, u8)>,
    text_color: Option<(u8, u8, u8)>,
    is_italic: bool,
}

impl MarkdownComponent {
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.into(),
            padding_x,
            padding_y,
            bg_color: None,
            text_color: None,
            is_italic: false,
        }
    }

    #[must_use]
    pub fn with_bg(mut self, r: u8, g: u8, b: u8) -> Self {
        self.bg_color = Some((r, g, b));
        self
    }

    #[must_use]
    pub fn with_text_color(mut self, r: u8, g: u8, b: u8) -> Self {
        self.text_color = Some((r, g, b));
        self
    }

    #[must_use]
    pub fn with_italic(mut self, italic: bool) -> Self {
        self.is_italic = italic;
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }
}

impl Component for MarkdownComponent {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        if self.text.is_empty() || self.text.trim().is_empty() {
            return Vec::new();
        }

        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let normalized = self.text.replace('\t', "   ");

        let rendered_lines = render_markdown_to_lines(&normalized, self.text_color, self.is_italic);

        let mut wrapped = Vec::new();
        for line in &rendered_lines {
            wrapped.extend(wrap_text_with_ansi(line, content_width));
        }

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);

        let mut content_lines = Vec::new();
        for line in &wrapped {
            let with_margins = format!("{left_margin}{line}{right_margin}");
            if let Some((r, g, b)) = self.bg_color {
                content_lines.push(apply_bg_to_line(&with_margins, width, r, g, b));
            } else {
                content_lines.push(pad_to_width(&with_margins, width));
            }
        }

        let empty_line = " ".repeat(width);
        let mut empty_lines = Vec::new();
        for _ in 0..self.padding_y {
            if let Some((r, g, b)) = self.bg_color {
                empty_lines.push(apply_bg_to_line(&empty_line, width, r, g, b));
            } else {
                empty_lines.push(empty_line.clone());
            }
        }

        let mut result = Vec::new();
        result.extend(empty_lines.iter().cloned());
        result.extend(content_lines);
        result.extend(empty_lines);
        result
    }
}

/// Render markdown text to styled terminal lines.
#[allow(clippy::too_many_lines)]
fn render_markdown_to_lines(
    text: &str,
    text_color: Option<(u8, u8, u8)>,
    is_italic: bool,
) -> Vec<String> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);

    let parser = Parser::new_ext(text, options);
    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut in_code_block = false;
    let mut in_heading = false;
    let mut list_depth: usize = 0;
    let mut first_paragraph = true;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    if !current_line.is_empty() || !lines.is_empty() {
                        flush_line(&mut current_line, &mut lines);
                    }
                    in_heading = true;
                    let prefix = "#".repeat(level as usize);
                    current_line
                        .push_str(&theme::fg(theme::MD_HEADING, &format!("\x1b[1m{prefix} ")));
                }
                Tag::CodeBlock(_) => {
                    flush_line(&mut current_line, &mut lines);
                    in_code_block = true;
                    lines.push(theme::fg(theme::MD_CODE_BLOCK_BORDER, "───"));
                }
                Tag::Paragraph => {
                    if !first_paragraph && !in_code_block {
                        flush_line(&mut current_line, &mut lines);
                    }
                    first_paragraph = false;
                }
                Tag::List(_) => {
                    if list_depth == 0 {
                        flush_line(&mut current_line, &mut lines);
                    }
                    list_depth += 1;
                }
                Tag::Item => {
                    flush_line(&mut current_line, &mut lines);
                    let indent = "  ".repeat(list_depth.saturating_sub(1));
                    let bullet = theme::fg(theme::MD_LIST_BULLET, "•");
                    let _ = write!(current_line, "{indent}{bullet} ");
                }
                Tag::Emphasis => current_line.push_str("\x1b[3m"),
                Tag::Strong => current_line.push_str("\x1b[1m"),
                Tag::Link { .. } | Tag::BlockQuote(_) => {
                    if matches!(tag, Tag::BlockQuote(_)) {
                        flush_line(&mut current_line, &mut lines);
                        current_line.push_str(&theme::fg(theme::MD_QUOTE, "│ "));
                    }
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_) => {
                    current_line.push_str("\x1b[0m");
                    flush_line(&mut current_line, &mut lines);
                    in_heading = false;
                }
                TagEnd::CodeBlock => {
                    lines.push(theme::fg(theme::MD_CODE_BLOCK_BORDER, "───"));
                    in_code_block = false;
                }
                TagEnd::Paragraph | TagEnd::Item | TagEnd::BlockQuote(_) => {
                    flush_line(&mut current_line, &mut lines);
                    if matches!(tag_end, TagEnd::List(_)) {
                        list_depth = list_depth.saturating_sub(1);
                    }
                }
                TagEnd::List(_) => {
                    list_depth = list_depth.saturating_sub(1);
                    if list_depth == 0 {
                        flush_line(&mut current_line, &mut lines);
                    }
                }
                TagEnd::Emphasis => current_line.push_str("\x1b[23m"),
                TagEnd::Strong => current_line.push_str("\x1b[22m"),
                _ => {}
            },
            Event::Text(text) => {
                let styled = if in_code_block {
                    theme::fg(theme::MD_CODE_BLOCK, &text)
                } else if in_heading {
                    text.to_string()
                } else if let Some(color) = text_color {
                    theme::fg(color, &text)
                } else if is_italic {
                    format!("\x1b[3m{text}\x1b[23m")
                } else {
                    text.to_string()
                };

                if in_code_block {
                    for (i, code_line) in styled.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut current_line, &mut lines);
                        }
                        let _ = write!(current_line, "  {code_line}");
                    }
                } else {
                    current_line.push_str(&styled);
                }
            }
            Event::Code(code) => {
                current_line.push_str(&theme::fg(theme::MD_CODE, &format!("`{code}`")));
            }
            Event::SoftBreak => current_line.push(' '),
            Event::HardBreak => flush_line(&mut current_line, &mut lines),
            Event::Rule => {
                flush_line(&mut current_line, &mut lines);
                lines.push(theme::fg(theme::MUTED, "───"));
            }
            _ => {}
        }
    }

    flush_line(&mut current_line, &mut lines);

    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

fn flush_line(current: &mut String, lines: &mut Vec<String>) {
    if !current.is_empty() {
        lines.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_plain_text() {
        let md = MarkdownComponent::new("Hello world", 1, 0);
        let lines = md.render(80);
        assert!(!lines.is_empty());
        let joined: String = lines.join("");
        assert!(joined.contains("Hello world"));
    }

    #[test]
    fn markdown_renders_heading() {
        let md = MarkdownComponent::new("# Title", 0, 0);
        let lines = md.render(80);
        assert!(!lines.is_empty());
        let joined: String = lines.join("");
        assert!(joined.contains("Title"));
    }

    #[test]
    fn markdown_renders_code_block() {
        let md = MarkdownComponent::new("```\nlet x = 1;\n```", 0, 0);
        let lines = md.render(80);
        assert!(lines.len() >= 3);
    }

    #[test]
    fn markdown_empty() {
        let md = MarkdownComponent::new("", 0, 0);
        let lines = md.render(80);
        assert!(lines.is_empty());
    }
}
