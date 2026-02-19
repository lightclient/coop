#!/usr/bin/env bash
#
# Link a second presage device to the recipient account (+17205818516)
# so we can test multi-device delivery.
#
# This creates a NEW presage DB at ./db/recipient-device2.db that acts
# as the recipient's "desktop" — a second linked device alongside
# signal-cli (device 1, the "phone").
#
# After linking, run the delivery test to see if both devices receive
# every message from coop.
#
# Usage:
#   Terminal 1:  ./tests/link-recipient-device.sh
#                (displays QR code / URL, waits for linking)
#
#   Terminal 2:  signal-cli -u +17205818516 addDevice --uri "<URL from terminal 1>"
#
set -euo pipefail

RECIPIENT="+17205818516"
DEVICE2_DB="$(cd "$(dirname "$0")/.." && pwd)/db/recipient-device2.db"
PRESAGE_CLI="/root/.cargo/git/checkouts/presage-ce211e77a0d9397d/1b81dfe"

echo "Linking a new presage device to $RECIPIENT..."
echo "DB path: $DEVICE2_DB"
echo

if [ -f "$DEVICE2_DB" ]; then
    echo "DB already exists. To re-link, delete it first:"
    echo "  rm $DEVICE2_DB"
    echo
    echo "Checking existing registration..."
    cd "$PRESAGE_CLI"
    cargo run -q --bin presage-cli -- --db-path "$DEVICE2_DB" whoami 2>&1 || true
    exit 0
fi

echo "Starting presage link-device..."
echo "A QR code / URL will appear. Copy the URL and run in another terminal:"
echo
echo "  signal-cli -u $RECIPIENT addDevice --uri '<URL>'"
echo

cd "$PRESAGE_CLI"
cargo run --bin presage-cli -- \
    --db-path "$DEVICE2_DB" \
    link-device \
    --device-name "test-desktop"
