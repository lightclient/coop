use chrono::{DateTime, Local, Utc};
use std::time::Instant;

/// Role for display messages in the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    System,
    ToolCall { name: String },
    ToolOutput { name: String },
}

/// A message to display in the TUI.
#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub role: DisplayRole,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    pub is_tool: bool,
}

impl DisplayMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::User,
            content: content.into(),
            timestamp: Utc::now(),
            is_tool: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::Assistant,
            content: content.into(),
            timestamp: Utc::now(),
            is_tool: false,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::System,
            content: content.into(),
            timestamp: Utc::now(),
            is_tool: false,
        }
    }

    pub fn tool_call(name: &str) -> Self {
        Self {
            role: DisplayRole::ToolCall {
                name: name.to_string(),
            },
            content: String::new(),
            timestamp: Utc::now(),
            is_tool: true,
        }
    }

    pub fn tool_output(name: &str, output: impl Into<String>) -> Self {
        Self {
            role: DisplayRole::ToolOutput {
                name: name.to_string(),
            },
            content: output.into(),
            timestamp: Utc::now(),
            is_tool: true,
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
#[derive(Debug)]
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
    /// Whether to show tool call output.
    pub verbose: bool,
    /// Cumulative token count for the session.
    pub token_count: u32,
    /// Model context window limit.
    pub context_limit: u32,
    /// When the current turn started.
    pub turn_started: Option<Instant>,
    /// Session name for display.
    pub session_name: String,
    /// Connection status text.
    pub connection_status: String,
}

impl App {
    pub fn new(
        agent_name: impl Into<String>,
        model_name: impl Into<String>,
        session_name: impl Into<String>,
        context_limit: u32,
    ) -> Self {
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
            verbose: false,
            token_count: 0,
            context_limit,
            turn_started: None,
            session_name: session_name.into(),
            connection_status: String::new(),
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
                .map_or(0, char::len_utf8);
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
                .map_or(0, char::len_utf8);
            self.cursor_pos -= prev;
        }
    }

    /// Move cursor right.
    pub fn cursor_right(&mut self) {
        if self.cursor_pos < self.input.len() {
            let next = self.input[self.cursor_pos..]
                .chars()
                .next()
                .map_or(0, char::len_utf8);
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

    /// Append text to the last assistant message, or create a new one.
    pub fn append_or_create_assistant(&mut self, text: &str) {
        if let Some(last) = self.messages.last_mut()
            && last.role == DisplayRole::Assistant
        {
            last.content.push_str(text);
            self.scroll_to_bottom();
            return;
        }
        self.push_message(DisplayMessage::assistant(text));
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

    /// Toggle verbose mode (show/hide tool call output).
    pub fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        let state = if self.verbose { "on" } else { "off" };
        self.push_message(DisplayMessage::system(format!("Verbose mode {state}.")));
    }

    /// Mark the start of a new agent turn.
    pub fn start_turn(&mut self) {
        self.is_loading = true;
        self.turn_started = Some(Instant::now());
    }

    /// Mark the end of an agent turn and update token count.
    pub fn end_turn(&mut self, tokens: u32) {
        self.is_loading = false;
        self.turn_started = None;
        self.token_count = tokens;
    }

    /// Elapsed time text for the current turn.
    pub fn elapsed_text(&self) -> String {
        match self.turn_started {
            Some(start) => {
                let secs = start.elapsed().as_secs();
                format!("{secs}s")
            }
            None => String::new(),
        }
    }

    /// Token usage as a percentage of the context limit.
    pub fn token_percent(&self) -> f64 {
        if self.context_limit == 0 {
            return 0.0;
        }
        f64::from(self.token_count) / f64::from(self.context_limit) * 100.0
    }
}
