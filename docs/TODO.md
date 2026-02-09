# TODO

## Project

- make it clear that deps like sccache need to be installed to compile
- make the just follow trace script print the message and tool messages more
  nicely
- ~~log rotation~~ ✅ **DONE** - Size and date-based rotation implemented
- Don't save keys to memory!!!
- handle timezone in cron
- need better per-user tracking of memories
- let user interject in the middle of a stream with new prompt / info
- inject user AGENTS.md
- trace-follow also needs to create destination folder if it doesn't exist

## Gateway

- add encryption and authentication flow to channel connections
- add support for blocking tool / request, e.g. i'm about to open a new group,
  wait until you see it then text me the name to confirm adding it to allowed
  list
- convert config to toml
- per channel prompt on how to use the channel
- config checking, to see how coop will behave on restart. would be good to have
  info about files loaded into memory after, etc so the agent can decide if it
  working as expected even if it is syntactically correct
- sessions persist across reboots
- multi agent and subagent support
- **More slash commands.** Currently have `/new`, `/clear`, `/status`, `/help`, `/verbose`, `/quit`. Add:
  - `/compact` — Trigger context summarization when running low on context window. Summarize older messages and replace with a condensed summary to free up tokens.
  - `/sessions` — List active sessions. The gateway already exposes `list_sessions()`. Useful in attach mode.
  - `/model` — Switch model mid-session. Would need provider hot-swap support in the gateway.
  - `/undo` — Roll back the last turn (user message + assistant response). The gateway already has `truncate_session()`.
  - `/retry` — Undo the last turn and re-send the same user input. Combines `/undo` with automatic re-submit.
- Slash commands should only be allowable by full trust users

## Agent

- auto rotation strategy for when rate limits kick in 
- "tokenization" of human names to unique identities in memories, sessions, etc.
  maybe with a confidence interface built into the token?

## Signal

- add support for chat history
- resolve user via phone number instead of uuid
- `typing should last until the agent responds`
- nicer error messages instead of raw json
- ~~should send delivered / read receipts~~

## TUI

- **Vim keybinding mode for input.** The input handler (`coop-tui/src/input.rs`) currently only supports emacs-style bindings (Ctrl-A/E/U/W). Add a vi modal editing mode — normal/insert with `Esc` to toggle, `hjkl` movement, `w`/`b` word motion, `dd`/`cc` line ops, etc. Consider a `/set vim` or `/set emacs` command to toggle, persisted in config. Could look at `tui-textarea` crate or roll our own since the input is single-line for now.
