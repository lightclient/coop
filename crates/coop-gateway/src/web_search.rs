use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;
use tracing::{Instrument, info, info_span};

use crate::web_security::wrap_external_content;

// ---------------------------------------------------------------------------
// Search provider config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum SearchProvider {
    Brave {
        api_key: String,
    },
    Perplexity {
        api_key: String,
        base_url: String,
        model: String,
    },
    Grok {
        api_key: String,
        model: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct SearchConfig {
    pub provider: Option<SearchProvider>,
    pub max_results: usize,
    pub timeout: Duration,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            provider: None,
            max_results: 5,
            timeout: Duration::from_secs(30),
        }
    }
}

// ---------------------------------------------------------------------------
// Search parameters
// ---------------------------------------------------------------------------

pub(crate) struct SearchParams {
    pub query: String,
    pub count: Option<usize>,
    pub country: Option<String>,
    pub freshness: Option<String>,
}

// ---------------------------------------------------------------------------
// Search dispatch
// ---------------------------------------------------------------------------

pub(crate) async fn search(
    client: &reqwest::Client,
    config: &SearchConfig,
    params: SearchParams,
) -> Result<serde_json::Value> {
    let Some(provider) = &config.provider else {
        return Ok(json!({
            "error": "no_search_provider",
            "message": "web_search requires an API key. Options: \
                (1) Set BRAVE_API_KEY for Brave Search (free tier: 2000 queries/month, \
                no credit card — sign up at https://brave.com/search/api/), \
                (2) Set PERPLEXITY_API_KEY for AI-powered search with citations, \
                (3) Set XAI_API_KEY for Grok search. \
                Then configure [tools.web.search] in coop.toml or set the environment variable."
        }));
    };

    match provider {
        SearchProvider::Brave { api_key } => search_brave(client, api_key, config, &params).await,
        SearchProvider::Perplexity {
            api_key,
            base_url,
            model,
        } => search_perplexity(client, api_key, base_url, model, config, &params).await,
        SearchProvider::Grok { api_key, model } => {
            search_grok(client, api_key, model, config, &params).await
        }
    }
}

// ---------------------------------------------------------------------------
// Brave Search
// ---------------------------------------------------------------------------

async fn search_brave(
    client: &reqwest::Client,
    api_key: &str,
    config: &SearchConfig,
    params: &SearchParams,
) -> Result<serde_json::Value> {
    let span = info_span!("web_search", query = %params.query, provider = "brave");

    async {
        let start = Instant::now();
        let count = params.count.unwrap_or(config.max_results).min(10);

        let mut url = reqwest::Url::parse("https://api.search.brave.com/res/v1/web/search")?;
        url.query_pairs_mut()
            .append_pair("q", &params.query)
            .append_pair("count", &count.to_string());

        if let Some(ref country) = params.country {
            url.query_pairs_mut().append_pair("country", country);
        }
        if let Some(ref freshness) = params.freshness {
            url.query_pairs_mut().append_pair("freshness", freshness);
        }

        let response = client
            .get(url)
            .header("Accept", "application/json")
            .header("X-Subscription-Token", api_key)
            .timeout(config.timeout)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            info!(status = %status, "brave search API error");
            bail!(
                "Brave Search API error {status}: {}",
                &body[..body.len().min(500)]
            );
        }

        let body: serde_json::Value = response.json().await?;
        #[allow(clippy::cast_possible_truncation)]
        let took_ms = start.elapsed().as_millis() as u64;

        let results: Vec<serde_json::Value> = body
            .get("web")
            .and_then(|w| w.get("results"))
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .take(count)
                    .map(|item| {
                        let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
                        let url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
                        let desc = item
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let age = item.get("age").and_then(|v| v.as_str());
                        let site = item
                            .get("meta_url")
                            .and_then(|m| m.get("hostname"))
                            .and_then(|v| v.as_str());

                        json!({
                            "title": wrap_external_content(title),
                            "url": url,
                            "description": wrap_external_content(desc),
                            "published": age,
                            "site_name": site,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        info!(
            query = %params.query,
            result_count = results.len(),
            took_ms,
            "brave search complete"
        );

        Ok(json!({
            "query": params.query,
            "provider": "brave",
            "count": results.len(),
            "took_ms": took_ms,
            "results": results,
        }))
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Perplexity Search
// ---------------------------------------------------------------------------

async fn search_perplexity(
    client: &reqwest::Client,
    api_key: &str,
    base_url: &str,
    model: &str,
    config: &SearchConfig,
    params: &SearchParams,
) -> Result<serde_json::Value> {
    let span = info_span!("web_search", query = %params.query, provider = "perplexity");

    async {
        let start = Instant::now();

        // Auto-detect base URL from API key prefix.
        let effective_base = if base_url.is_empty() {
            if api_key.starts_with("pplx-") {
                "https://api.perplexity.ai"
            } else {
                "https://openrouter.ai/api/v1"
            }
        } else {
            base_url
        };

        // Strip perplexity/ prefix for direct Perplexity API.
        let effective_model = if effective_base.contains("perplexity.ai") {
            model.strip_prefix("perplexity/").unwrap_or(model)
        } else {
            model
        };

        let url = format!("{effective_base}/chat/completions");

        let body = json!({
            "model": effective_model,
            "messages": [{"role": "user", "content": params.query}]
        });

        let response = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .timeout(config.timeout)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            info!(status = %status, "perplexity search API error");
            bail!(
                "Perplexity API error {status}: {}",
                &body[..body.len().min(500)]
            );
        }

        let resp: serde_json::Value = response.json().await?;
        #[allow(clippy::cast_possible_truncation)]
        let took_ms = start.elapsed().as_millis() as u64;

        let content = resp
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let citations: Vec<&str> = resp
            .get("citations")
            .and_then(|c| c.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        info!(
            query = %params.query,
            citation_count = citations.len(),
            took_ms,
            "perplexity search complete"
        );

        Ok(json!({
            "query": params.query,
            "provider": "perplexity",
            "model": model,
            "took_ms": took_ms,
            "content": wrap_external_content(content),
            "citations": citations,
        }))
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Grok Search
// ---------------------------------------------------------------------------

async fn search_grok(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    config: &SearchConfig,
    params: &SearchParams,
) -> Result<serde_json::Value> {
    let span = info_span!("web_search", query = %params.query, provider = "grok");

    async {
        let start = Instant::now();

        let body = json!({
            "model": model,
            "input": [{"role": "user", "content": params.query}],
            "tools": [{"type": "web_search"}]
        });

        let response = client
            .post("https://api.x.ai/v1/responses")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .timeout(config.timeout)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            info!(status = %status, "grok search API error");
            bail!("Grok API error {status}: {}", &body[..body.len().min(500)]);
        }

        let resp: serde_json::Value = response.json().await?;
        #[allow(clippy::cast_possible_truncation)]
        let took_ms = start.elapsed().as_millis() as u64;

        // Try Responses API format first, then fallback.
        let content = resp
            .get("output")
            .and_then(|o| o.as_array())
            .and_then(|arr| {
                arr.iter().find_map(|item| {
                    if item.get("type").and_then(|v| v.as_str()) == Some("message") {
                        item.get("content")
                            .and_then(|c| c.as_array())
                            .and_then(|blocks| {
                                blocks.iter().find_map(|b| {
                                    if b.get("type").and_then(|v| v.as_str()) == Some("output_text")
                                    {
                                        b.get("text").and_then(|v| v.as_str())
                                    } else {
                                        None
                                    }
                                })
                            })
                    } else {
                        None
                    }
                })
            })
            .or_else(|| resp.get("output_text").and_then(|v| v.as_str()))
            .unwrap_or("");

        let citations: Vec<&str> = resp
            .get("citations")
            .and_then(|c| c.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        info!(
            query = %params.query,
            citation_count = citations.len(),
            took_ms,
            "grok search complete"
        );

        Ok(json!({
            "query": params.query,
            "provider": "grok",
            "model": model,
            "took_ms": took_ms,
            "content": wrap_external_content(content),
            "citations": citations,
        }))
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Provider auto-detection
// ---------------------------------------------------------------------------

/// Detect search provider from config + environment variables.
#[allow(clippy::too_many_arguments)]
pub(crate) fn detect_provider(
    explicit_provider: Option<&str>,
    brave_key: Option<String>,
    perplexity_key: Option<String>,
    openrouter_key: Option<String>,
    grok_key: Option<String>,
    perplexity_base_url: Option<&str>,
    perplexity_model: Option<&str>,
    grok_model: Option<&str>,
) -> Option<SearchProvider> {
    let default_perplexity_model = "perplexity/sonar-pro";
    let default_grok_model = "grok-4-1-fast";

    match explicit_provider {
        Some("brave") => brave_key.map(|api_key| SearchProvider::Brave { api_key }),
        Some("perplexity") => {
            let api_key = perplexity_key.or(openrouter_key)?;
            let base_url = perplexity_base_url.unwrap_or("").to_owned();
            let model = perplexity_model
                .unwrap_or(default_perplexity_model)
                .to_owned();
            Some(SearchProvider::Perplexity {
                api_key,
                base_url,
                model,
            })
        }
        Some("grok") => grok_key.map(|api_key| SearchProvider::Grok {
            api_key,
            model: grok_model.unwrap_or(default_grok_model).to_owned(),
        }),
        _ => {
            // Auto-detect: Brave > Perplexity > Grok
            if let Some(api_key) = brave_key {
                return Some(SearchProvider::Brave { api_key });
            }
            if let Some(api_key) = perplexity_key.or(openrouter_key) {
                let base_url = perplexity_base_url.unwrap_or("").to_owned();
                let model = perplexity_model
                    .unwrap_or(default_perplexity_model)
                    .to_owned();
                return Some(SearchProvider::Perplexity {
                    api_key,
                    base_url,
                    model,
                });
            }
            if let Some(api_key) = grok_key {
                return Some(SearchProvider::Grok {
                    api_key,
                    model: grok_model.unwrap_or(default_grok_model).to_owned(),
                });
            }
            None
        }
    }
}

/// Resolve an API key from config value or environment variable.
pub(crate) fn resolve_key(config_value: Option<&str>, env_var: &str) -> Option<String> {
    if let Some(val) = config_value {
        if let Some(var_name) = val.strip_prefix("env:") {
            std::env::var(var_name).ok()
        } else if !val.is_empty() {
            Some(val.to_owned())
        } else {
            std::env::var(env_var).ok()
        }
    } else {
        std::env::var(env_var).ok()
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_provider_explicit_brave() {
        let p = detect_provider(
            Some("brave"),
            Some("key".to_owned()),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(matches!(p, Some(SearchProvider::Brave { .. })));
    }

    #[test]
    fn detect_provider_auto_brave_first() {
        let p = detect_provider(
            None,
            Some("brave-key".to_owned()),
            Some("pplx-key".to_owned()),
            None,
            Some("grok-key".to_owned()),
            None,
            None,
            None,
        );
        assert!(matches!(p, Some(SearchProvider::Brave { .. })));
    }

    #[test]
    fn detect_provider_auto_perplexity_second() {
        let p = detect_provider(
            None,
            None,
            Some("pplx-key".to_owned()),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(matches!(p, Some(SearchProvider::Perplexity { .. })));
    }

    #[test]
    fn detect_provider_auto_grok_third() {
        let p = detect_provider(
            None,
            None,
            None,
            None,
            Some("grok-key".to_owned()),
            None,
            None,
            None,
        );
        assert!(matches!(p, Some(SearchProvider::Grok { .. })));
    }

    #[test]
    fn detect_provider_none() {
        let p = detect_provider(None, None, None, None, None, None, None, None);
        assert!(p.is_none());
    }

    #[test]
    fn detect_provider_explicit_missing_key() {
        let p = detect_provider(Some("brave"), None, None, None, None, None, None, None);
        assert!(p.is_none());
    }

    #[test]
    fn perplexity_openrouter_fallback() {
        let p = detect_provider(
            Some("perplexity"),
            None,
            None,
            Some("sk-or-key".to_owned()),
            None,
            None,
            None,
            None,
        );
        assert!(matches!(p, Some(SearchProvider::Perplexity { .. })));
    }

    #[test]
    fn resolve_key_direct_value() {
        let result = resolve_key(Some("my-api-key-123"), "UNUSED");
        assert_eq!(result, Some("my-api-key-123".to_owned()));
    }

    #[test]
    fn resolve_key_env_prefix_missing() {
        let result = resolve_key(Some("env:COOP_TEST_NONEXISTENT_XYZ"), "UNUSED");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_key_fallback_env_uses_home() {
        // HOME is always set in CI/dev
        let result = resolve_key(None, "HOME");
        assert!(result.is_some());
    }

    #[test]
    fn resolve_key_empty_config() {
        let result = resolve_key(Some(""), "NONEXISTENT_VAR_XYZ");
        assert!(result.is_none());
    }

    #[test]
    fn resolve_key_none_config_missing_env() {
        let result = resolve_key(None, "COOP_TEST_NONEXISTENT_XYZ");
        assert!(result.is_none());
    }
}
