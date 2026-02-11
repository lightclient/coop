use std::collections::HashSet;

use anyhow::Result;
use coop_core::TrustLevel;
use coop_core::prompt::count_tokens;
use coop_memory::{Memory, MemoryQuery, ObservationIndex, accessible_stores};
use tracing::{debug, info, instrument};

use crate::config::MemoryPromptIndexConfig;

#[derive(Debug)]
struct RenderedPromptIndex {
    block: String,
    rendered_count: usize,
    token_estimate: usize,
    truncated: bool,
}

#[instrument(skip(memory, user_input), fields(trust = ?trust, limit = settings.limit, max_tokens = settings.max_tokens, has_user_input = !user_input.trim().is_empty()))]
pub(crate) async fn build_prompt_index(
    memory: &dyn Memory,
    trust: TrustLevel,
    settings: &MemoryPromptIndexConfig,
    user_input: &str,
) -> Result<Option<String>> {
    if !settings.enabled {
        info!(reason = "disabled", "memory prompt index skipped");
        return Ok(None);
    }

    let stores = accessible_stores(trust);
    if stores.is_empty() {
        info!(
            reason = "no_accessible_stores",
            "memory prompt index skipped"
        );
        return Ok(None);
    }

    let limit = settings.limit.max(1);

    // Search 1: recency-based (always runs)
    let recency_query = MemoryQuery {
        stores: stores.clone(),
        limit,
        ..Default::default()
    };
    let recency_results = memory.search(&recency_query).await?;

    // Search 2: query-relevant (only if user input has meaningful terms)
    let search_terms = extract_search_terms(user_input);
    let query_results = if let Some(terms) = &search_terms {
        let text_query = MemoryQuery {
            text: Some(terms.clone()),
            stores: stores.clone(),
            limit,
            ..Default::default()
        };
        match memory.search(&text_query).await {
            Ok(results) => {
                debug!(
                    query_result_count = results.len(),
                    "memory prompt index query-aware search complete"
                );
                results
            }
            Err(error) => {
                debug!(error = %error, "memory prompt index query-aware search failed, using recency only");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let results = merge_results(recency_results, query_results, limit);

    if results.is_empty() {
        info!(reason = "no_results", "memory prompt index skipped");
        return Ok(None);
    }

    let rendered = render_prompt_index(&results, settings.max_tokens.max(1));
    if rendered.rendered_count == 0 {
        info!(reason = "budget_exhausted", "memory prompt index skipped");
        return Ok(None);
    }

    info!(
        accessible_store_count = stores.len(),
        result_count = results.len(),
        rendered_count = rendered.rendered_count,
        token_estimate = rendered.token_estimate,
        truncated = rendered.truncated,
        "memory prompt index built"
    );

    Ok(Some(rendered.block))
}

/// Merge recency and query-relevant results, dedup by id.
///
/// Query-relevant results come first (they matched the user's input via
/// FTS/vector), then recency results fill remaining slots. Each group
/// is already internally sorted by its own scoring function, so we
/// don't re-sort across groups (recency and text-query scores use
/// different weight distributions and aren't directly comparable).
fn merge_results(
    recency: Vec<ObservationIndex>,
    query: Vec<ObservationIndex>,
    limit: usize,
) -> Vec<ObservationIndex> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(limit);

    // Query-relevant results first (already ranked by FTS/vector relevance).
    for result in query {
        if merged.len() >= limit {
            break;
        }
        if seen.insert(result.id) {
            merged.push(result);
        }
    }

    // Fill remaining slots with recency results.
    for result in recency {
        if merged.len() >= limit {
            break;
        }
        if seen.insert(result.id) {
            merged.push(result);
        }
    }

    merged
}

fn render_prompt_index(results: &[ObservationIndex], max_tokens: usize) -> RenderedPromptIndex {
    let mut lines = vec![
        "## Memory Index (DB)".to_owned(),
        "Use memory_get with observation IDs for full details.".to_owned(),
    ];

    let mut rendered_count = 0;
    let mut truncated = false;
    let mut token_estimate = count_tokens(&lines.join("\n"));

    for result in results {
        let line = format_row(result);
        let line_tokens = count_tokens(&line);

        if token_estimate.saturating_add(line_tokens) > max_tokens {
            truncated = true;
            break;
        }

        lines.push(line);
        token_estimate = token_estimate.saturating_add(line_tokens);
        rendered_count += 1;
    }

    if truncated {
        let marker = "- ... truncated to fit token budget.".to_owned();
        let marker_tokens = count_tokens(&marker);
        if token_estimate.saturating_add(marker_tokens) <= max_tokens || rendered_count > 0 {
            lines.push(marker);
            token_estimate = count_tokens(&lines.join("\n"));
        }
        debug!(rendered_count, max_tokens, "memory prompt index truncated");
    }

    RenderedPromptIndex {
        block: lines.join("\n"),
        rendered_count,
        token_estimate,
        truncated,
    }
}

fn format_row(entry: &ObservationIndex) -> String {
    format!(
        "- id={} store={} type={} title={} score={:.2} mentions={} created={}",
        entry.id,
        entry.store,
        entry.obs_type,
        compact_title(&entry.title),
        entry.score,
        entry.mention_count,
        entry.created_at.format("%Y-%m-%d"),
    )
}

/// Extract meaningful search terms from conversational user input.
///
/// FTS5 uses AND logic, so raw input like "tell me about the deployment
/// pipeline" would require every word to appear in the observation. We
/// strip common English stop words so FTS gets "deployment pipeline".
fn extract_search_terms(input: &str) -> Option<String> {
    #[rustfmt::skip]
    const STOP_WORDS: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could",
        "should", "may", "might", "can", "shall", "must",
        "i", "me", "my", "we", "our", "you", "your", "he", "she", "it",
        "its", "they", "them", "their", "this", "that", "these", "those",
        "in", "on", "at", "to", "for", "of", "with", "by", "from",
        "about", "into", "through", "during", "before", "after",
        "and", "or", "but", "not", "so", "if", "then", "else",
        "what", "which", "who", "when", "where", "how", "why",
        "all", "each", "every", "some", "any", "no", "just", "also",
        "tell", "show", "give", "get", "let", "know", "think",
        "please", "thanks", "ok", "hi", "hello",
    ];

    let terms: Vec<&str> = input
        .split_whitespace()
        .filter(|w| {
            let lower = w.to_lowercase();
            lower.len() >= 2 && !STOP_WORDS.contains(&lower.as_str())
        })
        .collect();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

fn compact_title(title: &str) -> String {
    let compact = title
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    if compact.chars().count() <= 80 {
        compact
    } else {
        compact.chars().take(80).collect::<String>() + "…"
    }
}
