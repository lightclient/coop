use crate::{
    sender::{OutgoingPushMessages, SendMessageResponse},
    unidentified_access::UnidentifiedAccess,
    utils::BASE64_RELAXED,
};

use super::*;
use base64::Engine;

impl<C: WebSocketType> SignalWebSocket<C> {
    pub async fn send_messages(
        &mut self,
        messages: OutgoingPushMessages,
    ) -> Result<SendMessageResponse, ServiceError> {
        tracing::info!(
            destination = %messages.destination.service_id_string(),
            message_count = messages.messages.len(),
            online = messages.online,
            ws_closed = self.is_closed(),
            "ws send_messages (identified)"
        );
        let request = WebSocketRequestMessage::new(Method::PUT)
            .path(format!(
                "/v1/messages/{}",
                messages.destination.service_id_string()
            ))
            .json(&messages)?;
        let result = self.request_json(request).await;
        match &result {
            Ok(resp) => tracing::info!(?resp, "ws send_messages succeeded (identified)"),
            Err(e) => tracing::error!(%e, "ws send_messages failed (identified)"),
        }
        result
    }

    pub async fn send_messages_unidentified(
        &mut self,
        messages: OutgoingPushMessages,
        access: &UnidentifiedAccess,
    ) -> Result<SendMessageResponse, ServiceError> {
        tracing::info!(
            destination = %messages.destination.service_id_string(),
            message_count = messages.messages.len(),
            online = messages.online,
            ws_closed = self.is_closed(),
            "ws send_messages (unidentified/sealed sender)"
        );
        let request = WebSocketRequestMessage::new(Method::PUT)
            .path(format!(
                "/v1/messages/{}",
                messages.destination.service_id_string()
            ))
            .header(
                "Unidentified-Access-Key",
                BASE64_RELAXED.encode(&access.key),
            )
            .json(&messages)?;
        let result = self.request_json(request).await;
        match &result {
            Ok(resp) => tracing::info!(?resp, "ws send_messages succeeded (unidentified)"),
            Err(e) => tracing::error!(%e, "ws send_messages failed (unidentified)"),
        }
        result
    }
}
