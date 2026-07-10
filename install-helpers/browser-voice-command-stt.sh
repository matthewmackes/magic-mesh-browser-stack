#!/usr/bin/env bash
# BROWSER-DD-11 — default Browser voice-command STT command.
#
# The mackesd browser_voice_command worker feeds the Browser request JSON on
# stdin and expects a transcript on stdout. This wrapper is deliberately honest:
# it only captures/transcribes when an operator-configured local capture command,
# local transcribe command, and local model are present. Otherwise it exits 69 so
# the worker publishes `unavailable` instead of fabricating a transcript.
set -euo pipefail

unavailable() {
  echo "browser-voice-command-stt: $*" >&2
  exit 69
}

WORK="$(mktemp -d /tmp/browser-voice-command-stt.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
REQUEST="$WORK/request.json"
WAV="$WORK/input.wav"
cat >"$REQUEST"
[ -s "$REQUEST" ] || unavailable "no Browser voice request supplied on stdin"

MODEL="${MDE_BROWSER_STT_MODEL:-${MDE_STT_MODEL:-/usr/share/magic-mesh/stt/browser-voice-command.model}}"
[ -f "$MODEL" ] || unavailable "STT model not found at $MODEL"

TRANSCRIBE="${MDE_BROWSER_STT_TRANSCRIBE_COMMAND:-${MDE_STT_TRANSCRIBE_COMMAND:-}}"
[ -n "$TRANSCRIBE" ] || unavailable "set MDE_BROWSER_STT_TRANSCRIBE_COMMAND to a local offline STT pipeline"

if [ -n "${MDE_BROWSER_STT_AUDIO_FILE:-}" ]; then
  [ -r "$MDE_BROWSER_STT_AUDIO_FILE" ] || unavailable "audio file not readable: $MDE_BROWSER_STT_AUDIO_FILE"
  cp "$MDE_BROWSER_STT_AUDIO_FILE" "$WAV"
elif [ -n "${MDE_BROWSER_STT_CAPTURE_COMMAND:-}" ]; then
  sh -c "$MDE_BROWSER_STT_CAPTURE_COMMAND" browser-voice-command-stt "$WAV" "$REQUEST"
elif command -v arecord >/dev/null 2>&1; then
  DURATION="${MDE_BROWSER_STT_CAPTURE_SECONDS:-5}"
  arecord -q -f S16_LE -r 16000 -c 1 -d "$DURATION" "$WAV"
else
  unavailable "no audio capture path configured; set MDE_BROWSER_STT_CAPTURE_COMMAND"
fi

[ -s "$WAV" ] || unavailable "audio capture produced no WAV data"

TRANSCRIPT="$WORK/transcript.txt"
sh -c "$TRANSCRIBE" browser-voice-command-stt "$WAV" "$REQUEST" "$MODEL" >"$TRANSCRIPT"
[ -s "$TRANSCRIPT" ] || unavailable "STT pipeline produced no transcript"
sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$TRANSCRIPT"
