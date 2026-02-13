#!/usr/bin/env bash
# Discover the coop account and test sender account from local signal-cli.
# Outputs a sourceable config to stdout.
#
# Usage:
#   eval "$(./scripts/discover-accounts.sh)"
#   echo "$COOP_NUMBER"     # coop's phone number
#   echo "$SENDER_CMD"      # full send command prefix
#   echo "$SENDER_NUMBER"   # test sender's phone number
set -euo pipefail

ACCOUNTS=$(signal-cli -o json listAccounts 2>/dev/null || echo "[]")
ACCOUNT_COUNT=$(echo "$ACCOUNTS" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")

if [ "$ACCOUNT_COUNT" -lt 2 ]; then
    echo "# ERROR: Need 2 signal-cli accounts, found $ACCOUNT_COUNT" >&2
    echo "# Run: signal-cli -o json listAccounts" >&2
    exit 1
fi

# Coop's account is the one with multiple devices (presage linked as secondary)
COOP_NUMBER=""
SENDER_NUMBER=""

for num in $(echo "$ACCOUNTS" | python3 -c "
import sys, json
for a in json.load(sys.stdin):
    print(a.get('number', ''))
" 2>/dev/null); do
    devices=$(signal-cli -a "$num" -o json listDevices 2>/dev/null || echo "[]")
    device_count=$(echo "$devices" | python3 -c "import sys,json; print(len(json.load(sys.stdin)))" 2>/dev/null || echo "0")
    if [ "$device_count" -gt 1 ]; then
        COOP_NUMBER="$num"
    else
        SENDER_NUMBER="$num"
    fi
done

# Fallback: if detection failed, assign by elimination
if [ -z "$COOP_NUMBER" ] || [ -z "$SENDER_NUMBER" ]; then
    ALL_NUMS=$(echo "$ACCOUNTS" | python3 -c "
import sys, json
for a in json.load(sys.stdin):
    print(a.get('number', ''))
" 2>/dev/null)
    FIRST=$(echo "$ALL_NUMS" | head -1)
    SECOND=$(echo "$ALL_NUMS" | tail -1)
    if [ -z "$COOP_NUMBER" ]; then COOP_NUMBER="$FIRST"; fi
    if [ -z "$SENDER_NUMBER" ]; then
        if [ "$FIRST" != "$COOP_NUMBER" ]; then SENDER_NUMBER="$FIRST";
        else SENDER_NUMBER="$SECOND"; fi
    fi
fi

# Verify sender account is registered (without sending a message, to avoid polluting traces)
DEVICES=$(signal-cli -a "$SENDER_NUMBER" -o json listDevices 2>&1 || true)
if ! echo "$DEVICES" | grep -q '"id"'; then
    echo "# ERROR: Sender $SENDER_NUMBER is not registered" >&2
    echo "# Output: $DEVICES" >&2
    echo "# May need: signal-cli -a $SENDER_NUMBER register && verify" >&2
    exit 1
fi

# Discover sender's UUID from signal-cli accounts data
SENDER_UUID=$(python3 -c "
import json, pathlib
data_dir = pathlib.Path.home() / '.local/share/signal-cli/data'
accounts = json.loads((data_dir / 'accounts.json').read_text())
for a in accounts.get('accounts', []):
    if a.get('number') == '$SENDER_NUMBER':
        path = data_dir / a['path']
        info = json.loads(path.read_text())
        aci = (info.get('aciAccountData') or {}).get('serviceId')
        if aci:
            print(aci)
            break
" 2>/dev/null || echo "")

# If not in signal-cli data, it will be discovered from traces on first message
if [ -z "$SENDER_UUID" ]; then
    echo "# WARNING: Could not determine sender UUID from signal-cli data" >&2
    echo "# It will be discovered from traces after first message" >&2
fi

cat <<EOF
COOP_NUMBER="$COOP_NUMBER"
SENDER_CMD="signal-cli -a $SENDER_NUMBER -o json"
SENDER_NUMBER="$SENDER_NUMBER"
SENDER_UUID="$SENDER_UUID"
EOF

echo "# Accounts: coop=$COOP_NUMBER sender=$SENDER_NUMBER" >&2
