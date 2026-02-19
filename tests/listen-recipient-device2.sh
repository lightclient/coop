#!/usr/bin/env bash
#
# Listen for messages on recipient device 2 (the linked presage instance).
# Run this alongside the delivery test to see which messages arrive at
# the "desktop" device vs the "phone" (signal-cli device 1).
#
# Usage:
#   Terminal 1: ./tests/listen-recipient-device2.sh
#   Terminal 2: ./tests/multidevice-delivery-test.sh 10
#   Terminal 3: signal-cli -u +17205818516 receive --timeout 30
#
# Compare output across terminals to find delivery gaps.
#
set -euo pipefail

DEVICE2_DB="$(cd "$(dirname "$0")/.." && pwd)/db/recipient-device2.db"
PRESAGE_CLI="/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe"

if [ ! -f "$DEVICE2_DB" ]; then
    echo "Device 2 DB not found. Run link-recipient-device.sh first."
    exit 1
fi

echo "Listening for messages on recipient device 2..."
echo "DB: $DEVICE2_DB"
echo "Press Ctrl+C to stop."
echo

cd "$PRESAGE_CLI"
exec cargo run -q --bin presage-cli -- \
    --db-path "$DEVICE2_DB" \
    receive
