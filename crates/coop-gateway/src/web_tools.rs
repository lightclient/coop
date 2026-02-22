use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use coop_core::traits::{ToolContext, ToolExecutor};
use coop_core::types::{ToolDef, ToolOutput};
use tracing::{debug, warn};

use crate::config::WebToolConfig;
use crate::web_cache::Cache;
use crate::web_fetch::{self, FetchConfig};
use crate::web_search::{self, SearchConfig, SearchParams};

#[allow(missing_debug_implementations)]
pub(crate) struct WebToolExecutor {
    search_config: SearchConfig,
    fetch_config: FetchConfig,
    search_cache: Mutex<Cache<serde_json::Value>>,
    fetch_cache: Mutex<Cache<serde_json::Value>>,
    search_cache_ttl: Duration,
    fetch_cache_ttl: Duration,
    client: reqwest::Client,
    fetch_enabled: bool,
}

impl WebToolExecutor {
    pub(crate) fn new(config: &WebToolConfig) -> Self {
        let search_config = build_search_config(config);
        let fetch_config = build_fetch_config(config);

        let search_cache_ttl =
            Duration::from_secs(u64::from(config.search.cache_ttl_minutes.unwrap_or(15)) * 60);
        let fetch_cache_ttl =
            Duration::from_secs(u64::from(config.fetch.cache_ttl_minutes.unwrap_or(15)) * 60);

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(u64::from(
                config.fetch.timeout_seconds.unwrap_or(30),
            )))
            .user_agent(
                config
                    .fetch
                    .user_agent
                    .clone()
                    .unwrap_or_else(|| "Mozilla/5.0 (compatible; Coop/1.0)".to_owned()),
            )
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        if search_config.provider.is_none() {
            warn!(
                "web_search: no search API key configured. The web_search tool will return setup \
                 instructions when called. Set BRAVE_API_KEY for Brave Search (free tier: 2000 \
                 queries/month at https://brave.com/search/api/), or PERPLEXITY_API_KEY / \
                 XAI_API_KEY for AI-powered search."
            );
        }

        Self {
            search_config,
            fetch_config,
            search_cache: Mutex::new(Cache::new()),
            fetch_cache: Mutex::new(Cache::new()),
            search_cache_ttl,
            fetch_cache_ttl,
            client,
            fetch_enabled: config.fetch.enabled.unwrap_or(true),
        }
    }

    fn search_def() -> ToolDef {
        ToolDef::new(
            "web_search",
            "Search the web. Returns titles, URLs, and snippets.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query string."
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results to return (1-10, default 5).",
                        "minimum": 1,
                        "maximum": 10
                    },
                    "country": {
                        "type": "string",
                        "description": "2-letter country code for regional results (e.g. 'US', 'DE')."
                    },
                    "freshness": {
                        "type": "string",
                        "description": "Filter by recency: 'pd' (24h), 'pw' (week), 'pm' (month), 'py' (year), or 'YYYY-MM-DDtoYYYY-MM-DD'. Brave only."
                    }
                },
                "required": ["query"]
            }),
        )
    }

    fn fetch_def() -> ToolDef {
        ToolDef::new(
            "web_fetch",
            "Fetch a URL and extract readable content as markdown or text. Use after web_search to read a specific result.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "HTTP or HTTPS URL to fetch."
                    },
                    "extract_mode": {
                        "type": "string",
                        "enum": ["markdown", "text"],
                        "description": "Output format (default: markdown)."
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default: 50000).",
                        "minimum": 100
                    }
                },
                "required": ["url"]
            }),
        )
    }

    async fn handle_search(&self, arguments: serde_json::Value) -> Result<ToolOutput> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'query' parameter"))?
            .to_owned();

        #[allow(clippy::cast_possible_truncation)]
        let count = arguments
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as usize);

        let country = arguments
            .get("country")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);

        let freshness = arguments
            .get("freshness")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);

        let cache_key = format!(
            "search:{}:{}:{}:{}",
            query,
            count.unwrap_or(0),
            country.as_deref().unwrap_or(""),
            freshness.as_deref().unwrap_or("")
        );

        {
            let cache = self.search_cache.lock().expect("search cache poisoned");
            if let Some(cached) = cache.get(&cache_key) {
                debug!(query = %query, "search cache hit");
                return Ok(ToolOutput::success(serde_json::to_string(cached)?));
            }
        }

        let params = SearchParams {
            query,
            count,
            country,
            freshness,
        };

        match web_search::search(&self.client, &self.search_config, params).await {
            Ok(result) => {
                {
                    let mut cache = self.search_cache.lock().expect("search cache poisoned");
                    cache.insert(&cache_key, result.clone(), self.search_cache_ttl);
                }
                Ok(ToolOutput::success(serde_json::to_string(&result)?))
            }
            Err(e) => Ok(ToolOutput::error(format!("{e:#}"))),
        }
    }

    async fn handle_fetch(&self, arguments: serde_json::Value) -> Result<ToolOutput> {
        if !self.fetch_enabled {
            return Ok(ToolOutput::error("web_fetch is disabled in config"));
        }

        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'url' parameter"))?
            .to_owned();

        let extract_mode = arguments
            .get("extract_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("markdown")
            .to_owned();

        #[allow(clippy::cast_possible_truncation)]
        let max_chars = arguments
            .get("max_chars")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as usize);

        let cache_key = format!("fetch:{url}:{extract_mode}");

        {
            let cache = self.fetch_cache.lock().expect("fetch cache poisoned");
            if let Some(cached) = cache.get(&cache_key) {
                debug!(url = %url, "fetch cache hit");
                return Ok(ToolOutput::success(serde_json::to_string(cached)?));
            }
        }

        match web_fetch::fetch_url(
            &self.client,
            &url,
            &extract_mode,
            max_chars,
            &self.fetch_config,
        )
        .await
        {
            Ok(result) => {
                {
                    let mut cache = self.fetch_cache.lock().expect("fetch cache poisoned");
                    cache.insert(&cache_key, result.clone(), self.fetch_cache_ttl);
                }
                Ok(ToolOutput::success(serde_json::to_string(&result)?))
            }
            Err(e) => Ok(ToolOutput::error(format!("{e:#}"))),
        }
    }
}

#[async_trait]
impl ToolExecutor for WebToolExecutor {
    async fn execute(
        &self,
        name: &str,
        arguments: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        match name {
            "web_search" => self.handle_search(arguments).await,
            "web_fetch" => self.handle_fetch(arguments).await,
            _ => Ok(ToolOutput::error(format!("unknown tool: {name}"))),
        }
    }

    fn tools(&self) -> Vec<ToolDef> {
        let mut tools = vec![Self::search_def()];
        if self.fetch_enabled {
            tools.push(Self::fetch_def());
        }
        tools
    }
}

fn build_search_config(config: &WebToolConfig) -> SearchConfig {
    let brave_key =
        web_search::resolve_key(config.search.brave.api_key.as_deref(), "BRAVE_API_KEY");
    let perplexity_key = web_search::resolve_key(
        config.search.perplexity.api_key.as_deref(),
        "PERPLEXITY_API_KEY",
    );
    let openrouter_key = web_search::resolve_key(None, "OPENROUTER_API_KEY");
    let grok_key = web_search::resolve_key(config.search.grok.api_key.as_deref(), "XAI_API_KEY");

    let provider = web_search::detect_provider(
        config.search.provider.as_deref(),
        brave_key,
        perplexity_key,
        openrouter_key,
        grok_key,
        config.search.perplexity.base_url.as_deref(),
        config.search.perplexity.model.as_deref(),
        config.search.grok.model.as_deref(),
    );

    SearchConfig {
        provider,
        max_results: config.search.max_results.unwrap_or(5),
        timeout: Duration::from_secs(u64::from(config.search.timeout_seconds.unwrap_or(30))),
    }
}

fn build_fetch_config(config: &WebToolConfig) -> FetchConfig {
    FetchConfig {
        max_chars: config.fetch.max_chars.unwrap_or(50_000),
        timeout: Duration::from_secs(u64::from(config.fetch.timeout_seconds.unwrap_or(30))),
        max_redirects: config.fetch.max_redirects.unwrap_or(3),
        user_agent: config
            .fetch
            .user_agent
            .clone()
            .unwrap_or_else(|| "Mozilla/5.0 (compatible; Coop/1.0)".to_owned()),
    }
}
