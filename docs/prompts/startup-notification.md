
I want to add a startup notification system to coop. Here's the use case: when coop is restarted via the `scripts/restart.sh` script, it comes back with no context and sits silently. The user doesn't know if the restart succeeded without manually messaging.

The design:

1. The restart script writes a JSON marker file to `/tmp/coop-restart-notify.json` after a successful restart, containing:
   - `user`: the user who triggered the restart (e.g. "matt")
   - `channel`: "signal" 
   - `target`: the user's signal UUID
   - `message`: a short message like "Restart complete. Back online."
   - `timestamp`: when the restart happened

2. During coop gateway startup (after all channels are initialized and signal is connected), check for this marker file. If it exists:
   - Parse it
   - Send the message to the specified target via the signal channel
   - Delete the marker file

3. Update `scripts/restart.sh` to write this marker file. It will need to accept the user/target info as arguments, or read them from coop.toml.

Key files to look at:
- `crates/coop-gateway/src/signal_loop.rs` or wherever the gateway startup sequence lives — this is where the notification check should go, AFTER signal is fully connected
- `crates/coop-channels/src/signal.rs` — the `send` method on the signal channel for sending the actual message  
- `scripts/restart.sh` — update to write the marker file

Keep it simple. The marker file approach is intentionally low-tech so the restart script (which runs outside coop) can communicate with the new coop process without needing IPC.
