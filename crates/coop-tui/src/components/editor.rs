use crate::engine::{Component, StyledLine};
use crate::theme;
use crate::utils::visible_width;

/// Editor component — the input field with borders, cursor, and scrolling.
/// Translation of pi's editor.js (simplified for coop — no autocomplete, no jump mode).
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Editor {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    padding_x: usize,
    scroll_offset: usize,
    border_color: (u8, u8, u8),
    focused: bool,
    /// Max visible lines as a fraction of terminal height (0.3)
    max_visible_lines: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            padding_x: 0,
            scroll_offset: 0,
            border_color: theme::THINKING_MEDIUM, // Default: #81a2be
            focused: true,
            max_visible_lines: 10,
        }
    }

    pub fn set_border_color(&mut self, color: (u8, u8, u8)) {
        self.border_color = color;
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub fn set_max_visible_lines(&mut self, max: usize) {
        self.max_visible_lines = max.max(1);
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(String::from).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.scroll_offset = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn insert_char(&mut self, c: char) {
        if c == '\n' {
            let rest = self.lines[self.cursor_line].split_off(self.cursor_col);
            self.cursor_line += 1;
            self.lines.insert(self.cursor_line, rest);
            self.cursor_col = 0;
        } else {
            self.lines[self.cursor_line].insert(self.cursor_col, c);
            self.cursor_col += c.len_utf8();
        }
    }

    pub fn delete_char(&mut self) {
        if self.cursor_col > 0 {
            let prev = self.lines[self.cursor_line][..self.cursor_col]
                .chars()
                .last()
                .map_or(0, char::len_utf8);
            self.cursor_col -= prev;
            self.lines[self.cursor_line].remove(self.cursor_col);
        } else if self.cursor_line > 0 {
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
        }
    }

    pub fn cursor_left(&mut self) {
        if self.cursor_col > 0 {
            let prev = self.lines[self.cursor_line][..self.cursor_col]
                .chars()
                .last()
                .map_or(0, char::len_utf8);
            self.cursor_col -= prev;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
        }
    }

    pub fn cursor_right(&mut self) {
        if self.cursor_col < self.lines[self.cursor_line].len() {
            let next = self.lines[self.cursor_line][self.cursor_col..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
            self.cursor_col += next;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    pub fn cursor_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_line].len());
        }
    }

    pub fn cursor_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_line].len());
        }
    }

    pub fn cursor_home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    pub fn take_text(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    fn border_color_str(&self, text: &str) -> String {
        theme::fg(self.border_color, text)
    }
}

impl Component for Editor {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        let max_padding = width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let content_width = width.saturating_sub(padding_x * 2).max(1);

        let border_char = "─";
        let horizontal = self.border_color_str(&border_char.repeat(width));

        let max_visible = self.max_visible_lines;
        let total_lines = self.lines.len();

        // Adjust scroll offset to keep cursor visible
        let mut scroll = self.scroll_offset;
        if self.cursor_line < scroll {
            scroll = self.cursor_line;
        } else if self.cursor_line >= scroll + max_visible {
            scroll = self.cursor_line - max_visible + 1;
        }
        let max_scroll = total_lines.saturating_sub(max_visible);
        scroll = scroll.min(max_scroll);

        let visible_end = (scroll + max_visible).min(total_lines);
        let visible_lines = &self.lines[scroll..visible_end];

        let mut result = Vec::new();
        let left_padding = " ".repeat(padding_x);

        // Top border with scroll indicator
        if scroll > 0 {
            let indicator = format!("─── ↑ {scroll} more ");
            let remaining = width.saturating_sub(visible_width(&indicator));
            result.push(self.border_color_str(&format!("{}{}", indicator, "─".repeat(remaining))));
        } else {
            result.push(horizontal.clone());
        }

        // Content lines
        for (i, line) in visible_lines.iter().enumerate() {
            let actual_line_idx = scroll + i;
            let is_cursor_line = actual_line_idx == self.cursor_line && self.focused;

            let mut display_text = line.clone();
            let mut line_vis_width = visible_width(&display_text);

            if is_cursor_line {
                let col = self.cursor_col.min(line.len());
                let before = &line[..col];
                let after = &line[col..];

                if after.is_empty() {
                    // Cursor at end — highlighted space
                    display_text = format!("{before}\x1b[7m \x1b[0m");
                    line_vis_width += 1;
                } else {
                    // Cursor on a character — highlight it
                    let first_char = after.chars().next().unwrap_or(' ');
                    let rest = &after[first_char.len_utf8()..];
                    display_text = format!("{before}\x1b[7m{first_char}\x1b[0m{rest}");
                }
            }

            let padding = " ".repeat(content_width.saturating_sub(line_vis_width));
            result.push(format!("{left_padding}{display_text}{padding}"));
        }

        // Bottom border with scroll indicator
        let lines_below = total_lines.saturating_sub(scroll + visible_lines.len());
        if lines_below > 0 {
            let indicator = format!("─── ↓ {lines_below} more ");
            let remaining = width.saturating_sub(visible_width(&indicator));
            result.push(self.border_color_str(&format!("{}{}", indicator, "─".repeat(remaining))));
        } else {
            result.push(horizontal);
        }

        result
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_renders_empty_state() {
        let editor = Editor::new();
        let lines = editor.render(80);
        // Top border + 1 content line + bottom border = 3
        assert_eq!(lines.len(), 3);
        // Content line should have inverse video cursor
        assert!(lines[1].contains("\x1b[7m"));
    }

    #[test]
    fn editor_renders_with_text() {
        let mut editor = Editor::new();
        editor.set_text("hello");
        let lines = editor.render(80);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("hello"));
    }

    #[test]
    fn editor_multiline() {
        let mut editor = Editor::new();
        editor.set_text("line1\nline2\nline3");
        let lines = editor.render(80);
        // Top border + 3 lines + bottom border = 5
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn editor_scroll_indicators() {
        let mut editor = Editor::new();
        editor.set_max_visible_lines(2);
        editor.set_text("line1\nline2\nline3\nline4");
        // Cursor is at end (line 3), scroll shows lines 2-3
        let lines = editor.render(80);
        // Top border (with ↑) + 2 visible + bottom border = 4
        // But the scroll offset starts at cursor_line - max + 1 = 3 - 2 + 1 = 2
        // Lines 2,3 are visible (0-indexed), lines 0,1 above → ↑ 2
        // No lines below → no ↓
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].contains("↑"),
            "expected ↑ in top border: {:?}",
            lines[0]
        );
    }
}
