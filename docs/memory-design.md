# Memory System Design

## What We Learned From OpenClaw

Our current memory is flat files + vector search:
- MEMORY.md (~3,500 tokens, loaded every session in full)
- Daily notes (read 2 days on boot, ~4,000 tokens)
- RECENT.md (~1,100 tokens, rolling 7-day summary)
- `memory_search` does vector similarity over .md files
- `memory_get` reads lines from a file

**Problems:**
1. MEMORY.md grows unbounded — no pruning pressure
2. Everything is unstructured prose — hard to search precisely
3. No way to know what's relevant without reading it all
4. Daily notes are write-only dumps — never compressed or promoted
5. Vector search is the only retrieval path — no full-text, no filtering by type/date
6. Memory triage is manual (agent decides what to write where during heartbeats)

## What Claude-Mem Gets Right

### Structured Observations
Instead of freeform notes, every memory is a **typed observation** with structure:
- **title** — compressed summary (~10 words)
- **narrative** — what happened
- **facts** — extracted key facts
- **concepts** — tags for discoverability  
- **type** — decision, discovery, bugfix, change, etc.
- **files** — what files were read/modified
- **timestamp** — when it happened

This structure enables precise retrieval: "show me all decisions about the trust model" or "what files did we modify yesterday."

### 3-Layer Progressive Disclosure
1. **Search** → compact index (title, type, date, token cost). ~50-100 tokens per result.
2. **Timeline** → chronological context around a result. See what happened before/after.
3. **Get** → full observation details. ~500-1000 tokens each.

The agent sees the index first, decides what's relevant, then fetches details. No bulk loading.

### Automatic Capture
Claude-mem captures observations automatically on every tool use. The agent doesn't have to decide "should I write this down" — it's always written down. A background worker then compresses raw tool output into structured observations using an LLM.

### SQLite + FTS5
Not just vector search — full-text search with SQLite FTS5. Supports exact phrases, boolean queries, column-specific search. Fast, embedded, no external service.

## Coop Memory Design

### Core Idea: Structured Observations in SQLite

Replace flat files with a database of typed, structured observations. Keep files for human-editable content (SOUL.md, AGENTS.md), but everything the agent learns goes into the database.

### Schema

```sql
-- Every memory is an observation
CREATE TABLE observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_key TEXT,
    store TEXT NOT NULL,              -- 'private', 'shared', 'social'
    
    -- Structure
    type TEXT NOT NULL,               -- see types below
    title TEXT NOT NULL,              -- compressed ~10 word summary
    narrative TEXT,                   -- what happened (longer)
    facts TEXT,                       -- extracted key facts (JSON array)
    tags TEXT,                        -- concept tags (JSON array)
    
    -- Context
    source TEXT,                      -- 'auto' (tool capture), 'agent' (explicit write), 'manual' (human)
    related_files TEXT,               -- files involved (JSON array)
    related_people TEXT,              -- people mentioned (JSON array)
    
    -- Metadata
    token_count INTEGER,             -- approximate token cost to include
    created_at INTEGER NOT NULL,     -- unix epoch ms
    expires_at INTEGER,              -- optional TTL for ephemeral observations
    
    -- Trust classification
    -- Inherited from store, but explicit for query efficiency
    min_trust TEXT NOT NULL           -- minimum trust level to read this
);

-- Full-text search
CREATE VIRTUAL TABLE observations_fts USING fts5(
    title, narrative, facts, tags,
    content='observations',
    content_rowid='id'
);

-- Indexes
CREATE INDEX idx_obs_agent ON observations(agent_id);
CREATE INDEX idx_obs_store ON observations(store);
CREATE INDEX idx_obs_type ON observations(type);
CREATE INDEX idx_obs_created ON observations(created_at DESC);
CREATE INDEX idx_obs_trust ON observations(min_trust);
CREATE INDEX idx_obs_people ON observations(related_people);

-- Session summaries (generated on session end)
CREATE TABLE session_summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    
    request TEXT,                     -- what the user asked for
    outcome TEXT,                     -- what was accomplished
    decisions TEXT,                   -- key decisions made (JSON array)
    open_items TEXT,                  -- unresolved items (JSON array)
    
    observation_count INTEGER,
    created_at INTEGER NOT NULL
);

-- People index (promoted from observations)
CREATE TABLE people (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL,
    name TEXT NOT NULL,
    store TEXT NOT NULL,              -- 'private', 'shared', 'social'
    
    -- Structured facts
    facts TEXT,                       -- JSON: birthday, relationship, etc.
    last_mentioned INTEGER,          -- last observation referencing them
    mention_count INTEGER DEFAULT 0,
    
    UNIQUE(agent_id, name)
);
```

### Observation Types

```
person      — learned something about a person
decision    — made or recorded a decision  
preference  — user preference or standing rule
event       — something that happened (meeting, task, etc.)
technical   — infrastructure, config, code change
discovery   — learned something new
task        — something to do or was done
```

### Memory Trait (from testing-strategy.md)

```rust
#[async_trait]
pub trait Memory: Send + Sync {
    /// Search observations — returns compact index (Layer 1)
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ObservationIndex>>;
    
    /// Get timeline around an observation (Layer 2)
    async fn timeline(&self, anchor: i64, before: usize, after: usize) -> Result<Vec<ObservationIndex>>;
    
    /// Fetch full observation details (Layer 3)
    async fn get(&self, ids: &[i64]) -> Result<Vec<Observation>>;
    
    /// Write a new observation
    async fn write(&self, obs: NewObservation) -> Result<i64>;
    
    /// Get/update people index
    async fn people(&self, query: &str) -> Result<Vec<Person>>;
    
    /// Generate session summary
    async fn summarize_session(&self, session_key: &SessionKey) -> Result<SessionSummary>;
}

pub struct MemoryQuery {
    pub text: Option<String>,         // full-text search
    pub store: Vec<String>,           // filter by store (trust-gated)
    pub types: Vec<String>,           // filter by observation type
    pub people: Vec<String>,          // filter by related people
    pub after: Option<DateTime>,      // date range
    pub before: Option<DateTime>,
    pub limit: usize,
}

/// Layer 1: compact index entry (~50-100 tokens)
pub struct ObservationIndex {
    pub id: i64,
    pub title: String,
    pub obs_type: String,
    pub store: String,
    pub created_at: DateTime,
    pub token_count: u32,
    pub related_people: Vec<String>,
}

/// Layer 3: full observation (~200-1000 tokens)  
pub struct Observation {
    pub id: i64,
    pub title: String,
    pub narrative: String,
    pub facts: Vec<String>,
    pub tags: Vec<String>,
    pub obs_type: String,
    pub store: String,
    pub related_files: Vec<String>,
    pub related_people: Vec<String>,
    pub created_at: DateTime,
    pub token_count: u32,
}
```

### How It Works in Practice

#### Session Start (Prompt Assembly)

```
System prompt:
  1. Agent personality (SOUL.md, AGENTS.md)         — ~1,500 tokens
  2. User context (who they are, trust level)        — ~200 tokens  
  3. Recent observation index (last 48h, titles only) — ~500 tokens
  4. Tool descriptions (including memory tools)       — ~300 tokens
  
  Total boot: ~2,500 tokens (vs ~13,000 today)
```

The recent observation index at boot looks like:

```
## Recent Memory (last 48h)
| ID | Time | Type | Title | Tokens |
|----|------|------|-------|--------|
| 847 | 2h ago | decision | Trust model: Bell-LaPadula inspired | ~120 |
| 846 | 2h ago | technical | Coop architecture: Rust agent gateway | ~350 |
| 845 | 3h ago | discovery | OpenClaw competitors: nothing matches full stack | ~200 |
| 844 | 5h ago | technical | Config patch bug: arrays replaced not merged | ~90 |

→ Use memory.search / memory.timeline / memory.get tools for details and older memories.
```

#### During Session (Agent Retrieves)

Agent uses 3-layer tools:
```
1. memory.search(query: "coop trust model", types: ["decision"]) 
   → returns index: [{id: 847, title: "Trust model: Bell-LaPadula inspired", tokens: 120}]

2. memory.timeline(anchor: 847, before: 3, after: 0)
   → returns context: what happened right before the trust decision

3. memory.get(ids: [847, 846])
   → returns full observations with narrative, facts, tags
```

#### During Session (Auto-Capture)

After every tool use, Coop captures what happened:
```rust
// After agent calls exec("ls -la ~/dev/coop")
auto_capture(ToolObservation {
    tool: "exec",
    input: "ls -la ~/dev/coop",
    output: "...",  // truncated
    session_key: current_session,
});
```

A background task (or the agent itself during idle moments) compresses raw tool captures into structured observations:

```
Raw: exec("ls -la ~/dev/coop") → [file listing]
Compressed: Observation {
    type: "technical",
    title: "Explored coop repo structure",
    facts: ["repo at ~/dev/coop", "contains docs/, no src/ yet"],
    tags: ["coop", "project-setup"],
    token_count: 45,
}
```

#### Session End (Summary)

When a session ends, generate a summary:
```
SessionSummary {
    request: "Design coop memory system inspired by claude-mem",
    outcome: "Designed structured observation model with SQLite + FTS5, 3-layer progressive disclosure",
    decisions: ["Use SQLite not flat files", "Typed observations", "Auto-capture tool use"],
    open_items: ["Implement compression worker", "Decide on vector search addition"],
}
```

### What About Files?

Files still exist for:
- **SOUL.md, AGENTS.md** — human-editable agent behavior (loaded at boot)
- **User workspace files** — whatever the user puts in their workspace
- **Agent workspace** — scratch space the agent can use

But memory (what the agent knows, remembers, has learned) lives in the database. No more MEMORY.md growing forever. No more daily notes that never get read again.

### Migration from Flat Files

For users coming from file-based memory (or OpenClaw):
```
coop memory import --file MEMORY.md --store private
```
Parse the markdown, extract observations, write to SQLite. One-time migration.

### Trust Gating

Memory queries are always filtered by trust:
```rust
impl SqliteMemory {
    async fn search(&self, query: &MemoryQuery, trust: TrustLevel) -> Result<Vec<ObservationIndex>> {
        let accessible_stores = trust.accessible_stores();
        
        // FTS5 search, filtered to accessible stores only
        sqlx::query_as!(ObservationIndex,
            "SELECT id, title, type, store, created_at, token_count, related_people
             FROM observations
             WHERE observations.id IN (
                 SELECT rowid FROM observations_fts WHERE observations_fts MATCH ?
             )
             AND store IN (?)
             ORDER BY created_at DESC
             LIMIT ?",
            query.text, accessible_stores, query.limit
        ).fetch_all(&self.pool).await
    }
}
```

A `familiar` trust user searching for "Alice's salary" gets zero results — the observation is in the `private` store, which `familiar` can't access.

### Vector Search (Optional, Phase 2)

Start with FTS5 only. Add vector embeddings later if full-text isn't sufficient:
- Embed observation titles + narratives
- Store in SQLite with `sqlite-vec` extension (pure SQLite, no ChromaDB dependency)
- Hybrid search: FTS5 for exact matches, vector for semantic similarity

### Retention & Compression

Observations accumulate. Over time:
1. **Recent (< 7 days):** Full observations, all fields
2. **Medium (7-90 days):** Title + facts preserved, narrative compressed
3. **Archive (> 90 days):** Title + facts only, narrative dropped
4. **Ephemeral:** Observations with `expires_at` get auto-deleted (e.g. "build output was X")

The people index is permanent — never compressed, never deleted.

### Comparison

| Aspect | OpenClaw (current) | Coop |
|--------|-------------------|------|
| Storage | Flat .md files | SQLite + FTS5 |
| Boot cost | ~13,000 tokens (bulk load) | ~2,500 tokens (index only) |
| Search | Vector similarity only | FTS5 + optional vector |
| Structure | Freeform prose | Typed observations with fields |
| Capture | Manual (agent decides) | Automatic (tool use) + manual |
| Retrieval | Read file, hope it's there | 3-layer: search → timeline → get |
| Filtering | None (read whole file) | By type, date, person, store, trust |
| People | Mixed into MEMORY.md | Dedicated people table |
| Pruning | Manual during heartbeats | Automatic retention tiers |
| Trust gating | Separate dirs + extraPaths | Query-level store filtering |
