use coop_core::types::Usage;
use genai::chat::{ChatResponse, StreamEnd};

pub(crate) fn usage_from_response(response: &ChatResponse) -> Usage {
    usage_from_genai(
        &response.usage,
        response
            .stop_reason
            .as_ref()
            .map(genai::chat::StopReason::raw),
    )
}

pub(crate) fn usage_from_stream_end(end: &StreamEnd) -> Usage {
    let usage = end
        .captured_usage
        .as_ref()
        .map_or_else(Usage::default, |usage| {
            usage_from_genai(
                usage,
                end.captured_stop_reason
                    .as_ref()
                    .map(genai::chat::StopReason::raw),
            )
        });

    if usage.stop_reason.is_some() {
        usage
    } else {
        Usage {
            stop_reason: end
                .captured_stop_reason
                .as_ref()
                .map(|reason| reason.raw().to_owned()),
            ..usage
        }
    }
}

fn usage_from_genai(usage: &genai::chat::Usage, stop_reason: Option<&str>) -> Usage {
    let cache_write_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cache_creation_tokens)
        .and_then(to_u32);
    let cache_read_tokens = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .and_then(to_u32);

    let input_tokens = usage
        .prompt_tokens
        .map(|value| {
            let mut effective = value;
            if let Some(cache_tokens) = cache_write_tokens {
                effective -= i32::try_from(cache_tokens).unwrap_or(i32::MAX);
            }
            if let Some(cache_tokens) = cache_read_tokens {
                effective -= i32::try_from(cache_tokens).unwrap_or(i32::MAX);
            }
            effective
        })
        .and_then(to_u32);

    Usage {
        input_tokens,
        output_tokens: usage.completion_tokens.and_then(to_u32),
        cache_read_tokens,
        cache_write_tokens,
        stop_reason: stop_reason.map(str::to_owned),
    }
}

fn to_u32(value: i32) -> Option<u32> {
    u32::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{PromptTokensDetails, StopReason};

    #[test]
    fn usage_mapping_splits_cache_tokens_out_of_prompt_total() {
        let response = ChatResponse {
            content: genai::chat::MessageContent::from_text("hi"),
            reasoning_content: None,
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "claude"),
            provider_model_iden: genai::ModelIden::new(
                genai::adapter::AdapterKind::Anthropic,
                "claude",
            ),
            stop_reason: Some(StopReason::Completed("end_turn".into())),
            usage: genai::chat::Usage {
                prompt_tokens: Some(150),
                prompt_tokens_details: Some(PromptTokensDetails {
                    cache_creation_tokens: Some(20),
                    cached_tokens: Some(30),
                    cache_creation_details: None,
                    audio_tokens: None,
                }),
                completion_tokens: Some(40),
                completion_tokens_details: None,
                total_tokens: Some(190),
            },
            captured_raw_body: None,
        };

        let usage = usage_from_response(&response);
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.cache_write_tokens, Some(20));
        assert_eq!(usage.cache_read_tokens, Some(30));
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn stream_end_stop_reason_is_preserved_without_usage() {
        let end = StreamEnd {
            captured_usage: None,
            captured_stop_reason: Some(StopReason::ToolCall("tool_calls".into())),
            captured_content: None,
            captured_reasoning_content: None,
        };

        let usage = usage_from_stream_end(&end);
        assert_eq!(usage.stop_reason.as_deref(), Some("tool_calls"));
    }
}
