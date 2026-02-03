use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;

/// Result of handling input.
#[derive(Debug)]
pub enum InputAction {
    /// No action needed.
    None,
    /// User submitted a message.
    Submit(String),
    /// User wants to quit.
    Quit,
    /// User wants to clear the session.
    Clear,
    /// User wants to toggle verbose mode.
    ToggleVerbose,
}

/// Handle a key event, updating app state and returning any action.
#[allow(clippy::too_many_lines)]
pub fn handle_key_event(app: &mut App, key: KeyEvent) -> InputAction {
    match (key.modifiers, key.code) {
        // Quit
        (KeyModifiers::CONTROL, KeyCode::Char('c')) => InputAction::Quit,
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => {
            if app.input.is_empty() {
                InputAction::Quit
            } else {
                InputAction::None
            }
        }

        // Newline in multi-line input
        (KeyModifiers::SHIFT, KeyCode::Enter) => {
            app.insert_char('\n');
            InputAction::None
        }

        // Submit
        (_, KeyCode::Enter) => {
            let input = app.take_input();
            let trimmed = input.trim().to_string();

            if trimmed.is_empty() {
                return InputAction::None;
            }

            // Handle commands
            match trimmed.as_str() {
                "/quit" | "/exit" | "/q" => InputAction::Quit,
                "/clear" | "/reset" => InputAction::Clear,
                "/verbose" | "/v" => InputAction::ToggleVerbose,
                _ => InputAction::Submit(trimmed),
            }
        }

        // Editing
        (_, KeyCode::Backspace) => {
            app.delete_char();
            InputAction::None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
            // Clear input line
            app.input.clear();
            app.cursor_pos = 0;
            InputAction::None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
            // Delete word backwards
            let before = &app.input[..app.cursor_pos];
            let trimmed = before.trim_end();
            let last_space = trimmed.rfind(' ').map_or(0, |i| i + 1);
            app.input = format!(
                "{}{}",
                &app.input[..last_space],
                &app.input[app.cursor_pos..]
            );
            app.cursor_pos = last_space;
            InputAction::None
        }

        // Cursor movement
        (_, KeyCode::Left) => {
            app.cursor_left();
            InputAction::None
        }
        (_, KeyCode::Right) => {
            app.cursor_right();
            InputAction::None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('a')) | (_, KeyCode::Home) => {
            app.cursor_home();
            InputAction::None
        }
        (KeyModifiers::CONTROL, KeyCode::Char('e')) | (_, KeyCode::End) => {
            app.cursor_end();
            InputAction::None
        }

        // Scrolling (Shift+Arrow always scrolls messages, must come before bare arrows)
        (KeyModifiers::SHIFT, KeyCode::Up) => {
            app.scroll_up(1);
            InputAction::None
        }
        (KeyModifiers::SHIFT, KeyCode::Down) => {
            app.scroll_down(1);
            InputAction::None
        }
        (_, KeyCode::PageUp) => {
            app.scroll_up(10);
            InputAction::None
        }
        (_, KeyCode::PageDown) => {
            app.scroll_down(10);
            InputAction::None
        }

        // Up/Down: navigate within multi-line input, or scroll messages
        (_, KeyCode::Up) => {
            if app.input_line_count() > 1 {
                app.cursor_up();
            } else {
                app.scroll_up(1);
            }
            InputAction::None
        }
        (_, KeyCode::Down) => {
            if app.input_line_count() > 1 {
                app.cursor_down();
            } else {
                app.scroll_down(1);
            }
            InputAction::None
        }

        // Regular character input
        (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(c)) => {
            app.insert_char(c);
            InputAction::None
        }

        _ => InputAction::None,
    }
}

/// Poll for the next crossterm event with a timeout.
pub fn poll_event(timeout: std::time::Duration) -> Option<Event> {
    if event::poll(timeout).ok()? {
        event::read().ok()
    } else {
        None
    }
}
