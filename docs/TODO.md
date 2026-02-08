# TODO

## Gateway

- add encryption and authentication flow to channel connections

## Agent

- auto rotation strategy for when rate limits kick in 
- "tokenization" of human names to unique identites in memories, sessions, etc.
  maybe with a confidence interface built into the token?

## Signal

- add support for chat history

## TUI

- **Vim keybinding mode for input.** The input handler (`coop-tui/src/input.rs`) currently only supports emacs-style bindings (Ctrl-A/E/U/W). Add a vi modal editing mode — normal/insert with `Esc` to toggle, `hjkl` movement, `w`/`b` word motion, `dd`/`cc` line ops, etc. Consider a `/set vim` or `/set emacs` command to toggle, persisted in config. Could look at `tui-textarea` crate or roll our own since the input is single-line for now.
