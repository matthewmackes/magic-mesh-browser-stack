#!/bin/bash
# BROWSER-DD-11: install the optional offline Piper voice for Browser read-aloud.
#
# The Browser read-aloud wrapper defaults to
# /usr/share/magic-mesh/tts/browser-read-aloud.onnx. This installer consumes a
# sha256-pinned manifest, caches the model/config assets, installs them under a
# versioned voice directory, and publishes the active model symlink. If the
# manifest is not configured it exits 78, matching the optional Widevine gate.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_MANIFEST="$REPO/packaging/browser/browser-read-aloud-voice.env"
INSTALLED_MANIFEST="/usr/share/magic-mesh/browser/browser-read-aloud-voice.env"
if [ -n "${MDE_BROWSER_TTS_VOICE_MANIFEST:-}" ]; then
  MANIFEST="$MDE_BROWSER_TTS_VOICE_MANIFEST"
elif [ -r "$REPO_MANIFEST" ]; then
  MANIFEST="$REPO_MANIFEST"
else
  MANIFEST="$INSTALLED_MANIFEST"
fi
[ -r "$MANIFEST" ] || { echo "install-browser-tts-voice: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

if [ -n "${MDE_BROWSER_TTS_VOICE_CACHE:-}" ]; then
  VOICE_CACHE="$MDE_BROWSER_TTS_VOICE_CACHE"
elif [ "$MANIFEST" = "$REPO_MANIFEST" ]; then
  VOICE_CACHE="$REPO/vendor/tts"
else
  VOICE_CACHE="/var/cache/magic-mesh/tts"
fi

log() { echo "install-browser-tts-voice: $*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "install-browser-tts-voice: missing $1" >&2; exit 2; }; }

require_manifest_field() {
  local name="$1"
  local value="${!name:-}"
  if [ -z "$value" ]; then
    echo "install-browser-tts-voice: operator must provide TTS_VOICE_VERSION, TTS_VOICE_MODEL_ASSET, TTS_VOICE_MODEL_URL, and TTS_VOICE_MODEL_SHA256 in $MANIFEST" >&2
    exit 78
  fi
}

require_manifest_field TTS_VOICE_VERSION
require_manifest_field TTS_VOICE_MODEL_ASSET
require_manifest_field TTS_VOICE_MODEL_URL
require_manifest_field TTS_VOICE_MODEL_SHA256

VOICE_ID="${MDE_BROWSER_TTS_VOICE_ID:-${TTS_VOICE_ID:-browser-read-aloud}}"
INSTALL_PARENT="${MDE_BROWSER_TTS_VOICE_INSTALL_PARENT:-$TTS_VOICE_INSTALL_PARENT}"
ACTIVE_MODEL="${MDE_BROWSER_TTS_MODEL:-${MDE_TTS_MODEL:-$TTS_VOICE_ACTIVE_MODEL}}"
INSTALL_ROOT="$INSTALL_PARENT/$VOICE_ID-$TTS_VOICE_VERSION"
MODEL_CACHE="$VOICE_CACHE/$TTS_VOICE_MODEL_ASSET"
CONFIG_CACHE=""
if [ -n "${TTS_VOICE_CONFIG_ASSET:-}" ]; then
  CONFIG_CACHE="$VOICE_CACHE/$TTS_VOICE_CONFIG_ASSET"
fi

verify_asset() {
  local path="$1"
  local sha="$2"
  [ -f "$path" ] || return 1
  echo "$sha  $path" | sha256sum -c - >/dev/null 2>&1
}

fetch_asset() {
  local label="$1" asset="$2" url="$3" sha="$4" size="${5:-}" path="$VOICE_CACHE/$asset"
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
    echo "install-browser-tts-voice: SHA256 MISMATCH for $label $asset" >&2
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

mkdir -p "$VOICE_CACHE" "$INSTALL_ROOT" "$(dirname "$ACTIVE_MODEL")"

fetch_asset "model" "$TTS_VOICE_MODEL_ASSET" "$TTS_VOICE_MODEL_URL" "$TTS_VOICE_MODEL_SHA256" "${TTS_VOICE_MODEL_SIZE_BYTES:-}"

MODEL_DEST="$INSTALL_ROOT/model.onnx"
install_asset "$MODEL_CACHE" "$MODEL_DEST" "$TTS_VOICE_MODEL_SHA256"

if [ -n "${TTS_VOICE_CONFIG_ASSET:-}" ]; then
  if [ -z "${TTS_VOICE_CONFIG_URL:-}" ] || [ -z "${TTS_VOICE_CONFIG_SHA256:-}" ]; then
    echo "install-browser-tts-voice: config asset requires TTS_VOICE_CONFIG_URL and TTS_VOICE_CONFIG_SHA256" >&2
    exit 78
  fi
  fetch_asset "config" "$TTS_VOICE_CONFIG_ASSET" "$TTS_VOICE_CONFIG_URL" "$TTS_VOICE_CONFIG_SHA256" "${TTS_VOICE_CONFIG_SIZE_BYTES:-}"
  install_asset "$CONFIG_CACHE" "$INSTALL_ROOT/model.onnx.json" "$TTS_VOICE_CONFIG_SHA256"
fi

cat >"$INSTALL_ROOT/mde-browser-read-aloud-voice.manifest" <<EOF
voice_id=$VOICE_ID
version=$TTS_VOICE_VERSION
model_asset=$TTS_VOICE_MODEL_ASSET
model_sha256=$TTS_VOICE_MODEL_SHA256
config_asset=${TTS_VOICE_CONFIG_ASSET:-}
config_sha256=${TTS_VOICE_CONFIG_SHA256:-}
EOF

ln -sfn "$MODEL_DEST" "$ACTIVE_MODEL"
if [ -f "$INSTALL_ROOT/model.onnx.json" ]; then
  ln -sfn "$INSTALL_ROOT/model.onnx.json" "$ACTIVE_MODEL.json"
fi
log "active Browser read-aloud voice: $ACTIVE_MODEL -> $MODEL_DEST"
