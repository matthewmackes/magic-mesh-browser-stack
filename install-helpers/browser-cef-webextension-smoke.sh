#!/usr/bin/env bash
# BROWSER-DD-5 — live CEF WebExtensions smoke runner.
#
# This wrapper proves the CEF extension host executed an unpacked WebExtension by
# loading the packaged smoke registry, enabling Power Mode for that fixture, and
# requiring the smoke extension's visible autofill marker through the CEF
# page-text probe.
set -euo pipefail

usage() {
  cat <<'USAGE'
browser-cef-webextension-smoke [--url URL] [--marker TEXT] [--registry PATH]

Runs mde-web-cef with:
  MDE_CEF_BROWSER_PROBE=1
  MDE_CEF_EXTENSION_POWER_MODE=true
  MDE_CEF_TEXT_PROBE_EXPECT=<marker>

Without --url, the runner serves a local login form and requires the packaged
smoke extension to call back to /mde-cef-extension-smoke?autofill=ok.

The default registry is the repo fixture when run from source, otherwise:
  /usr/share/magic-mesh/browser/webextensions-smoke.env
USAGE
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/.." 2>/dev/null && pwd || true)"
REPO_REGISTRY="$REPO/packaging/browser/webextensions-smoke.env"
INSTALLED_REGISTRY="/usr/share/magic-mesh/browser/webextensions-smoke.env"

REGISTRY="${MDE_CEF_SMOKE_REGISTRY:-}"
MARKER="${MDE_CEF_SMOKE_MARKER:-mde-cef-extension-autofill-ok}"
URL="${MDE_CEF_SMOKE_URL:-}"
HELPER="${MDE_CEF_SMOKE_HELPER:-mde-web-cef}"
PORT="${MDE_CEF_SMOKE_PORT:-0}"
RUN_TIMEOUT="${MDE_CEF_SMOKE_TIMEOUT:-30s}"
SERVER_PID=""
TMPDIR_SMOKE=""
SERVER_LOG=""
HELPER_LOG=""
CUSTOM_URL=0

cleanup() {
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" >/dev/null 2>&1 || true
  fi
  if [ -n "$TMPDIR_SMOKE" ]; then
    rm -rf "$TMPDIR_SMOKE"
  fi
  if [ -n "$HELPER_LOG" ]; then
    rm -f "$HELPER_LOG"
  fi
}
trap cleanup EXIT

while [ "$#" -gt 0 ]; do
  case "$1" in
    --url)
      [ "$#" -ge 2 ] || { echo "browser-cef-webextension-smoke: --url needs a value" >&2; exit 2; }
      URL="$2"
      CUSTOM_URL=1
      shift 2
      ;;
    --marker)
      [ "$#" -ge 2 ] || { echo "browser-cef-webextension-smoke: --marker needs a value" >&2; exit 2; }
      MARKER="$2"
      shift 2
      ;;
    --registry)
      [ "$#" -ge 2 ] || { echo "browser-cef-webextension-smoke: --registry needs a value" >&2; exit 2; }
      REGISTRY="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      echo "browser-cef-webextension-smoke: unknown argument $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [ -z "$REGISTRY" ]; then
  if [ -r "$REPO_REGISTRY" ]; then
    REGISTRY="$REPO_REGISTRY"
  else
    REGISTRY="$INSTALLED_REGISTRY"
  fi
fi

[ -r "$REGISTRY" ] || {
  echo "browser-cef-webextension-smoke: missing smoke registry $REGISTRY" >&2
  exit 78
}

if [ -z "$URL" ]; then
  command -v python3 >/dev/null 2>&1 || {
    echo "browser-cef-webextension-smoke: python3 required for the local smoke page" >&2
    exit 78
  }
  TMPDIR_SMOKE="$(mktemp -d)"
  SERVER_LOG="$TMPDIR_SMOKE/http.log"
  PORT_FILE="$TMPDIR_SMOKE/port"
  cat > "$TMPDIR_SMOKE/index.html" <<'HTML'
<!doctype html>
<html>
  <body>
    <form id="login">
      <label>User <input name="username" autocomplete="username"></label>
      <label>Password <input type="password" name="password" autocomplete="current-password"></label>
      <button>Sign in</button>
    </form>
  </body>
</html>
HTML
  python3 - "$TMPDIR_SMOKE" "$PORT_FILE" "$PORT" >"$SERVER_LOG" 2>&1 <<'PY' &
import functools
import http.server
import socketserver
import sys

root, port_file, port = sys.argv[1], sys.argv[2], int(sys.argv[3])

class ReuseTcpServer(socketserver.TCPServer):
    allow_reuse_address = True

handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=root)
with ReuseTcpServer(("127.0.0.1", port), handler) as httpd:
    with open(port_file, "w", encoding="utf-8") as fh:
        fh.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY
  SERVER_PID="$!"
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    [ -s "$PORT_FILE" ] && break
    sleep 0.1
  done
  [ -s "$PORT_FILE" ] || {
    echo "browser-cef-webextension-smoke: local smoke server did not start" >&2
    [ ! -s "$SERVER_LOG" ] || cat "$SERVER_LOG" >&2
    exit 78
  }
  PORT="$(cat "$PORT_FILE")"
  URL="http://127.0.0.1:$PORT/"
fi

export MDE_CEF_EXTENSION_REGISTRY="$REGISTRY"
export MDE_CEF_EXTENSION_POWER_MODE=true
export MDE_CEF_BROWSER_PROBE=1
export MDE_CEF_TEXT_PROBE_EXPECT="$MARKER"

HELPER_LOG="$(mktemp "${TMPDIR:-/tmp}/mde-cef-extension-helper.XXXXXX")"
set +e
if command -v timeout >/dev/null 2>&1; then
  timeout "$RUN_TIMEOUT" "$HELPER" render-once --url "$URL" >"$HELPER_LOG" 2>&1
else
  "$HELPER" render-once --url "$URL" >"$HELPER_LOG" 2>&1
fi
status=$?
set -e
cat "$HELPER_LOG"

if [ "$CUSTOM_URL" -eq 0 ]; then
  if grep -q 'GET /mde-cef-extension-smoke?marker=ok&autofill=ok' "$SERVER_LOG"; then
    echo "CEF_EXTENSION_AUTOFILL_SMOKE_READY url=$URL"
    exit 0
  fi
  if grep -q 'CEF_EXTENSIONS_WINDOWLESS_ALLOY_GATED' "$HELPER_LOG"; then
    echo "browser-cef-webextension-smoke: CEF windowless Alloy runtime cannot prove WebExtensions content scripts; use MDE_CEF_ALLOW_ALLOY_EXTENSION_SMOKE=1 only for diagnostics" >&2
    exit 78
  fi
  echo "browser-cef-webextension-smoke: extension autofill beacon not observed" >&2
  if [ -s "$SERVER_LOG" ]; then
    tail -n 20 "$SERVER_LOG" >&2 || true
  fi
  exit 78
fi

exit "$status"
