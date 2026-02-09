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

#[instrument(skip(memory), fields(trust = ?trust, limit = settings.limit, max_tokens = settings.max_tokens))]
pub(crate) async fn build_prompt_index(
    memory: &dyn Memory,
    trust: TrustLevel,
    settings: &MemoryPromptIndexConfig,
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

    let query = MemoryQuery {
        stores: stores.clone(),
        limit: settings.limit.max(1),
        ..Default::default()
    };

    let results = memory.search(&query).await?;
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
