#!/bin/bash
# BROWSER-DD-12: install the optional offline/mesh translation model for Browser.
#
# The Browser translation wrapper defaults to
# /usr/share/magic-mesh/translate/browser-translate.model. This installer consumes
# a sha256-pinned manifest, caches model/config assets, installs them under a
# versioned model directory, and publishes the active model symlink. If the
# manifest is not configured it exits 78, matching the optional Browser runtime
# gates.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_MANIFEST="$REPO/packaging/browser/browser-translate.env"
INSTALLED_MANIFEST="/usr/share/magic-mesh/browser/browser-translate.env"
if [ -n "${MDE_BROWSER_TRANSLATE_MODEL_MANIFEST:-}" ]; then
  MANIFEST="$MDE_BROWSER_TRANSLATE_MODEL_MANIFEST"
elif [ -r "$REPO_MANIFEST" ]; then
  MANIFEST="$REPO_MANIFEST"
else
  MANIFEST="$INSTALLED_MANIFEST"
fi
[ -r "$MANIFEST" ] || { echo "install-browser-translate-model: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

if [ -n "${MDE_BROWSER_TRANSLATE_MODEL_CACHE:-}" ]; then
  MODEL_CACHE_ROOT="$MDE_BROWSER_TRANSLATE_MODEL_CACHE"
elif [ "$MANIFEST" = "$REPO_MANIFEST" ]; then
  MODEL_CACHE_ROOT="$REPO/vendor/translate"
else
  MODEL_CACHE_ROOT="/var/cache/magic-mesh/translate"
fi

log() { echo "install-browser-translate-model: $*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "install-browser-translate-model: missing $1" >&2; exit 2; }; }

require_manifest_field() {
  local name="$1"
  local value="${!name:-}"
  if [ -z "$value" ]; then
    echo "install-browser-translate-model: operator must provide TRANSLATE_MODEL_VERSION, TRANSLATE_MODEL_ASSET, TRANSLATE_MODEL_URL, and TRANSLATE_MODEL_SHA256 in $MANIFEST" >&2
    exit 78
  fi
}

require_manifest_field TRANSLATE_MODEL_VERSION
require_manifest_field TRANSLATE_MODEL_ASSET
require_manifest_field TRANSLATE_MODEL_URL
require_manifest_field TRANSLATE_MODEL_SHA256

MODEL_ID="${MDE_BROWSER_TRANSLATE_MODEL_ID:-${TRANSLATE_MODEL_ID:-browser-translate}}"
INSTALL_PARENT="${MDE_BROWSER_TRANSLATE_MODEL_INSTALL_PARENT:-$TRANSLATE_MODEL_INSTALL_PARENT}"
ACTIVE_MODEL="${MDE_BROWSER_TRANSLATE_MODEL:-${MDE_TRANSLATE_MODEL:-$TRANSLATE_MODEL_ACTIVE_MODEL}}"
INSTALL_ROOT="$INSTALL_PARENT/$MODEL_ID-$TRANSLATE_MODEL_VERSION"
MODEL_CACHE="$MODEL_CACHE_ROOT/$TRANSLATE_MODEL_ASSET"
CONFIG_CACHE=""
if [ -n "${TRANSLATE_MODEL_CONFIG_ASSET:-}" ]; then
  CONFIG_CACHE="$MODEL_CACHE_ROOT/$TRANSLATE_MODEL_CONFIG_ASSET"
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
    echo "install-browser-translate-model: SHA256 MISMATCH for $label $asset" >&2
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

fetch_asset "model" "$TRANSLATE_MODEL_ASSET" "$TRANSLATE_MODEL_URL" "$TRANSLATE_MODEL_SHA256" "${TRANSLATE_MODEL_SIZE_BYTES:-}"

MODEL_DEST="$INSTALL_ROOT/model"
install_asset "$MODEL_CACHE" "$MODEL_DEST" "$TRANSLATE_MODEL_SHA256"

if [ -n "${TRANSLATE_MODEL_CONFIG_ASSET:-}" ]; then
  if [ -z "${TRANSLATE_MODEL_CONFIG_URL:-}" ] || [ -z "${TRANSLATE_MODEL_CONFIG_SHA256:-}" ]; then
    echo "install-browser-translate-model: config asset requires TRANSLATE_MODEL_CONFIG_URL and TRANSLATE_MODEL_CONFIG_SHA256" >&2
    exit 78
  fi
  fetch_asset "config" "$TRANSLATE_MODEL_CONFIG_ASSET" "$TRANSLATE_MODEL_CONFIG_URL" "$TRANSLATE_MODEL_CONFIG_SHA256" "${TRANSLATE_MODEL_CONFIG_SIZE_BYTES:-}"
  install_asset "$CONFIG_CACHE" "$INSTALL_ROOT/model.config" "$TRANSLATE_MODEL_CONFIG_SHA256"
fi

cat >"$INSTALL_ROOT/mde-browser-translate-model.manifest" <<EOF
model_id=$MODEL_ID
version=$TRANSLATE_MODEL_VERSION
model_asset=$TRANSLATE_MODEL_ASSET
model_sha256=$TRANSLATE_MODEL_SHA256
config_asset=${TRANSLATE_MODEL_CONFIG_ASSET:-}
config_sha256=${TRANSLATE_MODEL_CONFIG_SHA256:-}
runtime_note=${TRANSLATE_MODEL_RUNTIME_NOTE:-}
EOF

ln -sfn "$MODEL_DEST" "$ACTIVE_MODEL"
if [ -f "$INSTALL_ROOT/model.config" ]; then
  ln -sfn "$INSTALL_ROOT/model.config" "$ACTIVE_MODEL.config"
fi
log "active Browser translation model: $ACTIVE_MODEL -> $MODEL_DEST"
