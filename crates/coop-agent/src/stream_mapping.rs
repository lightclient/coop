use futures::StreamExt;
use genai::chat::{ChatStreamEvent, StreamEnd};
use tracing::warn;

use coop_core::traits::ProviderStream;
use coop_core::types::Message;

use crate::message_mapping::map_response_message;
use crate::request_trace::summarize_transport_error;
use crate::usage_mapping::usage_from_stream_end;

pub(crate) fn into_provider_stream(chat_stream: genai::chat::ChatStreamResponse) -> ProviderStream {
    let stream = chat_stream.stream.filter_map(|item| async move {
        match item {
            Ok(ChatStreamEvent::Chunk(chunk)) => {
                let message = Message::assistant().with_text(chunk.content);
                Some(Ok((Some(message), None)))
            }
            Ok(ChatStreamEvent::End(end)) => Some(Ok(final_stream_item(&end))),
            Ok(
                ChatStreamEvent::Start
                | ChatStreamEvent::ToolCallChunk(_)
                | ChatStreamEvent::ReasoningChunk(_)
                | ChatStreamEvent::ThoughtSignatureChunk(_),
            ) => None,
            Err(error) => {
                let transport = summarize_transport_error(&error);
                warn!(
                    transport_error_variant = transport.variant,
                    transport_error_kind = transport.kind,
                    transport_http_status = transport.http_status,
                    transport_reqwest_is_connect = transport.reqwest_is_connect,
                    transport_reqwest_is_timeout = transport.reqwest_is_timeout,
                    transport_reqwest_is_request = transport.reqwest_is_request,
                    transport_reqwest_is_body = transport.reqwest_is_body,
                    transport_reqwest_is_decode = transport.reqwest_is_decode,
                    transport_url = %transport.url,
                    transport_source_chain = %transport.source_chain,
                    transport_response_body_excerpt = %transport.body_excerpt,
                    error = %error,
                    error_debug = ?error,
                    "provider stream item failed"
                );
                Some(Err(anyhow::Error::new(error)))
            }
        }
    });

    Box::pin(stream)
}

pub(crate) fn final_stream_item(
    end: &StreamEnd,
) -> (Option<Message>, Option<coop_core::types::Usage>) {
    let message = end
        .captured_content
        .as_ref()
        .map_or_else(Message::assistant, |content| {
            map_response_message(content, end.captured_reasoning_content.as_deref())
        });
    let usage = usage_from_stream_end(end);
    (Some(message), Some(usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{MessageContent, StopReason, Usage};

    #[test]
    fn final_stream_item_includes_text_and_usage() {
        let end = StreamEnd {
            captured_usage: Some(Usage {
                prompt_tokens: Some(12),
                prompt_tokens_details: None,
                completion_tokens: Some(4),
                completion_tokens_details: None,
                total_tokens: Some(16),
            }),
            captured_stop_reason: Some(StopReason::Completed("stop".into())),
            captured_content: Some(MessageContent::from_text("hello")),
            captured_reasoning_content: None,
            captured_response_id: None,
        };

        let (message, usage) = final_stream_item(&end);
        assert_eq!(message.expect("message").text(), "hello");
        assert_eq!(usage.expect("usage").input_tokens, Some(12));
    }
}
