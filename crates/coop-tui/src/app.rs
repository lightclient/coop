use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::time::Instant;

/// Role for display messages in the TUI.
#[derive(Debug, Clone, PartialEq)]
pub enum DisplayRole {
    User,
    Assistant,
    System,
    ToolCall { name: String, arguments: Value },
    ToolOutput { name: String, is_error: bool },
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

    pub fn tool_call(name: &str, arguments: &Value) -> Self {
        let content = format_tool_args(name, arguments);
        Self {
            role: DisplayRole::ToolCall {
                name: name.to_string(),
                arguments: arguments.clone(),
            },
            content,
            timestamp: Utc::now(),
            is_tool: true,
        }
    }

    pub fn tool_output(name: &str, output: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: DisplayRole::ToolOutput {
                name: name.to_string(),
                is_error,
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

/// Format tool arguments into a human-readable summary line.
fn format_tool_args(name: &str, args: &Value) -> String {
    match name {
        "bash" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            // Show first line, truncate if long
            let first_line = cmd.lines().next().unwrap_or(cmd);
            if first_line.len() > 120 {
                format!("{}…", &first_line[..120])
            } else if cmd.lines().count() > 1 {
                format!("{first_line} …")
            } else {
                first_line.to_string()
            }
        }
        "read_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            let offset = args.get("offset").and_then(Value::as_u64);
            let limit = args.get("limit").and_then(Value::as_u64);
            match (offset, limit) {
                (Some(o), Some(l)) => format!("{path} (lines {o}–{})", o + l),
                (Some(o), None) => format!("{path} (from line {o})"),
                _ => path.to_string(),
            }
        }
        "write_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            let len = args
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |s| s.lines().count());
            format!("{path} ({len} lines)")
        }
        "list_directory" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            path.to_string()
        }
        _ => {
            // Generic: show compact JSON of arguments
            let s = serde_json::to_string(args).unwrap_or_default();
            if s.len() > 120 {
                format!("{}…", &s[..120])
            } else {
                s
            }
        }
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

    /// Move cursor up one line in multi-line input.
    pub fn cursor_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row > 0 {
            self.cursor_pos = self.pos_from_row_col(row - 1, col);
        }
    }

    /// Move cursor down one line in multi-line input.
    pub fn cursor_down(&mut self) {
        let (row, col) = self.cursor_row_col();
        let line_count = self.input.lines().count().max(1);
        if row + 1 < line_count {
            self.cursor_pos = self.pos_from_row_col(row + 1, col);
        }
    }

    /// Move cursor to the start of the current line.
    pub fn cursor_home(&mut self) {
        let before = &self.input[..self.cursor_pos];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        self.cursor_pos = line_start;
    }

    /// Move cursor to the end of the current line.
    pub fn cursor_end(&mut self) {
        let after = &self.input[self.cursor_pos..];
        let line_end = after
            .find('\n')
            .map_or(self.input.len(), |i| self.cursor_pos + i);
        self.cursor_pos = line_end;
    }

    /// Get the (row, col) of the cursor in the input text.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.input[..self.cursor_pos];
        let row = before.matches('\n').count();
        let col = before
            .rfind('\n')
            .map_or(before.len(), |i| before.len() - i - 1);
        (row, col)
    }

    /// Convert (row, col) back to a byte position, clamping to line length.
    fn pos_from_row_col(&self, target_row: usize, target_col: usize) -> usize {
        let mut pos = 0;
        for (i, line) in self.input.split('\n').enumerate() {
            if i == target_row {
                return pos + target_col.min(line.len());
            }
            pos += line.len() + 1; // +1 for the \n
        }
        self.input.len()
    }

    /// Number of lines in the input buffer.
    pub fn input_line_count(&self) -> usize {
        if self.input.is_empty() {
            1
        } else {
            self.input.lines().count() + usize::from(self.input.ends_with('\n'))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new("test-agent", "test-model", "main", 200_000)
    }

    #[test]
    fn single_line_input() {
        let mut app = test_app();
        app.insert_char('h');
        app.insert_char('i');
        assert_eq!(app.input, "hi");
        assert_eq!(app.input_line_count(), 1);
        assert_eq!(app.cursor_row_col(), (0, 2));
    }

    #[test]
    fn multiline_insert_newline() {
        let mut app = test_app();
        for c in "line1".chars() {
            app.insert_char(c);
        }
        app.insert_char('\n');
        for c in "line2".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "line1\nline2");
        assert_eq!(app.input_line_count(), 2);
        assert_eq!(app.cursor_row_col(), (1, 5));
    }

    #[test]
    fn cursor_up_down_navigation() {
        let mut app = test_app();
        app.input = "abc\ndef\nghi".to_string();
        app.cursor_pos = app.input.len(); // end of "ghi"
        assert_eq!(app.cursor_row_col(), (2, 3));

        app.cursor_up();
        assert_eq!(app.cursor_row_col(), (1, 3));
        assert_eq!(&app.input[app.cursor_pos..=app.cursor_pos], "\n");

        app.cursor_up();
        assert_eq!(app.cursor_row_col(), (0, 3));

        // Already at top, should stay
        app.cursor_up();
        assert_eq!(app.cursor_row_col(), (0, 3));

        app.cursor_down();
        assert_eq!(app.cursor_row_col(), (1, 3));
    }

    #[test]
    fn cursor_up_clamps_to_shorter_line() {
        let mut app = test_app();
        app.input = "ab\nc\ndefgh".to_string();
        app.cursor_pos = app.input.len(); // end of "defgh", col=5
        assert_eq!(app.cursor_row_col(), (2, 5));

        app.cursor_up(); // line "c" only has 1 char, col clamped to 1
        assert_eq!(app.cursor_row_col(), (1, 1));

        app.cursor_up(); // line "ab" has 2 chars, col clamped to 1 (sticky)
        assert_eq!(app.cursor_row_col(), (0, 1));
    }

    #[test]
    fn cursor_home_end_multiline() {
        let mut app = test_app();
        app.input = "abc\ndefgh".to_string();
        app.cursor_pos = 6; // middle of "defgh" → "de|fgh"
        assert_eq!(app.cursor_row_col(), (1, 2));

        app.cursor_home();
        assert_eq!(app.cursor_row_col(), (1, 0));
        assert_eq!(app.cursor_pos, 4); // right after '\n'

        app.cursor_end();
        assert_eq!(app.cursor_row_col(), (1, 5));
        assert_eq!(app.cursor_pos, 9); // end of string
    }

    #[test]
    fn input_line_count_trailing_newline() {
        let mut app = test_app();
        app.input = "hello\n".to_string();
        assert_eq!(app.input_line_count(), 2);

        app.input = "a\nb\n".to_string();
        assert_eq!(app.input_line_count(), 3);
    }

    #[test]
    fn take_input_preserves_newlines() {
        let mut app = test_app();
        app.input = "line1\nline2\nline3".to_string();
        app.cursor_pos = app.input.len();
        let taken = app.take_input();
        assert_eq!(taken, "line1\nline2\nline3");
        assert_eq!(app.input, "");
        assert_eq!(app.cursor_pos, 0);
    }
}
