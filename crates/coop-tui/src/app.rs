use chrono::{DateTime, Local, Utc};

/// Role for display messages in the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    System,
}

/// A message to display in the TUI.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

impl DisplayMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::User,
            content: content.into(),
            timestamp: Utc::now(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::Assistant,
            content: content.into(),
            timestamp: Utc::now(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::System,
            content: content.into(),
            timestamp: Utc::now(),
        }
    }

    pub fn local_time(&self) -> String {
        self.timestamp
            .with_timezone(&Local)
            .format("%H:%M")
            .to_string()
    }
}

/// Application state for the TUI.
pub struct App {
    /// Chat messages to display.
    pub messages: Vec<DisplayMessage>,
    /// Current input buffer.
    pub input: String,
    /// Cursor position in input.
    pub cursor_pos: usize,
    /// Scroll offset for message area.
    pub scroll: u16,
    /// Agent name for display.
    pub agent_name: String,
    /// Model name for display.
    pub model_name: String,
    /// Whether we're waiting for agent response.
    pub is_loading: bool,
    /// Loading animation frame counter.
    pub loading_frame: usize,
    /// Should the app exit?
    pub should_quit: bool,
}

impl App {
    pub fn new(agent_name: impl Into<String>, model_name: impl Into<String>) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            scroll: 0,
            agent_name: agent_name.into(),
            model_name: model_name.into(),
            is_loading: false,
            loading_frame: 0,
            should_quit: false,
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    /// Delete the character before the cursor.
    pub fn delete_char(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor_pos -= prev;
            self.input.remove(self.cursor_pos);
        }
    }

    /// Move cursor left.
    pub fn cursor_left(&mut self) {
        if self.cursor_pos > 0 {
            let prev = self.input[..self.cursor_pos]
                .chars()
                .last()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor_pos -= prev;
        }
    }

    /// Move cursor right.
    pub fn cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            let next = self.input[self.cursor_pos..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(0);
            self.cursor_pos += next;
        }
    }

    /// Take the current input, resetting the buffer.
    pub fn take_input(&mut self) -> String {
        let input = self.input.clone();
        self.input.clear();
        self.cursor_pos = 0;
        input
    }

    /// Add a message and auto-scroll to bottom.
    pub fn push_message(&mut self, msg: DisplayMessage) {
        self.messages.push(msg);
        self.scroll_to_bottom();
    }

    /// Scroll to the bottom of messages.
    pub fn scroll_to_bottom(&mut self) {
        // Will be clamped during rendering
        self.scroll = u16::MAX;
    }

    /// Scroll up by n lines.
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    /// Scroll down by n lines.
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n);
    }

    /// Clear all messages and reset session.
    pub fn clear(&mut self) {
        self.messages.clear();
        self.scroll = 0;
        self.push_message(DisplayMessage::system("Session cleared."));
    }

    /// Loading spinner text.
    pub fn loading_text(&self) -> &str {
        const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[self.loading_frame % FRAMES.len()]
    }

    /// Advance loading animation.
    pub fn tick_loading(&mut self) {
        if self.is_loading {
            self.loading_frame = self.loading_frame.wrapping_add(1);
        }
    }
}
