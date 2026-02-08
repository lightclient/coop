use crate::engine::{Component, StyledLine};
use crate::theme;
use crate::utils::visible_width;

/// Footer component — 2-line status bar.
/// Translation of pi's footer.js.
#[derive(Debug)]
pub struct Footer {
    working_dir: String,
    git_branch: Option<String>,
    model_name: String,
    total_input: u32,
    total_output: u32,
    cache_read: u32,
    cache_write: u32,
    cost: f64,
    context_tokens: u32,
    context_window: u32,
    auto_compact: bool,
    using_subscription: bool,
    thinking_level: Option<String>,
}

impl Footer {
    pub fn new(working_dir: &str, model_name: &str, context_window: u32) -> Self {
        Self {
            working_dir: working_dir.to_owned(),
            git_branch: None,
            model_name: model_name.to_owned(),
            total_input: 0,
            total_output: 0,
            cache_read: 0,
            cache_write: 0,
            cost: 0.0,
            context_tokens: 0,
            context_window,
            auto_compact: true,
            using_subscription: false,
            thinking_level: None,
        }
    }

    pub fn set_usage(
        &mut self,
        input: u32,
        output: u32,
        cache_read: u32,
        cache_write: u32,
        context_tokens: u32,
    ) {
        self.total_input = input;
        self.total_output = output;
        self.cache_read = cache_read;
        self.cache_write = cache_write;
        self.context_tokens = context_tokens;
    }

    pub fn set_cost(&mut self, cost: f64) {
        self.cost = cost;
    }

    pub fn set_using_subscription(&mut self, v: bool) {
        self.using_subscription = v;
    }

    pub fn set_thinking_level(&mut self, level: Option<String>) {
        self.thinking_level = level;
    }

    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.git_branch = branch;
    }

    pub fn set_working_dir(&mut self, dir: &str) {
        dir.clone_into(&mut self.working_dir);
    }
}

fn format_tokens(count: u32) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", f64::from(count) / 1000.0)
    } else if count < 1_000_000 {
        format!("{}k", count / 1000)
    } else if count < 10_000_000 {
        format!("{:.1}M", f64::from(count) / 1_000_000.0)
    } else {
        format!("{}M", count / 1_000_000)
    }
}

impl Component for Footer {
    fn render(&self, width: usize) -> Vec<StyledLine> {
        // Line 1: working directory (+ branch)
        let mut pwd = self.working_dir.clone();
        if let Some(ref branch) = self.git_branch {
            pwd = format!("{pwd} ({branch})");
        }
        if pwd.len() > width {
            let half = width / 2;
            if half > 2 {
                let start = &pwd[..half - 1];
                let end_start = pwd.len().saturating_sub(half - 1);
                let end = &pwd[end_start..];
                pwd = format!("{start}...{end}");
            } else {
                pwd = pwd[..width].to_string();
            }
        }

        // Line 2: stats + model
        let mut stats_parts = Vec::new();
        if self.total_input > 0 {
            stats_parts.push(format!("↑{}", format_tokens(self.total_input)));
        }
        if self.total_output > 0 {
            stats_parts.push(format!("↓{}", format_tokens(self.total_output)));
        }
        if self.cache_read > 0 {
            stats_parts.push(format!("R{}", format_tokens(self.cache_read)));
        }
        if self.cache_write > 0 {
            stats_parts.push(format!("W{}", format_tokens(self.cache_write)));
        }
        if self.cost > 0.0 || self.using_subscription {
            let sub = if self.using_subscription {
                " (sub)"
            } else {
                ""
            };
            stats_parts.push(format!("${:.3}{sub}", self.cost));
        }

        let context_pct = if self.context_window > 0 {
            f64::from(self.context_tokens) / f64::from(self.context_window) * 100.0
        } else {
            0.0
        };
        let auto_ind = if self.auto_compact { " (auto)" } else { "" };
        let context_str = format!(
            "{:.1}%/{}{}",
            context_pct,
            format_tokens(self.context_window),
            auto_ind
        );
        // Colorize if high usage
        let context_display = if context_pct > 90.0 {
            theme::fg(theme::ERROR, &context_str)
        } else if context_pct > 70.0 {
            theme::fg(theme::WARNING, &context_str)
        } else {
            context_str
        };
        stats_parts.push(context_display);

        let stats_left = stats_parts.join(" ");
        let stats_left_width = visible_width(&stats_left);

        // Right side: model name + thinking level
        let mut right_side = self.model_name.clone();
        if let Some(ref level) = self.thinking_level {
            right_side = format!("{right_side} • {level}");
        }

        let min_padding = 2;
        let right_width = visible_width(&right_side);
        let total_needed = stats_left_width + min_padding + right_width;

        let stats_line = if total_needed <= width {
            let padding = " ".repeat(width - stats_left_width - right_width);
            format!("{stats_left}{padding}{right_side}")
        } else {
            let avail = width.saturating_sub(stats_left_width + min_padding);
            if avail > 3 {
                let truncated = &right_side[..avail.min(right_side.len())];
                let padding = " ".repeat(width.saturating_sub(stats_left_width + truncated.len()));
                format!("{stats_left}{padding}{truncated}")
            } else {
                stats_left
            }
        };

        vec![
            theme::fg(theme::DIM, &pwd),
            theme::fg(theme::DIM, &stats_line),
        ]
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_renders_two_lines() {
        let f = Footer::new("~/coop", "claude-sonnet-4-20250514", 200_000);
        let lines = f.render(120);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn footer_contains_model_name() {
        let f = Footer::new("~/coop", "claude-sonnet-4-20250514", 200_000);
        let lines = f.render(120);
        // The second line should contain the model name (inside dim styling)
        assert!(lines[1].contains("claude-sonnet-4-20250514"));
    }
}
