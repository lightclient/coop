# TODO

## Agent

- auto rotation strategy for when rate limits kick in 

## Signal

- add typing while preparing response

## TUI

- **Vim keybinding mode for input.** The input handler (`coop-tui/src/input.rs`) currently only supports emacs-style bindings (Ctrl-A/E/U/W). Add a vi modal editing mode — normal/insert with `Esc` to toggle, `hjkl` movement, `w`/`b` word motion, `dd`/`cc` line ops, etc. Consider a `/set vim` or `/set emacs` command to toggle, persisted in config. Could look at `tui-textarea` crate or roll our own since the input is single-line for now.
