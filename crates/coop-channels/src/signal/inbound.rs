use chrono::{DateTime, Utc};
use coop_core::{InboundKind, InboundMessage};
use presage::libsignal_service::content::{Content, ContentBody, DataMessage};
use presage::proto::data_message::Quote;
use presage::proto::{
    AttachmentPointer, BodyRange, EditMessage, Preview, ReceiptMessage, TypingMessage, body_range,
    receipt_message,
};
use tracing::{debug, field, info_span};

use super::name_resolver::SignalNameResolver;

/// Parse a Signal Content into an InboundMessage without tracing.
/// Used by the history query path to avoid trace noise.
pub(crate) fn parse_content(
    content: &Content,
    resolver: Option<&SignalNameResolver>,
) -> Option<InboundMessage> {
    let sender = content.metadata.sender.raw_uuid().to_string();
    let timestamp = content.metadata.timestamp;

    match &content.body {
        ContentBody::DataMessage(data_message) => {
            inbound_from_data_message(data_message, &sender, timestamp, resolver)
        }
        ContentBody::EditMessage(edit_message) => {
            inbound_from_edit_message(edit_message, &sender, timestamp, resolver)
        }
        ContentBody::TypingMessage(typing_message) => {
            let (chat_id, is_group, reply_to) =
                chat_context_from_typing_message(typing_message, &sender);
            Some(InboundMessage {
                channel: "signal".to_owned(),
                sender,
                content: String::new(),
                chat_id,
                is_group,
                timestamp: from_epoch_millis(timestamp),
                reply_to,
                kind: InboundKind::Typing,
                message_timestamp: Some(timestamp),
                group_revision: None,
            })
        }
        ContentBody::ReceiptMessage(receipt_message) => {
            let content_text = prepend_sender_context(
                &format_receipt_message(receipt_message),
                &sender,
                None,
                timestamp,
                resolver,
            );
            Some(InboundMessage {
                channel: "signal".to_owned(),
                sender: sender.clone(),
                content: content_text,
                chat_id: None,
                is_group: false,
                timestamp: from_epoch_millis(timestamp),
                reply_to: Some(sender),
                kind: InboundKind::Receipt,
                message_timestamp: Some(timestamp),
                group_revision: None,
            })
        }
        ContentBody::SynchronizeMessage(sync_message) => {
            inbound_from_sync_message(sync_message, &sender, timestamp, resolver)
        }
        _ => None,
    }
}

pub(super) fn inbound_from_content(
    content: &Content,
    resolver: Option<&SignalNameResolver>,
) -> Option<InboundMessage> {
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

    let inbound = parse_content(content, resolver);

    if let Some(inbound) = inbound {
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

        debug!(
            signal.inbound_kind = signal_inbound_kind_name(&inbound.kind),
            signal.chat_id = ?inbound.chat_id,
            signal.is_group = inbound.is_group,
            signal.message_timestamp = ?inbound.message_timestamp,
            signal.raw_content = %inbound.content,
            "signal inbound parsed and emitted"
        );

        Some(inbound)
    } else {
        let body_name = signal_content_body_name(&content.body);
        if matches!(
            body_name,
            "data_message" | "edit_message" | "synchronize_message"
        ) {
            debug!("signal inbound dropped/empty");
        } else {
            debug!("signal inbound unsupported body variant");
        }
        None
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
        InboundKind::Command => "command",
    }
}

fn inbound_from_sync_message(
    sync_message: &presage::proto::SyncMessage,
    sender: &str,
    timestamp: u64,
    resolver: Option<&SignalNameResolver>,
) -> Option<InboundMessage> {
    let sent = sync_message.sent.as_ref()?;

    if let Some(data_message) = sent.message.as_ref() {
        return inbound_from_data_message(data_message, sender, timestamp, resolver);
    }

    if let Some(edit_message) = sent.edit_message.as_ref() {
        return inbound_from_edit_message(edit_message, sender, timestamp, resolver);
    }

    None
}

fn inbound_from_data_message(
    data_message: &DataMessage,
    sender: &str,
    timestamp: u64,
    resolver: Option<&SignalNameResolver>,
) -> Option<InboundMessage> {
    let (kind, body) = format_data_message(data_message, resolver)?;
    let (chat_id, is_group, reply_to) = chat_context_from_data_message(data_message, sender);

    // Detect slash commands: raw body starting with `/` becomes a Command
    // with the unwrapped text so the router can match it directly.
    if kind == InboundKind::Text && body.starts_with('/') {
        return Some(InboundMessage {
            channel: "signal".to_owned(),
            sender: sender.to_owned(),
            content: body,
            chat_id,
            is_group,
            timestamp: from_epoch_millis(timestamp),
            reply_to,
            kind: InboundKind::Command,
            message_timestamp: Some(timestamp),
            group_revision: None,
        });
    }

    Some(InboundMessage {
        channel: "signal".to_owned(),
        sender: sender.to_owned(),
        content: prepend_sender_context(&body, sender, chat_id.as_deref(), timestamp, resolver),
        chat_id,
        is_group,
        timestamp: from_epoch_millis(timestamp),
        reply_to,
        kind,
        message_timestamp: Some(timestamp),
        group_revision: None,
    })
}

fn inbound_from_edit_message(
    edit_message: &EditMessage,
    sender: &str,
    timestamp: u64,
    resolver: Option<&SignalNameResolver>,
) -> Option<InboundMessage> {
    let data_message = edit_message.data_message.as_ref()?;
    let (_original_kind, body) = format_data_message(data_message, resolver)?;

    let target_timestamp = edit_message
        .target_sent_timestamp
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());
    let edited_body = format!("[edited message at {target_timestamp}]\n{body}");
    let (chat_id, is_group, reply_to) = chat_context_from_data_message(data_message, sender);

    Some(InboundMessage {
        channel: "signal".to_owned(),
        sender: sender.to_owned(),
        content: prepend_sender_context(
            &edited_body,
            sender,
            chat_id.as_deref(),
            timestamp,
            resolver,
        ),
        chat_id,
        is_group,
        timestamp: from_epoch_millis(timestamp),
        reply_to,
        kind: InboundKind::Edit,
        message_timestamp: Some(timestamp),
        group_revision: None,
    })
}

fn format_data_message(
    data_message: &DataMessage,
    resolver: Option<&SignalNameResolver>,
) -> Option<(InboundKind, String)> {
    if let Some(reaction) = data_message.reaction.as_ref() {
        let emoji = reaction.emoji.as_deref().unwrap_or("❓");
        let target_timestamp = reaction
            .target_sent_timestamp
            .map_or_else(|| "unknown".to_owned(), |value| value.to_string());

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
        lines.push(format_reply_context(quote, resolver));
    }

    if let Some(mut body) = body_text {
        if let Some(resolver) = resolver {
            body = resolve_mentions(&body, &data_message.body_ranges, resolver);
        }
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

fn format_reply_context(quote: &Quote, resolver: Option<&SignalNameResolver>) -> String {
    let mut quoted_text = quote.text.as_deref().map_or_else(
        || "<quoted message>".to_owned(),
        |text| text.replace('"', "\\\""),
    );

    // Resolve mentions in quoted text if body_ranges are available.
    if let Some(resolver) = resolver {
        if quote.body_ranges.is_empty() {
            // Strip leftover U+FFFC placeholders when no ranges are available.
            quoted_text = quoted_text.replace('\u{FFFC}', "");
        } else {
            quoted_text = resolve_mentions(&quoted_text, &quote.body_ranges, resolver);
        }
    }

    let quote_timestamp = quote
        .id
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());

    // Resolve quote author if available.
    let author = quote
        .author_aci
        .as_deref()
        .and_then(|aci| resolver.map(|r| r.display_name(aci)));

    match author {
        Some(name) => format!("[reply to {name}: \"{quoted_text}\" (at {quote_timestamp})]"),
        None => format!("[reply to \"{quoted_text}\" (at {quote_timestamp})]"),
    }
}

pub(super) fn format_attachment_metadata(attachment: &AttachmentPointer) -> String {
    let file_name = attachment.file_name.as_deref().unwrap_or("unnamed");
    let content_type = attachment
        .content_type
        .as_deref()
        .unwrap_or("application/octet-stream");
    let size = attachment
        .size
        .map_or_else(|| "unknown".to_owned(), |value| value.to_string());

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
        (None, false, Some(sender.to_owned()))
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
        (None, false, Some(sender.to_owned()))
    }
}

fn prepend_sender_context(
    body: &str,
    sender: &str,
    chat_id: Option<&str>,
    timestamp: u64,
    resolver: Option<&SignalNameResolver>,
) -> String {
    let sender_display = resolver.map_or_else(|| sender.to_owned(), |r| r.sender_header(sender));
    let header = match chat_id {
        Some(chat_id) => format!("[from {sender_display} in {chat_id} at {timestamp}]"),
        None => format!("[from {sender_display} at {timestamp}]"),
    };

    if body.is_empty() {
        header
    } else {
        format!("{header}\n{body}")
    }
}

/// Replace U+FFFC mention placeholders with resolved `@name` text.
///
/// Signal encodes mentions as U+FFFC characters in the body with the
/// actual user UUID stored in `DataMessage.body_ranges`. Offsets in
/// body_ranges are in UTF-16 code units.
fn resolve_mentions(
    body: &str,
    body_ranges: &[BodyRange],
    resolver: &SignalNameResolver,
) -> String {
    let mut mentions: Vec<(u32, u32, &str)> = body_ranges
        .iter()
        .filter_map(|range| {
            let start = range.start?;
            let length = range.length?;
            if let Some(body_range::AssociatedValue::MentionAci(aci)) = &range.associated_value {
                Some((start, length, aci.as_str()))
            } else {
                None
            }
        })
        .collect();

    if mentions.is_empty() {
        return body.to_owned();
    }

    mentions.sort_by_key(|(start, _, _)| *start);

    let utf16: Vec<u16> = body.encode_utf16().collect();
    let mut result = String::new();
    let mut pos: usize = 0;

    for (start, length, aci) in &mentions {
        let start = *start as usize;
        let length = *length as usize;

        if start > pos {
            result.push_str(&String::from_utf16_lossy(&utf16[pos..start]));
        }

        result.push_str(&resolver.mention_text(aci));
        pos = start + length;
    }

    if pos < utf16.len() {
        result.push_str(&String::from_utf16_lossy(&utf16[pos..]));
    }

    result
}

fn from_epoch_millis(timestamp: u64) -> DateTime<Utc> {
    i64::try_from(timestamp)
        .ok()
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .unwrap_or_else(Utc::now)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use presage::libsignal_service::content::Metadata;
    use presage::libsignal_service::prelude::Uuid;
    use presage::libsignal_service::protocol::ServiceId;
    use presage::proto::data_message::Reaction;
    use presage::proto::{
        BodyRange, DataMessage, GroupContextV2, Preview, SyncMessage, TypingMessage, body_range,
        receipt_message, sync_message, typing_message,
    };

    fn test_sender() -> String {
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned()
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
                    emoji: Some("😀".to_owned()),
                    target_sent_timestamp: Some(55),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            1000,
        );

        let inbound = inbound_from_content(&content, None).unwrap();

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
                body: Some("reply text".to_owned()),
                quote: Some(Quote {
                    id: Some(33),
                    text: Some("original".to_owned()),
                    ..Default::default()
                }),
                preview: vec![Preview {
                    url: Some("https://example.com".to_owned()),
                    title: Some("Example".to_owned()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            2000,
        );

        let inbound = inbound_from_content(&content, None).unwrap();

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
                    file_name: Some("image.jpg".to_owned()),
                    content_type: Some("image/jpeg".to_owned()),
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

        let inbound = inbound_from_content(&content, None).unwrap();

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
                    body: Some("updated".to_owned()),
                    ..Default::default()
                }),
            }),
            4000,
        );

        let inbound = inbound_from_content(&content, None).unwrap();

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

        let inbound = inbound_from_content(&content, None).unwrap();

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

        let inbound = inbound_from_content(&content, None).unwrap();

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
                    body: Some("sync body".to_owned()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let content = test_content(ContentBody::SynchronizeMessage(sync), 7000);
        let inbound = inbound_from_content(&content, None).unwrap();

        assert_eq!(inbound.kind, InboundKind::Text);
        assert!(inbound.content.contains("sync body"));
    }

    #[test]
    fn inbound_formats_synchronized_edit_messages() {
        let sync = SyncMessage {
            sent: Some(sync_message::Sent {
                edit_message: Some(EditMessage {
                    target_sent_timestamp: Some(88),
                    data_message: Some(DataMessage {
                        body: Some("sync updated".to_owned()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let content = test_content(ContentBody::SynchronizeMessage(sync), 8000);
        let inbound = inbound_from_content(&content, None).unwrap();

        assert_eq!(inbound.kind, InboundKind::Edit);
        assert!(inbound.content.contains("[edited message at 88]"));
        assert!(inbound.content.contains("sync updated"));
    }

    #[test]
    fn slash_command_detected_as_command_kind() {
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                body: Some("/status".to_owned()),
                ..Default::default()
            }),
            9000,
        );

        let inbound = inbound_from_content(&content, None).unwrap();

        assert_eq!(inbound.kind, InboundKind::Command);
        assert_eq!(inbound.content, "/status");
        assert!(
            !inbound.content.contains("[from"),
            "command content should not have sender prefix"
        );
    }

    #[test]
    fn regular_text_not_detected_as_command() {
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                body: Some("hello there".to_owned()),
                ..Default::default()
            }),
            9001,
        );

        let inbound = inbound_from_content(&content, None).unwrap();

        assert_eq!(inbound.kind, InboundKind::Text);
        assert!(inbound.content.contains("[from"));
    }

    // -----------------------------------------------------------------------
    // Mention resolution tests
    // -----------------------------------------------------------------------

    fn test_resolver() -> SignalNameResolver {
        let agent_aci = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_owned();
        SignalNameResolver::build(
            agent_aci,
            "reid".to_owned(),
            &[(test_sender(), "alice".to_owned())],
            &[(test_sender(), "Alice Walker".to_owned())],
        )
    }

    #[test]
    fn mention_resolved_to_display_name() {
        // U+FFFC = mention placeholder
        let body = "hello \u{FFFC}".to_owned();
        let body_ranges = vec![BodyRange {
            start: Some(6),
            length: Some(1),
            associated_value: Some(body_range::AssociatedValue::MentionAci(test_sender())),
        }];
        let resolver = test_resolver();
        let result = resolve_mentions(&body, &body_ranges, &resolver);
        assert_eq!(result, "hello @Alice Walker");
    }

    #[test]
    fn self_mention_resolved_to_agent_name() {
        let agent_aci = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_owned();
        let body = "hey \u{FFFC}".to_owned();
        let body_ranges = vec![BodyRange {
            start: Some(4),
            length: Some(1),
            associated_value: Some(body_range::AssociatedValue::MentionAci(agent_aci)),
        }];
        let resolver = test_resolver();
        let result = resolve_mentions(&body, &body_ranges, &resolver);
        assert_eq!(result, "hey @reid");
    }

    #[test]
    fn unknown_mention_resolved_to_unknown() {
        let body = "ask \u{FFFC}".to_owned();
        let body_ranges = vec![BodyRange {
            start: Some(4),
            length: Some(1),
            associated_value: Some(body_range::AssociatedValue::MentionAci(
                "99999999-9999-9999-9999-999999999999".to_owned(),
            )),
        }];
        let resolver = test_resolver();
        let result = resolve_mentions(&body, &body_ranges, &resolver);
        assert_eq!(result, "ask @unknown");
    }

    #[test]
    fn multiple_mentions_resolved() {
        let agent_aci = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_owned();
        let body = "\u{FFFC} and \u{FFFC}".to_owned();
        let body_ranges = vec![
            BodyRange {
                start: Some(0),
                length: Some(1),
                associated_value: Some(body_range::AssociatedValue::MentionAci(test_sender())),
            },
            BodyRange {
                start: Some(6),
                length: Some(1),
                associated_value: Some(body_range::AssociatedValue::MentionAci(agent_aci)),
            },
        ];
        let resolver = test_resolver();
        let result = resolve_mentions(&body, &body_ranges, &resolver);
        assert_eq!(result, "@Alice Walker and @reid");
    }

    #[test]
    fn no_body_ranges_returns_body_unchanged() {
        let body = "hello world";
        let result = resolve_mentions(body, &[], &test_resolver());
        assert_eq!(result, "hello world");
    }

    #[test]
    fn sender_header_uses_resolver() {
        let resolver = test_resolver();
        let result = prepend_sender_context("hello", &test_sender(), None, 1000, Some(&resolver));
        assert!(result.contains("[from Alice Walker (user:alice) at 1000]"));
        assert!(result.contains("hello"));
    }

    #[test]
    fn sender_header_without_resolver_uses_raw_uuid() {
        let result = prepend_sender_context("hello", &test_sender(), None, 1000, None);
        assert!(result.contains(&format!("[from {} at 1000]", test_sender())));
    }

    #[test]
    fn inbound_with_resolver_resolves_mentions() {
        let agent_aci = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let resolver = test_resolver();
        let content = test_content(
            ContentBody::DataMessage(DataMessage {
                body: Some("hey \u{FFFC}".to_owned()),
                body_ranges: vec![BodyRange {
                    start: Some(4),
                    length: Some(1),
                    associated_value: Some(body_range::AssociatedValue::MentionAci(
                        agent_aci.to_owned(),
                    )),
                }],
                ..Default::default()
            }),
            5000,
        );

        let inbound = inbound_from_content(&content, Some(&resolver)).unwrap();
        assert!(inbound.content.contains("hey @reid"));
        assert!(inbound.content.contains("[from Alice Walker (user:alice)"));
    }

    // -----------------------------------------------------------------------
    // Quote resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn quote_mention_resolved_with_body_ranges() {
        let resolver = test_resolver();
        let agent_aci = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".to_owned();
        let quote = Quote {
            id: Some(100),
            author_aci: Some(test_sender()),
            text: Some("hey \u{FFFC}".to_owned()),
            body_ranges: vec![BodyRange {
                start: Some(4),
                length: Some(1),
                associated_value: Some(body_range::AssociatedValue::MentionAci(agent_aci)),
            }],
            ..Default::default()
        };
        let result = format_reply_context(&quote, Some(&resolver));
        assert!(result.contains("hey @reid"), "got: {result}");
        assert!(
            result.contains("Alice Walker"),
            "should resolve author, got: {result}"
        );
    }

    #[test]
    fn quote_strips_fffc_without_body_ranges() {
        let resolver = test_resolver();
        let quote = Quote {
            id: Some(200),
            text: Some("hey \u{FFFC} there".to_owned()),
            ..Default::default()
        };
        let result = format_reply_context(&quote, Some(&resolver));
        assert!(!result.contains('\u{FFFC}'), "should strip U+FFFC");
        assert!(result.contains("hey  there"), "got: {result}");
    }

    #[test]
    fn quote_without_resolver_preserves_raw() {
        let quote = Quote {
            id: Some(300),
            text: Some("hello world".to_owned()),
            author_aci: Some(test_sender()),
            ..Default::default()
        };
        let result = format_reply_context(&quote, None);
        assert!(result.contains("hello world"));
        // No resolver → no author resolution
        assert!(!result.contains("Alice"));
    }
}
