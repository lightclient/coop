use coop_tui::engine::Component;
/// Tests that verify coop-tui components produce output matching pi TUI captures.
///
/// The reference captures live in tests/captures/ and were collected from
/// `pi --model claude-3-5-haiku-latest` running in a 120×40 tmux pane.
use coop_tui::utils::{fg_rgb, visible_width};
use coop_tui::{Editor, Footer, Spacer, Text, ToolBox};

// -- Editor tests (from pi-initial-ansi.txt) --

#[test]
fn editor_empty_border_color_matches_pi() {
    let editor = Editor::new();
    let lines = editor.render(120);

    // Pi uses border color #81a2be = rgb(129,162,190)
    let expected_border_start = "\x1b[38;2;129;162;190m";
    assert!(
        lines[0].starts_with(expected_border_start),
        "top border should start with #81a2be color code, got: {:?}",
        &lines[0][..50.min(lines[0].len())]
    );

    let last = lines.last().unwrap();
    assert!(
        last.starts_with(expected_border_start),
        "bottom border should start with #81a2be color code"
    );
}

#[test]
fn editor_empty_has_inverse_video_cursor() {
    let editor = Editor::new();
    let lines = editor.render(120);

    // Content line (line 1) should have inverse video cursor: \x1b[7m \x1b[0m
    assert!(
        lines[1].contains("\x1b[7m"),
        "empty editor should show inverse video cursor"
    );
}

#[test]
fn editor_border_is_full_width() {
    let editor = Editor::new();
    let width = 80;
    let lines = editor.render(width);

    // Border line should be exactly `width` visible characters (all ─)
    let border_vis = visible_width(&lines[0]);
    assert_eq!(
        border_vis, width,
        "border visible width should match render width"
    );
}

#[test]
fn editor_renders_three_lines_when_empty() {
    let editor = Editor::new();
    let lines = editor.render(80);
    // top border + 1 content line + bottom border
    assert_eq!(lines.len(), 3);
}

// -- Footer tests (from pi captures) --

#[test]
fn footer_renders_two_lines() {
    let f = Footer::new("~/coop/coop", "claude-opus-4-6", 200_000);
    let lines = f.render(120);
    assert_eq!(lines.len(), 2);
}

#[test]
fn footer_line1_has_dim_color() {
    let mut f = Footer::new("~/coop/coop", "claude-opus-4-6", 200_000);
    f.set_git_branch(Some("main".to_string()));
    let lines = f.render(120);

    // Pi uses dim color #666666 = rgb(102,102,102)
    let expected_dim = "\x1b[38;2;102;102;102m";
    assert!(
        lines[0].starts_with(expected_dim),
        "footer line 1 should use dim color #666666, got: {:?}",
        &lines[0][..50.min(lines[0].len())]
    );
}

#[test]
fn footer_line1_contains_working_dir_and_branch() {
    let mut f = Footer::new("~/coop/coop", "claude-opus-4-6", 200_000);
    f.set_git_branch(Some("main".to_string()));
    let lines = f.render(120);

    // Should contain "~/coop/coop (main)" in the dim-wrapped text
    assert!(
        lines[0].contains("~/coop/coop (main)"),
        "footer line 1 should show working dir + branch"
    );
}

#[test]
fn footer_line2_has_model_name() {
    let f = Footer::new("~/coop/coop", "claude-opus-4-6", 200_000);
    let lines = f.render(120);
    assert!(
        lines[1].contains("claude-opus-4-6"),
        "footer line 2 should contain model name"
    );
}

#[test]
fn footer_line2_has_context_percent() {
    let f = Footer::new("~/coop/coop", "claude-opus-4-6", 200_000);
    let lines = f.render(120);
    assert!(
        lines[1].contains("0.0%/200k"),
        "footer should show context usage percentage, got: {:?}",
        lines[1]
    );
}

// -- Spacer tests --

#[test]
fn spacer_zero_is_empty() {
    let s = Spacer::new(0);
    assert!(s.render(80).is_empty());
}

#[test]
fn spacer_three_lines() {
    let s = Spacer::new(3);
    let lines = s.render(80);
    assert_eq!(lines.len(), 3);
    for l in &lines {
        assert!(l.is_empty());
    }
}

// -- Text tests --

#[test]
fn text_empty_returns_nothing() {
    let t = Text::new("", 1, 1);
    assert!(t.render(80).is_empty());
}

#[test]
fn text_with_padding() {
    let t = Text::new("hello", 1, 1);
    let lines = t.render(80);
    // padding_y=1: 1 empty + 1 content + 1 empty = 3
    assert_eq!(lines.len(), 3);
}

// -- ToolBox tests (from pi-tool-ansi.txt) --

#[test]
fn tool_box_success_bg_color() {
    let mut tb = ToolBox::new(1, 1).with_bg(0x28, 0x32, 0x28);
    tb.set_lines(vec!["test output".to_string()]);
    let lines = tb.render(80);

    // Pi uses tool success bg #283228 = rgb(40,50,40)
    let expected_bg = "\x1b[48;2;40;50;40m";
    for line in &lines {
        assert!(
            line.contains(expected_bg),
            "tool box should use success bg color #283228"
        );
    }
}

#[test]
fn tool_box_error_bg_color() {
    let mut tb = ToolBox::new(1, 1).with_bg(0x3c, 0x28, 0x28);
    tb.set_lines(vec![fg_rgb(0xcc, 0x66, 0x66, "Error: not found")]);
    let lines = tb.render(80);

    let expected_bg = "\x1b[48;2;60;40;40m";
    for line in &lines {
        assert!(
            line.contains(expected_bg),
            "tool box error should use bg #3c2828"
        );
    }
}

#[test]
fn tool_box_empty_renders_nothing() {
    let tb = ToolBox::new(1, 1);
    assert!(tb.render(80).is_empty());
}

// -- Visible width tests --

#[test]
fn visible_width_with_24bit_color() {
    let s = "\x1b[38;2;129;162;190m────\x1b[0m";
    assert_eq!(visible_width(s), 4);
}

#[test]
fn visible_width_with_inverse() {
    let s = "\x1b[7m \x1b[0m";
    assert_eq!(visible_width(s), 1);
}

#[test]
fn visible_width_with_bg_color() {
    let s = "\x1b[48;2;52;53;65mhello\x1b[0m";
    assert_eq!(visible_width(s), 5);
}
