use chrono::{DateTime, Utc};
use coop_core::{InboundKind, InboundMessage};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::proto::data_message::Quote;
use presage::proto::{
    AttachmentPointer, EditMessage, Preview, ReceiptMessage, TypingMessage, receipt_message,
};
use tracing::{field, info, info_span};

enum ParseOutcome {
    Parsed(InboundMessage),
    UnsupportedBodyVariant,
    DroppedEmpty,
}

pub(super) fn inbound_from_content(content: &Content) -> Option<InboundMessage> {
    let sender = content.metadata.sender.raw_uuid().to_string();
    let timestamp = content.metadata.timestamp;
    let content_body = signal_content_body_name(&content.body);
    let span = info_span!(
        "signal_inbound_parse",
        signal.sender = %sender,
        signal.content_body = content_body,
        signal.timestamp = timestamp,
        signal.inbound_kind = field::Empty,
        signal.chat_id = field::Empty,
        signal.is_group = field::Empty,
        signal.message_timestamp = field::Empty,
        signal.raw_content = field::Empty,
    );

    let _guard = span.enter();

    let outcome = match &content.body {
        ContentBody::DataMessage(data_message) => {
            inbound_from_data_message(data_message, &sender, timestamp)
                .map_or(ParseOutcome::DroppedEmpty, ParseOutcome::Parsed)
        }
        ContentBody::EditMessage(edit_message) => {
            inbound_from_edit_message(edit_message, &sender, timestamp)
                .map_or(ParseOutcome::DroppedEmpty, ParseOutcome::Parsed)
        }
        ContentBody::TypingMessage(typing_message) => {
            let (chat_id, is_group, reply_to) =
                chat_context_from_typing_message(typing_message, &sender);
            ParseOutcome::Parsed(InboundMessage {
                channel: "signal".to_string(),
                sender,
                content: String::new(),
                chat_id,
                is_group,
                timestamp: from_epoch_millis(timestamp),
                reply_to,
                kind: InboundKind::Typing,
                message_timestamp: Some(timestamp),
            })
        }
        ContentBody::ReceiptMessage(receipt_message) => {
            let content_text = prepend_sender_context(
                &format_receipt_message(receipt_message),
                &sender,
                None,
                timestamp,
            );
            ParseOutcome::Parsed(InboundMessage {
                channel: "signal".to_string(),
                sender: sender.clone(),
                content: content_text,
                chat_id: None,
                is_group: false,
                timestamp: from_epoch_millis(timestamp),
                reply_to: Some(sender),
                kind: InboundKind::Receipt,
                message_timestamp: Some(timestamp),
            })
        }
        ContentBody::SynchronizeMessage(sync_message) => {
            inbound_from_sync_message(sync_message, &sender, timestamp)
                .map_or(ParseOutcome::DroppedEmpty, ParseOutcome::Parsed)
        }
        _ => ParseOutcome::UnsupportedBodyVariant,
    };

    match outcome {
        ParseOutcome::Parsed(inbound) => {
            span.record(
                "signal.inbound_kind",
                signal_inbound_kind_name(&inbound.kind),
            );
            if let Some(chat_id) = inbound.chat_id.as_deref() {
                span.record("signal.chat_id", chat_id);
            }
            span.record("signal.is_group", inbound.is_group);
            if let Some(message_timestamp) = inbound.message_timestamp {
                span.record("signal.message_timestamp", message_timestamp);
            }
            span.record("signal.raw_content", field::display(&inbound.content));

            info!(
                signal.inbound_kind = signal_inbound_kind_name(&inbound.kind),
                signal.chat_id = ?inbound.chat_id,
                signal.is_group = inbound.is_group,
                signal.message_timestamp = ?inbound.message_timestamp,
                signal.raw_content = %inbound.content,
                "signal inbound parsed and emitted"
            );

            Some(inbound)
        }
        ParseOutcome::UnsupportedBodyVariant => {
            info!("signal inbound unsupported body variant");
            None
        }
        ParseOutcome::DroppedEmpty => {
            info!("signal inbound dropped/empty");
            None
        }
    }
}

fn signal_content_body_name(content_body: &ContentBody) -> &'static str {
    match content_body {
        ContentBody::DataMessage(_) => "data_message",
        ContentBody::EditMessage(_) => "edit_message",
        ContentBody::TypingMessage(_) => "typing_message",
        ContentBody::ReceiptMessage(_) => "receipt_message",
        ContentBody::SynchronizeMessage(_) => "synchronize_message",
        _ => "unsupported",
    }
}

fn signal_inbound_kind_name(kind: &InboundKind) -> &'static str {
    match kind {
        InboundKind::Text => "text",
        InboundKind::Reaction => "reaction",
        InboundKind::Typing => "typing",
        InboundKind::Receipt => "receipt",
        InboundKind::Edit => "edit",
        InboundKind::Attachment => "attachment",
    }
}

fn inbound_from_sync_message(
    sync_message: &presage::proto::SyncMessage,
    sender: &str,
    timestamp: u64,
) -> Option<InboundMessage> {
    let sent = sync_message.sent.as_ref()?;

    if let Some(data_message) = sent.message.as_ref() {
        return inbound_from_data_message(data_message, sender, timestamp);
    }

    if let Some(edit_message) = sent.edit_message.as_ref() {
        return inbound_from_edit_message(edit_message, sender, timestamp);
    }

    None
}

fn inbound_from_data_message(
    data_message: &DataMessage,
    sender: &str,
    timestamp: u64,
) -> Option<InboundMessage> {
    let (kind, body) = format_data_message(data_message)?;
    let (chat_id, is_group, reply_to) = chat_context_from_data_message(data_message, sender);

    Some(InboundMessage {
        channel: "signal".to_string(),
        sender: sender.to_string(),
        content: prepend_sender_context(&body, sender, chat_id.as_deref(), timestamp),
        chat_id,
        is_group,
        timestamp: from_epoch_millis(timestamp),
        reply_to,
        kind,
        message_timestamp: Some(timestamp),
    })
}

fn inbound_from_edit_message(
    edit_message: &EditMessage,
    sender: &str,
    timestamp: u64,
) -> Option<InboundMessage> {
    let data_message = edit_message.data_message.as_ref()?;
    let (_original_kind, body) = format_data_message(data_message)?;

    let target_timestamp = edit_message
        .target_sent_timestamp
        .map_or_else(|| "unknown".to_string(), |value| value.to_string());
    let edited_body = format!("[edited message at {target_timestamp}]\n{body}");
    let (chat_id, is_group, reply_to) = chat_context_from_data_message(data_message, sender);

    Some(InboundMessage {
        channel: "signal".to_string(),
        sender: sender.to_string(),
        content: prepend_sender_context(&edited_body, sender, chat_id.as_deref(), timestamp),
        chat_id,
        is_group,
        timestamp: from_epoch_millis(timestamp),
        reply_to,
        kind: InboundKind::Edit,
        message_timestamp: Some(timestamp),
    })
}

fn format_data_message(data_message: &DataMessage) -> Option<(InboundKind, String)> {
    if let Some(reaction) = data_message.reaction.as_ref() {
        let emoji = reaction.emoji.as_deref().unwrap_or("❓");
        let target_timestamp = reaction
            .target_sent_timestamp
            .map_or_else(|| "unknown".to_string(), |value| value.to_string());

        let reaction_text = if reaction.remove.unwrap_or(false) {
            format!("[removed reaction {emoji} from message at {target_timestamp}]")
        } else {
            format!("[reacted {emoji} to message at {target_timestamp}]")
        };

        return Some((InboundKind::Reaction, reaction_text));
    }

    let body_text = data_message
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
        .map(ToOwned::to_owned);
    let has_text_body = body_text.is_some();

    let mut lines = Vec::new();

    if let Some(quote) = data_message.quote.as_ref() {
        lines.push(format_reply_context(quote));
    }

    if let Some(body) = body_text {
        lines.push(body);
    }

    for attachment in &data_message.attachments {
        lines.push(format_attachment_metadata(attachment));
    }

    for preview in &data_message.preview {
        lines.push(format_link_preview(preview));
    }

    if lines.is_empty() {
        return None;
    }

    let kind = if !data_message.attachments.is_empty() && !has_text_body {
        InboundKind::Attachment
    } else {
        InboundKind::Text
    };

    Some((kind, lines.join("\n")))
}

fn format_reply_context(quote: &Quote) -> String {
    let quoted_text = quote.text.as_deref().map_or_else(
        || "<quoted message>".to_string(),
        |text| text.replace('"', "\\\""),
    );
    let quote_timestamp = quote
        .id
        .map_or_else(|| "unknown".to_string(), |value| value.to_string());

    format!("[reply to \"{quoted_text}\" (at {quote_timestamp})]")
}

fn format_attachment_metadata(attachment: &AttachmentPointer) -> String {
    let file_name = attachment.file_name.as_deref().unwrap_or("unnamed");
    let content_type = attachment
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let size = attachment
        .size
        .map_or_else(|| "unknown".to_string(), |value| value.to_string());

    format!("[attachment: {file_name} ({content_type}, {size} bytes)]")
}

fn format_link_preview(preview: &Preview) -> String {
    let url = preview.url.as_deref().unwrap_or("unknown");
    let title = preview.title.as_deref().unwrap_or("untitled");

    format!("[link: {url} — \"{title}\"]")
}

fn format_receipt_message(receipt_message: &ReceiptMessage) -> String {
    let receipt_kind = receipt_message
        .r#type
        .and_then(|value| receipt_message::Type::try_from(value).ok())
        .map_or("unknown", |kind| match kind {
            receipt_message::Type::Delivery => "delivery",
            receipt_message::Type::Read => "read",
            receipt_message::Type::Viewed => "viewed",
        });

    if receipt_message.timestamp.is_empty() {
        format!("[receipt: {receipt_kind}]")
    } else {
        let joined = receipt_message
            .timestamp
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("[receipt: {receipt_kind} for messages at {joined}]")
    }
}

fn chat_context_from_data_message(
    data_message: &DataMessage,
    sender: &str,
) -> (Option<String>, bool, Option<String>) {
    if let Some(master_key) = data_message
        .group_v2
        .as_ref()
        .and_then(|group| group.master_key.as_ref())
    {
        let chat_id = format!("group:{}", hex::encode(master_key));
        let reply_to = Some(chat_id.clone());
        (Some(chat_id), true, reply_to)
    } else {
        (None, false, Some(sender.to_string()))
    }
}

fn chat_context_from_typing_message(
    typing_message: &TypingMessage,
    sender: &str,
) -> (Option<String>, bool, Option<String>) {
    if let Some(group_id) = typing_message.group_id.as_ref() {
        let chat_id = format!("group:{}", hex::encode(group_id));
        let reply_to = Some(chat_id.clone());
        (Some(chat_id), true, reply_to)
    } else {
        (None, false, Some(sender.to_string()))
    }
}

fn prepend_sender_context(
    body: &str,
    sender: &str,
    chat_id: Option<&str>,
    timestamp: u64,
) -> String {
    let header = match chat_id {
        Some(chat_id) => format!("[from {sender} in {chat_id} at {timestamp}]"),
        None => format!("[from {sender} at {timestamp}]"),
    };

    if body.is_empty() {
        header
    } else {
        format!("{header}\n{body}")
    }
}

fn from_epoch_millis(timestamp: u64) -> DateTime<Utc> {
    i64::try_from(timestamp)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::ServiceId;
    use presage::proto::data_message::Reaction;
    use presage::proto::{
        DataMessage, GroupContextV2, Preview, SyncMessage, TypingMessage, receipt_message,
        sync_message, typing_message,
    };

    fn test_sender() -> String {
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string()
    }

    fn test_content(body: ContentBody, timestamp: u64) -> Content {
        let sender_uuid = Uuid::parse_str(&test_sender()).unwrap();
        let destination_uuid = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();

        Content {
            metadata: Metadata {
                sender: ServiceId::Aci(sender_uuid.into()),
                destination: ServiceId::Aci(destination_uuid.into()),
                sender_device: 1_u32.try_into().unwrap(),
                timestamp,
                needs_receipt: false,
                unidentified_sender: false,
                was_plaintext: false,
                server_guid: None,
            },
            body,
        }
    }

    #[test]
    fn inbound_formats_reaction_messages() {
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                reaction: Some(Reaction {
                    emoji: Some("😀".to_string()),
                    target_sent_timestamp: Some(55),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            1000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Reaction);
        assert_eq!(inbound.message_timestamp, Some(1000));
        assert!(inbound.content.contains("[reacted 😀 to message at 55]"));
        assert!(
            inbound
                .content
                .contains("[from aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa at 1000]")
        );
    }

    #[test]
    fn inbound_formats_quotes_and_previews() {
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                body: Some("reply text".to_string()),
                quote: Some(Quote {
                    id: Some(33),
                    text: Some("original".to_string()),
                    ..Default::default()
                }),
                preview: vec![Preview {
                    url: Some("https://example.com".to_string()),
                    title: Some("Example".to_string()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            2000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Text);
        assert!(inbound.content.contains("[reply to \"original\" (at 33)]"));
        assert!(inbound.content.contains("reply text"));
        assert!(
            inbound
                .content
                .contains("[link: https://example.com — \"Example\"]")
        );
    }

    #[test]
    fn inbound_formats_attachment_metadata() {
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                attachments: vec![AttachmentPointer {
                    file_name: Some("image.jpg".to_string()),
                    content_type: Some("image/jpeg".to_string()),
                    size: Some(321),
                    ..Default::default()
                }],
                group_v2: Some(GroupContextV2 {
                    master_key: Some(vec![0x11; 32]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            3000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Attachment);
        assert!(inbound.is_group);
        assert!(
            inbound
                .content
                .contains("[attachment: image.jpg (image/jpeg, 321 bytes)]")
        );
        assert!(inbound.content.contains("in group:"));
    }

    #[test]
    fn inbound_formats_edit_messages() {
        let content = test_content(
            ContentBody::EditMessage(EditMessage {
                target_sent_timestamp: Some(77),
                data_message: Some(DataMessage {
                    body: Some("updated".to_string()),
                    ..Default::default()
                }),
            }),
            4000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Edit);
        assert!(inbound.content.contains("[edited message at 77]"));
        assert!(inbound.content.contains("updated"));
    }

    #[test]
    fn inbound_formats_typing_messages() {
        let content = test_content(
            ContentBody::TypingMessage(TypingMessage {
                action: Some(typing_message::Action::Started.into()),
                ..Default::default()
            }),
            5000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Typing);
        assert_eq!(inbound.content, "");
    }

    #[test]
    fn inbound_formats_receipt_messages() {
        let content = test_content(
            ContentBody::ReceiptMessage(ReceiptMessage {
                r#type: Some(receipt_message::Type::Read.into()),
                timestamp: vec![11, 12],
            }),
            6000,
        );

        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Receipt);
        assert!(
            inbound
                .content
                .contains("[receipt: read for messages at 11, 12]")
        );
    }

    #[test]
    fn inbound_formats_synchronized_sent_messages() {
        let sync = SyncMessage {
            sent: Some(sync_message::Sent {
                message: Some(DataMessage {
                    body: Some("sync body".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let content = test_content(ContentBody::SynchronizeMessage(sync), 7000);
        let inbound = inbound_from_content(&content).unwrap();

        assert_eq!(inbound.kind, InboundKind::Text);
        assert!(inbound.content.contains("sync body"));
    }
}
