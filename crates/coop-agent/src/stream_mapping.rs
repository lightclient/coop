use futures::StreamExt;
use genai::chat::{ChatStreamEvent, StreamEnd};

use coop_core::traits::ProviderStream;
use coop_core::types::Message;

use crate::message_mapping::map_response_message;
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
            Err(error) => Some(Err(anyhow::anyhow!(error.to_string()))),
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
        };

        let (message, usage) = final_stream_item(&end);
        assert_eq!(message.expect("message").text(), "hello");
        assert_eq!(usage.expect("usage").input_tokens, Some(12));
    }
}
