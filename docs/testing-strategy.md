# Testing Strategy

## The Problem We're Solving

With OpenClaw, we couldn't know if something was fixed without testing it live. iMessage group routing? Send a real message, check if it routes correctly. Signal reconnection? Kill the connection, hope it recovers. Config patching? Apply it, see if agents disappear.

This is unacceptable. Every integration in Coop must be testable without the real external service.

## Core Principle: Traits at Every Boundary

Every point where our code touches something external is a trait. The real implementation talks to the outside world. Tests use fakes that simulate the behavior.

The boundary is the contract. If our code works correctly with the fake, and the real implementation fulfills the same contract, then it works in production.

```
Our Code ←→ Trait Boundary ←→ External World
                  ↕
              Fake (tests)
```

## The Boundaries

### 1. Channel Trait
The interface between Coop and messaging platforms.

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn id(&self) -> &str;
    async fn recv(&mut self) -> Result<InboundMessage>;
    async fn send(&self, msg: OutboundMessage) -> Result<SendReceipt>;
    async fn probe(&self) -> Result<ChannelHealth>;
}
```

**What we test with fakes:**
- Message arrives → routed to correct session
- Group message → correct trust ceiling applied
- Unknown sender → defaults to public trust
- Channel goes unhealthy → reconnect logic triggers
- Outbound message → correct channel receives it
- Media message → transcription pipeline invoked

**Fake implementation:**
```rust
pub struct FakeChannel {
    inbound: mpsc::Receiver<InboundMessage>,
    outbound: Arc<Mutex<Vec<OutboundMessage>>>,
    health: Arc<AtomicBool>,
}

impl FakeChannel {
    /// Simulate an inbound message
    pub fn inject(&self, msg: InboundMessage) { ... }
    
    /// Assert what was sent outbound
    pub fn sent_messages(&self) -> Vec<OutboundMessage> { ... }
    
    /// Simulate channel going down
    pub fn set_unhealthy(&self) { ... }
}
```

**What the real implementations handle (NOT our logic to test):**
- Signal protocol details
- Telegram Bot API specifics
- iMessage binary quirks (like `;+;` group detection)

**Integration test for real channels:**
Each real channel impl gets a small suite that tests its own contract — parse a known JSON payload into `InboundMessage`, serialize an `OutboundMessage` into the expected wire format. These are unit tests on the adapter, not end-to-end.

```rust
#[test]
fn signal_parses_group_message() {
    let raw = include_str!("fixtures/signal_group_msg.json");
    let msg = SignalChannel::parse_inbound(raw).unwrap();
    assert!(msg.is_group);
    assert_eq!(msg.sender, ContactId::Signal("+15555550100".into()));
}

#[test]
fn signal_parses_dm() {
    let raw = include_str!("fixtures/signal_dm.json");
    let msg = SignalChannel::parse_inbound(raw).unwrap();
    assert!(!msg.is_group);
}

#[test]
fn imessage_group_detection_from_guid() {
    // The exact bug we hit with OpenClaw
    let msg = IMessageChannel::parse_inbound(json!({
        "chat_guid": "any;+;06f6aa83",
        "is_group": false,  // binary lies!
    })).unwrap();
    assert!(msg.is_group);  // we detect from guid
}
```

### 2. Agent Runtime Trait
The interface between Coop and the LLM execution engine (Goose).

```rust
#[async_trait]
pub trait AgentRuntime: Send + Sync {
    /// Send a turn to the agent, get a response
    async fn turn(
        &self,
        messages: &[Message],
        system_prompt: &str,
        tools: &[ToolDef],
        on_token: &dyn Fn(String) + Send + Sync,
    ) -> Result<AgentResponse>;
}

pub struct AgentResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
}
```

**What we test with fakes:**
- Conversation history passed correctly to agent
- System prompt assembled with right trust-level context
- Tool calls dispatched to correct tool handler
- Tool results fed back into next turn
- Streaming tokens forwarded to the channel
- Token budget / compaction triggers

**Fake implementation:**
```rust
pub struct FakeAgentRuntime {
    responses: VecDeque<AgentResponse>,
}

impl FakeAgentRuntime {
    /// Queue a response the fake agent will return
    pub fn queue_response(&mut self, resp: AgentResponse) { ... }
    
    /// Queue a tool call the fake agent will make
    pub fn queue_tool_call(&mut self, tool: &str, args: Value) { ... }
}
```

### 3. Tool Trait
The interface between the agent runtime and tool execution.

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;  // JSON Schema for parameters
    async fn execute(&self, params: Value, ctx: &ToolContext) -> Result<ToolResult>;
}

pub struct ToolContext {
    pub trust: TrustLevel,
    pub workspace: PathBuf,
    pub session_key: SessionKey,
}
```

**What we test:**
- Tool respects trust level (exec denied at `familiar`)
- Tool operates within workspace bounds (no path traversal)
- Tool returns structured results
- Error handling (tool fails gracefully, agent gets error message)

**Fake implementation:**
```rust
pub struct FakeTool {
    name: String,
    results: VecDeque<ToolResult>,
    calls: Arc<Mutex<Vec<(Value, ToolContext)>>>,
}

impl FakeTool {
    /// Assert tool was called with expected params
    pub fn assert_called_with(&self, expected: Value) { ... }
    
    /// Assert tool was never called (trust denied)
    pub fn assert_not_called(&self) { ... }
}
```

### 4. Memory Trait
The interface between Coop and the memory/vector search layer.

```rust
#[async_trait]
pub trait MemoryIndex: Send + Sync {
    async fn search(&self, query: &str, stores: &[&str], limit: usize) -> Result<Vec<MemoryHit>>;
    async fn reindex(&self, store: &str, path: &Path) -> Result<()>;
}

pub struct MemoryHit {
    pub store: String,
    pub file: PathBuf,
    pub line_start: usize,
    pub line_end: usize,
    pub snippet: String,
    pub score: f32,
}
```

**What we test with fakes:**
- Search respects trust-level store filtering
- Agent at `familiar` trust can't get results from `private` store
- Memory index returns results in score order
- Reindexing picks up new/changed files

**Fake implementation:**
```rust
pub struct FakeMemoryIndex {
    entries: HashMap<String, Vec<MemoryHit>>,  // store -> hits
}

impl FakeMemoryIndex {
    pub fn with_entries(store: &str, hits: Vec<MemoryHit>) -> Self { ... }
}
```

### 5. Session Store Trait
The interface between session management and persistence.

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load(&self, key: &SessionKey) -> Result<Option<SessionState>>;
    async fn save(&self, key: &SessionKey, state: &SessionState) -> Result<()>;
    async fn list(&self, filter: SessionFilter) -> Result<Vec<SessionKey>>;
    async fn delete(&self, key: &SessionKey) -> Result<()>;
}
```

**Implementations:**
- `InMemorySessionStore` — for phase 1 and tests
- `SqliteSessionStore` — for production (phase 3)

Both implement the same trait. The session manager doesn't know or care which one it's using.

---

## What's Pure Logic (No Trait Needed)

Some things are just functions. They don't touch the outside world. Test them directly.

### Trust Resolution
```rust
pub fn resolve_trust(user: &User, situation: &Situation) -> TrustLevel {
    min(user.trust, situation.ceiling)
}

#[test]
fn owner_in_group_gets_familiar() {
    let alice = User { trust: TrustLevel::Full, .. };
    let group = Situation { ceiling: TrustLevel::Familiar };
    assert_eq!(resolve_trust(&alice, &group), TrustLevel::Familiar);
}

#[test]
fn unknown_user_in_dm_gets_public() {
    let unknown = User { trust: TrustLevel::Public, .. };
    let dm = Situation { ceiling: TrustLevel::Full };
    assert_eq!(resolve_trust(&unknown, &dm), TrustLevel::Public);
}
```

### Message Routing
```rust
pub fn route_message(msg: &InboundMessage, config: &Config) -> RouteDecision {
    // identify user, identify situation, resolve trust, pick session
}

#[test]
fn known_user_dm_routes_to_their_session() { ... }

#[test]
fn known_user_in_group_routes_to_group_session() { ... }

#[test]
fn unknown_user_gets_public_session() { ... }

#[test]
fn group_with_ceiling_override_uses_override() { ... }
```

### Config Parsing & Validation
```rust
#[test]
fn parses_minimal_config() { ... }

#[test]
fn rejects_config_missing_agent() { ... }

#[test]
fn trust_levels_must_be_ordered() { ... }

#[test]
fn config_patch_merges_users_by_name() {
    // THE BUG THAT WIPED OUR AGENTS
    let base = parse_config("...");
    let patch = json!({"users": [{"name": "carol", "trust": "familiar"}]});
    let merged = merge_config(&base, &patch);
    // All original users still present
    assert!(merged.users.iter().any(|u| u.name == "alice"));
    assert!(merged.users.iter().any(|u| u.name == "bob"));
    // New user added
    assert!(merged.users.iter().any(|u| u.name == "carol"));
}
```

### Prompt Assembly
```rust
#[test]
fn full_trust_prompt_includes_private_memory_index() { ... }

#[test]
fn familiar_trust_prompt_excludes_private_memory_index() { ... }

#[test]
fn prompt_includes_user_context() { ... }
```

---

## Test Hierarchy

```
Unit Tests (fast, no I/O)
├── Trust resolution
├── Message routing logic
├── Config parsing + validation
├── Config merge (patch) logic
├── Prompt assembly
├── Trust-gated tool filtering
└── Message content parsing (per-channel fixtures)

Integration Tests (with fakes, still fast)
├── Full message flow: inject → route → session → agent → response → outbound
├── Tool call flow: agent requests tool → trust check → execute → result
├── Memory search flow: agent searches → trust filters stores → results
├── Session lifecycle: create → turns → compaction → resume
├── Multi-user: two users same agent, different trust, different memory
└── Error flows: channel down, agent error, tool failure

Adapter Tests (per real integration, with fixtures)
├── Signal: parse real JSON payloads, serialize outbound
├── Telegram: parse webhook payloads, serialize Bot API calls
├── iMessage: parse imsg binary output, handle guid quirks
├── Goose: serialize/deserialize subprocess communication
└── SQLite: session store CRUD operations

End-to-End Tests (slow, optional, real services)
├── Goose with real LLM API (needs API key)
└── SQLite with real database
```

---

## Fixture-Driven Channel Testing

Every channel adapter ships with a `fixtures/` directory containing real (anonymized) payloads captured from production:

```
crates/coop-channels/src/signal/fixtures/
├── dm_text.json
├── dm_with_attachment.json
├── group_text.json
├── group_reaction.json
├── group_member_joined.json
└── voice_message.json
```

These are the ground truth. When we encounter a new bug (like the iMessage group detection issue), we capture the payload that triggered it, add it as a fixture, write a failing test, then fix it. The fixture stays forever — regression proof.

```rust
#[test]
fn regression_imessage_group_false_negative() {
    // Bug: imsg binary reports is_group=false for group chats
    // Fix: detect from guid containing ";+;"
    let raw = include_str!("fixtures/group_is_group_false.json");
    let msg = IMessageAdapter::parse(raw).unwrap();
    assert!(msg.is_group, "guid ;+; should override is_group=false");
}
```

---

## The Contract Test Pattern

For each trait, we write a shared test suite that any implementation must pass:

```rust
// In coop-agent/src/tests/runtime_contract.rs

pub fn test_runtime_contract(runtime: impl AgentRuntime) {
    test_simple_response(&runtime);
    test_tool_call_and_result(&runtime);
    test_streaming_tokens(&runtime);
    test_empty_history(&runtime);
}

// Then in each implementation:
#[test]
fn fake_runtime_fulfills_contract() {
    test_runtime_contract(FakeAgentRuntime::new());
}

#[test]
fn goose_runtime_fulfills_contract() {
    test_runtime_contract(GooseRuntime::new(test_config()));
}
```

This ensures the fake behaves like the real thing. If the fake passes but the real implementation fails, the bug is in the adapter, not our logic.

---

## What This Gets Us

1. **Confidence:** Change routing logic → run tests → know it works. No "deploy and hope."
2. **Regression proof:** Every bug becomes a fixture + test. It can never come back.
3. **Fast iteration:** Full test suite runs in seconds (no real APIs, no real services).
4. **Clear boundaries:** If a test fails, you know exactly which layer broke.
5. **Adapter isolation:** Signal breaks? Only Signal adapter tests fail. Core logic untouched.
