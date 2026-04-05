use std::time::Duration;

use anyhow::Result;
use reqwest::header::AUTHORIZATION;

use crate::provider_spec::{ProviderKind, ProviderSpec};
use crate::request_trace::{
    TransportErrorTrace, TransportProbeTrace, summarize_transport_probe_reqwest_error,
    summarize_transport_probe_response,
};

const MODELS_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn build_probe_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(MODELS_PROBE_TIMEOUT)
        .build()?)
}

pub(crate) async fn probe_transport_failure(
    client: &reqwest::Client,
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
    auth_header: Option<&str>,
) -> Option<TransportProbeTrace> {
    if !should_probe_models_endpoint(kind, spec, transport) {
        return None;
    }

    let url = format!("{}models", spec.normalized_base_url()?);
    let mut request = client.get(&url);
    let has_authorization_override = spec
        .extra_headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case(AUTHORIZATION.as_str()));

    if !has_authorization_override && let Some(auth_header) = auth_header {
        request = request.header(AUTHORIZATION, auth_header);
    }

    for (name, value) in &spec.extra_headers {
        request = request.header(name, value);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            match response.text().await {
                Ok(body) => Some(summarize_transport_probe_response(
                    "models", &url, status, &body,
                )),
                Err(error) => Some(summarize_transport_probe_reqwest_error(
                    "models", &url, &error,
                )),
            }
        }
        Err(error) => Some(summarize_transport_probe_reqwest_error(
            "models", &url, &error,
        )),
    }
}

fn should_probe_models_endpoint(
    kind: ProviderKind,
    spec: &ProviderSpec,
    transport: &TransportErrorTrace,
) -> bool {
    kind == ProviderKind::OpenAiCompatible
        && spec.normalized_base_url().is_some()
        && transport.http_status.is_none()
        && (transport.reqwest_is_connect
            || transport.reqwest_is_timeout
            || transport.reqwest_is_request
            || transport.reqwest_is_body
            || transport.reqwest_is_decode)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn only_openai_compatible_transport_failures_probe_models() {
        let spec = ProviderSpec {
            kind: ProviderKind::OpenAiCompatible,
            model: "demo-model".into(),
            default_model: None,
            default_model_context_limit: None,
            model_context_limits: BTreeMap::new(),
            api_keys: Vec::new(),
            api_key_env: None,
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            extra_headers: BTreeMap::new(),
            refresh_token: None,
        };
        let transport = TransportErrorTrace {
            variant: "web_model_call",
            kind: "reqwest",
            http_status: None,
            reqwest_is_connect: true,
            reqwest_is_timeout: false,
            reqwest_is_request: true,
            reqwest_is_body: false,
            reqwest_is_decode: false,
            url: "http://127.0.0.1:11434/v1/chat/completions".into(),
            source_chain: "connect error".into(),
            body_excerpt: String::new(),
        };

        assert!(should_probe_models_endpoint(
            ProviderKind::OpenAiCompatible,
            &spec,
            &transport,
        ));
        assert!(!should_probe_models_endpoint(
            ProviderKind::OpenAi,
            &spec,
            &transport,
        ));
    }
}
