//! Prompt builder: assembles system prompts from layered workspace files with token awareness.
//!
//! Every piece of context has a known token cost. Trust level gates visibility.
//! Files that don't fit the budget overflow to a "priced menu" the agent can fetch on demand.

use crate::TrustLevel;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

static BPE: LazyLock<tiktoken_rs::CoreBPE> =
    LazyLock::new(|| tiktoken_rs::cl100k_base().expect("failed to load cl100k_base BPE"));

/// Count tokens in a string using cl100k_base encoding.
pub fn count_tokens(text: &str) -> usize {
    BPE.encode_ordinary(text).len()
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
/// Note: Anthropic's only cache type is `"ephemeral"` (~5 min TTL). Goose sends the entire
/// system prompt as one block. These hints drive *ordering*, not explicit cache breakpoints.
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
    /// Scan workspace files and build the index.
    pub fn scan(workspace: &Path, file_configs: &[PromptFileConfig]) -> Result<Self> {
        let mut files = HashMap::new();
        for cfg in file_configs {
            let full_path = workspace.join(&cfg.path);
            if let Some(indexed) = Self::index_file(&full_path, cfg)? {
                files.insert(cfg.path.clone(), indexed);
            }
        }
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

        let mut lines = vec!["## Available Context".to_string()];
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
            content: content.to_string(),
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
    token_budget: usize,
    file_configs: Vec<PromptFileConfig>,
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
            token_budget: Self::DEFAULT_BUDGET,
            file_configs: default_file_configs(),
        }
    }

    #[must_use]
    pub fn trust(mut self, trust: TrustLevel) -> Self {
        self.trust = trust;
        self
    }

    #[must_use]
    pub fn session_kind(mut self, kind: &str) -> Self {
        self.session_kind = Some(kind.to_string());
        self
    }

    #[must_use]
    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    #[must_use]
    pub fn channel(mut self, channel: &str) -> Self {
        self.channel = Some(channel.to_string());
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

    /// Build the prompt, using a pre-scanned workspace index.
    pub fn build(&self, index: &WorkspaceIndex) -> Result<BuiltPrompt> {
        let mut layers: Vec<PromptLayer> = Vec::new();
        let mut used_tokens: usize = 0;
        let mut overflow: Vec<MemoryIndexEntry> = Vec::new();

        // Reserve tokens for runtime context + memory index (layers 3-4).
        let runtime_reserve = 500;
        let file_budget = self.token_budget.saturating_sub(runtime_reserve);

        // --- Layers 0-2: Workspace files, ordered by config, trust-gated ---
        for cfg in &self.file_configs {
            // Trust gate: skip files the current trust level can't see.
            if self.trust > cfg.min_trust {
                continue;
            }

            let Some(indexed) = index.get(&cfg.path) else {
                continue; // Missing files are skipped silently.
            };

            let remaining = file_budget.saturating_sub(used_tokens);
            if remaining == 0 {
                // No budget left — everything goes to the menu.
                overflow.push(indexed.entry.clone());
                continue;
            }

            if indexed.entry.tokens <= remaining {
                // File fits entirely.
                let content = std::fs::read_to_string(self.workspace.join(&cfg.path))
                    .with_context(|| format!("failed to read {}", cfg.path))?;
                let counted = Counted::new(content);
                used_tokens += counted.tokens;
                layers.push(PromptLayer {
                    name: Self::layer_name(&cfg.path),
                    content: format!("## {}\n{}", cfg.path, counted.content),
                    tokens: counted.tokens,
                    cache: cfg.cache,
                });
            } else if remaining >= 200 {
                // Partial fit — truncate with marker.
                let content = std::fs::read_to_string(self.workspace.join(&cfg.path))
                    .with_context(|| format!("failed to read {}", cfg.path))?;
                let truncated = truncate_to_budget(&content, &cfg.path, remaining);
                used_tokens += truncated.tokens;
                layers.push(PromptLayer {
                    name: Self::layer_name(&cfg.path),
                    content: format!("## {}\n{}", cfg.path, truncated.content),
                    tokens: truncated.tokens,
                    cache: cfg.cache,
                });
            } else {
                // Too little room even for a truncation — send to menu.
                overflow.push(indexed.entry.clone());
            }
        }

        // --- Layer 3: Runtime context ---
        let runtime = self.build_runtime_context();
        used_tokens += runtime.tokens;
        layers.push(PromptLayer {
            name: "runtime",
            content: runtime.content,
            tokens: runtime.tokens,
            cache: CacheHint::Volatile,
        });

        // --- Layer 4: Memory index (priced menu) ---
        // Show overflow files + any trust-visible files not already inlined.
        let budget_remaining = self.token_budget.saturating_sub(used_tokens);
        let menu_entries = self.build_menu(index, &layers, &overflow);
        if !menu_entries.is_empty() {
            let menu = Self::render_menu(&menu_entries, budget_remaining);
            layers.push(PromptLayer {
                name: "memory_index",
                content: menu.content,
                tokens: menu.tokens,
                cache: CacheHint::Volatile,
            });
        }

        let total_tokens = layers.iter().map(|l| l.tokens).sum();
        let budget_remaining = self.token_budget.saturating_sub(total_tokens);

        Ok(BuiltPrompt {
            layers,
            total_tokens,
            available_via_tool: menu_entries,
            budget_remaining,
        })
    }

    fn build_runtime_context(&self) -> Counted {
        let mut parts = vec!["## Runtime".to_string()];

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
        let mut lines = vec!["## Available Context".to_string()];
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
    fn token_counts_are_exact() {
        let text = "The quick brown fox jumps over the lazy dog.";
        let expected = BPE.encode_ordinary(text).len();
        assert_eq!(count_tokens(text), expected);
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
}
