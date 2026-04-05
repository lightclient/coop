use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use reqwest::{Method, Url};
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::models_dev;
use crate::openai_codex::extract_account_id;
use crate::provider_spec::ProviderKind;
use crate::sync_http;

const DEFAULT_FALLBACK_CONTEXT: usize = 128_000;
const CODEX_CONTEXT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex/";
const LOCAL_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1", "0.0.0.0"];
const CONTEXT_KEYS: &[&str] = &[
    "context_length",
    "context_window",
    "max_context_length",
    "max_position_embeddings",
    "max_model_len",
    "max_input_tokens",
    "max_sequence_length",
    "max_seq_len",
    "n_ctx_train",
    "n_ctx",
];
const MODEL_DEFAULTS: &[(&str, usize)] = &[
    ("claude-opus-4-6", 1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-opus-4.6", 1_000_000),
    ("claude-sonnet-4.6", 1_000_000),
    ("gpt-5.4", 1_050_000),
    ("gpt-5-codex", 400_000),
    ("gpt-5-mini", 400_000),
    ("gpt-5", 400_000),
    ("gpt-4.1", 1_047_576),
    ("gemini", 1_048_576),
    ("minimax", 204_800),
    ("kimi", 262_144),
    ("glm", 202_752),
    ("llama", 131_072),
    ("qwen", 131_072),
    ("deepseek", 128_000),
    ("claude", 200_000),
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextLimitInput<'a> {
    pub kind: ProviderKind,
    pub model: &'a str,
    pub base_url: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub configured_limit: Option<usize>,
}

pub(crate) fn resolve_context_limit(input: ContextLimitInput<'_>) -> usize {
    if let Some(limit) = input.configured_limit.filter(|limit| *limit > 0) {
        return resolved_limit(input.model, input.base_url, limit, "config");
    }

    let model = input.model.trim();
    let base_url = resolution_base_url(input.kind, input.base_url, input.api_key);

    if let Some(base_url) = base_url.as_deref()
        && let Some(limit) = get_cached_context_limit(model, base_url)
    {
        return resolved_limit(model, Some(base_url), limit, "cache");
    }

    if let Some(base_url) = base_url.as_deref()
        && is_custom_endpoint(base_url)
        && models_dev::infer_provider_from_base_url(base_url).is_none()
    {
        if let Some(limit) = fetch_endpoint_context_limit(model, base_url, input.api_key) {
            save_context_limit(model, base_url, limit);
            return resolved_limit(model, Some(base_url), limit, "endpoint_models");
        }
        if is_local_endpoint(base_url)
            && let Some(limit) = query_local_context_limit(model, base_url)
        {
            save_context_limit(model, base_url, limit);
            return resolved_limit(model, Some(base_url), limit, "local_endpoint");
        }
        return resolved_limit(model, Some(base_url), DEFAULT_FALLBACK_CONTEXT, "fallback");
    }

    if effective_provider(input.kind, base_url.as_deref(), input.api_key) == Some("anthropic")
        && let Some(limit) =
            query_anthropic_context_limit(model, base_url.as_deref(), input.api_key)
    {
        if let Some(base_url) = base_url.as_deref() {
            save_context_limit(model, base_url, limit);
        }
        return resolved_limit(model, base_url.as_deref(), limit, "anthropic_models");
    }

    if let Some(provider) = effective_provider(input.kind, base_url.as_deref(), input.api_key)
        && let Some(limit) = models_dev::lookup_context_limit(provider, model)
    {
        return resolved_limit(model, base_url.as_deref(), limit, "models_dev");
    }

    if let Some(limit) = hardcoded_context_limit(model) {
        return resolved_limit(model, base_url.as_deref(), limit, "defaults");
    }

    if let Some(base_url) = base_url.as_deref()
        && is_local_endpoint(base_url)
        && let Some(limit) = query_local_context_limit(model, base_url)
    {
        save_context_limit(model, base_url, limit);
        return resolved_limit(model, Some(base_url), limit, "local_endpoint");
    }

    resolved_limit(
        model,
        base_url.as_deref(),
        DEFAULT_FALLBACK_CONTEXT,
        "fallback",
    )
}

fn resolved_limit(model: &str, base_url: Option<&str>, limit: usize, source: &str) -> usize {
    debug!(
        model,
        base_url,
        context_limit = limit,
        source,
        "resolved model context limit"
    );
    limit
}

#[allow(dead_code)]
pub(crate) fn parse_context_limit_from_error(error: &str) -> Option<usize> {
    let lower = error.to_ascii_lowercase();
    let numbers = lower
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| part.len() >= 4)
        .filter_map(|part| part.parse::<usize>().ok())
        .collect::<Vec<_>>();
    for limit in numbers {
        if !(1024..=10_000_000).contains(&limit) {
            continue;
        }
        let limit_text = limit.to_string();
        if let Some(index) = lower.find(&limit_text) {
            let start = index.saturating_sub(32);
            let end = (index + limit_text.len() + 32).min(lower.len());
            let window = &lower[start..end];
            if window.contains("context") || window.contains("window") || window.contains("limit") {
                return Some(limit);
            }
        }
    }
    None
}

fn resolution_base_url(
    kind: ProviderKind,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Option<String> {
    if kind == ProviderKind::OpenAi && api_key.and_then(extract_account_id).is_some() {
        return Some(CODEX_CONTEXT_BASE_URL.to_owned());
    }

    base_url
        .and_then(normalize_base_url)
        .or_else(|| match kind {
            ProviderKind::Anthropic => Some("https://api.anthropic.com/".to_owned()),
            ProviderKind::Gemini => {
                Some("https://generativelanguage.googleapis.com/v1beta/".to_owned())
            }
            ProviderKind::OpenAi => Some("https://api.openai.com/v1/".to_owned()),
            ProviderKind::OpenAiCompatible => None,
            ProviderKind::Ollama => Some("http://localhost:11434/v1/".to_owned()),
        })
}

fn effective_provider(
    kind: ProviderKind,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Option<&'static str> {
    if kind == ProviderKind::OpenAi && api_key.and_then(extract_account_id).is_some() {
        return Some("openai");
    }
    match kind {
        ProviderKind::Anthropic => Some("anthropic"),
        ProviderKind::Gemini => Some("gemini"),
        ProviderKind::OpenAi => Some("openai"),
        ProviderKind::OpenAiCompatible => {
            base_url.and_then(models_dev::infer_provider_from_base_url)
        }
        ProviderKind::Ollama => None,
    }
}

fn hardcoded_context_limit(model: &str) -> Option<usize> {
    let lower = model.trim().to_ascii_lowercase();
    MODEL_DEFAULTS.iter().find_map(|(pattern, limit)| {
        lower
            .contains(&pattern.to_ascii_lowercase())
            .then_some(*limit)
    })
}

fn is_custom_endpoint(base_url: &str) -> bool {
    let normalized = base_url.trim();
    !normalized.is_empty() && !normalized.contains("openrouter.ai")
}

fn is_local_endpoint(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if LOCAL_HOSTS.contains(&host) {
        return true;
    }
    if let Ok(addr) = host.parse::<IpAddr>() {
        return match addr {
            IpAddr::V4(addr) => addr.is_private() || addr.is_loopback() || addr.is_link_local(),
            IpAddr::V6(addr) => addr.is_loopback() || addr.is_unique_local(),
        };
    }
    false
}

fn fetch_endpoint_context_limit(
    model: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> Option<usize> {
    let client = http_client(2)?;
    let model = model.to_owned();
    let base_url = base_url.to_owned();
    let api_key = api_key.map(str::to_owned);
    sync_http::run(async move {
        let mut candidates = vec![base_url.trim_end_matches('/').to_owned()];
        if base_url.trim_end_matches('/').ends_with("/v1") {
            candidates.push(
                base_url
                    .trim_end_matches('/')
                    .trim_end_matches("/v1")
                    .to_owned(),
            );
        } else {
            candidates.push(format!("{}/v1", base_url.trim_end_matches('/')));
        }
        candidates.sort();
        candidates.dedup();

        for candidate in candidates {
            let request = client
                .request(
                    Method::GET,
                    format!("{}/models", candidate.trim_end_matches('/')),
                )
                .header(
                    reqwest::header::USER_AGENT,
                    format!("coop/{}", env!("CARGO_PKG_VERSION")),
                );
            let request = if let Some(api_key) = api_key.as_deref().filter(|key| !key.is_empty()) {
                request.bearer_auth(api_key)
            } else {
                request
            };
            let Ok(response) = request.send().await else {
                continue;
            };
            let Ok(response) = response.error_for_status() else {
                continue;
            };
            let Ok(payload) = response.json::<Value>().await else {
                continue;
            };
            if let Some(limit) = match_model_in_endpoint_payload(&model, &payload) {
                return Some(limit);
            }
        }
        None
    })
}

fn match_model_in_endpoint_payload(model: &str, payload: &Value) -> Option<usize> {
    let data = payload.get("data")?.as_array()?;
    let mut entries = Vec::new();
    for item in data {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(limit) = extract_context_length(item) else {
            continue;
        };
        entries.push((id.to_owned(), limit));
    }

    for candidate in model_candidates(model) {
        if let Some(limit) = entries
            .iter()
            .find_map(|(id, limit)| id.eq_ignore_ascii_case(&candidate).then_some(*limit))
        {
            return Some(limit);
        }
    }

    if entries.len() == 1 {
        return entries.first().map(|(_, limit)| *limit);
    }

    let requested = model.trim().to_ascii_lowercase();
    entries.into_iter().find_map(|(id, limit)| {
        let candidate = id.to_ascii_lowercase();
        (candidate.contains(&requested) || requested.contains(&candidate)).then_some(limit)
    })
}

fn query_local_context_limit(model: &str, base_url: &str) -> Option<usize> {
    let client = http_client(2)?;
    let model = model.to_owned();
    let server_root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned();
    sync_http::run(async move {
        let server_type = detect_local_server_type(&client, &server_root).await;

        if server_type == Some("ollama")
            && let Some(limit) = query_ollama_context_limit(&client, &model, &server_root).await
        {
            return Some(limit);
        }

        if server_type == Some("lm-studio")
            && let Some(limit) = query_lm_studio_context_limit(&client, &model, &server_root).await
        {
            return Some(limit);
        }

        let direct_model = strip_known_provider_prefix(&model);
        let paths = [
            format!("{server_root}/v1/models/{direct_model}"),
            format!("{server_root}/v1/models"),
        ];
        for path in paths {
            let Ok(response) = client.get(path).send().await else {
                continue;
            };
            let Ok(response) = response.error_for_status() else {
                continue;
            };
            let Ok(payload) = response.json::<Value>().await else {
                continue;
            };
            if payload.get("data").is_some() {
                if let Some(limit) = match_model_in_endpoint_payload(&direct_model, &payload) {
                    return Some(limit);
                }
            } else if let Some(limit) = extract_context_length(&payload) {
                return Some(limit);
            }
        }

        None
    })
}

async fn detect_local_server_type(
    client: &reqwest::Client,
    server_root: &str,
) -> Option<&'static str> {
    if client
        .get(format!("{server_root}/api/v1/models"))
        .send()
        .await
        .ok()?
        .status()
        .is_success()
    {
        return Some("lm-studio");
    }
    if let Ok(response) = client.get(format!("{server_root}/api/tags")).send().await
        && response.status().is_success()
        && response.json::<Value>().await.ok()?.get("models").is_some()
    {
        return Some("ollama");
    }
    if let Ok(response) = client.get(format!("{server_root}/v1/props")).send().await
        && response.status().is_success()
    {
        return Some("llamacpp");
    }
    if let Ok(response) = client.get(format!("{server_root}/version")).send().await
        && response.status().is_success()
        && response
            .json::<Value>()
            .await
            .ok()?
            .get("version")
            .is_some()
    {
        return Some("vllm");
    }
    None
}

async fn query_ollama_context_limit(
    client: &reqwest::Client,
    model: &str,
    server_root: &str,
) -> Option<usize> {
    let payload = client
        .post(format!("{server_root}/api/show"))
        .json(&json!({ "name": strip_known_provider_prefix(model) }))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()?;

    let model_info = payload.get("model_info")?.as_object()?;
    for (key, value) in model_info {
        if key.contains("context_length")
            && let Some(limit) = value.as_u64().and_then(|limit| usize::try_from(limit).ok())
        {
            return Some(limit);
        }
    }

    let parameters = payload.get("parameters")?.as_str()?;
    for line in parameters.lines() {
        if line.contains("num_ctx") {
            let limit = line.split_whitespace().last()?.parse::<usize>().ok()?;
            return Some(limit);
        }
    }

    None
}

async fn query_lm_studio_context_limit(
    client: &reqwest::Client,
    model: &str,
    server_root: &str,
) -> Option<usize> {
    let payload = client
        .get(format!("{server_root}/api/v1/models"))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<Value>()
        .await
        .ok()?;

    for item in payload.get("models")?.as_array()? {
        let matches = item
            .get("key")
            .and_then(Value::as_str)
            .is_some_and(|key| model_id_matches(key, model))
            || item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| model_id_matches(id, model));
        if !matches {
            continue;
        }

        if let Some(limit) = item
            .get("loaded_instances")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|instance| {
                instance
                    .get("config")
                    .and_then(|config| config.get("context_length"))
                    .and_then(Value::as_u64)
                    .and_then(|limit| usize::try_from(limit).ok())
            })
        {
            return Some(limit);
        }

        if let Some(limit) = item
            .get("max_context_length")
            .or_else(|| item.get("context_length"))
            .and_then(Value::as_u64)
            .and_then(|limit| usize::try_from(limit).ok())
        {
            return Some(limit);
        }
    }

    None
}

fn model_id_matches(candidate: &str, requested: &str) -> bool {
    candidate == requested
        || candidate
            .rsplit_once('/')
            .is_some_and(|(_, bare)| bare == requested)
}

fn extract_context_length(value: &Value) -> Option<usize> {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if CONTEXT_KEYS.contains(&key.as_str())
                    && let Some(limit) = coerce_reasonable_int(nested)
                {
                    return Some(limit);
                }
                if let Some(limit) = extract_context_length(nested) {
                    return Some(limit);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(extract_context_length),
        _ => None,
    }
}

fn coerce_reasonable_int(value: &Value) -> Option<usize> {
    let parsed = match value {
        Value::Number(number) => usize::try_from(number.as_u64()?).ok()?,
        Value::String(text) => text.trim().replace(',', "").parse().ok()?,
        _ => return None,
    };
    (1024..=10_000_000).contains(&parsed).then_some(parsed)
}

fn query_anthropic_context_limit(
    model: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Option<usize> {
    let api_key = api_key?;
    if api_key.contains("sk-ant-oat") {
        return None;
    }
    let client = http_client(2)?;
    let base = normalize_base_url(base_url.unwrap_or("https://api.anthropic.com/"))?;
    let root = base
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned();
    let requested = strip_known_provider_prefix(model);
    let api_key = api_key.to_owned();
    sync_http::run(async move {
        let payload = client
            .get(format!("{root}/v1/models?limit=1000"))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json::<Value>()
            .await
            .ok()?;

        for item in payload.get("data")?.as_array()? {
            if item.get("id").and_then(Value::as_str) == Some(requested.as_str()) {
                return item
                    .get("max_input_tokens")
                    .and_then(Value::as_u64)
                    .and_then(|limit| usize::try_from(limit).ok());
            }
        }
        None
    })
}

fn context_cache_path() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(path) = test_context_cache_path() {
        return Some(path);
    }
    coop_config_dir().map(|dir| dir.join("context_length_cache.json"))
}

#[cfg(test)]
fn test_context_cache_override() -> &'static Mutex<Option<PathBuf>> {
    static PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    PATH.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn test_context_cache_path() -> Option<PathBuf> {
    test_context_cache_override()
        .lock()
        .expect("test context cache mutex poisoned")
        .clone()
}

#[cfg(test)]
fn set_test_context_cache_path(path: PathBuf) {
    *test_context_cache_override()
        .lock()
        .expect("test context cache mutex poisoned") = Some(path);
}

fn get_cached_context_limit(model: &str, base_url: &str) -> Option<usize> {
    let path = context_cache_path()?;
    let bytes = fs::read(path).ok()?;
    let payload: Value = serde_json::from_slice(&bytes).ok()?;
    payload
        .get("context_lengths")?
        .get(cache_key(model, base_url))?
        .as_u64()
        .and_then(|limit| usize::try_from(limit).ok())
}

fn save_context_limit(model: &str, base_url: &str, limit: usize) {
    let Some(path) = context_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let mut context_lengths = BTreeMap::new();
    if let Ok(existing) = fs::read(&path)
        && let Ok(payload) = serde_json::from_slice::<Value>(&existing)
        && let Some(entries) = payload.get("context_lengths").and_then(Value::as_object)
    {
        for (key, value) in entries {
            if let Some(limit) = value.as_u64().and_then(|limit| usize::try_from(limit).ok()) {
                context_lengths.insert(key.clone(), limit);
            }
        }
    }

    let key = cache_key(model, base_url);
    if context_lengths.get(&key) == Some(&limit) {
        return;
    }
    context_lengths.insert(key, limit);

    let payload = json!({ "context_lengths": context_lengths });
    let Ok(bytes) = serde_json::to_vec(&payload) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(tmp, &path);
        info!(
            model,
            base_url,
            context_limit = limit,
            "cached model context limit"
        );
    }
}

fn cache_key(model: &str, base_url: &str) -> String {
    format!("{}@{}", model.trim(), base_url.trim())
}

fn coop_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join("coop"));
        }
    }
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .map(|home| home.join(".coop"))
}

fn normalize_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.ends_with('/') {
        Some(trimmed.to_owned())
    } else {
        Some(format!("{trimmed}/"))
    }
}

fn strip_known_provider_prefix(model: &str) -> String {
    let trimmed = model.trim();
    for prefix in ["anthropic/", "openai/", "openai-compatible/", "ollama/"] {
        if let Some(stripped) = trimmed.strip_prefix(prefix) {
            return stripped.to_owned();
        }
    }
    trimmed.to_owned()
}

fn model_candidates(model: &str) -> Vec<String> {
    let stripped = strip_known_provider_prefix(model);
    let mut candidates = vec![model.trim().to_owned()];
    if stripped != model.trim() {
        candidates.push(stripped);
    }
    if let Some((_, bare)) = model.trim().rsplit_once('/') {
        candidates.push(bare.to_owned());
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn http_client(timeout_secs: u64) -> Option<reqwest::Client> {
    sync_http::client(std::time::Duration::from_secs(timeout_secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn configured_override_wins() {
        let limit = resolve_context_limit(ContextLimitInput {
            kind: ProviderKind::OpenAi,
            model: "gpt-5.4",
            base_url: None,
            api_key: None,
            configured_limit: Some(42),
        });
        assert_eq!(limit, 42);
    }

    #[test]
    fn defaults_include_current_openai_and_anthropic_windows() {
        assert_eq!(hardcoded_context_limit("gpt-5.4"), Some(1_050_000));
        assert_eq!(hardcoded_context_limit("gpt-5-codex"), Some(400_000));
        assert_eq!(
            hardcoded_context_limit("claude-sonnet-4.6"),
            Some(1_000_000)
        );
    }

    #[test]
    fn parses_context_limit_from_error_text() {
        assert_eq!(
            parse_context_limit_from_error(
                "OpenAI API error: This model's maximum context length is 128000 tokens"
            ),
            Some(128_000)
        );
        assert_eq!(
            parse_context_limit_from_error(
                "context_length_exceeded: maximum context length is 131072"
            ),
            Some(131_072)
        );
    }

    #[test]
    fn caches_context_by_model_and_base_url() {
        let dir = std::env::temp_dir().join(format!(
            "coop-context-cache-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("create temp context cache dir");
        set_test_context_cache_path(dir.join("context_length_cache.json"));
        save_context_limit("gpt-5.4", "https://api.openai.com/v1/", 1_050_000);
        assert_eq!(
            get_cached_context_limit("gpt-5.4", "https://api.openai.com/v1/"),
            Some(1_050_000)
        );
        assert_eq!(
            get_cached_context_limit("gpt-5.4", "https://chatgpt.com/backend-api/codex/"),
            None
        );
    }

    #[test]
    fn local_endpoint_detection_handles_private_hosts() {
        assert!(is_local_endpoint("http://localhost:11434/v1"));
        assert!(is_local_endpoint("http://192.168.1.22:1234/v1"));
        assert!(!is_local_endpoint("https://api.openai.com/v1"));
    }
}
