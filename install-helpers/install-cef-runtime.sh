#!/bin/bash
# BROWSER-DD-1: install the pinned Chromium/CEF runtime used by `mde-web-cef`.
#
# The CEF helper is packaged separately from the native CEF payload. This script
# gives the farm and live Workstations an idempotent, sha256-pinned way to fetch
# the prebuilt Linux64 minimal CEF distribution, extract it under a versioned
# `/opt/mde/cef-runtimes/` directory, and publish `/opt/mde/cef` for the shell
# and helper runtime gates.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_MANIFEST="$REPO/packaging/browser/cef-linux64-minimal.env"
INSTALLED_MANIFEST="/usr/share/magic-mesh/browser/cef-linux64-minimal.env"
if [ -n "${MDE_CEF_MANIFEST:-}" ]; then
  MANIFEST="$MDE_CEF_MANIFEST"
elif [ -r "$REPO_MANIFEST" ]; then
  MANIFEST="$REPO_MANIFEST"
else
  MANIFEST="$INSTALLED_MANIFEST"
fi
[ -r "$MANIFEST" ] || { echo "install-cef-runtime: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

if [ -n "${MDE_CEF_CACHE:-}" ]; then
  CEF_CACHE="$MDE_CEF_CACHE"
elif [ "$MANIFEST" = "$REPO_MANIFEST" ]; then
  CEF_CACHE="$REPO/vendor/cef"
else
  CEF_CACHE="/var/cache/magic-mesh/cef"
fi
INSTALL_PARENT="${MDE_CEF_INSTALL_PARENT:-$CEF_INSTALL_PARENT}"
ACTIVE_LINK="${MDE_CEF_ACTIVE_LINK:-$CEF_ACTIVE_LINK}"
INSTALL_ROOT="$INSTALL_PARENT/$CEF_VERSION-$CEF_PLATFORM-$CEF_TYPE"
ARCHIVE="$CEF_CACHE/$CEF_ASSET"

log() { echo "install-cef-runtime: $*"; }
need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "install-cef-runtime: missing $1" >&2; exit 2; }; }

verify_archive() {
  [ -f "$ARCHIVE" ] || return 1
  echo "$CEF_SHA256  $ARCHIVE" | sha256sum -c - >/dev/null 2>&1
}

normalize_release_resources() {
  [ -d "$INSTALL_ROOT/Release" ] || {
    echo "install-cef-runtime: missing $INSTALL_ROOT/Release after extract" >&2
    exit 1
  }
  [ -d "$INSTALL_ROOT/Resources" ] || {
    echo "install-cef-runtime: missing $INSTALL_ROOT/Resources after extract" >&2
    exit 1
  }

  shopt -s nullglob
  local asset source target
  for source in "$INSTALL_ROOT/Resources/icudtl.dat" "$INSTALL_ROOT"/Resources/*.pak; do
    asset="$(basename "$source")"
    target="$INSTALL_ROOT/Release/$asset"
    if [ -e "$target" ] && [ ! -L "$target" ]; then
      log "Release/$asset already present"
      continue
    fi
    ln -sfn "../Resources/$asset" "$target"
    log "linked Release/$asset -> ../Resources/$asset"
  done
  shopt -u nullglob
}

activate_runtime() {
  [ -f "$INSTALL_ROOT/Release/libcef.so" ] || {
    echo "install-cef-runtime: missing $INSTALL_ROOT/Release/libcef.so after extract" >&2
    exit 1
  }
  normalize_release_resources
  ln -sfn "$INSTALL_ROOT" "$ACTIVE_LINK"
  log "active runtime: $ACTIVE_LINK -> $INSTALL_ROOT"
}

verify_render_smoke() {
  case "${MDE_CEF_SKIP_RENDER_SMOKE:-}" in
    1|true|TRUE|yes|YES)
      log "render smoke skipped by MDE_CEF_SKIP_RENDER_SMOKE"
      return 0
      ;;
  esac

  local helper="${MDE_CEF_RENDER_SMOKE_HELPER:-/usr/bin/mde-web-cef}"
  local bridge="${MDE_CEF_RENDER_SMOKE_BRIDGE:-/usr/libexec/mackesd/mde-web-cef-renderer}"
  local smoke_timeout="${MDE_CEF_RENDER_SMOKE_TIMEOUT:-30s}"
  if [ ! -x "$helper" ] || [ ! -x "$bridge" ]; then
    log "render smoke skipped: helper or renderer bridge is not installed"
    return 0
  fi

  need_cmd timeout
  local smoke_log
  smoke_log="$(mktemp "${TMPDIR:-/tmp}/mde-cef-render-smoke.XXXXXX")"
  if MDE_CEF_ROOT="$ACTIVE_LINK" MDE_CEF_BRIDGE_BIN="$bridge" \
      timeout "$smoke_timeout" "$helper" render-once \
        --url 'data:text/html,<html><body><h1>MCNF CEF runtime smoke</h1></body></html>' \
        --width 320 --height 240 >"$smoke_log" 2>&1 \
      && grep -q 'CEF_BROWSER_PAINT_READY' "$smoke_log"; then
    log "render smoke passed"
    rm -f "$smoke_log"
    return 0
  fi

  echo "install-cef-runtime: render smoke failed" >&2
  cat "$smoke_log" >&2 || true
  rm -f "$smoke_log"
  exit 1
}

verify_wire_smoke() {
  case "${MDE_CEF_SKIP_WIRE_SMOKE:-}" in
    1|true|TRUE|yes|YES)
      log "wire smoke skipped by MDE_CEF_SKIP_WIRE_SMOKE"
      return 0
      ;;
  esac

  local verifier="${MDE_CEF_WIRE_SMOKE_VERIFIER:-/usr/libexec/mackesd/cef-verify}"
  local helper="${MDE_CEF_WIRE_SMOKE_HELPER:-/usr/bin/mde-web-cef}"
  local smoke_timeout="${MDE_CEF_WIRE_SMOKE_TIMEOUT:-35s}"
  local budget="${MDE_CEF_WIRE_SMOKE_BUDGET:-20}"
  local url="${MDE_CEF_WIRE_SMOKE_URL:-data:text/html,%3Ctitle%3Emde-cef-wire-smoke%3C/title%3E%3Ch1%3Emde-cef-wire-smoke%3C/h1%3E}"
  if [ ! -x "$verifier" ] || [ ! -x "$helper" ]; then
    log "wire smoke skipped: verifier or helper is not installed"
    return 0
  fi

  need_cmd timeout
  local smoke_log
  smoke_log="$(mktemp "${TMPDIR:-/tmp}/mde-cef-wire-smoke.XXXXXX")"
  if MDE_CEF_ROOT="$ACTIVE_LINK" timeout "$smoke_timeout" "$verifier" "$helper" "$url" "$budget" >"$smoke_log" 2>&1 \
      && grep -q 'VERIFY RESULT=PASS' "$smoke_log" \
      && grep -q 'VERIFY on_paint_ready' "$smoke_log"; then
    log "wire smoke passed"
    rm -f "$smoke_log"
    return 0
  fi

  echo "install-cef-runtime: wire smoke failed" >&2
  cat "$smoke_log" >&2 || true
  rm -f "$smoke_log"
  exit 1
}

need_cmd curl
need_cmd sha256sum
need_cmd tar
need_cmd bzip2
need_cmd mktemp

mkdir -p "$CEF_CACHE" "$INSTALL_PARENT"

if verify_archive; then
  log "$CEF_ASSET already cached + sha256 verified"
else
  log "fetching $CEF_ASSET ($CEF_SIZE_BYTES bytes)"
  curl -fsSL --retry 3 "$CEF_URL" -o "$ARCHIVE.tmp"
  echo "$CEF_SHA256  $ARCHIVE.tmp" | sha256sum -c - >/dev/null 2>&1 || {
    rm -f "$ARCHIVE.tmp"
    echo "install-cef-runtime: SHA256 MISMATCH for $CEF_ASSET" >&2
    exit 1
  }
  mv "$ARCHIVE.tmp" "$ARCHIVE"
fi

if [ -f "$INSTALL_ROOT/Release/libcef.so" ]; then
  log "$CEF_VERSION already extracted"
  activate_runtime
  verify_render_smoke
  verify_wire_smoke
  exit 0
fi

tmp="$(mktemp -d "$INSTALL_PARENT/.cef-extract.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
log "extracting to $INSTALL_ROOT"
tar -xjf "$ARCHIVE" -C "$tmp"
topdir="$(find "$tmp" -mindepth 1 -maxdepth 1 -type d | head -1)"
[ -n "$topdir" ] || { echo "install-cef-runtime: archive had no top-level directory" >&2; exit 1; }
[ -f "$topdir/Release/libcef.so" ] || {
  echo "install-cef-runtime: archive does not contain Release/libcef.so" >&2
  exit 1
}
rm -rf "$INSTALL_ROOT.new"
mv "$topdir" "$INSTALL_ROOT.new"
rm -rf "$INSTALL_ROOT"
mv "$INSTALL_ROOT.new" "$INSTALL_ROOT"
printf 'version=%s\nchromium=%s\nchannel=%s\nasset=%s\nsha256=%s\n' \
  "$CEF_VERSION" "$CEF_CHROMIUM_VERSION" "$CEF_CHANNEL" "$CEF_ASSET" "$CEF_SHA256" \
  > "$INSTALL_ROOT/mde-cef-runtime.manifest"
activate_runtime
verify_render_smoke
verify_wire_smoke
