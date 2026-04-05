#!/bin/bash
set -euo pipefail

LAUNCHER_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$LAUNCHER_DIR/../.." && pwd)"
APP_NAME="Coop Launcher.app"
SOURCE_APP="$LAUNCHER_DIR/dist/$APP_NAME"
TARGET_APP="$HOME/Applications/$APP_NAME"
SUPPORT_DIR="$HOME/Library/Application Support/CoopLauncher"
CONFIG_PATH="$SUPPORT_DIR/config.json"
DEFAULT_TRACE_PATH="$HOME/.coop/logs/trace.jsonl"
RAW_TRACE_PATH="${COOP_TRACE_FILE:-$DEFAULT_TRACE_PATH}"
INSTALLED_BINARY="${CARGO_HOME:-$HOME/.cargo}/bin/coop"
COOP_CONFIG_PATH="${COOP_CONFIG:-$HOME/.coop/coop.toml}"
export RAW_TRACE_PATH COOP_CONFIG_PATH

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Coop Launcher can only be installed on macOS." >&2
  exit 1
fi

TRACE_PATH="$(python3 - <<'PY'
import os
print(os.path.abspath(os.path.expanduser(os.environ["RAW_TRACE_PATH"])))
PY
)"
COOP_CONFIG_PATH="$(python3 - <<'PY'
import os
print(os.path.abspath(os.path.expanduser(os.environ["COOP_CONFIG_PATH"])))
PY
)"
TRACE_DIR="$(dirname "$TRACE_PATH")"

"$LAUNCHER_DIR/build-app.sh"

mkdir -p "$HOME/Applications"
rm -rf "$TARGET_APP"
ditto "$SOURCE_APP" "$TARGET_APP"
codesign --verify --deep --strict "$TARGET_APP"

mkdir -p "$SUPPORT_DIR" "$TRACE_DIR"

export COOP_LAUNCHER_REPO_PATH="$REPO_ROOT"
export COOP_LAUNCHER_CONFIG_PATH="$CONFIG_PATH"
export COOP_LAUNCHER_TRACE_PATH="$TRACE_PATH"
export COOP_LAUNCHER_INSTALLED_BINARY="$INSTALLED_BINARY"
export COOP_LAUNCHER_COOP_CONFIG_PATH="$COOP_CONFIG_PATH"

python3 <<'PY'
import json
import os
from pathlib import Path

config_path = Path(os.environ["COOP_LAUNCHER_CONFIG_PATH"])
repo_path = os.environ["COOP_LAUNCHER_REPO_PATH"]
trace_path = os.environ["COOP_LAUNCHER_TRACE_PATH"]
installed_binary = os.environ["COOP_LAUNCHER_INSTALLED_BINARY"]
coop_config_path = os.environ["COOP_LAUNCHER_COOP_CONFIG_PATH"]

config_path.parent.mkdir(parents=True, exist_ok=True)

if config_path.exists():
    config = json.loads(config_path.read_text())
else:
    config = {}

config["repo_path"] = repo_path
config["launch_mode"] = "installedBinary"
config["custom_executable_path"] = installed_binary
config["arguments"] = ["start", "--config", coop_config_path]
environment = dict(config.get("environment") or {})
environment.setdefault("RUST_LOG", "info")
config["environment"] = environment
config["trace_file"] = trace_path
config["window_title"] = "Coop Launcher (gateway)"

config_path.write_text(json.dumps(config, indent=2, sort_keys=True) + "\n")
PY

echo "Installed: $TARGET_APP"
echo "Configured: $CONFIG_PATH"
echo "Repo path: $REPO_ROOT"
echo "Installed binary: $INSTALLED_BINARY"
echo "Gateway config: $COOP_CONFIG_PATH"
echo "Trace file: $TRACE_PATH"
echo "Next: open \"$TARGET_APP\""
