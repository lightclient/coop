use crate::engine::{Component, StyledLine};
use crate::theme;
use crate::utils::pad_to_width;

/// Status line component — shows spinner, elapsed time, or error messages.
#[derive(Debug)]
pub struct StatusLine {
    spinner_frame: usize,
    is_loading: bool,
    elapsed: String,
    error_message: Option<String>,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusLine {
    pub fn new() -> Self {
        Self {
            spinner_frame: 0,
            is_loading: false,
            elapsed: String::new(),
            error_message: None,
        }
    }

    pub fn set_loading(&mut self, loading: bool) {
        self.is_loading = loading;
    }

    pub fn set_elapsed(&mut self, elapsed: &str) {
        self.elapsed = elapsed.to_string();
    }

    pub fn set_error(&mut self, msg: Option<String>) {
        self.error_message = msg;
    }

    pub fn tick(&mut self) {
        if self.is_loading {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
    }

    fn spinner_text(&self) -> &str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[self.spinner_frame % FRAMES.len()]
    }
}

impl Component for StatusLine {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        if let Some(ref err) = self.error_message {
            let line = theme::fg(theme::ERROR, err);
            return vec![pad_to_width(&line, width)];
        }
        if self.is_loading {
            let spinner = theme::fg(theme::ACCENT, self.spinner_text());
            let elapsed = theme::fg(theme::MUTED, &format!(" {}", self.elapsed));
            let line = format!("{spinner}{elapsed}");
            return vec![pad_to_width(&line, width)];
        }
        vec![String::new()]
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
