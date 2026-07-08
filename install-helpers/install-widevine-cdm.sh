#!/bin/bash
# BROWSER-DD-4: install the optional Widevine CDM used by `mde-web-cef`.
#
# The CDM is not redistributed in the RPM. This script consumes an
# operator-provided manifest with WIDEVINE_URL + WIDEVINE_SHA256, verifies the
# archive, extracts it into a versioned /opt/mde/widevine-cdms directory, and
# publishes /opt/mde/widevine for the CEF helper. When the manifest is not
# configured, the script exits 78 so first-run provisioning records an honest
# gate without breaking non-DRM browsing.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_MANIFEST="$REPO/packaging/browser/widevine-linux64.env"
INSTALLED_MANIFEST="/usr/share/magic-mesh/browser/widevine-linux64.env"
if [ -n "${MDE_WIDEVINE_MANIFEST:-}" ]; then
  MANIFEST="$MDE_WIDEVINE_MANIFEST"
elif [ -r "$REPO_MANIFEST" ]; then
  MANIFEST="$REPO_MANIFEST"
else
  MANIFEST="$INSTALLED_MANIFEST"
fi
[ -r "$MANIFEST" ] || { echo "install-widevine-cdm: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

if [ -n "${MDE_WIDEVINE_CACHE:-}" ]; then
  WIDEVINE_CACHE="$MDE_WIDEVINE_CACHE"
elif [ "$MANIFEST" = "$REPO_MANIFEST" ]; then
  WIDEVINE_CACHE="$REPO/vendor/widevine"
else
  WIDEVINE_CACHE="/var/cache/magic-mesh/widevine"
fi

log() { echo "install-widevine-cdm: $*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "install-widevine-cdm: missing $1" >&2; exit 2; }; }

require_manifest_field() {
  local name="$1"
  local value="${!name:-}"
  if [ -z "$value" ]; then
    echo "install-widevine-cdm: operator must provide WIDEVINE_URL and WIDEVINE_SHA256 in $MANIFEST" >&2
    exit 78
  fi
}

require_manifest_field WIDEVINE_VERSION
require_manifest_field WIDEVINE_ASSET
require_manifest_field WIDEVINE_URL
require_manifest_field WIDEVINE_SHA256

INSTALL_PARENT="${MDE_WIDEVINE_INSTALL_PARENT:-$WIDEVINE_INSTALL_PARENT}"
ACTIVE_LINK="${MDE_WIDEVINE_ACTIVE_LINK:-$WIDEVINE_ACTIVE_LINK}"
INSTALL_ROOT="$INSTALL_PARENT/$WIDEVINE_VERSION-$WIDEVINE_PLATFORM"
ARCHIVE="$WIDEVINE_CACHE/$WIDEVINE_ASSET"

verify_archive() {
  [ -f "$ARCHIVE" ] || return 1
  echo "$WIDEVINE_SHA256  $ARCHIVE" | sha256sum -c - >/dev/null 2>&1
}

find_cdm_lib() {
  find "$1" -type f -name libwidevinecdm.so | head -1
}

activate_cdm() {
  [ -f "$INSTALL_ROOT/libwidevinecdm.so" ] || {
    echo "install-widevine-cdm: missing $INSTALL_ROOT/libwidevinecdm.so after extract" >&2
    exit 1
  }
  ln -sfn "$INSTALL_ROOT" "$ACTIVE_LINK"
  log "active CDM: $ACTIVE_LINK -> $INSTALL_ROOT"
}

need_cmd curl
need_cmd sha256sum
need_cmd tar
need_cmd mktemp
need_cmd find

mkdir -p "$WIDEVINE_CACHE" "$INSTALL_PARENT"

if verify_archive; then
  log "$WIDEVINE_ASSET already cached + sha256 verified"
else
  if [ -n "${WIDEVINE_SIZE_BYTES:-}" ]; then
    log "fetching $WIDEVINE_ASSET ($WIDEVINE_SIZE_BYTES bytes)"
  else
    log "fetching $WIDEVINE_ASSET"
  fi
  curl -fsSL --retry 3 "$WIDEVINE_URL" -o "$ARCHIVE.tmp"
  echo "$WIDEVINE_SHA256  $ARCHIVE.tmp" | sha256sum -c - >/dev/null 2>&1 || {
    rm -f "$ARCHIVE.tmp"
    echo "install-widevine-cdm: SHA256 MISMATCH for $WIDEVINE_ASSET" >&2
    exit 1
  }
  mv "$ARCHIVE.tmp" "$ARCHIVE"
fi

if [ -f "$INSTALL_ROOT/libwidevinecdm.so" ]; then
  log "$WIDEVINE_VERSION already extracted"
  activate_cdm
  exit 0
fi

tmp="$(mktemp -d "$INSTALL_PARENT/.widevine-extract.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
log "extracting to $INSTALL_ROOT"
case "$ARCHIVE" in
  *.tar.xz) tar -xJf "$ARCHIVE" -C "$tmp" ;;
  *.tar.gz | *.tgz) tar -xzf "$ARCHIVE" -C "$tmp" ;;
  *.tar.bz2 | *.tbz2) tar -xjf "$ARCHIVE" -C "$tmp" ;;
  *.tar) tar -xf "$ARCHIVE" -C "$tmp" ;;
  *)
    echo "install-widevine-cdm: unsupported archive type $ARCHIVE" >&2
    exit 2
    ;;
esac
cdm_lib="$(find_cdm_lib "$tmp")"
[ -n "$cdm_lib" ] || {
  echo "install-widevine-cdm: archive does not contain libwidevinecdm.so" >&2
  exit 1
}
rm -rf "$INSTALL_ROOT.new"
mkdir -p "$INSTALL_ROOT.new"
cp "$cdm_lib" "$INSTALL_ROOT.new/libwidevinecdm.so"
manifest="$(dirname "$cdm_lib")/manifest.json"
if [ -f "$manifest" ]; then
  cp "$manifest" "$INSTALL_ROOT.new/manifest.json"
elif [ -f "$tmp/manifest.json" ]; then
  cp "$tmp/manifest.json" "$INSTALL_ROOT.new/manifest.json"
fi
rm -rf "$INSTALL_ROOT"
mv "$INSTALL_ROOT.new" "$INSTALL_ROOT"
printf 'version=%s\nplatform=%s\nasset=%s\nsha256=%s\n' \
  "$WIDEVINE_VERSION" "$WIDEVINE_PLATFORM" "$WIDEVINE_ASSET" "$WIDEVINE_SHA256" \
  > "$INSTALL_ROOT/mde-widevine-cdm.manifest"
activate_cdm
