use crate::types::ToolOutput;
use serde_json::Value;
use tracing::debug;

pub fn reject_unknown_fields(
    tool_name: &str,
    arguments: &Value,
    allowed_fields: &[&str],
) -> Option<ToolOutput> {
    let Some(object) = arguments.as_object() else {
        debug!(
            tool = tool_name,
            "tool arguments rejected: expected JSON object"
        );
        return Some(ToolOutput::error(format!(
            "{tool_name} arguments must be a JSON object"
        )));
    };

    let mut unknown_fields = object
        .keys()
        .filter(|field| !allowed_fields.contains(&field.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    if unknown_fields.is_empty() {
        return None;
    }

    unknown_fields.sort_unstable();
    unknown_fields.dedup();
    debug!(tool = tool_name, unknown_fields = ?unknown_fields, "tool arguments rejected: unknown fields");

    let label = if unknown_fields.len() == 1 {
        "field"
    } else {
        "fields"
    };
    Some(ToolOutput::error(format!(
        "unknown {label} for {tool_name}: {}",
        unknown_fields.join(", ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_known_fields() {
        let value = serde_json::json!({"path": "hello.txt", "offset": 3});
        assert!(reject_unknown_fields("read_file", &value, &["path", "offset"]).is_none());
    }

    #[test]
    fn rejects_unknown_fields() {
        let value = serde_json::json!({"path": "hello.txt", "oops": true});
        let Some(error) = reject_unknown_fields("read_file", &value, &["path"]) else {
            panic!("expected unknown field error");
        };
        assert!(error.is_error);
        assert!(error.content.contains("unknown field"));
        assert!(error.content.contains("oops"));
    }

    #[test]
    fn rejects_non_object_arguments() {
        let value = serde_json::json!("not an object");
        let Some(error) = reject_unknown_fields("read_file", &value, &["path"]) else {
            panic!("expected JSON object error");
        };
        assert!(error.is_error);
        assert!(error.content.contains("JSON object"));
    }
}
