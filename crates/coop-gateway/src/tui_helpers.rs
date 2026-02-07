use coop_core::Content;
use coop_tui::{App, Container, Editor, Footer, MarkdownComponent, StatusLine, Text, ToolBox, Tui};
use std::collections::HashMap;

/// Rebuild the chat container from the app's message list.
#[allow(clippy::too_many_lines)]
pub(crate) fn update_chat_messages(tui: &mut Tui, app: &App, chat_idx: usize) {
    let chat = tui.root_mut().children_mut()[chat_idx]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Container>());
    let Some(chat) = chat else { return };
    chat.clear();

    for msg in &app.messages {
        match &msg.role {
            coop_tui::DisplayRole::User => {
                let md =
                    MarkdownComponent::new(msg.content.clone(), 1, 1).with_bg(0x34, 0x35, 0x41);
                chat.add_child(Box::new(md));
            }
            coop_tui::DisplayRole::Assistant => {
                let md = MarkdownComponent::new(msg.content.clone(), 1, 1);
                chat.add_child(Box::new(md));
            }
            coop_tui::DisplayRole::System => {
                let styled = coop_tui::utils::fg_rgb(0x80, 0x80, 0x80, &msg.content);
                chat.add_child(Box::new(Text::new(styled, 1, 0)));
            }
            coop_tui::DisplayRole::ToolCall { name, .. } => {
                if !app.verbose {
                    continue;
                }
                let (icon, verb) = tool_label(name);
                let header = if verb == "Run" {
                    format!("{icon} {verb} {name}")
                } else {
                    format!("{icon} {verb}")
                };
                let content = format!(
                    "{}\n{}",
                    coop_tui::utils::bold(&coop_tui::utils::fg_rgb(0xff, 0xff, 0x00, &header)),
                    coop_tui::utils::fg_rgb(0x50, 0x50, 0x50, &msg.content)
                );
                let mut tb = ToolBox::new(1, 1).with_bg(0x28, 0x28, 0x32);
                tb.set_lines(vec![content]);
                chat.add_child(Box::new(tb));
            }
            coop_tui::DisplayRole::ToolOutput { name, is_error } => {
                if !app.verbose {
                    continue;
                }
                let bg = if *is_error {
                    (0x3c, 0x28, 0x28)
                } else {
                    (0x28, 0x32, 0x28)
                };
                let text_color = if *is_error {
                    (0xcc, 0x66, 0x66)
                } else {
                    (0x80, 0x80, 0x80)
                };

                let content_lines: Vec<&str> = msg.content.lines().collect();
                let display = if content_lines.len() > 20 {
                    let mut lines = Vec::new();
                    for l in &content_lines[..10] {
                        lines.push(coop_tui::utils::fg_rgb(
                            text_color.0,
                            text_color.1,
                            text_color.2,
                            l,
                        ));
                    }
                    lines.push(coop_tui::utils::fg_rgb(
                        0x80,
                        0x80,
                        0x80,
                        &format!(
                            "... ({} earlier lines, ctrl+o to expand)",
                            content_lines.len() - 20
                        ),
                    ));
                    for l in &content_lines[content_lines.len() - 10..] {
                        lines.push(coop_tui::utils::fg_rgb(
                            text_color.0,
                            text_color.1,
                            text_color.2,
                            l,
                        ));
                    }
                    lines
                } else {
                    content_lines
                        .iter()
                        .map(|l| {
                            coop_tui::utils::fg_rgb(text_color.0, text_color.1, text_color.2, l)
                        })
                        .collect()
                };

                let _ = name;
                let mut tb = ToolBox::new(1, 1).with_bg(bg.0, bg.1, bg.2);
                tb.set_lines(display);
                chat.add_child(Box::new(tb));
            }
        }
    }
}

pub(crate) fn sync_editor_from_app(tui: &mut Tui, app: &App, editor_idx: usize) {
    let editor = tui.root_mut().children_mut()[editor_idx]
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<Editor>());
    if let Some(e) = editor {
        e.set_text(&app.input);
    }
}

fn tool_label(name: &str) -> (&'static str, &'static str) {
    match name {
        "bash" => ("⚡", "Execute"),
        "read_file" | "Read" => ("📄", "Read"),
        "write_file" | "Write" => ("✏️", "Write"),
        "list_directory" => ("📂", "List"),
        _ => ("🔧", "Run"),
    }
}

/// Extract (output, `is_error`) from a `TurnEvent::ToolResult` message.
pub(crate) fn extract_tool_result(message: &coop_core::Message) -> (String, bool) {
    message
        .content
        .iter()
        .find_map(|content| match content {
            Content::ToolResult {
                output, is_error, ..
            } => Some((output.clone(), *is_error)),
            _ => None,
        })
        .unwrap_or_else(|| (message.text(), false))
}

/// Format the welcome banner with ANSI colors.
pub(crate) fn format_tui_welcome(version: &str, model: &str, working_dir: &str) -> String {
    let lc = coop_tui::theme::fg_code(coop_tui::theme::MD_HEADING);
    let ic = coop_tui::theme::fg_code(coop_tui::theme::MUTED);
    let bc = "\x1b[1m\x1b[37m"; // bold white
    let r = coop_tui::theme::RESET;
    format!(
        "\
{lc}  ████████      {r}
{lc}██▓▓▓▓▓▓▓▓██    {r}{bc}Coop v{version}{r}
{lc}██▓▓▓▓▓▓▓▓██    {r}{ic}{model}{r}
{lc}██▓▓██▓▓██▓▓██  {r}{ic}{working_dir}{r}
{lc}██▓▓▓▓▓▓▓▓██    {r}
{lc}  ████████      {r}"
    )
}

pub(crate) fn detect_git_branch() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Build the initial TUI layout and return the `Tui` + `App`.
pub(crate) fn build_tui(
    agent_id: &str,
    model: &str,
    session: &str,
    working_dir: &str,
    context_window: u32,
) -> (Tui, App, HashMap<String, String>) {
    let git_branch = detect_git_branch();

    let mut tui = Tui::new();
    let mut app = App::new(agent_id, model, session, context_window);
    app.version = env!("CARGO_PKG_VERSION").to_string();
    app.working_dir = working_dir.to_string();

    let welcome = format_tui_welcome(env!("CARGO_PKG_VERSION"), model, working_dir);

    tui.root_mut().add_child(Box::new(Text::new(welcome, 1, 1)));
    tui.root_mut().add_child(Box::new(Container::new())); // chat container
    tui.root_mut().add_child(Box::new(coop_tui::Spacer::new(0))); // dynamic spacer
    tui.root_mut().add_child(Box::new(StatusLine::new()));
    tui.root_mut().add_child(Box::new(Editor::new()));
    let mut footer = Footer::new(working_dir, model, context_window);
    footer.set_git_branch(git_branch);
    tui.root_mut().add_child(Box::new(footer));

    let tool_names: HashMap<String, String> = HashMap::new();
    (tui, app, tool_names)
}

/// Resolve the working directory, shortening `$HOME` to `~`.
pub(crate) fn resolve_working_dir() -> String {
    let cwd = std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && cwd.starts_with(&home) {
        format!("~{}", &cwd[home.len()..])
    } else {
        cwd
    }
}

#[cfg(feature = "signal")]
pub(crate) fn resolve_config_path(
    base_dir: &std::path::Path,
    configured_path: &str,
) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(configured_path);
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}
