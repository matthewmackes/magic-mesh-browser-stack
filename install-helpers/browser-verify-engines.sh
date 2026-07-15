#!/usr/bin/env bash
# Browser two-engine operational verifier.
#
# Runs the installed shell-equivalent wire harness against both browser helpers:
# Chromium/CEF (`mde-web-cef`) and Servo (`mde-web-preview`). The harness proves
# nav, painted frames, and pointer/key/text page response over the same helper
# socket path the egui shell consumes.
set -euo pipefail

usage() {
  cat <<'USAGE'
browser-verify-engines [--engine cef|servo|all] [--budget SECONDS] [--timeout DURATION]

Default:
  browser-verify-engines --engine all

Environment overrides:
  MDE_BROWSER_VERIFY_VERIFIER=/usr/libexec/mackesd/cef-verify
  MDE_BROWSER_VERIFY_CEF_HELPER=/usr/bin/mde-web-cef
  MDE_BROWSER_VERIFY_SERVO_HELPER=/usr/bin/mde-web-preview
  MDE_BROWSER_VERIFY_CEF_ROOT=/opt/mde/cef
  MDE_BROWSER_VERIFY_CEF_BRIDGE=/usr/libexec/mackesd/mde-web-cef-renderer
  MDE_BROWSER_VERIFY_BUDGET=30
  MDE_BROWSER_VERIFY_TIMEOUT=45s
  MDE_BROWSER_VERIFY_KEEP_LOGS=1
  MDE_BROWSER_VERIFY_SKIP_PROCESS_CHECK=1

The CEF root/bridge overrides are only exported for the CEF run. Existing
MDE_CEF_ROOT and MDE_CEF_BRIDGE_BIN are otherwise inherited.
USAGE
}

log() { printf 'browser-verify-engines: %s\n' "$*"; }
die() {
  printf 'browser-verify-engines: %s\n' "$*" >&2
  exit 1
}
need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

VERIFIER="${MDE_BROWSER_VERIFY_VERIFIER:-/usr/libexec/mackesd/cef-verify}"
CEF_HELPER="${MDE_BROWSER_VERIFY_CEF_HELPER:-/usr/bin/mde-web-cef}"
SERVO_HELPER="${MDE_BROWSER_VERIFY_SERVO_HELPER:-/usr/bin/mde-web-preview}"
BUDGET="${MDE_BROWSER_VERIFY_BUDGET:-30}"
RUN_TIMEOUT="${MDE_BROWSER_VERIFY_TIMEOUT:-45s}"
ENGINE="all"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --engine)
      [ "$#" -ge 2 ] || die "--engine needs cef, servo, or all"
      ENGINE="$2"
      shift 2
      ;;
    --budget)
      [ "$#" -ge 2 ] || die "--budget needs seconds"
      BUDGET="$2"
      shift 2
      ;;
    --timeout)
      [ "$#" -ge 2 ] || die "--timeout needs a timeout(1) duration"
      RUN_TIMEOUT="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

case "$ENGINE" in
  cef|servo|all) ;;
  *) die "--engine must be cef, servo, or all" ;;
esac
case "$BUDGET" in
  ''|*[!0-9]*) die "--budget must be an integer number of seconds" ;;
esac

[ -x "$VERIFIER" ] || die "verifier is not executable: $VERIFIER"
need_cmd timeout
need_cmd mktemp
need_cmd grep
need_cmd sort
need_cmd comm
need_cmd pgrep
need_cmd ps
need_cmd id

TMPDIR_VERIFY="$(mktemp -d "${TMPDIR:-/tmp}/mde-browser-verify.XXXXXX")"
BEFORE_PIDS="$TMPDIR_VERIFY/before.pids"
AFTER_PIDS="$TMPDIR_VERIFY/after.pids"

cleanup() {
  if [ "${MDE_BROWSER_VERIFY_KEEP_LOGS:-0}" != "1" ]; then
    rm -rf "$TMPDIR_VERIFY"
  else
    log "kept logs in $TMPDIR_VERIFY"
  fi
}
trap cleanup EXIT

helper_pid_pattern='(^|/)(cef-verify|mde-web-cef|mde-web-preview|mde-web-cef-renderer)( |$)'
snapshot_browser_pids() {
  pgrep -u "$(id -u)" -f "$helper_pid_pattern" 2>/dev/null | sort -n || true
}

verify_no_new_processes() {
  case "${MDE_BROWSER_VERIFY_SKIP_PROCESS_CHECK:-}" in
    1|true|TRUE|yes|YES)
      log "process cleanup check skipped by MDE_BROWSER_VERIFY_SKIP_PROCESS_CHECK"
      return 0
      ;;
  esac
  snapshot_browser_pids > "$AFTER_PIDS"
  if comm -13 "$BEFORE_PIDS" "$AFTER_PIDS" | grep -q .; then
    echo "browser-verify-engines: helper/verifier processes survived the probe:" >&2
    while read -r pid; do
      [ -n "$pid" ] || continue
      ps -p "$pid" -o pid=,comm=,args= >&2 || true
    done < <(comm -13 "$BEFORE_PIDS" "$AFTER_PIDS")
    exit 1
  fi
  log "process cleanup passed"
}

run_engine() {
  local engine="$1"
  local helper="$2"
  local log_file="$TMPDIR_VERIFY/$engine.log"

  [ -x "$helper" ] || die "$engine helper is not executable: $helper"
  log "running $engine verifier helper=$helper budget=${BUDGET}s timeout=$RUN_TIMEOUT"

  local env_args=("MDE_BROWSER_VERIFY_INPUT=1")
  if [ "$engine" = "cef" ]; then
    if [ -n "${MDE_BROWSER_VERIFY_CEF_ROOT:-}" ]; then
      env_args+=("MDE_CEF_ROOT=$MDE_BROWSER_VERIFY_CEF_ROOT")
    fi
    if [ -n "${MDE_BROWSER_VERIFY_CEF_BRIDGE:-}" ]; then
      env_args+=("MDE_CEF_BRIDGE_BIN=$MDE_BROWSER_VERIFY_CEF_BRIDGE")
    fi
  fi

  if env "${env_args[@]}" timeout "$RUN_TIMEOUT" "$VERIFIER" "$helper" "" "$BUDGET" \
      >"$log_file" 2>&1 \
      && grep -q 'VERIFY RESULT=PASS' "$log_file" \
      && grep -q 'VERIFY on_paint_ready' "$log_file" \
      && grep -Eq 'mde-browser-verify-p1-k1-tm|P:1 K:1 T:m' "$log_file"; then
    log "$engine display/input verifier passed"
    return 0
  fi

  echo "browser-verify-engines: $engine verifier failed" >&2
  cat "$log_file" >&2 || true
  exit 1
}

snapshot_browser_pids > "$BEFORE_PIDS"
case "$ENGINE" in
  cef)
    run_engine cef "$CEF_HELPER"
    ;;
  servo)
    run_engine servo "$SERVO_HELPER"
    ;;
  all)
    run_engine cef "$CEF_HELPER"
    run_engine servo "$SERVO_HELPER"
    ;;
esac
verify_no_new_processes
log "PASS"
