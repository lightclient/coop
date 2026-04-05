use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use reqwest::Url;
use serde_json::Value;
use tracing::debug;

use crate::sync_http;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(2);

const URL_PROVIDER_MAP: &[(&str, &str)] = &[
    ("api.openai.com", "openai"),
    ("chatgpt.com", "openai"),
    ("api.anthropic.com", "anthropic"),
    ("api.z.ai", "zai"),
    ("api.moonshot.ai", "kimi-coding"),
    ("api.kimi.com", "kimi-coding"),
    ("api.minimax", "minimax"),
    ("dashscope.aliyuncs.com", "alibaba"),
    ("dashscope-intl.aliyuncs.com", "alibaba"),
    ("openrouter.ai", "openrouter"),
    ("generativelanguage.googleapis.com", "google"),
    ("inference-api.nousresearch.com", "nous"),
    ("api.deepseek.com", "deepseek"),
    ("api.githubcopilot.com", "copilot"),
    ("models.github.ai", "copilot"),
    ("api.fireworks.ai", "fireworks"),
];

const PROVIDER_TO_MODELS_DEV: &[(&str, &str)] = &[
    ("openrouter", "openrouter"),
    ("anthropic", "anthropic"),
    ("zai", "zai"),
    ("kimi-coding", "kimi-for-coding"),
    ("minimax", "minimax"),
    ("minimax-cn", "minimax-cn"),
    ("deepseek", "deepseek"),
    ("alibaba", "alibaba"),
    ("copilot", "github-copilot"),
    ("fireworks", "fireworks-ai"),
    ("openai", "openai"),
];

#[derive(Default)]
struct ModelsDevCache {
    data: Option<Arc<Value>>,
    fetched_at: Option<SystemTime>,
}

fn cache() -> &'static Mutex<ModelsDevCache> {
    static CACHE: OnceLock<Mutex<ModelsDevCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ModelsDevCache::default()))
}

pub(crate) fn infer_provider_from_base_url(base_url: &str) -> Option<&'static str> {
    let normalized = normalize_base_url(base_url)?;
    let parsed = Url::parse(&normalized).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    URL_PROVIDER_MAP
        .iter()
        .find_map(|(needle, provider)| host.contains(needle).then_some(*provider))
}

pub(crate) fn lookup_context_limit(provider: &str, model: &str) -> Option<usize> {
    let provider_id = PROVIDER_TO_MODELS_DEV
        .iter()
        .find_map(|(name, mapped)| (*name == provider).then_some(*mapped))?;
    let registry = load_registry()?;
    lookup_in_registry(registry.as_ref(), provider_id, model)
}

fn load_registry() -> Option<Arc<Value>> {
    {
        let state = cache().lock().expect("models.dev cache mutex poisoned");
        if let (Some(data), Some(fetched_at)) = (&state.data, state.fetched_at)
            && fetched_at.elapsed().unwrap_or(CACHE_TTL) < CACHE_TTL
        {
            return Some(Arc::clone(data));
        }
    }

    if let Some(data) = load_registry_from_disk() {
        let mut state = cache().lock().expect("models.dev cache mutex poisoned");
        state.data = Some(Arc::clone(&data));
        state.fetched_at = Some(SystemTime::now());
        drop(state);
        return Some(data);
    }

    let client = sync_http::client(NETWORK_TIMEOUT)?;

    let Some(parsed) = sync_http::run(async move {
        let response = client
            .get(MODELS_DEV_URL)
            .header(
                reqwest::header::USER_AGENT,
                format!("coop/{}", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .ok()?;
        response.error_for_status().ok()?.json::<Value>().await.ok()
    }) else {
        debug!("failed to fetch models.dev registry");
        return None;
    };

    let data = Arc::new(parsed);
    save_registry_to_disk(data.as_ref());
    let mut state = cache().lock().expect("models.dev cache mutex poisoned");
    state.data = Some(Arc::clone(&data));
    state.fetched_at = Some(SystemTime::now());
    drop(state);
    Some(data)
}

fn lookup_in_registry(registry: &Value, provider: &str, model: &str) -> Option<usize> {
    let models = registry.get(provider)?.get("models")?.as_object()?;

    for candidate in model_candidates(model) {
        if let Some(limit) = models
            .get(candidate)
            .and_then(extract_context_limit_from_entry)
            .filter(|limit| *limit > 0)
        {
            return Some(limit);
        }
    }

    let requested = model.trim().to_ascii_lowercase();
    for (candidate, entry) in models {
        let bare_match = candidate
            .rsplit_once('/')
            .is_some_and(|(_, bare)| bare.eq_ignore_ascii_case(model.trim()));
        if (candidate.to_ascii_lowercase() == requested || bare_match)
            && let Some(limit) = extract_context_limit_from_entry(entry).filter(|limit| *limit > 0)
        {
            return Some(limit);
        }
    }

    None
}

fn extract_context_limit_from_entry(entry: &Value) -> Option<usize> {
    entry
        .get("limit")?
        .get("context")?
        .as_u64()
        .and_then(|limit| usize::try_from(limit).ok())
}

fn model_candidates(model: &str) -> Vec<&str> {
    let trimmed = model.trim();
    let mut candidates = vec![trimmed];
    for prefix in ["anthropic/", "openai/", "openai-compatible/", "ollama/"] {
        if let Some(stripped) = trimmed.strip_prefix(prefix) {
            candidates.push(stripped);
        }
    }
    candidates
}

fn load_registry_from_disk() -> Option<Arc<Value>> {
    let path = registry_cache_path()?;
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok().map(Arc::new)
}

fn save_registry_to_disk(value: &Value) {
    let Some(path) = registry_cache_path() else {
        return;
    };
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).is_ok() {
        let _ = fs::rename(tmp, path);
    }
}

fn registry_cache_path() -> Option<PathBuf> {
    coop_config_dir().map(|dir| dir.join("models_dev_cache.json"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn infers_provider_from_base_url() {
        assert_eq!(
            infer_provider_from_base_url("https://api.openai.com/v1"),
            Some("openai")
        );
        assert_eq!(
            infer_provider_from_base_url("https://api.githubcopilot.com/chat/completions"),
            Some("copilot")
        );
        assert_eq!(
            infer_provider_from_base_url("http://localhost:11434/v1"),
            None
        );
    }

    #[test]
    fn provider_aware_lookup_returns_correct_limit() {
        let registry = json!({
            "openai": {
                "models": {
                    "gpt-5.4": { "limit": { "context": 1_050_000 } }
                }
            },
            "github-copilot": {
                "models": {
                    "gpt-5.4": { "limit": { "context": 400_000 } }
                }
            }
        });

        assert_eq!(
            lookup_in_registry(&registry, "openai", "gpt-5.4"),
            Some(1_050_000)
        );
        assert_eq!(
            lookup_in_registry(&registry, "github-copilot", "gpt-5.4"),
            Some(400_000)
        );
    }

    #[test]
    fn lookup_accepts_prefixed_first_party_models() {
        let registry = json!({
            "openai": {
                "models": {
                    "gpt-5.4": { "limit": { "context": 1_050_000 } }
                }
            },
            "anthropic": {
                "models": {
                    "claude-sonnet-4-6": { "limit": { "context": 1_000_000 } }
                }
            }
        });

        assert_eq!(
            lookup_in_registry(&registry, "openai", "openai/gpt-5.4"),
            Some(1_050_000)
        );
        assert_eq!(
            lookup_in_registry(&registry, "anthropic", "anthropic/claude-sonnet-4-6"),
            Some(1_000_000)
        );
    }
}
