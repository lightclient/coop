use std::fmt;
use std::fmt::Write as _;
use std::io::{self, Write};

use crossterm::terminal;

/// A styled line of text containing ANSI escape sequences.
/// Each line must not exceed the width passed to render().
pub type StyledLine = String;

/// The pi Component interface, translated to Rust.
pub trait Component: Send {
    /// Render the component at the given width.
    /// Returns lines of ANSI-styled text, each ≤ width visible characters.
    fn render(&self, width: usize) -> Vec<StyledLine>;

    /// Handle keyboard input. Returns true if the input was consumed.
    fn handle_input(&mut self, _data: &[u8]) -> bool {
        false
    }

    /// Clear cached render state (called on theme changes, resize).
    fn invalidate(&mut self) {}

    /// Downcast to concrete type. Required for accessing typed component state.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// Container — identical to pi's Container.
/// Children are rendered top-to-bottom by concatenating their lines.
#[derive(Default)]
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }

    pub fn clear(&mut self) {
        self.children.clear();
    }

    pub fn children(&self) -> &[Box<dyn Component>] {
        &self.children
    }

    pub fn children_mut(&mut self) -> &mut Vec<Box<dyn Component>> {
        &mut self.children
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        self.children.iter().flat_map(|c| c.render(width)).collect()
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl fmt::Debug for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Container")
            .field("children_count", &self.children.len())
            .finish()
    }
}

/// Segment reset sequence (matches pi's `TUI.SEGMENT_RESET`).
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// TUI — the differential renderer, translation of tui.js.
///
/// This is NOT alternate-screen. It uses inline rendering where content grows
/// upward into terminal scrollback. The working area is at the bottom of the
/// terminal.
pub struct Tui {
    root: Container,
    previous_lines: Vec<String>,
    previous_width: u16,
    cursor_row: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_viewport_top: usize,
    render_requested: bool,
    stopped: bool,
}

impl Default for Tui {
    fn default() -> Self {
        Self::new()
    }
}

impl Tui {
    pub fn new() -> Self {
        Self {
            root: Container::new(),
            previous_lines: Vec::new(),
            previous_width: 0,
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            render_requested: false,
            stopped: false,
        }
    }

    pub fn root(&self) -> &Container {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Container {
        &mut self.root
    }

    /// Start the TUI: enter raw mode, hide cursor.
    pub fn start(&self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        crossterm::execute!(stdout, crossterm::cursor::Hide)?;
        Ok(())
    }

    /// Stop the TUI: move cursor to end of content, show cursor, disable raw mode.
    pub fn stop(&mut self) -> io::Result<()> {
        self.stopped = true;
        let mut stdout = io::stdout();

        if !self.previous_lines.is_empty() {
            let target_row = self.previous_lines.len();
            #[allow(clippy::cast_possible_wrap)]
            let line_diff = target_row as isize - self.hardware_cursor_row as isize;
            if line_diff > 0 {
                write!(stdout, "\x1b[{line_diff}B")?;
            } else if line_diff < 0 {
                write!(stdout, "\x1b[{}A", -line_diff)?;
            }
            write!(stdout, "\r\n")?;
        }

        crossterm::execute!(stdout, crossterm::cursor::Show)?;
        terminal::disable_raw_mode()?;
        stdout.flush()?;
        Ok(())
    }

    pub fn request_render(&mut self) {
        self.render_requested = true;
    }

    pub fn force_render(&mut self) {
        self.previous_lines.clear();
        self.previous_width = 0;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
        self.render_requested = true;
    }

    pub fn render_if_needed(&mut self) -> io::Result<()> {
        if self.render_requested && !self.stopped {
            self.render_requested = false;
            self.do_render()?;
        }
        Ok(())
    }

    /// The differential renderer — translation of tui.js `doRender()`.
    #[allow(
        clippy::too_many_lines,
        clippy::cast_possible_wrap,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn do_render(&mut self) -> io::Result<()> {
        let (term_w, term_h) = terminal::size()?;
        let width = term_w as usize;
        let height = term_h as usize;

        if width == 0 || height == 0 {
            return Ok(());
        }

        let mut viewport_top = self.max_lines_rendered.saturating_sub(height);
        let mut prev_viewport_top = self.previous_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        // Render all components
        let mut new_lines: Vec<String> = self.root.render(width);
        for line in &mut new_lines {
            line.push_str(SEGMENT_RESET);
        }

        let width_changed = self.previous_width != 0 && self.previous_width != term_w;

        let mut stdout = io::stdout();

        // --- Full render helper ---
        macro_rules! full_render {
            ($clear:expr) => {{
                let mut buffer = String::new();
                let _ = write!(buffer, "\x1b[?2026h");
                if $clear {
                    buffer.push_str("\x1b[3J\x1b[2J\x1b[H");
                }
                for (i, line) in new_lines.iter().enumerate() {
                    if i > 0 {
                        buffer.push_str("\r\n");
                    }
                    buffer.push_str(line);
                }
                buffer.push_str("\x1b[?2026l");
                stdout.write_all(buffer.as_bytes())?;
                stdout.flush()?;

                self.cursor_row = new_lines.len().saturating_sub(1);
                self.hardware_cursor_row = self.cursor_row;
                if $clear {
                    self.max_lines_rendered = new_lines.len();
                } else {
                    self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
                }
                self.previous_viewport_top = self.max_lines_rendered.saturating_sub(height);
                write!(stdout, "\x1b[?25l")?;
                stdout.flush()?;
                self.previous_lines = new_lines;
                self.previous_width = term_w;
                return Ok(());
            }};
        }

        // First render
        if self.previous_lines.is_empty() && !width_changed {
            full_render!(false);
        }

        // Width changed
        if width_changed {
            full_render!(true);
        }

        // Find first and last changed lines
        let mut first_changed: Option<usize> = None;
        let mut last_changed: Option<usize> = None;
        let max_lines = new_lines.len().max(self.previous_lines.len());

        for i in 0..max_lines {
            let old = self.previous_lines.get(i).map_or("", String::as_str);
            let new = new_lines.get(i).map_or("", String::as_str);
            if old != new {
                if first_changed.is_none() {
                    first_changed = Some(i);
                }
                last_changed = Some(i);
            }
        }

        let appended = new_lines.len() > self.previous_lines.len();
        if appended {
            if first_changed.is_none() {
                first_changed = Some(self.previous_lines.len());
            }
            last_changed = Some(new_lines.len() - 1);
        }

        let Some(first_changed) = first_changed else {
            self.previous_viewport_top = self.max_lines_rendered.saturating_sub(height);
            self.previous_lines = new_lines;
            self.previous_width = term_w;
            return Ok(());
        };
        let last_changed = last_changed.unwrap_or(first_changed);

        // If first change is above the previous viewport, full re-render
        let prev_content_vp_top = self.previous_lines.len().saturating_sub(height);
        if first_changed < prev_content_vp_top {
            full_render!(true);
        }

        // All changes are in deleted lines
        if first_changed >= new_lines.len() {
            self.handle_deleted_lines(
                &new_lines,
                viewport_top,
                prev_viewport_top,
                hardware_cursor_row,
                height,
                &mut stdout,
            )?;
            self.previous_lines = new_lines;
            self.previous_width = term_w;
            self.previous_viewport_top = self.max_lines_rendered.saturating_sub(height);
            return Ok(());
        }

        let append_start =
            appended && first_changed == self.previous_lines.len() && first_changed > 0;

        // Build differential update buffer

        let mut buffer = String::from("\x1b[?2026h");

        let prev_viewport_bottom = prev_viewport_top + height - 1;
        let move_target_row = if append_start {
            first_changed - 1
        } else {
            first_changed
        };

        if move_target_row > prev_viewport_bottom {
            let current_screen_row = (hardware_cursor_row as isize - prev_viewport_top as isize)
                .clamp(0, height as isize - 1) as usize;
            let move_to_bottom = (height - 1).saturating_sub(current_screen_row);
            if move_to_bottom > 0 {
                let _ = write!(buffer, "\x1b[{move_to_bottom}B");
            }
            let scroll = move_target_row - prev_viewport_bottom;
            for _ in 0..scroll {
                buffer.push_str("\r\n");
            }
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target_row;
        }

        let line_diff = Self::compute_line_diff(
            move_target_row,
            hardware_cursor_row,
            prev_viewport_top,
            viewport_top,
        );
        if line_diff > 0 {
            let _ = write!(buffer, "\x1b[{line_diff}B");
        } else if line_diff < 0 {
            let _ = write!(buffer, "\x1b[{}A", -line_diff);
        }

        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last_changed.min(new_lines.len() - 1);
        for (idx, line) in new_lines
            .iter()
            .enumerate()
            .take(render_end + 1)
            .skip(first_changed)
        {
            if idx > first_changed {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }

        let mut final_cursor_row = render_end;

        if self.previous_lines.len() > new_lines.len() {
            if render_end < new_lines.len() - 1 {
                let move_down = new_lines.len() - 1 - render_end;
                let _ = write!(buffer, "\x1b[{move_down}B");
                final_cursor_row = new_lines.len() - 1;
            }
            let extra = self.previous_lines.len() - new_lines.len();
            for _ in 0..extra {
                buffer.push_str("\r\n\x1b[2K");
            }
            let _ = write!(buffer, "\x1b[{extra}A");
        }

        buffer.push_str("\x1b[?2026l");
        stdout.write_all(buffer.as_bytes())?;
        stdout.flush()?;

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.previous_viewport_top = self.max_lines_rendered.saturating_sub(height);

        write!(stdout, "\x1b[?25l")?;
        stdout.flush()?;

        self.previous_lines = new_lines;
        self.previous_width = term_w;

        Ok(())
    }

    #[allow(clippy::cast_possible_wrap)]
    fn compute_line_diff(
        target_row: usize,
        hw_cursor: usize,
        prev_vp_top: usize,
        vp_top: usize,
    ) -> isize {
        let current_screen_row = hw_cursor as isize - prev_vp_top as isize;
        let target_screen_row = target_row as isize - vp_top as isize;
        target_screen_row - current_screen_row
    }

    #[allow(clippy::cast_possible_wrap)]
    fn handle_deleted_lines(
        &mut self,
        new_lines: &[String],
        viewport_top: usize,
        prev_viewport_top: usize,
        hardware_cursor_row: usize,
        height: usize,
        stdout: &mut io::Stdout,
    ) -> io::Result<()> {
        if self.previous_lines.len() <= new_lines.len() {
            return Ok(());
        }

        let mut buffer = String::from("\x1b[?2026h");
        let target_row = new_lines.len().saturating_sub(1);
        let line_diff = Self::compute_line_diff(
            target_row,
            hardware_cursor_row,
            prev_viewport_top,
            viewport_top,
        );
        if line_diff > 0 {
            let _ = write!(buffer, "\x1b[{line_diff}B");
        } else if line_diff < 0 {
            let _ = write!(buffer, "\x1b[{}A", -line_diff);
        }
        buffer.push('\r');

        let extra = self.previous_lines.len() - new_lines.len();
        if extra > height {
            // Too many to clear individually — not handled here
            buffer.push_str("\x1b[?2026l");
            stdout.write_all(buffer.as_bytes())?;
            stdout.flush()?;
            return Ok(());
        }
        if extra > 0 {
            buffer.push_str("\x1b[1B");
        }
        for i in 0..extra {
            buffer.push_str("\r\x1b[2K");
            if i < extra - 1 {
                buffer.push_str("\x1b[1B");
            }
        }
        if extra > 0 {
            let _ = write!(buffer, "\x1b[{extra}A");
        }
        buffer.push_str("\x1b[?2026l");
        stdout.write_all(buffer.as_bytes())?;
        stdout.flush()?;

        self.cursor_row = target_row;
        self.hardware_cursor_row = target_row;
        Ok(())
    }

    /// Get the terminal size.
    pub fn terminal_size() -> io::Result<(u16, u16)> {
        terminal::size()
    }
}

impl fmt::Debug for Tui {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tui")
            .field("max_lines_rendered", &self.max_lines_rendered)
            .field("previous_width", &self.previous_width)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticComponent {
        lines: Vec<String>,
    }

    impl Component for StaticComponent {
        fn render(&self, _width: usize) -> Vec<StyledLine> {
            self.lines.clone()
        }
    }

    #[test]
    fn container_concatenates_children() {
        let mut c = Container::new();
        c.add_child(Box::new(StaticComponent {
            lines: vec!["line1".into()],
        }));
        c.add_child(Box::new(StaticComponent {
            lines: vec!["line2".into(), "line3".into()],
        }));
        let result = c.render(80);
        assert_eq!(result, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn container_clear() {
        let mut c = Container::new();
        c.add_child(Box::new(StaticComponent {
            lines: vec!["x".into()],
        }));
        c.clear();
        assert!(c.render(80).is_empty());
    }
}
