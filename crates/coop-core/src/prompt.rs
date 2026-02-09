//! Prompt builder: assembles system prompts from layered workspace files with token awareness.
//!
//! Every piece of context has a known token cost. Trust level gates visibility.
//! Files that don't fit the budget overflow to a "priced menu" the agent can fetch on demand.

use crate::TrustLevel;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, info_span, trace};

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

/// Count tokens in a string using cl100k_base encoding.
///
/// When the `tokenizer` feature is disabled, falls back to a rough
/// chars/4 estimate (sufficient for prompt budgeting during dev builds).
#[cfg(feature = "tokenizer")]
pub fn count_tokens(text: &str) -> usize {
    static BPE: std::sync::LazyLock<tiktoken_rs::CoreBPE> = std::sync::LazyLock::new(|| {
        tiktoken_rs::cl100k_base().expect("failed to load cl100k_base BPE")
    });
    BPE.encode_ordinary(text).len()
}

#[cfg(not(feature = "tokenizer"))]
pub fn count_tokens(text: &str) -> usize {
    // Rough approximation: ~4 chars per token for English text.
    text.len() / 4
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// Content paired with its token cost.
#[derive(Debug, Clone)]
pub struct Counted {
    pub content: String,
    pub tokens: usize,
}

impl Counted {
    pub fn new(content: String) -> Self {
        let tokens = count_tokens(&content);
        Self { content, tokens }
    }

    pub fn empty() -> Self {
        Self {
            content: String::new(),
            tokens: 0,
        }
    }
}

/// Describes how often a prompt layer's content changes. Used to order layers so that
/// stable content comes first in the flat string — Anthropic's prefix caching automatically
/// gives ~90% input token discount on identical leading bytes across API calls.
///
/// Note: Anthropic's only cache type is `"ephemeral"` (~5 min TTL). These hints drive
/// *ordering*, not explicit cache breakpoints.
/// See `docs/system-prompt-design.md` for full details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheHint {
    /// Identity/behavior — same across turns and sessions.
    Stable,
    /// User context, workspace files — same within a session.
    Session,
    /// Runtime context, memory index — changes every turn.
    Volatile,
}

/// A single layer of the assembled prompt.
#[derive(Debug, Clone)]
pub struct PromptLayer {
    pub name: &'static str,
    pub content: String,
    pub tokens: usize,
    pub cache: CacheHint,
}

/// One entry in the "priced menu" shown to the agent.
#[derive(Debug, Clone)]
pub struct MemoryIndexEntry {
    pub path: String,
    pub tokens: usize,
    pub description: String,
    pub min_trust: TrustLevel,
}

/// The final assembled prompt.
#[derive(Debug)]
pub struct BuiltPrompt {
    pub layers: Vec<PromptLayer>,
    pub total_tokens: usize,
    pub available_via_tool: Vec<MemoryIndexEntry>,
    pub budget_remaining: usize,
}

impl BuiltPrompt {
    /// Concatenate all layers into a single flat string.
    pub fn to_flat_string(&self) -> String {
        self.layers
            .iter()
            .map(|l| l.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ---------------------------------------------------------------------------
// File config
// ---------------------------------------------------------------------------

/// Configuration for a single workspace file that may be included in the prompt.
#[derive(Debug, Clone)]
pub struct PromptFileConfig {
    /// Relative path within the workspace (e.g. "SOUL.md").
    pub path: String,
    /// Minimum trust level required to see this file.
    pub min_trust: TrustLevel,
    /// Cache hint for prompt caching.
    pub cache: CacheHint,
    /// One-line description for the memory index menu.
    pub description: String,
}

/// Default file conventions matching OpenClaw's proven semantic filenames.
pub fn default_file_configs() -> Vec<PromptFileConfig> {
    vec![
        PromptFileConfig {
            path: "SOUL.md".into(),
            min_trust: TrustLevel::Familiar,
            cache: CacheHint::Stable,
            description: "Agent personality and voice".into(),
        },
        PromptFileConfig {
            path: "AGENTS.md".into(),
            min_trust: TrustLevel::Familiar,
            cache: CacheHint::Stable,
            description: "Behavioral instructions".into(),
        },
        PromptFileConfig {
            path: "TOOLS.md".into(),
            min_trust: TrustLevel::Familiar,
            cache: CacheHint::Session,
            description: "Tool setup notes".into(),
        },
        PromptFileConfig {
            path: "IDENTITY.md".into(),
            min_trust: TrustLevel::Familiar,
            cache: CacheHint::Session,
            description: "Agent identity".into(),
        },
        PromptFileConfig {
            path: "USER.md".into(),
            min_trust: TrustLevel::Inner,
            cache: CacheHint::Session,
            description: "Per-user info".into(),
        },
        PromptFileConfig {
            path: "MEMORY.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Session,
            description: "Long-term curated memory".into(),
        },
        PromptFileConfig {
            path: "HEARTBEAT.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Volatile,
            description: "Periodic check tasks".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

/// A skill discovered from a `skills/*/SKILL.md` file.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Path to the SKILL.md relative to the workspace (e.g. "skills/tmux/SKILL.md").
    pub path: String,
}

/// Parse YAML frontmatter from a SKILL.md file, extracting `name` and `description`.
fn parse_skill_frontmatter(content: &str) -> Option<(String, String)> {
    let content = content.strip_prefix("---")?;
    let end = content.find("---")?;
    let frontmatter = &content[..end];

    let mut name = None;
    let mut description = None;

    for line in frontmatter.lines() {
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(rest.trim().trim_matches('"').to_owned());
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = Some(rest.trim().trim_matches('"').to_owned());
        }
    }

    Some((name?, description?))
}

/// Scan `{workspace}/skills/*/SKILL.md` for skill entries.
pub fn scan_skills(workspace: &Path) -> Vec<SkillEntry> {
    let skills_dir = workspace.join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Vec::new();
    };

    let mut skills = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let skill_file = entry.path().join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_file) else {
            continue;
        };
        let Some((name, description)) = parse_skill_frontmatter(&content) else {
            debug!(
                path = %skill_file.display(),
                "SKILL.md missing name/description frontmatter, skipping"
            );
            continue;
        };
        let rel_path = format!("skills/{}/SKILL.md", entry.file_name().to_string_lossy());
        debug!(skill = %name, path = %rel_path, "discovered skill");
        skills.push(SkillEntry {
            name,
            description,
            path: rel_path,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

// ---------------------------------------------------------------------------
// Channel context
// ---------------------------------------------------------------------------

/// Extract the channel family from a channel identifier.
///
/// Returns the part before the first colon, or the whole string if there is no
/// colon. E.g. `"terminal:default"` → `"terminal"`, `"signal"` → `"signal"`.
pub fn channel_family(channel: &str) -> &str {
    channel.split(':').next().unwrap_or(channel)
}

/// Built-in formatting instructions for known channels.
///
/// Returns `None` for channels that need no special formatting hints (e.g.
/// terminal, which supports rich text).
pub fn default_channel_prompt(channel: &str) -> Option<&'static str> {
    match channel_family(channel) {
        "signal" => Some(concat!(
            "You are responding via Signal messenger.\n",
            "\n",
            "Formatting: Signal renders everything as plain text. ",
            "Do not use markdown syntax: no asterisks, backticks, ",
            "code fences, headers, or bullet markers. Write in plain text ",
            "with short paragraphs. Use line breaks for structure.\n",
            "\n",
            "Tone: This is a chat conversation, not a terminal session. ",
            "Write like a knowledgeable friend texting — concise, warm, natural. ",
            "Match the user's energy and length. A quick question gets a quick answer. ",
            "Don't over-explain or narrate your process unless asked.\n",
            "\n",
            "Session continuity: Conversations on Signal may span hours or days. ",
            "When a user returns, just continue naturally — don't announce that ",
            "you're 'picking up where we left off' or summarize previous context ",
            "unprompted. If you need to recall something, just reference it ",
            "naturally ('right, the build issue' not 'Let me review our prior ",
            "conversation...').\n",
            "\n",
            "Reply behavior: The user receives ONE message from you at the end of ",
            "your turn — all your tool calls run silently and only the final text ",
            "reply is delivered. Do not narrate each tool call; just do the work ",
            "and share the result.\n",
            "\n",
            "Proactive updates (signal_send): For tasks that take a long time ",
            "(multi-step builds, complex research, spawning sub-agents), use ",
            "signal_send to notify the user BEFORE starting the long work. ",
            "Keep the heads-up brief and natural: 'This will take a minute, ",
            "I need to run the full test suite.' Then do the work. Your final ",
            "reply arrives separately when you're done. Use this sparingly — ",
            "only when the wait would otherwise be confusing.\n",
            "\n",
            "Tool call style: Default to silent tool use. Just call the tool. ",
            "Only narrate when it genuinely helps: multi-step work, risky ",
            "actions (deletions, config changes), or when the user asked you ",
            "to explain. Keep narration brief.\n",
            "\n",
            "Tool results: Share results conversationally, not as a log. ",
            "Bad: 'I ran the build command. Here is the output: [paste]'. ",
            "Good: 'Build is passing now, the fix was a missing lifetime bound.' ",
            "Only include raw output (logs, errors, code) when the user needs ",
            "to see it, and keep it brief.",
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Workspace index
// ---------------------------------------------------------------------------

/// Cached metadata about a workspace file: token count + mtime.
#[derive(Debug, Clone)]
struct IndexedFile {
    entry: MemoryIndexEntry,
    mtime: SystemTime,
    #[allow(dead_code)]
    cache: CacheHint,
}

/// Scans workspace files, caches token counts keyed by mtime for invalidation.
#[derive(Debug)]
pub struct WorkspaceIndex {
    files: HashMap<String, IndexedFile>,
}

impl WorkspaceIndex {
    /// Create an empty index (no files).
    pub fn empty() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    /// Scan workspace files and build the index.
    pub fn scan(workspace: &Path, file_configs: &[PromptFileConfig]) -> Result<Self> {
        let _span = info_span!("workspace_scan", workspace = %workspace.display()).entered();
        let mut files = HashMap::new();
        for cfg in file_configs {
            let full_path = workspace.join(&cfg.path);
            if let Some(indexed) = Self::index_file(&full_path, cfg)? {
                debug!(
                    file = %cfg.path,
                    tokens = indexed.entry.tokens,
                    min_trust = ?cfg.min_trust,
                    "indexed workspace file"
                );
                files.insert(cfg.path.clone(), indexed);
            } else {
                trace!(file = %cfg.path, "workspace file not found");
            }
        }
        debug!(file_count = files.len(), "workspace scan complete");
        Ok(Self { files })
    }

    /// Re-scan files whose mtime has changed. Returns true if anything changed.
    pub fn refresh(&mut self, workspace: &Path, file_configs: &[PromptFileConfig]) -> Result<bool> {
        let mut changed = false;
        for cfg in file_configs {
            let full_path = workspace.join(&cfg.path);
            let Ok(meta) = std::fs::metadata(&full_path) else {
                // File removed — drop from index.
                if self.files.remove(&cfg.path).is_some() {
                    debug!(file = %cfg.path, "workspace file removed from index");
                    changed = true;
                }
                continue;
            };
            let current_mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

            let needs_update = match self.files.get(&cfg.path) {
                Some(existing) => existing.mtime != current_mtime,
                None => true,
            };

            if needs_update && let Some(indexed) = Self::index_file(&full_path, cfg)? {
                debug!(
                    file = %cfg.path,
                    tokens = indexed.entry.tokens,
                    "workspace file re-indexed"
                );
                self.files.insert(cfg.path.clone(), indexed);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Get index entries visible at a given trust level.
    pub fn entries_for_trust(&self, trust: TrustLevel) -> Vec<&MemoryIndexEntry> {
        self.files
            .values()
            .filter(|f| trust <= f.entry.min_trust)
            .map(|f| &f.entry)
            .collect()
    }

    /// Render the "priced menu" of available files for a given trust level.
    pub fn render_index(&self, trust: TrustLevel, budget_remaining: usize) -> Counted {
        let entries = self.entries_for_trust(trust);
        if entries.is_empty() {
            return Counted::empty();
        }

        let mut lines = vec!["## Available Context".to_owned()];
        for entry in &entries {
            lines.push(format!(
                "- {} ({} tok) — {}",
                entry.path, entry.tokens, entry.description
            ));
        }
        lines.push(String::new());
        lines.push(format!(
            "Use memory_get to load what you need. Budget: ~{}k remaining.",
            budget_remaining / 1000
        ));

        Counted::new(lines.join("\n"))
    }

    /// Read and index a single file. Returns None if the file doesn't exist.
    fn index_file(full_path: &Path, cfg: &PromptFileConfig) -> Result<Option<IndexedFile>> {
        let metadata = match std::fs::metadata(full_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to stat {}", full_path.display()));
            }
        };

        let content = std::fs::read_to_string(full_path)
            .with_context(|| format!("failed to read {}", full_path.display()))?;
        let tokens = count_tokens(&content);
        let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);

        Ok(Some(IndexedFile {
            entry: MemoryIndexEntry {
                path: cfg.path.clone(),
                tokens,
                description: cfg.description.clone(),
                min_trust: cfg.min_trust,
            },
            mtime,
            cache: cfg.cache,
        }))
    }

    /// Get the indexed file metadata (for internal use by the builder).
    fn get(&self, path: &str) -> Option<&IndexedFile> {
        self.files.get(path)
    }
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// Truncate content to fit within a token budget, adding a marker if truncated.
fn truncate_to_budget(content: &str, path: &str, budget: usize) -> Counted {
    let tokens = count_tokens(content);
    if tokens <= budget {
        return Counted {
            content: content.to_owned(),
            tokens,
        };
    }

    // Binary-search-ish: take lines until we exceed budget, leaving room for the marker.
    let marker_budget = 30; // tokens reserved for the truncation marker
    let target = budget.saturating_sub(marker_budget);

    let lines: Vec<&str> = content.lines().collect();
    let mut kept = String::new();
    let mut kept_tokens = 0;
    let mut line_count = 0;

    for line in &lines {
        let line_with_nl = format!("{line}\n");
        let line_tokens = count_tokens(&line_with_nl);
        if kept_tokens + line_tokens > target {
            break;
        }
        kept.push_str(&line_with_nl);
        kept_tokens += line_tokens;
        line_count += 1;
    }

    let marker = format!(
        "\n[truncated at {kept_tokens}/{tokens} tokens — use memory_get(\"{path}\", from={line_count}) for remainder]"
    );
    kept.push_str(&marker);

    Counted::new(kept)
}

// ---------------------------------------------------------------------------
// Prompt builder
// ---------------------------------------------------------------------------

/// Builder for assembling a trust-gated, token-budgeted system prompt.
#[derive(Debug)]
pub struct PromptBuilder {
    workspace: PathBuf,
    agent_id: String,
    trust: TrustLevel,
    session_kind: Option<String>,
    model: Option<String>,
    channel: Option<String>,
    user: Option<String>,
    token_budget: usize,
    file_configs: Vec<PromptFileConfig>,
    user_file_configs: Vec<PromptFileConfig>,
    skills: Vec<SkillEntry>,
}

impl PromptBuilder {
    /// Default token budget: 30k tokens for the system prompt.
    const DEFAULT_BUDGET: usize = 30_000;

    pub fn new(workspace: PathBuf, agent_id: String) -> Self {
        Self {
            workspace,
            agent_id,
            trust: TrustLevel::Public,
            session_kind: None,
            model: None,
            channel: None,
            user: None,
            token_budget: Self::DEFAULT_BUDGET,
            file_configs: default_file_configs(),
            user_file_configs: Vec::new(),
            skills: Vec::new(),
        }
    }

    #[must_use]
    pub fn trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    #[must_use]
    pub fn session_kind(mut self, kind: &str) -> Self {
        self.session_kind = Some(kind.to_owned());
        self
    }

    #[must_use]
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    #[must_use]
    pub fn channel(mut self, channel: &str) -> Self {
        self.channel = Some(channel.to_owned());
        self
    }

    #[must_use]
    pub fn user(mut self, user: &str) -> Self {
        self.user = Some(user.to_owned());
        self
    }

    #[must_use]
    pub fn skills(mut self, skills: Vec<SkillEntry>) -> Self {
        self.skills = skills;
        self
    }

    #[must_use]
    pub fn token_budget(mut self, budget: usize) -> Self {
        self.token_budget = budget;
        self
    }

    #[must_use]
    pub fn file_configs(mut self, configs: Vec<PromptFileConfig>) -> Self {
        self.file_configs = configs;
        self
    }

    #[must_use]
    pub fn user_file_configs(mut self, configs: Vec<PromptFileConfig>) -> Self {
        self.user_file_configs = configs;
        self
    }

    /// Build the prompt, using a pre-scanned workspace index.
    pub fn build(&self, index: &WorkspaceIndex) -> Result<BuiltPrompt> {
        let _span = info_span!(
            "prompt_build",
            trust = ?self.trust,
            budget = self.token_budget,
            agent = %self.agent_id,
        )
        .entered();

        let runtime_reserve = 500;
        let file_budget = self.token_budget.saturating_sub(runtime_reserve);

        let (mut layers, mut used_tokens, mut overflow) =
            self.build_file_layers(index, file_budget)?;

        // Per-user file layers (after shared files, before runtime).
        if let Some(user) = &self.user
            && !self.user_file_configs.is_empty()
        {
            let remaining = file_budget.saturating_sub(used_tokens);
            let user_dir = self.workspace.join(format!("users/{user}"));
            let (extra_layers, extra_tokens, extra_overflow) =
                self.build_scoped_file_layers(&user_dir, &self.user_file_configs, remaining)?;
            used_tokens += extra_tokens;
            layers.extend(extra_layers);
            overflow.extend(extra_overflow);
        }

        // Channel context layer (after user memory, before runtime).
        if let Some(layer) = self.build_channel_context()? {
            used_tokens += layer.tokens;
            layers.push(layer);
        }

        // Runtime context layer.
        let runtime = self.build_runtime_context();
        used_tokens += runtime.tokens;
        layers.push(PromptLayer {
            name: "runtime",
            content: runtime.content,
            tokens: runtime.tokens,
            cache: CacheHint::Volatile,
        });

        // Skills index.
        if !self.skills.is_empty() {
            let skills_layer = Self::render_skills(&self.skills);
            used_tokens += skills_layer.tokens;
            layers.push(PromptLayer {
                name: "skills",
                content: skills_layer.content,
                tokens: skills_layer.tokens,
                cache: CacheHint::Stable,
            });
        }

        // Memory index (priced menu) for overflow + not-inlined files.
        let budget_remaining = self.token_budget.saturating_sub(used_tokens);
        let menu_entries = self.build_menu(index, &layers, &overflow);
        if !menu_entries.is_empty() {
            let menu = Self::render_menu(&menu_entries, budget_remaining);
            debug!(
                menu_items = menu_entries.len(),
                budget_remaining, "memory index menu added"
            );
            layers.push(PromptLayer {
                name: "memory_index",
                content: menu.content,
                tokens: menu.tokens,
                cache: CacheHint::Volatile,
            });
        }

        let total_tokens = layers.iter().map(|l| l.tokens).sum();
        let budget_remaining = self.token_budget.saturating_sub(total_tokens);

        let layer_names: Vec<&str> = layers.iter().map(|l| l.name).collect();
        debug!(
            total_tokens,
            budget_remaining,
            layer_count = layers.len(),
            ?layer_names,
            "prompt built"
        );

        Ok(BuiltPrompt {
            layers,
            total_tokens,
            available_via_tool: menu_entries,
            budget_remaining,
        })
    }

    /// Process workspace files: trust-gate, budget-check, include or overflow each.
    fn build_file_layers(
        &self,
        index: &WorkspaceIndex,
        file_budget: usize,
    ) -> Result<(Vec<PromptLayer>, usize, Vec<MemoryIndexEntry>)> {
        let mut layers = Vec::new();
        let mut used_tokens: usize = 0;
        let mut overflow = Vec::new();

        for cfg in &self.file_configs {
            if self.trust > cfg.min_trust {
                debug!(
                    file = %cfg.path,
                    file_trust = ?cfg.min_trust,
                    session_trust = ?self.trust,
                    "file excluded by trust gate"
                );
                continue;
            }

            let Some(indexed) = index.get(&cfg.path) else {
                trace!(file = %cfg.path, "file not in workspace");
                continue;
            };

            let remaining = file_budget.saturating_sub(used_tokens);
            if remaining == 0 {
                debug!(
                    file = %cfg.path,
                    tokens = indexed.entry.tokens,
                    "file overflowed to menu (no budget remaining)"
                );
                overflow.push(indexed.entry.clone());
                continue;
            }

            let header = Self::layer_header(&cfg.path);

            if indexed.entry.tokens <= remaining {
                let content = std::fs::read_to_string(self.workspace.join(&cfg.path))
                    .with_context(|| format!("failed to read {}", cfg.path))?;
                let counted = Counted::new(content);
                used_tokens += counted.tokens;
                debug!(
                    file = %cfg.path,
                    tokens = counted.tokens,
                    used_tokens,
                    "file included in prompt"
                );
                layers.push(PromptLayer {
                    name: Self::layer_name(&cfg.path),
                    content: format!("{header}\n{}", counted.content),
                    tokens: counted.tokens,
                    cache: cfg.cache,
                });
            } else if remaining >= 200 {
                let content = std::fs::read_to_string(self.workspace.join(&cfg.path))
                    .with_context(|| format!("failed to read {}", cfg.path))?;
                let truncated = truncate_to_budget(&content, &cfg.path, remaining);
                used_tokens += truncated.tokens;
                debug!(
                    file = %cfg.path,
                    original_tokens = indexed.entry.tokens,
                    truncated_tokens = truncated.tokens,
                    "file truncated to fit budget"
                );
                layers.push(PromptLayer {
                    name: Self::layer_name(&cfg.path),
                    content: format!("{header}\n{}", truncated.content),
                    tokens: truncated.tokens,
                    cache: cfg.cache,
                });
            } else {
                debug!(
                    file = %cfg.path,
                    tokens = indexed.entry.tokens,
                    remaining,
                    "file overflowed to menu (insufficient budget)"
                );
                overflow.push(indexed.entry.clone());
            }
        }

        Ok((layers, used_tokens, overflow))
    }

    /// Process files from an arbitrary root directory (used for per-user files).
    fn build_scoped_file_layers(
        &self,
        root: &Path,
        configs: &[PromptFileConfig],
        budget: usize,
    ) -> Result<(Vec<PromptLayer>, usize, Vec<MemoryIndexEntry>)> {
        let mut layers = Vec::new();
        let mut used_tokens: usize = 0;
        let mut overflow = Vec::new();

        for cfg in configs {
            if self.trust > cfg.min_trust {
                debug!(
                    file = %cfg.path,
                    file_trust = ?cfg.min_trust,
                    session_trust = ?self.trust,
                    scope = "user",
                    "user file excluded by trust gate"
                );
                continue;
            }

            let full_path = root.join(&cfg.path);
            let content = match std::fs::read_to_string(&full_path) {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    trace!(file = %cfg.path, scope = "user", "user file not found");
                    continue;
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("failed to read {}", full_path.display()));
                }
            };

            if content.trim().is_empty() {
                trace!(file = %cfg.path, scope = "user", "user file empty, skipping");
                continue;
            }

            let counted = Counted::new(content.clone());
            let remaining = budget.saturating_sub(used_tokens);

            if remaining == 0 {
                debug!(
                    file = %cfg.path,
                    tokens = counted.tokens,
                    scope = "user",
                    "user file overflowed to menu (no budget remaining)"
                );
                overflow.push(MemoryIndexEntry {
                    path: format!("user:{}", cfg.path),
                    tokens: counted.tokens,
                    description: cfg.description.clone(),
                    min_trust: cfg.min_trust,
                });
                continue;
            }

            let header = format!("## {} (user)", cfg.path);

            if counted.tokens <= remaining {
                used_tokens += counted.tokens;
                debug!(
                    file = %cfg.path,
                    tokens = counted.tokens,
                    used_tokens,
                    scope = "user",
                    "user file included in prompt"
                );
                layers.push(PromptLayer {
                    name: "user_file",
                    content: format!("{header}\n{content}"),
                    tokens: counted.tokens,
                    cache: cfg.cache,
                });
            } else if remaining >= 200 {
                let truncated = truncate_to_budget(&content, &cfg.path, remaining);
                used_tokens += truncated.tokens;
                debug!(
                    file = %cfg.path,
                    original_tokens = counted.tokens,
                    truncated_tokens = truncated.tokens,
                    scope = "user",
                    "user file truncated to fit budget"
                );
                layers.push(PromptLayer {
                    name: "user_file",
                    content: format!("{header}\n{}", truncated.content),
                    tokens: truncated.tokens,
                    cache: cfg.cache,
                });
            } else {
                debug!(
                    file = %cfg.path,
                    tokens = counted.tokens,
                    remaining,
                    scope = "user",
                    "user file overflowed to menu (insufficient budget)"
                );
                overflow.push(MemoryIndexEntry {
                    path: format!("user:{}", cfg.path),
                    tokens: counted.tokens,
                    description: cfg.description.clone(),
                    min_trust: cfg.min_trust,
                });
            }
        }

        Ok((layers, used_tokens, overflow))
    }

    fn render_skills(skills: &[SkillEntry]) -> Counted {
        let mut lines = vec![
            "## Skills".to_owned(),
            String::new(),
            "The following skills provide specialized instructions for specific tasks.".to_owned(),
            "Use read_file to load a skill when the task matches its description.".to_owned(),
            String::new(),
        ];
        for skill in skills {
            lines.push(format!(
                "- **{}** (`{}`) — {}",
                skill.name, skill.path, skill.description
            ));
        }
        Counted::new(lines.join("\n"))
    }

    /// Build a channel-specific formatting context layer.
    ///
    /// Checks for `channels/<family>.md` in the workspace first (user override),
    /// then falls back to built-in defaults from [`default_channel_prompt`].
    fn build_channel_context(&self) -> Result<Option<PromptLayer>> {
        let Some(channel) = &self.channel else {
            return Ok(None);
        };

        let family = channel_family(channel);

        // Try workspace file override first.
        let rel_path = format!("channels/{family}.md");
        let full_path = self.workspace.join(&rel_path);

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) if !c.trim().is_empty() => {
                debug!(channel, path = %rel_path, "using workspace channel prompt");
                c
            }
            Ok(_) => {
                trace!(channel, path = %rel_path, "workspace channel file empty, trying built-in");
                match default_channel_prompt(channel) {
                    Some(builtin) => builtin.to_owned(),
                    None => return Ok(None),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match default_channel_prompt(channel) {
                    Some(builtin) => {
                        debug!(channel, "using built-in channel prompt");
                        builtin.to_owned()
                    }
                    None => return Ok(None),
                }
            }
            Err(e) => return Err(e).with_context(|| format!("failed to read {rel_path}")),
        };

        let header = format!("## Channel: {family}");
        let counted = Counted::new(format!("{header}\n{content}"));

        debug!(
            channel,
            family,
            tokens = counted.tokens,
            "channel context included in prompt"
        );

        Ok(Some(PromptLayer {
            name: "channel_context",
            content: counted.content,
            tokens: counted.tokens,
            cache: CacheHint::Session,
        }))
    }

    fn build_runtime_context(&self) -> Counted {
        let mut parts = vec!["## Runtime".to_owned()];

        let now = chrono::Local::now();
        parts.push(format!("- Date/time: {}", now.format("%Y-%m-%d %H:%M %Z")));
        parts.push(format!("- Agent: {}", self.agent_id));

        if let Some(model) = &self.model {
            parts.push(format!("- Model: {model}"));
        }
        if let Some(channel) = &self.channel {
            parts.push(format!("- Channel: {channel}"));
        }
        if let Some(kind) = &self.session_kind {
            parts.push(format!("- Session: {kind}"));
        }
        if let Some(user) = &self.user {
            parts.push(format!("- User: {user}"));
            parts.push(format!("- User home: users/{user}/"));
        }
        parts.push(format!("- Trust: {:?}", self.trust));

        Counted::new(parts.join("\n"))
    }

    /// Collect entries that should appear in the tool menu (overflow + not-inlined).
    fn build_menu(
        &self,
        index: &WorkspaceIndex,
        inlined: &[PromptLayer],
        overflow: &[MemoryIndexEntry],
    ) -> Vec<MemoryIndexEntry> {
        let inlined_names: Vec<&str> = inlined.iter().map(|l| l.name).collect();
        let mut menu: Vec<MemoryIndexEntry> = overflow.to_vec();

        // Add trust-visible files that weren't inlined and aren't already in overflow.
        let overflow_paths: Vec<&str> = overflow.iter().map(|e| e.path.as_str()).collect();
        for entry in index.entries_for_trust(self.trust) {
            let layer_name = Self::layer_name(&entry.path);
            if !inlined_names.contains(&layer_name)
                && !overflow_paths.contains(&entry.path.as_str())
            {
                menu.push(entry.clone());
            }
        }

        menu
    }

    fn render_menu(entries: &[MemoryIndexEntry], budget_remaining: usize) -> Counted {
        let mut lines = vec!["## Available Context".to_owned()];
        for entry in entries {
            lines.push(format!(
                "- {} ({} tok) — {}",
                entry.path, entry.tokens, entry.description
            ));
        }
        lines.push(String::new());
        lines.push(format!(
            "Use memory_get to load what you need. Budget: ~{}k remaining.",
            budget_remaining / 1000
        ));
        Counted::new(lines.join("\n"))
    }

    fn layer_header(path: &str) -> String {
        format!("## {path}")
    }

    /// Derive a stable layer name from a file path.
    fn layer_name(path: &str) -> &'static str {
        // Leak a static string for each unique file — there are only ~7 of them.
        // This is fine because we have a small, fixed set of known files.
        match path {
            "SOUL.md" => "soul",
            "AGENTS.md" => "agents",
            "TOOLS.md" => "tools",
            "IDENTITY.md" => "identity",
            "USER.md" => "user",
            "MEMORY.md" => "memory",
            "HEARTBEAT.md" => "heartbeat",
            _ => "workspace_file",
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a temp workspace with given files.
    fn setup_workspace(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn count_tokens_works() {
        let tokens = count_tokens("Hello, world!");
        assert!(tokens > 0);
        assert!(tokens < 10);
    }

    #[test]
    fn counted_new() {
        let c = Counted::new("Hello, world!".into());
        assert_eq!(c.tokens, count_tokens("Hello, world!"));
    }

    #[test]
    fn full_trust_includes_private_files() {
        let dir = setup_workspace(&[
            ("SOUL.md", "I am an agent."),
            ("MEMORY.md", "Alice's birthday is January 1."),
            ("USER.md", "User info here."),
        ]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("I am an agent."),
            "SOUL.md should be included"
        );
        assert!(
            flat.contains("Alice's birthday"),
            "MEMORY.md should be included at full trust"
        );
        assert!(
            flat.contains("User info here"),
            "USER.md should be included at full trust"
        );
    }

    #[test]
    fn familiar_trust_excludes_private_files() {
        let dir = setup_workspace(&[
            ("SOUL.md", "I am an agent."),
            ("MEMORY.md", "Alice's birthday is January 1."),
            ("USER.md", "User info here."),
            ("HEARTBEAT.md", "Check email."),
        ]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Familiar)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("I am an agent."),
            "SOUL.md visible at familiar"
        );
        assert!(
            !flat.contains("Alice's birthday"),
            "MEMORY.md should NOT be visible at familiar"
        );
        assert!(
            !flat.contains("User info here"),
            "USER.md should NOT be visible at familiar"
        );
        assert!(
            !flat.contains("Check email"),
            "HEARTBEAT.md should NOT be visible at familiar"
        );

        // Private files should not appear in the available_via_tool menu either.
        let menu_paths: Vec<&str> = prompt
            .available_via_tool
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert!(
            !menu_paths.contains(&"MEMORY.md"),
            "MEMORY.md should NOT appear in menu at familiar trust"
        );
        assert!(
            !menu_paths.contains(&"HEARTBEAT.md"),
            "HEARTBEAT.md should NOT appear in menu at familiar trust"
        );
    }

    #[test]
    fn missing_files_are_skipped() {
        let dir = setup_workspace(&[("SOUL.md", "I exist.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(flat.contains("I exist."));
        assert!(!flat.contains("[MISSING]"));
    }

    #[test]
    fn overflow_files_appear_in_tool_menu() {
        // Create a file that exceeds a tiny budget.
        let big_content = "word ".repeat(5000); // ~5000 tokens
        let dir = setup_workspace(&[("SOUL.md", "I am an agent."), ("MEMORY.md", &big_content)]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .token_budget(200) // Very small budget.
            .build(&index)
            .unwrap();

        // SOUL.md might fit (it's tiny), but MEMORY.md should overflow.
        let menu_paths: Vec<&str> = prompt
            .available_via_tool
            .iter()
            .map(|e| e.path.as_str())
            .collect();
        assert!(
            menu_paths.contains(&"MEMORY.md"),
            "MEMORY.md should be in the tool menu when budget is exceeded"
        );
    }

    #[test]
    fn stable_layers_come_before_volatile() {
        let dir = setup_workspace(&[
            ("SOUL.md", "personality"),
            ("AGENTS.md", "behavior"),
            ("HEARTBEAT.md", "tasks"),
        ]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let cache_order: Vec<CacheHint> = prompt.layers.iter().map(|l| l.cache).collect();

        // Find positions of stable and volatile layers.
        let last_stable = cache_order.iter().rposition(|c| *c == CacheHint::Stable);
        let first_volatile = cache_order.iter().position(|c| *c == CacheHint::Volatile);

        if let (Some(ls), Some(fv)) = (last_stable, first_volatile) {
            assert!(
                ls < fv,
                "All Stable layers should come before Volatile layers. Order: {cache_order:?}"
            );
        }
    }

    #[test]
    fn budget_enforcement() {
        let content_a = "a ".repeat(100); // ~100 tokens
        let content_b = "b ".repeat(100);
        let dir = setup_workspace(&[("SOUL.md", &content_a), ("AGENTS.md", &content_b)]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .token_budget(150) // Only room for ~one file + runtime.
            .build(&index)
            .unwrap();

        // Total should not wildly exceed budget (the runtime layer is small but always included).
        assert!(
            prompt.total_tokens < 300,
            "Total tokens {} should be reasonably bounded",
            prompt.total_tokens
        );
    }

    #[test]
    #[cfg(feature = "tokenizer")]
    fn token_counts_are_exact() {
        let text = "The quick brown fox jumps over the lazy dog.";
        let bpe = tiktoken_rs::cl100k_base().unwrap();
        let expected = bpe.encode_ordinary(text).len();
        assert_eq!(count_tokens(text), expected);
    }

    #[test]
    #[cfg(not(feature = "tokenizer"))]
    fn token_counts_are_approximate() {
        let text = "The quick brown fox jumps over the lazy dog.";
        let approx = count_tokens(text);
        assert!(approx > 0);
        assert_eq!(approx, text.len() / 4);
    }

    #[test]
    fn workspace_index_mtime_invalidation() {
        let dir = setup_workspace(&[("SOUL.md", "version 1")]);
        let configs = default_file_configs();

        let mut index = WorkspaceIndex::scan(dir.path(), &configs).unwrap();
        let original_tokens = index.get("SOUL.md").unwrap().entry.tokens;

        // Modify the file.
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(
            dir.path().join("SOUL.md"),
            "version 2 with more content added",
        )
        .unwrap();

        let changed = index.refresh(dir.path(), &configs).unwrap();
        assert!(changed, "refresh should detect mtime change");

        let new_tokens = index.get("SOUL.md").unwrap().entry.tokens;
        assert_ne!(original_tokens, new_tokens, "token count should update");
    }

    #[test]
    fn public_trust_sees_nothing() {
        let dir = setup_workspace(&[("SOUL.md", "soul"), ("MEMORY.md", "private")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Public)
            .build(&index)
            .unwrap();

        // Only runtime layer should be present (no workspace files visible at public trust).
        let file_layers: Vec<&str> = prompt
            .layers
            .iter()
            .filter(|l| l.name != "runtime" && l.name != "memory_index")
            .map(|l| l.name)
            .collect();
        assert!(
            file_layers.is_empty(),
            "Public trust should see no workspace files, got: {file_layers:?}"
        );

        // Menu should also be empty.
        assert!(
            prompt.available_via_tool.is_empty(),
            "Public trust should have empty tool menu"
        );
    }

    #[test]
    fn truncation_adds_marker() {
        let content = (0..200)
            .map(|i| format!("Line {i}: some content here"))
            .collect::<Vec<_>>()
            .join("\n");
        let tokens = count_tokens(&content);
        assert!(tokens > 100, "test content should be large enough");

        let truncated = truncate_to_budget(&content, "TEST.md", 100);
        assert!(truncated.content.contains("[truncated at"));
        assert!(truncated.content.contains("memory_get"));
        assert!(truncated.tokens <= 130); // budget + marker overhead
    }

    #[test]
    fn runtime_layer_includes_metadata() {
        let dir = setup_workspace(&[]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .model("claude-opus-4-5")
            .channel("signal")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(flat.contains("Agent: reid"));
        assert!(flat.contains("Model: claude-opus-4-5"));
        assert!(flat.contains("Channel: signal"));
    }

    #[test]
    fn runtime_layer_includes_user_and_home() {
        let dir = setup_workspace(&[]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .user("alice")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(flat.contains("User: alice"), "should contain user name");
        assert!(
            flat.contains("User home: users/alice/"),
            "should contain user home dir"
        );
    }

    #[test]
    fn runtime_layer_omits_user_when_unset() {
        let dir = setup_workspace(&[]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(!flat.contains("User:"), "should not contain user line");
        assert!(
            !flat.contains("User home:"),
            "should not contain user home line"
        );
    }

    #[test]
    fn per_user_file_included_in_prompt() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("users/alice")).unwrap();
        fs::write(dir.path().join("users/alice/USER.md"), "Alice likes rust.").unwrap();

        let user_configs = vec![PromptFileConfig {
            path: "USER.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Session,
            description: "Per-user info".into(),
        }];

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .user("alice")
            .user_file_configs(user_configs)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("## USER.md (user)"),
            "should have per-user file header"
        );
        assert!(
            flat.contains("Alice likes rust."),
            "should include per-user file content"
        );

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            layer_names.contains(&"user_file"),
            "should have user_file layer, got: {layer_names:?}"
        );
    }

    #[test]
    fn per_user_file_not_included_without_user() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("users/alice")).unwrap();
        fs::write(dir.path().join("users/alice/USER.md"), "Alice likes rust.").unwrap();

        let user_configs = vec![PromptFileConfig {
            path: "USER.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Session,
            description: "Per-user info".into(),
        }];

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .user_file_configs(user_configs)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            !flat.contains("Alice likes rust."),
            "should not include per-user file without user set"
        );
    }

    #[test]
    fn shared_and_user_files_both_included() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent."), ("TOOLS.md", "Shared tools.")]);
        fs::create_dir_all(dir.path().join("users/alice")).unwrap();
        fs::write(dir.path().join("users/alice/TOOLS.md"), "Alice's tools.").unwrap();

        let user_configs = vec![PromptFileConfig {
            path: "TOOLS.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Session,
            description: "Per-user tool notes".into(),
        }];

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .user("alice")
            .user_file_configs(user_configs)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("Shared tools."),
            "shared TOOLS.md content should be present"
        );
        assert!(
            flat.contains("## TOOLS.md (user)"),
            "per-user TOOLS.md should have (user) header"
        );
        assert!(
            flat.contains("Alice's tools."),
            "per-user TOOLS.md content should be present"
        );
    }

    #[test]
    fn missing_per_user_file_is_fine() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let user_configs = vec![PromptFileConfig {
            path: "USER.md".into(),
            min_trust: TrustLevel::Full,
            cache: CacheHint::Session,
            description: "Per-user info".into(),
        }];

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .user("alice")
            .user_file_configs(user_configs)
            .build(&index)
            .unwrap();

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            !layer_names.contains(&"user_file"),
            "should not have user_file layer when file doesn't exist"
        );
    }

    #[test]
    fn parse_skill_frontmatter_extracts_name_and_description() {
        let content = "---\nname: tmux\ndescription: \"Remote control tmux\"\n---\n# Body";
        let (name, desc) = parse_skill_frontmatter(content).unwrap();
        assert_eq!(name, "tmux");
        assert_eq!(desc, "Remote control tmux");
    }

    #[test]
    fn parse_skill_frontmatter_returns_none_for_missing_fields() {
        assert!(parse_skill_frontmatter("---\nname: tmux\n---\n").is_none());
        assert!(parse_skill_frontmatter("---\ndescription: foo\n---\n").is_none());
        assert!(parse_skill_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn scan_skills_discovers_skill_files() {
        let dir = setup_workspace(&[]);
        fs::create_dir_all(dir.path().join("skills/tmux")).unwrap();
        fs::write(
            dir.path().join("skills/tmux/SKILL.md"),
            "---\nname: tmux\ndescription: \"Control tmux sessions\"\n---\n# tmux skill",
        )
        .unwrap();
        fs::create_dir_all(dir.path().join("skills/uv")).unwrap();
        fs::write(
            dir.path().join("skills/uv/SKILL.md"),
            "---\nname: uv\ndescription: \"Use uv for Python\"\n---\n# uv skill",
        )
        .unwrap();

        let skills = scan_skills(dir.path());
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "tmux");
        assert_eq!(skills[0].path, "skills/tmux/SKILL.md");
        assert_eq!(skills[1].name, "uv");
    }

    #[test]
    fn scan_skills_skips_invalid_frontmatter() {
        let dir = setup_workspace(&[]);
        fs::create_dir_all(dir.path().join("skills/broken")).unwrap();
        fs::write(
            dir.path().join("skills/broken/SKILL.md"),
            "no frontmatter here",
        )
        .unwrap();

        let skills = scan_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn scan_skills_returns_empty_without_skills_dir() {
        let dir = setup_workspace(&[]);
        let skills = scan_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn skills_included_in_prompt() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("skills/tmux")).unwrap();
        fs::write(
            dir.path().join("skills/tmux/SKILL.md"),
            "---\nname: tmux\ndescription: \"Control tmux sessions\"\n---\n# tmux",
        )
        .unwrap();

        let skills = scan_skills(dir.path());
        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .skills(skills)
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(flat.contains("## Skills"), "should have skills header");
        assert!(flat.contains("tmux"), "should list tmux skill");
        assert!(
            flat.contains("Control tmux sessions"),
            "should include skill description"
        );
        assert!(
            flat.contains("skills/tmux/SKILL.md"),
            "should include skill path"
        );

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            layer_names.contains(&"skills"),
            "should have skills layer, got: {layer_names:?}"
        );
    }

    #[test]
    fn no_skills_layer_when_empty() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "reid".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            !layer_names.contains(&"skills"),
            "should not have skills layer when no skills"
        );
    }

    // -- Channel context tests --

    #[test]
    fn channel_family_extracts_base_name() {
        assert_eq!(channel_family("signal"), "signal");
        assert_eq!(channel_family("terminal:default"), "terminal");
        assert_eq!(channel_family("discord:guild:123"), "discord");
        assert_eq!(channel_family(""), "");
    }

    #[test]
    fn default_channel_prompt_signal_returns_content() {
        let prompt = default_channel_prompt("signal");
        assert!(prompt.is_some());
        let text = prompt.unwrap();
        assert!(text.contains("Signal"), "should mention Signal");
        assert!(text.contains("plain text"), "should mention plain text");
    }

    #[test]
    fn default_channel_prompt_terminal_returns_none() {
        assert!(default_channel_prompt("terminal:default").is_none());
        assert!(default_channel_prompt("terminal").is_none());
    }

    #[test]
    fn default_channel_prompt_unknown_returns_none() {
        assert!(default_channel_prompt("discord").is_none());
        assert!(default_channel_prompt("webhook").is_none());
    }

    #[test]
    fn channel_context_included_for_signal() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("signal")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("## Channel: signal"),
            "should have channel context header"
        );
        assert!(
            flat.contains("plain text"),
            "should contain Signal formatting instructions"
        );

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            layer_names.contains(&"channel_context"),
            "should have channel_context layer, got: {layer_names:?}"
        );
    }

    #[test]
    fn channel_context_not_included_for_terminal() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("terminal:default")
            .build(&index)
            .unwrap();

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            !layer_names.contains(&"channel_context"),
            "terminal should not have channel_context layer"
        );
    }

    #[test]
    fn channel_context_not_included_without_channel() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .build(&index)
            .unwrap();

        let layer_names: Vec<&str> = prompt.layers.iter().map(|l| l.name).collect();
        assert!(
            !layer_names.contains(&"channel_context"),
            "no channel set = no channel_context layer"
        );
    }

    #[test]
    fn workspace_channel_file_overrides_builtin() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("channels")).unwrap();
        fs::write(
            dir.path().join("channels/signal.md"),
            "Custom signal instructions: be extra terse.",
        )
        .unwrap();

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("signal")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("Custom signal instructions"),
            "should use workspace file content"
        );
        assert!(
            !flat.contains("Signal has no formatting support"),
            "should NOT contain built-in default when workspace file exists"
        );
    }

    #[test]
    fn workspace_channel_file_for_unknown_channel() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("channels")).unwrap();
        fs::write(
            dir.path().join("channels/discord.md"),
            "Discord supports markdown. Use it.",
        )
        .unwrap();

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("discord")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("## Channel: discord"),
            "should have channel header"
        );
        assert!(
            flat.contains("Discord supports markdown"),
            "should use workspace file for unknown channel"
        );
    }

    #[test]
    fn empty_workspace_channel_file_falls_back_to_builtin() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);
        fs::create_dir_all(dir.path().join("channels")).unwrap();
        fs::write(dir.path().join("channels/signal.md"), "   \n  ").unwrap();

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("signal")
            .build(&index)
            .unwrap();

        let flat = prompt.to_flat_string();
        assert!(
            flat.contains("Signal renders everything as plain text"),
            "empty workspace file should fall back to built-in"
        );
    }

    #[test]
    fn channel_context_layer_is_session_cache() {
        let dir = setup_workspace(&[("SOUL.md", "I am an agent.")]);

        let index = WorkspaceIndex::scan(dir.path(), &default_file_configs()).unwrap();
        let prompt = PromptBuilder::new(dir.path().to_path_buf(), "test".into())
            .trust(TrustLevel::Full)
            .channel("signal")
            .build(&index)
            .unwrap();

        let channel_layer = prompt
            .layers
            .iter()
            .find(|l| l.name == "channel_context")
            .expect("should have channel_context layer");
        assert_eq!(
            channel_layer.cache,
            CacheHint::Session,
            "channel context should use Session cache hint"
        );
    }
}
