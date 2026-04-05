use std::collections::HashSet;

use coop_core::ToolDef;

use crate::config::SubagentProfileConfig;

const ALWAYS_DENIED_TOOLS: &[&str] = &[
    "subagents",
    "signal_send",
    "signal_reply",
    "signal_react",
    "signal_send_image",
    "config_write",
    "reminder",
];

pub(crate) fn filter_child_tools(
    all_tools: &[ToolDef],
    parent_visible_tools: &[String],
    profile: Option<&SubagentProfileConfig>,
    request_tools: &[String],
    depth: u32,
    max_spawn_depth: u32,
) -> Vec<ToolDef> {
    let parent_visible: HashSet<&str> = if parent_visible_tools.is_empty() {
        all_tools.iter().map(|tool| tool.name.as_str()).collect()
    } else {
        parent_visible_tools.iter().map(String::as_str).collect()
    };

    let profile_tools: Option<HashSet<&str>> = profile
        .and_then(|profile| profile.tools.as_ref())
        .map(|tools| tools.iter().map(String::as_str).collect());
    let requested_tools: Option<HashSet<&str>> =
        (!request_tools.is_empty()).then(|| request_tools.iter().map(String::as_str).collect());

    let allow_spawn = profile.is_some_and(|profile| profile.allow_spawn) && depth < max_spawn_depth;

    all_tools
        .iter()
        .filter(|tool| parent_visible.contains(tool.name.as_str()))
        .filter(|tool| {
            profile_tools
                .as_ref()
                .is_none_or(|tools| tools.contains(tool.name.as_str()))
        })
        .filter(|tool| {
            requested_tools
                .as_ref()
                .is_none_or(|tools| tools.contains(tool.name.as_str()))
        })
        .filter(|tool| {
            if tool.name == "subagent_spawn" {
                return allow_spawn;
            }
            !ALWAYS_DENIED_TOOLS.contains(&tool.name.as_str())
        })
        .cloned()
        .collect()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> ToolDef {
        ToolDef::new(name, name, serde_json::json!({"type": "object"}))
    }

    #[test]
    fn child_tools_are_intersection_of_parent_profile_and_request() {
        let all_tools = vec![
            tool("bash"),
            tool("read_file"),
            tool("write_file"),
            tool("subagent_spawn"),
        ];
        let profile = SubagentProfileConfig {
            model: None,
            tools: Some(vec![
                "bash".into(),
                "read_file".into(),
                "subagent_spawn".into(),
            ]),
            prompt_mode: None,
            default_timeout_seconds: None,
            default_max_turns: None,
            allow_spawn: false,
        };

        let filtered = filter_child_tools(
            &all_tools,
            &["bash".into(), "read_file".into(), "write_file".into()],
            Some(&profile),
            &["read_file".into(), "write_file".into()],
            1,
            2,
        );

        let names: Vec<&str> = filtered.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["read_file"]);
    }

    #[test]
    fn denylist_wins_over_parent_visibility() {
        let all_tools = vec![tool("signal_send"), tool("config_write"), tool("bash")];
        let filtered = filter_child_tools(
            &all_tools,
            &["signal_send".into(), "config_write".into(), "bash".into()],
            None,
            &[],
            1,
            2,
        );
        let names: Vec<&str> = filtered.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, vec!["bash"]);
    }

    #[test]
    fn spawn_capability_requires_profile_opt_in_and_depth_budget() {
        let all_tools = vec![tool("subagent_spawn"), tool("bash")];
        let profile = SubagentProfileConfig {
            model: None,
            tools: Some(vec!["subagent_spawn".into(), "bash".into()]),
            prompt_mode: None,
            default_timeout_seconds: None,
            default_max_turns: None,
            allow_spawn: true,
        };

        let allowed = filter_child_tools(&all_tools, &[], Some(&profile), &[], 1, 3);
        let denied = filter_child_tools(&all_tools, &[], Some(&profile), &[], 3, 3);

        assert!(allowed.iter().any(|tool| tool.name == "subagent_spawn"));
        assert!(!denied.iter().any(|tool| tool.name == "subagent_spawn"));
    }
}
