# Coop Architecture

## What Is Coop
A personal agent gateway built in Rust. Coop manages the lifecycle of AI agents — routing messages from channels (Signal, Telegram, terminal, etc.) to agent sessions, enforcing trust-based access control, persisting conversations, and scheduling background work.

The agent runtime (LLM tool-calling loop, MCP, compaction) is delegated to Goose. Coop is everything around it.

## Core Concepts

- **Agent** — the mind. Model, personality, tools, MCP extensions.
- **User** — a person. Has a trust level, contact identifiers, optional workspace.
- **Session** — a conversation between a user and an agent. Owns message history and context.
- **Channel** — a transport (Signal, terminal, webchat). Receives and sends messages.
- **Memory Store** — classified collection of memories. Trust level determines visibility.

## Trust Model

Two inputs determine what the agent can access in any session:

1. **Trust** — per user. How much the agent trusts this person.
2. **Ceiling** — per situation. Maximum disclosure appropriate for the context.

Effective trust = `min(user.trust, situation.ceiling)`. Situations can only lower access.

```
Trust Levels (ordered):
  full     → [private, shared, social]
  inner    → [shared, social]
  familiar → [social]
  public   → []
```

## High-Level Architecture

```
┌────────────────────────────────────────────────┐
│                 Coop Gateway                    │
│              (tokio, single binary)             │
│                                                 │
│  Channels → Router → Sessions → Agent Pool     │
│                                                 │
│  Scheduler    Memory Layer    Config            │
└────────────────────────────────────────────────┘
```

See `docs/design.md` for the full design document with config examples, permission model details, and build phases.
