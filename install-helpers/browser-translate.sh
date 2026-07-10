#!/usr/bin/env bash
# BROWSER-DD-12 — default Browser private translation command.
#
# The mackesd browser_translate worker feeds the Browser request JSON on stdin
# and expects translated text on stdout. This wrapper is deliberately honest: it
# only translates when an operator-configured local/mesh translation command and
# local model are present. Otherwise it exits 69 so the worker publishes
# `unavailable` instead of fabricating translated text.
set -euo pipefail

unavailable() {
  echo "browser-translate: $*" >&2
  exit 69
}

WORK="$(mktemp -d /tmp/browser-translate.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT
REQUEST="$WORK/request.json"
TRANSLATION="$WORK/translation.txt"
cat >"$REQUEST"
[ -s "$REQUEST" ] || unavailable "no Browser translation request supplied on stdin"

MODEL="${MDE_BROWSER_TRANSLATE_MODEL:-${MDE_TRANSLATE_MODEL:-/usr/share/magic-mesh/translate/browser-translate.model}}"
[ -f "$MODEL" ] || unavailable "translation model not found at $MODEL"

ENGINE="${MDE_BROWSER_TRANSLATE_ENGINE_COMMAND:-${MDE_TRANSLATE_ENGINE_COMMAND:-}}"
[ -n "$ENGINE" ] || unavailable "set MDE_BROWSER_TRANSLATE_ENGINE_COMMAND to a local offline/mesh translation pipeline"

sh -c "$ENGINE" browser-translate "$REQUEST" "$MODEL" >"$TRANSLATION"
[ -s "$TRANSLATION" ] || unavailable "translation pipeline produced no text"
sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$TRANSLATION"
