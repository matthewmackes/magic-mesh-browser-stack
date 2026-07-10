#!/bin/bash
# BROWSER-DD-11: install the optional offline STT model for Browser voice commands.
#
# The Browser voice-command wrapper defaults to
# /usr/share/magic-mesh/stt/browser-voice-command.model. This installer consumes a
# sha256-pinned manifest, caches the model/config assets, installs them under a
# versioned model directory, and publishes the active model symlink. If the
# manifest is not configured it exits 78, matching the optional Browser runtime
# gates.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_MANIFEST="$REPO/packaging/browser/browser-voice-command-stt.env"
INSTALLED_MANIFEST="/usr/share/magic-mesh/browser/browser-voice-command-stt.env"
if [ -n "${MDE_BROWSER_STT_MODEL_MANIFEST:-}" ]; then
  MANIFEST="$MDE_BROWSER_STT_MODEL_MANIFEST"
elif [ -r "$REPO_MANIFEST" ]; then
  MANIFEST="$REPO_MANIFEST"
else
  MANIFEST="$INSTALLED_MANIFEST"
fi
[ -r "$MANIFEST" ] || { echo "install-browser-stt-model: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

if [ -n "${MDE_BROWSER_STT_MODEL_CACHE:-}" ]; then
  MODEL_CACHE_ROOT="$MDE_BROWSER_STT_MODEL_CACHE"
elif [ "$MANIFEST" = "$REPO_MANIFEST" ]; then
  MODEL_CACHE_ROOT="$REPO/vendor/stt"
else
  MODEL_CACHE_ROOT="/var/cache/magic-mesh/stt"
fi

log() { echo "install-browser-stt-model: $*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "install-browser-stt-model: missing $1" >&2; exit 2; }; }

require_manifest_field() {
  local name="$1"
  local value="${!name:-}"
  if [ -z "$value" ]; then
    echo "install-browser-stt-model: operator must provide STT_MODEL_VERSION, STT_MODEL_ASSET, STT_MODEL_URL, and STT_MODEL_SHA256 in $MANIFEST" >&2
    exit 78
  fi
}

require_manifest_field STT_MODEL_VERSION
require_manifest_field STT_MODEL_ASSET
require_manifest_field STT_MODEL_URL
require_manifest_field STT_MODEL_SHA256

MODEL_ID="${MDE_BROWSER_STT_MODEL_ID:-${STT_MODEL_ID:-browser-voice-command}}"
INSTALL_PARENT="${MDE_BROWSER_STT_MODEL_INSTALL_PARENT:-$STT_MODEL_INSTALL_PARENT}"
ACTIVE_MODEL="${MDE_BROWSER_STT_MODEL:-${MDE_STT_MODEL:-$STT_MODEL_ACTIVE_MODEL}}"
INSTALL_ROOT="$INSTALL_PARENT/$MODEL_ID-$STT_MODEL_VERSION"
MODEL_CACHE="$MODEL_CACHE_ROOT/$STT_MODEL_ASSET"
CONFIG_CACHE=""
if [ -n "${STT_MODEL_CONFIG_ASSET:-}" ]; then
  CONFIG_CACHE="$MODEL_CACHE_ROOT/$STT_MODEL_CONFIG_ASSET"
fi

verify_asset() {
  local path="$1"
  local sha="$2"
  [ -f "$path" ] || return 1
  echo "$sha  $path" | sha256sum -c - >/dev/null 2>&1
}

fetch_asset() {
  local label="$1" asset="$2" url="$3" sha="$4" size="${5:-}" path="$MODEL_CACHE_ROOT/$asset"
  if verify_asset "$path" "$sha"; then
    log "$asset already cached + sha256 verified"
    return 0
  fi
  if [ -n "$size" ]; then
    log "fetching $asset ($size bytes)"
  else
    log "fetching $asset"
  fi
  curl -fsSL --retry 3 "$url" -o "$path.tmp"
  echo "$sha  $path.tmp" | sha256sum -c - >/dev/null 2>&1 || {
    rm -f "$path.tmp"
    echo "install-browser-stt-model: SHA256 MISMATCH for $label $asset" >&2
    exit 1
  }
  mv "$path.tmp" "$path"
}

install_asset() {
  local src="$1" dest="$2" sha="$3"
  install -D -m 0644 "$src" "$dest"
  echo "$sha  $dest" | sha256sum -c - >/dev/null 2>&1
}

need_cmd curl
need_cmd sha256sum
need_cmd install
need_cmd ln

mkdir -p "$MODEL_CACHE_ROOT" "$INSTALL_ROOT" "$(dirname "$ACTIVE_MODEL")"

fetch_asset "model" "$STT_MODEL_ASSET" "$STT_MODEL_URL" "$STT_MODEL_SHA256" "${STT_MODEL_SIZE_BYTES:-}"

MODEL_DEST="$INSTALL_ROOT/model"
install_asset "$MODEL_CACHE" "$MODEL_DEST" "$STT_MODEL_SHA256"

if [ -n "${STT_MODEL_CONFIG_ASSET:-}" ]; then
  if [ -z "${STT_MODEL_CONFIG_URL:-}" ] || [ -z "${STT_MODEL_CONFIG_SHA256:-}" ]; then
    echo "install-browser-stt-model: config asset requires STT_MODEL_CONFIG_URL and STT_MODEL_CONFIG_SHA256" >&2
    exit 78
  fi
  fetch_asset "config" "$STT_MODEL_CONFIG_ASSET" "$STT_MODEL_CONFIG_URL" "$STT_MODEL_CONFIG_SHA256" "${STT_MODEL_CONFIG_SIZE_BYTES:-}"
  install_asset "$CONFIG_CACHE" "$INSTALL_ROOT/model.config" "$STT_MODEL_CONFIG_SHA256"
fi

cat >"$INSTALL_ROOT/mde-browser-stt-model.manifest" <<EOF
model_id=$MODEL_ID
version=$STT_MODEL_VERSION
model_asset=$STT_MODEL_ASSET
model_sha256=$STT_MODEL_SHA256
config_asset=${STT_MODEL_CONFIG_ASSET:-}
config_sha256=${STT_MODEL_CONFIG_SHA256:-}
runtime_note=${STT_MODEL_RUNTIME_NOTE:-}
EOF

ln -sfn "$MODEL_DEST" "$ACTIVE_MODEL"
if [ -f "$INSTALL_ROOT/model.config" ]; then
  ln -sfn "$INSTALL_ROOT/model.config" "$ACTIVE_MODEL.config"
fi
log "active Browser STT model: $ACTIVE_MODEL -> $MODEL_DEST"
