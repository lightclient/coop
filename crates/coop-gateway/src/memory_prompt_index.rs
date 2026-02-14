use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{Duration, Utc};
use coop_core::TrustLevel;
use coop_core::prompt::count_tokens;
use coop_memory::{
    Memory, MemoryQuery, ObservationIndex, SessionSummary, accessible_stores, normalize_file_path,
};
use tracing::{debug, instrument};

use crate::config::MemoryPromptIndexConfig;

#[derive(Debug)]
struct RenderedPromptIndex {
    block: String,
    rendered_count: usize,
    rendered_file_linked_count: usize,
    rendered_session_count: usize,
    token_estimate: usize,
    truncated: bool,
}

#[derive(Debug, Clone)]
struct FileLinkedObservation {
    index: ObservationIndex,
    files: Vec<String>,
}

#[allow(clippy::too_many_lines)]
#[instrument(skip(memory, user_input), fields(trust = ?trust, limit = settings.limit, max_tokens = settings.max_tokens, recent_days = settings.recent_days, include_file_links = settings.include_file_links, has_user_input = !user_input.trim().is_empty()))]
pub(crate) async fn build_prompt_index(
    memory: &dyn Memory,
    trust: TrustLevel,
    settings: &MemoryPromptIndexConfig,
    user_input: &str,
) -> Result<Option<String>> {
    if !settings.enabled {
        debug!(reason = "disabled", "memory prompt index skipped");
        return Ok(None);
    }

    let stores = accessible_stores(trust);
    if stores.is_empty() {
        debug!(
            reason = "no_accessible_stores",
            "memory prompt index skipped"
        );
        return Ok(None);
    }

    let limit = settings.limit.max(1);

    let recent_cutoff = Utc::now() - Duration::days(i64::from(settings.recent_days));
    let recent_query = MemoryQuery {
        stores: stores.clone(),
        after: Some(recent_cutoff),
        limit,
        ..Default::default()
    };
    let recent_results = memory.search(&recent_query).await?;

    let search_terms = extract_search_terms(user_input);
    let relevance_results = if let Some(terms) = &search_terms {
        let text_query = MemoryQuery {
            text: Some(terms.clone()),
            stores: stores.clone(),
            limit,
            ..Default::default()
        };
        match memory.search(&text_query).await {
            Ok(results) => {
                debug!(
                    relevance_result_count = results.len(),
                    "memory prompt index relevance search complete"
                );
                results
            }
            Err(error) => {
                debug!(error = %error, "memory prompt index relevance search failed, using recent results only");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let file_linked_results = if settings.include_file_links {
        let file_paths = extract_file_paths(user_input);
        if file_paths.is_empty() {
            Vec::new()
        } else {
            let mut linked_indexes = Vec::new();
            for path in &file_paths {
                let prefix_match = path.ends_with('/');
                match memory.search_by_file(path, prefix_match, 5).await {
                    Ok(rows) => {
                        linked_indexes
                            .extend(rows.into_iter().filter(|row| stores.contains(&row.store)));
                    }
                    Err(error) => {
                        debug!(
                            path = %path,
                            prefix_match,
                            error = %error,
                            "memory prompt index file search failed"
                        );
                    }
                }
            }

            debug!(
                path_count = file_paths.len(),
                observation_count = linked_indexes.len(),
                "file_linked_observations"
            );

            hydrate_file_links(memory, linked_indexes, limit).await
        }
    } else {
        Vec::new()
    };

    let mut results = merge_results(recent_results, relevance_results, limit);
    let file_linked_indexes = file_linked_results
        .iter()
        .map(|row| row.index.clone())
        .collect::<Vec<_>>();
    results = merge_additional_results(results, file_linked_indexes, limit);

    if results.is_empty() && file_linked_results.is_empty() {
        debug!(reason = "no_results", "memory prompt index skipped");
        return Ok(None);
    }

    let session_summaries = if trust == TrustLevel::Full {
        match memory.recent_session_summaries(5).await {
            Ok(summaries) => summaries,
            Err(error) => {
                debug!(
                    error = %error,
                    "failed to load recent session summaries for prompt index"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let rendered = render_prompt_index(
        &results,
        &file_linked_results,
        &session_summaries,
        settings.max_tokens.max(1),
    );
    if rendered.rendered_count == 0 && rendered.rendered_file_linked_count == 0 {
        debug!(reason = "budget_exhausted", "memory prompt index skipped");
        return Ok(None);
    }

    debug!(
        accessible_store_count = stores.len(),
        result_count = results.len(),
        file_linked_result_count = file_linked_results.len(),
        rendered_count = rendered.rendered_count,
        rendered_file_linked_count = rendered.rendered_file_linked_count,
        rendered_session_count = rendered.rendered_session_count,
        token_estimate = rendered.token_estimate,
        truncated = rendered.truncated,
        "memory prompt index built"
    );

    Ok(Some(rendered.block))
}

/// Merge recent and relevance results, dedup by id.
///
/// Recent results come first to guarantee short-term continuity.
/// Relevance results fill remaining slots for long-term recall.
/// Each group keeps its internal ordering.
fn merge_results(
    recent: Vec<ObservationIndex>,
    relevance: Vec<ObservationIndex>,
    limit: usize,
) -> Vec<ObservationIndex> {
    let mut seen = HashSet::new();
    let mut merged = Vec::with_capacity(limit);

    for result in recent {
        if merged.len() >= limit {
            break;
        }
        if seen.insert(result.id) {
            merged.push(result);
        }
    }

    for result in relevance {
        if merged.len() >= limit {
            break;
        }
        if seen.insert(result.id) {
            merged.push(result);
        }
    }

    merged
}

fn merge_additional_results(
    existing: Vec<ObservationIndex>,
    additional: Vec<ObservationIndex>,
    limit: usize,
) -> Vec<ObservationIndex> {
    if existing.len() >= limit {
        return existing;
    }

    let mut seen = existing.iter().map(|row| row.id).collect::<HashSet<_>>();
    let mut merged = existing;

    for result in additional {
        if merged.len() >= limit {
            break;
        }
        if seen.insert(result.id) {
            merged.push(result);
        }
    }

    merged
}

async fn hydrate_file_links(
    memory: &dyn Memory,
    linked_indexes: Vec<ObservationIndex>,
    limit: usize,
) -> Vec<FileLinkedObservation> {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for row in linked_indexes {
        if deduped.len() >= limit {
            break;
        }
        if seen.insert(row.id) {
            deduped.push(row);
        }
    }

    if deduped.is_empty() {
        return Vec::new();
    }

    let ids = deduped.iter().map(|row| row.id).collect::<Vec<_>>();
    let details = match memory.get(&ids).await {
        Ok(rows) => rows,
        Err(error) => {
            debug!(error = %error, "memory prompt index file detail fetch failed");
            return deduped
                .into_iter()
                .map(|index| FileLinkedObservation {
                    index,
                    files: Vec::new(),
                })
                .collect();
        }
    };

    let files_by_id = details
        .into_iter()
        .map(|obs| {
            let mut files = Vec::new();
            for file in obs.related_files {
                let normalized = normalize_file_path(&file);
                if normalized.is_empty() || files.contains(&normalized) {
                    continue;
                }
                files.push(normalized);
            }
            (obs.id, files)
        })
        .collect::<HashMap<_, _>>();

    deduped
        .into_iter()
        .map(|index| FileLinkedObservation {
            files: files_by_id.get(&index.id).cloned().unwrap_or_default(),
            index,
        })
        .collect()
}

fn render_prompt_index(
    results: &[ObservationIndex],
    file_linked: &[FileLinkedObservation],
    summaries: &[SessionSummary],
    max_tokens: usize,
) -> RenderedPromptIndex {
    let mut lines = vec![
        "## Memory Index (DB)".to_owned(),
        "Use memory_get with observation IDs for full details.".to_owned(),
    ];

    let mut rendered_count = 0;
    let mut rendered_file_linked_count = 0;
    let mut rendered_session_count = 0;
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

    if !file_linked.is_empty() {
        let heading = "### File-linked observations".to_owned();
        let heading_tokens = count_tokens(&heading);

        if token_estimate.saturating_add(heading_tokens) <= max_tokens {
            lines.push(String::new());
            lines.push(heading);
            token_estimate = count_tokens(&lines.join("\n"));

            for row in file_linked {
                let line = format_file_linked_row(row);
                let line_tokens = count_tokens(&line);
                if token_estimate.saturating_add(line_tokens) > max_tokens {
                    truncated = true;
                    break;
                }

                lines.push(line);
                token_estimate = token_estimate.saturating_add(line_tokens);
                rendered_file_linked_count += 1;
            }
        } else {
            truncated = true;
        }
    }

    if !summaries.is_empty() && rendered_count > 0 {
        let heading = "## Recent Sessions".to_owned();
        let heading_tokens = count_tokens(&heading);
        if token_estimate.saturating_add(heading_tokens) <= max_tokens {
            lines.push(String::new());
            lines.push(heading);
            token_estimate = count_tokens(&lines.join("\n"));

            for summary in summaries {
                let line = format_session_summary(summary);
                let line_tokens = count_tokens(&line);
                if token_estimate.saturating_add(line_tokens) > max_tokens {
                    truncated = true;
                    break;
                }

                lines.push(line);
                token_estimate = token_estimate.saturating_add(line_tokens);
                rendered_session_count += 1;
            }
        } else {
            truncated = true;
        }
    }

    if truncated {
        let marker = "- ... truncated to fit token budget.".to_owned();
        let marker_tokens = count_tokens(&marker);
        if token_estimate.saturating_add(marker_tokens) <= max_tokens
            || rendered_count > 0
            || rendered_file_linked_count > 0
        {
            lines.push(marker);
            token_estimate = count_tokens(&lines.join("\n"));
        }
        debug!(rendered_count, max_tokens, "memory prompt index truncated");
    }

    RenderedPromptIndex {
        block: lines.join("\n"),
        rendered_count,
        rendered_file_linked_count,
        rendered_session_count,
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

fn format_file_linked_row(row: &FileLinkedObservation) -> String {
    let files = if row.files.is_empty() {
        "-".to_owned()
    } else {
        row.files
            .iter()
            .map(|file| short_file_label(file))
            .collect::<Vec<_>>()
            .join(", ")
    };

    format!(
        "- id={} files=[{}] title={}",
        row.index.id,
        files,
        compact_title(&row.index.title),
    )
}

fn short_file_label(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let label = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if label.is_empty() {
        trimmed.to_owned()
    } else {
        label.to_owned()
    }
}

fn format_session_summary(summary: &SessionSummary) -> String {
    let request = compact_title(&summary.request);
    let outcome = compact_title(&summary.outcome);
    format!(
        "- session={} request={} outcome={} obs={} date={}",
        summary.session_key,
        if request.is_empty() { "-" } else { &request },
        if outcome.is_empty() { "-" } else { &outcome },
        summary.observation_count,
        summary.created_at.format("%Y-%m-%d"),
    )
}

/// Extract meaningful search terms from conversational user input.
///
/// Tokenizes on alphanumeric boundaries (like OpenClaw's regex approach),
/// which naturally decomposes contractions and possessives:
/// `"what's"` → `["what", "s"]`, `"Ariel's"` → `["Ariel", "s"]`.
/// Then filters stop words so FTS gets clean content terms.
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
        "tell", "show", "give", "get", "let", "know", "think", "make",
        "many", "much", "very", "really", "going",
        "please", "thanks", "ok", "hi", "hello",
        "s", "t", "re", "ve", "ll", "d", "m",
    ];

    let terms: Vec<&str> = alphanumeric_tokens(input)
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

fn extract_file_paths(input: &str) -> Vec<String> {
    let mut paths = Vec::new();

    for token in input.split_whitespace() {
        let trimmed = token
            .trim_start_matches(['(', '[', '{', '"', '\'', '`'])
            .trim_end_matches([',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '`'])
            .trim_end_matches('.');

        if trimmed.is_empty() {
            continue;
        }

        let looks_like_path = trimmed.contains('/') || trimmed.starts_with("./");
        if !looks_like_path || trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            continue;
        }

        let normalized = normalize_file_path(trimmed);
        if normalized.is_empty() {
            continue;
        }

        let is_directory = trimmed.ends_with('/');
        let has_extension = normalized
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty());

        if !is_directory && !has_extension {
            continue;
        }

        if !paths.contains(&normalized) {
            paths.push(normalized);
        }
    }

    paths
}

/// Iterate alphanumeric token runs from input.
///
/// Splits on any non-alphanumeric/underscore boundary, so punctuation,
/// apostrophes, hyphens, and unicode quotes all act as delimiters.
/// `"what's the Ariel's recipe?"` → `["what", "s", "the", "Ariel", "s", "recipe"]`
fn alphanumeric_tokens(input: &str) -> impl Iterator<Item = &str> {
    input
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
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
