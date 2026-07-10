#!/usr/bin/env bash
# BROWSER-DD-11 — default Browser read-aloud TTS command.
#
# The mackesd browser_read_aloud worker feeds page text on stdin. This wrapper is
# deliberately honest: it speaks through Piper only when a local model and audio
# player are present, and exits 69 for "unavailable" so the worker can publish an
# unavailable status instead of fake playback.
set -euo pipefail

unavailable() {
  echo "browser-read-aloud-tts: $*" >&2
  exit 69
}

command -v piper >/dev/null 2>&1 || unavailable "piper command is not installed"

MODEL="${MDE_BROWSER_TTS_MODEL:-${MDE_TTS_MODEL:-/usr/share/magic-mesh/tts/browser-read-aloud.onnx}}"
[ -f "$MODEL" ] || unavailable "voice model not found at $MODEL"

PLAYER="${MDE_BROWSER_TTS_PLAYER:-${MDE_TTS_PLAYER:-}}"
if [ -z "$PLAYER" ]; then
  if command -v paplay >/dev/null 2>&1; then
    PLAYER='paplay "$1"'
  elif command -v aplay >/dev/null 2>&1; then
    PLAYER='aplay -q "$1"'
  else
    unavailable "no audio player found; install PulseAudio/PipeWire paplay or ALSA aplay"
  fi
fi

WORK="$(mktemp -d /tmp/browser-read-aloud-tts.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
TEXT="$WORK/text.txt"
WAV="$WORK/speech.wav"
cat >"$TEXT"
[ -s "$TEXT" ] || unavailable "no text supplied on stdin"

piper --model "$MODEL" --output_file "$WAV" <"$TEXT"
sh -c "$PLAYER" browser-read-aloud-tts "$WAV"
