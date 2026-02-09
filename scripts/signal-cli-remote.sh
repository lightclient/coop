#!/bin/bash
# Wrapper that routes signal-cli commands to a remote host via SSH.
# Uses the "signal-host" SSH config alias (see ~/.ssh/config).
#
# Usage: ./scripts/signal-cli-remote.sh -o json send -m "hello" TARGET
exec ssh signal-host /opt/homebrew/bin/signal-cli "$@"
