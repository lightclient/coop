use genai::chat::{ChatRequest, ChatRole, ContentPart};
use std::error::Error as StdError;

use crate::provider_spec::{ProviderKind, ProviderSpec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequestTrace {
    pub system_count: usize,
    pub user_count: usize,
    pub assistant_count: usize,
    pub tool_count: usize,
    pub text_part_count: usize,
    pub binary_part_count: usize,
    pub tool_call_part_count: usize,
    pub tool_response_part_count: usize,
    pub reasoning_part_count: usize,
    pub thought_signature_part_count: usize,
    pub assistant_reasoning_message_count: usize,
    pub json_bytes: usize,
    pub json_hash: String,
    pub tool_names: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderTrace {
    pub base_url: String,
    pub auth_mode: &'static str,
    pub extra_header_count: usize,
    pub extra_header_names: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct TransportErrorTrace {
    pub variant: &'static str,
    pub kind: &'static str,
    pub http_status: Option<u16>,
    pub reqwest_is_connect: bool,
    pub reqwest_is_timeout: bool,
    pub reqwest_is_request: bool,
    pub reqwest_is_body: bool,
    pub reqwest_is_decode: bool,
    pub url: String,
    pub source_chain: String,
    pub body_excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct TransportProbeTrace {
    pub target: &'static str,
    pub kind: &'static str,
    pub http_status: Option<u16>,
    pub reqwest_is_connect: bool,
    pub reqwest_is_timeout: bool,
    pub reqwest_is_request: bool,
    pub reqwest_is_body: bool,
    pub reqwest_is_decode: bool,
    pub url: String,
    pub source_chain: String,
    pub body_excerpt: String,
}

pub(crate) fn summarize_chat_request(chat_request: &ChatRequest) -> RequestTrace {
    let mut system_count = usize::from(chat_request.system.is_some());
    let mut user_count = 0;
    let mut assistant_count = 0;
    let mut tool_count = 0;
    let mut text_part_count = 0;
    let mut binary_part_count = 0;
    let mut tool_call_part_count = 0;
    let mut tool_response_part_count = 0;
    let mut reasoning_part_count = 0;
    let mut thought_signature_part_count = 0;
    let mut assistant_reasoning_message_count = 0;

    for message in &chat_request.messages {
        match message.role {
            ChatRole::System => system_count += 1,
            ChatRole::User => user_count += 1,
            ChatRole::Assistant => assistant_count += 1,
            ChatRole::Tool => tool_count += 1,
        }

        let mut assistant_has_reasoning = false;
        for part in message.content.parts() {
            match part {
                ContentPart::Text(_) => text_part_count += 1,
                ContentPart::Binary(_) => binary_part_count += 1,
                ContentPart::ToolCall(_) => tool_call_part_count += 1,
                ContentPart::ToolResponse(_) => tool_response_part_count += 1,
                ContentPart::ReasoningContent(_) => {
                    reasoning_part_count += 1;
                    assistant_has_reasoning = true;
                }
                ContentPart::ThoughtSignature(_) => thought_signature_part_count += 1,
                ContentPart::Custom(_) => {}
            }
        }

        if message.role == ChatRole::Assistant && assistant_has_reasoning {
            assistant_reasoning_message_count += 1;
        }
    }

    let (json_bytes, json_hash) = serde_json::to_vec(chat_request).map_or_else(
        |_| (0, String::new()),
        |bytes| {
            let len = bytes.len();
            (len, stable_hash_hex(&bytes))
        },
    );

    let tool_names = chat_request
        .tools
        .as_ref()
        .map_or_else(String::new, |tools| {
            tools
                .iter()
                .map(|tool| tool.name.to_string())
                .collect::<Vec<_>>()
                .join(",")
        });

    RequestTrace {
        system_count,
        user_count,
        assistant_count,
        tool_count,
        text_part_count,
        binary_part_count,
        tool_call_part_count,
        tool_response_part_count,
        reasoning_part_count,
        thought_signature_part_count,
        assistant_reasoning_message_count,
        json_bytes,
        json_hash,
        tool_names,
    }
}

pub(crate) fn summarize_provider_trace(
    kind: ProviderKind,
    spec: &ProviderSpec,
    has_keys: bool,
) -> ProviderTrace {
    let auth_mode = if has_keys {
        "api-key"
    } else if kind == ProviderKind::OpenAiCompatible {
        "empty-bearer"
    } else {
        "none"
    };

    ProviderTrace {
        base_url: spec.normalized_base_url().unwrap_or_default(),
        auth_mode,
        extra_header_count: spec.extra_headers.len(),
        extra_header_names: spec
            .extra_headers
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
    }
}

pub(crate) fn summarize_transport_error(error: &genai::Error) -> TransportErrorTrace {
    match error {
        genai::Error::WebModelCall { webc_error, .. } => {
            summarize_webc_error("web_model_call", webc_error)
        }
        genai::Error::WebStream { error, .. } => summarize_box_error("web_stream", error.as_ref()),
        genai::Error::HttpError { status, body, .. } => TransportErrorTrace {
            variant: "http_error",
            kind: "http_status",
            http_status: Some(status.as_u16()),
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: truncate(body),
        },
        _ => TransportErrorTrace {
            variant: "other",
            kind: "other",
            http_status: None,
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: String::new(),
        },
    }
}

pub(crate) fn summarize_transport_probe_response(
    target: &'static str,
    url: &str,
    status: u16,
    body: &str,
) -> TransportProbeTrace {
    TransportProbeTrace {
        target,
        kind: "http_status",
        http_status: Some(status),
        reqwest_is_connect: false,
        reqwest_is_timeout: false,
        reqwest_is_request: false,
        reqwest_is_body: false,
        reqwest_is_decode: false,
        url: url.to_owned(),
        source_chain: String::new(),
        body_excerpt: truncate(body),
    }
}

pub(crate) fn summarize_transport_probe_reqwest_error(
    target: &'static str,
    url: &str,
    error: &reqwest::Error,
) -> TransportProbeTrace {
    TransportProbeTrace {
        target,
        kind: "reqwest",
        http_status: error.status().map(|status| status.as_u16()),
        reqwest_is_connect: error.is_connect(),
        reqwest_is_timeout: error.is_timeout(),
        reqwest_is_request: error.is_request(),
        reqwest_is_body: error.is_body(),
        reqwest_is_decode: error.is_decode(),
        url: url.to_owned(),
        source_chain: error_source_chain(error),
        body_excerpt: String::new(),
    }
}

fn summarize_webc_error(variant: &'static str, error: &genai::webc::Error) -> TransportErrorTrace {
    match error {
        genai::webc::Error::Reqwest(reqwest_error) => TransportErrorTrace {
            variant,
            kind: "reqwest",
            http_status: reqwest_error.status().map(|status| status.as_u16()),
            reqwest_is_connect: reqwest_error.is_connect(),
            reqwest_is_timeout: reqwest_error.is_timeout(),
            reqwest_is_request: reqwest_error.is_request(),
            reqwest_is_body: reqwest_error.is_body(),
            reqwest_is_decode: reqwest_error.is_decode(),
            url: reqwest_error
                .url()
                .map_or_else(String::new, ToString::to_string),
            source_chain: error_source_chain(reqwest_error),
            body_excerpt: String::new(),
        },
        genai::webc::Error::ResponseFailedStatus { status, body, .. } => TransportErrorTrace {
            variant,
            kind: "http_status",
            http_status: Some(status.as_u16()),
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: truncate(body),
        },
        genai::webc::Error::ResponseFailedNotJson { body, .. } => TransportErrorTrace {
            variant,
            kind: "response_not_json",
            http_status: None,
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: truncate(body),
        },
        genai::webc::Error::ResponseFailedInvalidJson { body, .. } => TransportErrorTrace {
            variant,
            kind: "response_invalid_json",
            http_status: None,
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: truncate(body),
        },
        genai::webc::Error::JsonValueExt(_) => TransportErrorTrace {
            variant,
            kind: "other",
            http_status: None,
            reqwest_is_connect: false,
            reqwest_is_timeout: false,
            reqwest_is_request: false,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: String::new(),
            source_chain: error.to_string(),
            body_excerpt: String::new(),
        },
    }
}

fn summarize_box_error(
    variant: &'static str,
    error: &(dyn StdError + Send + Sync + 'static),
) -> TransportErrorTrace {
    if let Some(genai_error) = error.downcast_ref::<genai::Error>() {
        let mut summary = summarize_transport_error(genai_error);
        summary.variant = variant;
        return summary;
    }

    let source_chain = error_source_chain(error);
    let source_chain_lower = source_chain.to_ascii_lowercase();

    TransportErrorTrace {
        variant,
        kind: "boxed",
        http_status: None,
        reqwest_is_connect: source_chain_lower.contains("connect")
            || source_chain_lower.contains("connection refused"),
        reqwest_is_timeout: source_chain_lower.contains("timed out"),
        reqwest_is_request: source_chain_lower.contains("error sending request")
            || source_chain_lower.contains("request"),
        reqwest_is_body: source_chain_lower.contains("body"),
        reqwest_is_decode: source_chain_lower.contains("decode"),
        url: extract_url(&source_chain).unwrap_or_default(),
        source_chain,
        body_excerpt: String::new(),
    }
}

fn error_source_chain(error: &(dyn StdError + 'static)) -> String {
    let mut chain = vec![error.to_string()];
    let mut current = error.source();

    while let Some(source) = current {
        chain.push(source.to_string());
        current = source.source();
    }

    truncate(&chain.join(" -> "))
}

fn truncate(value: &str) -> String {
    const MAX_LEN: usize = 400;
    if value.len() <= MAX_LEN {
        value.to_owned()
    } else {
        format!("{}…", &value[..MAX_LEN])
    }
}

fn extract_url(value: &str) -> Option<String> {
    let start = value.find("http://").or_else(|| value.find("https://"))?;
    let suffix = &value[start..];
    let end = suffix.find([')', ' ', '\n']).unwrap_or(suffix.len());
    Some(suffix[..end].to_owned())
}

fn stable_hash_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{ChatMessage, MessageContent, Tool, ToolCall, ToolResponse};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[test]
    fn summarize_chat_request_counts_parts() {
        let request = ChatRequest::default()
            .append_message(ChatMessage::system("system"))
            .append_message(ChatMessage::user("hello"))
            .append_message(ChatMessage::assistant(MessageContent::from_parts(vec![
                ContentPart::ReasoningContent("thinking".into()),
                ContentPart::Text("final".into()),
                ContentPart::ToolCall(ToolCall {
                    call_id: "call_1".into(),
                    fn_name: "bash".into(),
                    fn_arguments: json!({"command": "pwd"}),
                    thought_signatures: None,
                }),
            ])))
            .append_message(ChatMessage::from(ToolResponse::new("call_1", "ok")))
            .with_tools([Tool::new("bash").with_description("run bash")]);

        let trace = summarize_chat_request(&request);
        assert_eq!(trace.system_count, 1);
        assert_eq!(trace.user_count, 1);
        assert_eq!(trace.assistant_count, 1);
        assert_eq!(trace.tool_count, 1);
        assert_eq!(trace.text_part_count, 3);
        assert_eq!(trace.tool_call_part_count, 1);
        assert_eq!(trace.tool_response_part_count, 1);
        assert_eq!(trace.reasoning_part_count, 1);
        assert_eq!(trace.assistant_reasoning_message_count, 1);
        assert!(!trace.json_hash.is_empty());
        assert_eq!(trace.tool_names, "bash");
    }

    #[test]
    fn summarize_provider_trace_marks_empty_bearer() {
        let spec = ProviderSpec {
            kind: ProviderKind::OpenAiCompatible,
            model: "demo-model".into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            extra_headers: BTreeMap::from([("x-test".into(), "1".into())]),
            refresh_token: None,
            reasoning: None,
        };

        let trace = summarize_provider_trace(ProviderKind::OpenAiCompatible, &spec, false);
        assert_eq!(trace.auth_mode, "empty-bearer");
        assert_eq!(trace.base_url, "http://127.0.0.1:11434/v1/");
        assert_eq!(trace.extra_header_count, 1);
        assert_eq!(trace.extra_header_names, "x-test");
    }

    #[test]
    fn summarize_transport_error_reads_status_body() {
        let error = genai::Error::WebModelCall {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "demo-model"),
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: reqwest::StatusCode::BAD_REQUEST,
                body: "bad request body".into(),
                headers: Box::new(reqwest::header::HeaderMap::new()),
            },
        };

        let trace = summarize_transport_error(&error);
        assert_eq!(trace.variant, "web_model_call");
        assert_eq!(trace.kind, "http_status");
        assert_eq!(trace.http_status, Some(400));
        assert!(trace.body_excerpt.contains("bad request body"));
    }

    #[test]
    fn summarize_transport_probe_response_reads_status_body() {
        let trace = summarize_transport_probe_response(
            "models",
            "http://127.0.0.1:11434/v1/models",
            200,
            r#"{"object":"list"}"#,
        );

        assert_eq!(trace.target, "models");
        assert_eq!(trace.kind, "http_status");
        assert_eq!(trace.http_status, Some(200));
        assert!(trace.body_excerpt.contains("object"));
    }
}
