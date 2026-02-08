use crate::engine::{Component, StyledLine};
use crate::utils::{apply_bg_to_line, pad_to_width, visible_width, wrap_text_with_ansi};

/// Box component — a container with padding and background color.
/// Direct translation of pi's box.js.
#[derive(Debug)]
pub struct ToolBox {
    children_lines: Vec<String>,
    padding_x: usize,
    padding_y: usize,
    bg_color: Option<(u8, u8, u8)>,
}

impl ToolBox {
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            children_lines: Vec::new(),
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

    pub fn set_content(&mut self, text: &str, content_width: usize) {
        self.children_lines.clear();
        if text.is_empty() {
            return;
        }
        let left_pad = " ".repeat(self.padding_x);
        let wrapped = wrap_text_with_ansi(text, content_width);
        for line in wrapped {
            self.children_lines.push(format!("{left_pad}{line}"));
        }
    }

    pub fn set_lines(&mut self, lines: Vec<String>) {
        self.children_lines = lines;
    }

    fn apply_bg(&self, line: &str, width: usize) -> String {
        let vis = visible_width(line);
        let pad_needed = width.saturating_sub(vis);
        let padded = format!("{}{}", line, " ".repeat(pad_needed));
        if let Some((r, g, b)) = self.bg_color {
            apply_bg_to_line(&padded, width, r, g, b)
        } else {
            pad_to_width(&padded, width)
        }
    }
}

impl Component for ToolBox {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        if self.children_lines.is_empty() {
            return Vec::new();
        }

        let mut result = Vec::new();

        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        for line in &self.children_lines {
            result.push(self.apply_bg(line, width));
        }

        for _ in 0..self.padding_y {
            result.push(self.apply_bg("", width));
        }

        result
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_box_empty() {
        let b = ToolBox::new(1, 1);
        assert!(b.render(80).is_empty());
    }

    #[test]
    fn tool_box_with_content() {
        let mut b = ToolBox::new(1, 1).with_bg(0x28, 0x32, 0x28);
        b.set_content("hello", 78);
        let lines = b.render(80);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(line.contains("\x1b[48;2;40;50;40m"));
        }
    }
}
