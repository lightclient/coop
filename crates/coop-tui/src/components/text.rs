use crate::engine::{Component, StyledLine};
use crate::utils::{apply_bg_to_line, pad_to_width, wrap_text_with_ansi};

/// Text component — displays multi-line text with word wrapping.
/// Direct translation of pi's text.js.
#[derive(Debug)]
pub struct Text {
    content: String,
    padding_x: usize,
    padding_y: usize,
    bg_color: Option<(u8, u8, u8)>,
}

impl Text {
    pub fn new(text: impl Into<String>, padding_x: usize, padding_y: usize) -> Self {
        Self {
            content: text.into(),
            padding_x,
            padding_y,
            bg_color: None,
        }
    }

    #[must_use]
    pub fn with_bg(mut self, r: u8, g: u8, b: u8) -> Self {
        self.bg_color = Some((r, g, b));
        self
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.content = text.into();
    }
}

impl Component for Text {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        if self.content.is_empty() || self.content.trim().is_empty() {
            return Vec::new();
        }

        let normalized = self.content.replace('\t', "   ");
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let wrapped = wrap_text_with_ansi(&normalized, content_width);

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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renders_with_padding() {
        let t = Text::new("hello", 1, 0);
        let lines = t.render(20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(' '));
    }

    #[test]
    fn text_empty_returns_nothing() {
        let t = Text::new("", 1, 1);
        let lines = t.render(80);
        assert!(lines.is_empty());
    }

    #[test]
    fn text_with_padding_y() {
        let t = Text::new("hi", 0, 1);
        let lines = t.render(80);
        assert_eq!(lines.len(), 3);
    }
}
