// Dark theme colors from pi's dark.json, resolved to RGB values.

// Core UI colors
pub const ACCENT: (u8, u8, u8) = (0x8a, 0xbe, 0xb7);
pub const BORDER: (u8, u8, u8) = (0x5f, 0x87, 0xff);
pub const BORDER_ACCENT: (u8, u8, u8) = (0x00, 0xd7, 0xff);
pub const BORDER_MUTED: (u8, u8, u8) = (0x50, 0x50, 0x50);
pub const SUCCESS: (u8, u8, u8) = (0xb5, 0xbd, 0x68);
pub const ERROR: (u8, u8, u8) = (0xcc, 0x66, 0x66);
pub const WARNING: (u8, u8, u8) = (0xff, 0xff, 0x00);
pub const MUTED: (u8, u8, u8) = (0x80, 0x80, 0x80);
pub const DIM: (u8, u8, u8) = (0x66, 0x66, 0x66);
pub const DARK_GRAY: (u8, u8, u8) = (0x50, 0x50, 0x50);

// Backgrounds
pub const USER_MSG_BG: (u8, u8, u8) = (0x34, 0x35, 0x41);
pub const TOOL_PENDING_BG: (u8, u8, u8) = (0x28, 0x28, 0x32);
pub const TOOL_SUCCESS_BG: (u8, u8, u8) = (0x28, 0x32, 0x28);
pub const TOOL_ERROR_BG: (u8, u8, u8) = (0x3c, 0x28, 0x28);

// Markdown
pub const MD_HEADING: (u8, u8, u8) = (0xf0, 0xc6, 0x74);
pub const MD_LINK: (u8, u8, u8) = (0x81, 0xa2, 0xbe);
pub const MD_CODE: (u8, u8, u8) = (0x8a, 0xbe, 0xb7);
pub const MD_LIST_BULLET: (u8, u8, u8) = (0x8a, 0xbe, 0xb7);
pub const MD_CODE_BLOCK: (u8, u8, u8) = (0xb5, 0xbd, 0x68);
pub const MD_CODE_BLOCK_BORDER: (u8, u8, u8) = (0x80, 0x80, 0x80);
pub const MD_QUOTE: (u8, u8, u8) = (0x80, 0x80, 0x80);

// Thinking
pub const THINKING_TEXT: (u8, u8, u8) = (0x80, 0x80, 0x80);
pub const THINKING_MEDIUM: (u8, u8, u8) = (0x81, 0xa2, 0xbe);

/// Foreground color wrapper.
pub fn fg(color: (u8, u8, u8), text: &str) -> String {
    let (r, g, b) = color;
    format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
}

/// Background color wrapper.
pub fn bg(color: (u8, u8, u8), text: &str) -> String {
    let (r, g, b) = color;
    format!("\x1b[48;2;{r};{g};{b}m{text}\x1b[0m")
}

/// Foreground color code only (no reset).
pub fn fg_code(color: (u8, u8, u8)) -> String {
    let (r, g, b) = color;
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Background color code only (no reset).
pub fn bg_code(color: (u8, u8, u8)) -> String {
    let (r, g, b) = color;
    format!("\x1b[48;2;{r};{g};{b}m")
}

pub const RESET: &str = "\x1b[0m";
