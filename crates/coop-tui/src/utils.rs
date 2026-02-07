use unicode_width::UnicodeWidthChar;

/// Calculate the visible width of a string in terminal columns,
/// ignoring ANSI escape sequences.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip ANSI escape sequences
            if let Some(&next) = chars.peek() {
                match next {
                    '[' => {
                        // CSI sequence: ESC [ ... (letter)
                        chars.next();
                        while let Some(&ch) = chars.peek() {
                            if ch.is_ascii_alphabetic()
                                || ch == 'm'
                                || ch == 'G'
                                || ch == 'K'
                                || ch == 'H'
                                || ch == 'J'
                            {
                                chars.next();
                                break;
                            }
                            chars.next();
                        }
                    }
                    ']' => {
                        // OSC sequence: ESC ] ... BEL or ESC ] ... ST
                        chars.next();
                        while let Some(&ch) = chars.peek() {
                            if ch == '\x07' {
                                chars.next();
                                break;
                            }
                            if ch == '\x1b' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    '_' => {
                        // APC sequence: ESC _ ... BEL or ESC _ ... ST
                        chars.next();
                        while let Some(&ch) = chars.peek() {
                            if ch == '\x07' {
                                chars.next();
                                break;
                            }
                            if ch == '\x1b' {
                                chars.next();
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                            chars.next();
                        }
                    }
                    _ => {}
                }
            }
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Truncate a string to fit within `max_width` visible columns.
/// ANSI-aware: escape sequences don't count toward width.
pub fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut current_width = 0;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy entire ANSI escape sequence
            result.push(c);
            if let Some(&next) = chars.peek() {
                match next {
                    '[' => {
                        result.push(chars.next().unwrap());
                        while let Some(&ch) = chars.peek() {
                            result.push(chars.next().unwrap());
                            if ch.is_ascii_alphabetic()
                                || ch == 'm'
                                || ch == 'G'
                                || ch == 'K'
                                || ch == 'H'
                                || ch == 'J'
                            {
                                break;
                            }
                        }
                    }
                    ']' | '_' => {
                        result.push(chars.next().unwrap());
                        while let Some(&ch) = chars.peek() {
                            result.push(chars.next().unwrap());
                            if ch == '\x07' {
                                break;
                            }
                            if ch == '\x1b' {
                                if chars.peek() == Some(&'\\') {
                                    result.push(chars.next().unwrap());
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let w = UnicodeWidthChar::width(c).unwrap_or(0);
            if current_width + w > max_width {
                break;
            }
            result.push(c);
            current_width += w;
        }
    }
    result
}

/// Word-wrap text preserving ANSI escape sequences.
/// Returns lines each ≤ `width` visible characters.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if width == 0 {
        return vec![String::new()];
    }

    let mut result = Vec::new();

    // Track active ANSI state to carry across lines
    let mut active_codes = String::new();

    for input_line in text.split('\n') {
        let line_with_prefix = if result.is_empty() {
            input_line.to_string()
        } else {
            format!("{active_codes}{input_line}")
        };

        let line_width = visible_width(&line_with_prefix);
        if line_width <= width {
            update_ansi_state(&line_with_prefix, &mut active_codes);
            result.push(line_with_prefix);
            continue;
        }

        // Need to wrap this line
        let wrapped = wrap_single_line(&line_with_prefix, width);
        for line in &wrapped {
            update_ansi_state(line, &mut active_codes);
        }
        result.extend(wrapped);
    }

    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

#[allow(clippy::too_many_lines)]
fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }

    let mut lines = Vec::new();
    let mut current_line = String::new();
    let mut current_width = 0;
    let mut current_word = String::new();
    let mut word_width = 0;
    let mut active_codes = String::new();

    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Accumulate ANSI sequence
            let mut seq = String::from(c);
            if let Some(&next) = chars.peek() {
                match next {
                    '[' => {
                        seq.push(chars.next().unwrap());
                        while let Some(&ch) = chars.peek() {
                            seq.push(chars.next().unwrap());
                            if ch.is_ascii_alphabetic()
                                || ch == 'm'
                                || ch == 'G'
                                || ch == 'K'
                                || ch == 'H'
                                || ch == 'J'
                            {
                                break;
                            }
                        }
                    }
                    ']' | '_' => {
                        seq.push(chars.next().unwrap());
                        while let Some(&ch) = chars.peek() {
                            seq.push(chars.next().unwrap());
                            if ch == '\x07' {
                                break;
                            }
                            if ch == '\x1b' {
                                if chars.peek() == Some(&'\\') {
                                    seq.push(chars.next().unwrap());
                                }
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            current_word.push_str(&seq);
            continue;
        }

        let char_width = UnicodeWidthChar::width(c).unwrap_or(0);

        if c == ' ' {
            // Flush word first
            if !current_word.is_empty() {
                if current_width + word_width > width && current_width > 0 {
                    // Wrap
                    lines.push(current_line.trimmed_end());
                    current_line.clone_from(&active_codes);
                    current_width = 0;
                }
                current_line.push_str(&current_word);
                current_width += word_width;
                update_ansi_state(&current_word, &mut active_codes);
                current_word.clear();
                word_width = 0;
            }
            // Add the space
            if current_width + char_width <= width {
                current_line.push(c);
                current_width += char_width;
            } else {
                lines.push(current_line.trimmed_end());
                current_line.clone_from(&active_codes);
                current_width = 0;
            }
        } else {
            // Force-break if single word is too long
            if word_width + char_width > width && word_width > 0 {
                // Flush what we have
                if current_width + word_width > width && current_width > 0 {
                    lines.push(current_line.trimmed_end());
                    current_line.clone_from(&active_codes);
                    current_width = 0;
                }
                current_line.push_str(&current_word);
                current_width += word_width;
                update_ansi_state(&current_word, &mut active_codes);
                current_word.clear();
                word_width = 0;

                if current_width + char_width > width {
                    lines.push(current_line.trimmed_end());
                    current_line.clone_from(&active_codes);
                    current_width = 0;
                }
            }
            current_word.push(c);
            word_width += char_width;
        }
    }

    // Flush remaining word
    if !current_word.is_empty() {
        if current_width + word_width > width && current_width > 0 {
            lines.push(current_line.trimmed_end());
            current_line.clone_from(&active_codes);
        }
        current_line.push_str(&current_word);
    }

    if !current_line.is_empty() || lines.is_empty() {
        lines.push(current_line.trimmed_end());
    }

    lines
}

trait TrimEnd {
    fn trimmed_end(&self) -> String;
}

impl TrimEnd for String {
    fn trimmed_end(&self) -> String {
        self.trim_end().to_string()
    }
}

fn update_ansi_state(text: &str, active_codes: &mut String) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            continue;
        }
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next(); // skip '['
        let mut params = String::new();
        while let Some(&ch) = chars.peek() {
            if ch == 'm' {
                chars.next();
                break;
            }
            if ch.is_ascii_alphabetic() {
                chars.next();
                params.clear(); // Not an SGR sequence
                break;
            }
            params.push(chars.next().unwrap());
        }
        if params.is_empty() || params == "0" {
            active_codes.clear();
        } else {
            // Simple: just track the full code as an override
            *active_codes = format!("\x1b[{params}m");
        }
    }
}

/// Wrap text in 24-bit foreground color.
pub fn fg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
}

/// Wrap text in 24-bit background color.
pub fn bg_rgb(r: u8, g: u8, b: u8, text: &str) -> String {
    format!("\x1b[48;2;{r};{g};{b}m{text}\x1b[0m")
}

/// Bold text.
pub fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[0m")
}

/// Italic text.
pub fn italic(text: &str) -> String {
    format!("\x1b[3m{text}\x1b[0m")
}

/// Inverse video text.
pub fn inverse(text: &str) -> String {
    format!("\x1b[7m{text}\x1b[0m")
}

/// Pad a line to exactly `width` visible characters with spaces.
pub fn pad_to_width(line: &str, width: usize) -> String {
    let vis = visible_width(line);
    if vis >= width {
        line.to_string()
    } else {
        format!("{}{}", line, " ".repeat(width - vis))
    }
}

/// Apply a background color to a line, padding to full width.
pub fn apply_bg_to_line(line: &str, width: usize, r: u8, g: u8, b: u8) -> String {
    let vis = visible_width(line);
    let padding = if vis < width {
        " ".repeat(width - vis)
    } else {
        String::new()
    };
    format!("\x1b[48;2;{r};{g};{b}m{line}{padding}\x1b[0m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_width_plain() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn visible_width_with_ansi() {
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(visible_width("\x1b[38;2;128;128;128mtest\x1b[0m"), 4);
    }

    #[test]
    fn truncate_plain() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn truncate_with_ansi() {
        let s = "\x1b[31mhello world\x1b[0m";
        let t = truncate_to_width(s, 5);
        assert_eq!(visible_width(&t), 5);
        assert!(t.contains("\x1b[31m"));
    }

    #[test]
    fn wrap_simple() {
        let lines = wrap_text_with_ansi("hello world foo", 6);
        assert!(lines.len() >= 2);
        for line in &lines {
            assert!(visible_width(line) <= 6, "line too wide: {line:?}");
        }
    }

    #[test]
    fn wrap_preserves_newlines() {
        let lines = wrap_text_with_ansi("line1\nline2", 80);
        assert_eq!(lines.len(), 2);
    }
}
