#!/bin/bash
# BROWSER-DD-1: mirror the pinned CEF runtime tarball into DO Spaces.
#
# This is an operator/build helper for upstream-outage resilience. It does not
# print or manage Spaces credentials; rclone reads the configured remote. The
# install helper still verifies the SHA-256 after download, so the mirror cannot
# silently change the runtime payload.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST="${MDE_CEF_MANIFEST:-$REPO/packaging/browser/cef-linux64-minimal.env}"
[ -r "$MANIFEST" ] || { echo "mirror-cef-runtime: missing manifest $MANIFEST" >&2; exit 2; }
# shellcheck source=/dev/null
. "$MANIFEST"

CACHE="${MDE_CEF_CACHE:-$REPO/vendor/cef}"
ARCHIVE="${MDE_CEF_ARCHIVE:-$CACHE/$CEF_ASSET}"
REMOTE="${MDE_CEF_SPACES_REMOTE:-$CEF_SPACES_REMOTE}"
PUSH=0

while [ $# -gt 0 ]; do
  case "$1" in
    --push) PUSH=1; shift ;;
    --dry-run) PUSH=0; shift ;;
    -h|--help)
      sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "mirror-cef-runtime: unknown arg '$1'" >&2; exit 2 ;;
  esac
done

need_cmd() { command -v "$1" >/dev/null 2>&1 || { echo "mirror-cef-runtime: missing $1" >&2; exit 2; }; }
need_cmd curl
need_cmd rclone
need_cmd sha256sum

if [ "$PUSH" -eq 0 ]; then
  cache_state="missing"
  if [ -f "$ARCHIVE" ] && echo "$CEF_SHA256  $ARCHIVE" | sha256sum -c - >/dev/null 2>&1; then
    cache_state="present+verified"
  fi
  cat <<EOF
mirror-cef-runtime --dry-run (nothing fetched, nothing pushed):
  source:  $ARCHIVE ($cache_state)
  sha256:  $CEF_SHA256
  remote:  $REMOTE

To mirror:
  install-helpers/mirror-cef-runtime-to-spaces.sh --push
EOF
  exit 0
fi

mkdir -p "$CACHE"
if [ -f "$ARCHIVE" ] && echo "$CEF_SHA256  $ARCHIVE" | sha256sum -c - >/dev/null 2>&1; then
  echo "mirror-cef-runtime: archive already cached + verified: $ARCHIVE"
else
  echo "mirror-cef-runtime: fetching $CEF_ASSET"
  curl -fsSL --retry 3 "$CEF_URL" -o "$ARCHIVE.tmp"
  echo "$CEF_SHA256  $ARCHIVE.tmp" | sha256sum -c - >/dev/null 2>&1 || {
    rm -f "$ARCHIVE.tmp"
    echo "mirror-cef-runtime: SHA256 MISMATCH for $CEF_ASSET" >&2
    exit 1
  }
  mv "$ARCHIVE.tmp" "$ARCHIVE"
fi

echo "mirror-cef-runtime: copying $ARCHIVE -> $REMOTE"
rclone copyto "$ARCHIVE" "$REMOTE"
echo "mirror-cef-runtime: verifying remote stream hash"
remote_sha="$(rclone cat "$REMOTE" | sha256sum | awk '{print $1}')"
if [ "$remote_sha" != "$CEF_SHA256" ]; then
  echo "mirror-cef-runtime: remote SHA256 mismatch: $remote_sha != $CEF_SHA256" >&2
  exit 1
fi
echo "mirror-cef-runtime: mirrored OK ($remote_sha)"
