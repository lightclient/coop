use libsignal_core::DeviceId;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    configuration::Endpoint,
    utils::{serde_device_id, serde_optional_base64},
    websocket::registration::DeviceActivationRequest,
};

use super::{
    response::ReqwestExt, HttpAuth, HttpAuthOverride, PushService, ServiceError,
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkAccountAttributes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signaling_key: Option<String>,
    pub registration_id: u32,
    pub voice: bool,
    pub video: bool,
    pub fetches_messages: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration_lock: Option<String>,
    #[serde(default, with = "serde_optional_base64")]
    pub unidentified_access_key: Option<Vec<u8>>,
    pub unrestricted_unidentified_access: bool,
    pub discoverable_by_phone_number: bool,
    pub pni_registration_id: u32,
    pub capabilities: LinkCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_password: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkCapabilities {
    pub storage: bool,
    pub versioned_expiration_timer: bool,
    pub attachment_backfill: bool,
    pub spqr: bool,
}

impl Default for LinkCapabilities {
    fn default() -> Self {
        Self {
            storage: true,
            versioned_expiration_timer: true,
            attachment_backfill: true,
            spqr: true,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkResponse {
    #[serde(rename = "uuid")]
    pub aci: Uuid,
    pub pni: Uuid,
    #[serde(with = "serde_device_id")]
    pub device_id: DeviceId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRequest {
    pub verification_code: String,
    pub account_attributes: LinkAccountAttributes,
    #[serde(flatten)]
    pub device_activation_request: DeviceActivationRequest,
}

impl PushService {
    pub async fn link_device(
        &mut self,
        link_request: &LinkRequest,
        http_auth: HttpAuth,
    ) -> Result<LinkResponse, ServiceError> {
        self.request(
            Method::PUT,
            Endpoint::service("/v1/devices/link"),
            HttpAuthOverride::Identified(http_auth),
        )?
        .json(&link_request)
        .send()
        .await?
        .service_error_for_status()
        .await?
        .json()
        .await
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::{LinkAccountAttributes, LinkCapabilities};

    #[test]
    fn link_account_attributes_serialize_with_current_capability_names() {
        let value = serde_json::to_value(LinkAccountAttributes {
            signaling_key: None,
            registration_id: 1234,
            voice: true,
            video: true,
            fetches_messages: true,
            registration_lock: None,
            unidentified_access_key: Some(vec![1, 2, 3]),
            unrestricted_unidentified_access: false,
            discoverable_by_phone_number: true,
            pni_registration_id: 5678,
            capabilities: LinkCapabilities::default(),
            name: Some("device-name".to_owned()),
            recovery_password: None,
        })
        .unwrap();

        assert_eq!(value["voice"], true);
        assert_eq!(value["video"], true);
        assert_eq!(value["capabilities"]["storage"], true);
        assert_eq!(value["capabilities"]["versionedExpirationTimer"], true);
        assert_eq!(value["capabilities"]["attachmentBackfill"], true);
        assert_eq!(value["capabilities"]["spqr"], true);
        assert!(value["capabilities"].get("deleteSync").is_none());
        assert!(value["capabilities"].get("ssre2").is_none());
    }
}
