#!/usr/bin/env bash
# Verify the clean-clone boundary of the currently admitted standalone
# workspace. This intentionally does not claim that the native CEF/Servo
# workspaces are complete; those remain separate extraction gates.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if command -v rg >/dev/null 2>&1; then
    dependency_search=(rg -n --hidden --glob '!.git/**' --glob 'Cargo.toml')
else
    dependency_search=(grep -REIn --exclude-dir=.git --include='Cargo.toml')
fi

if "${dependency_search[@]}" \
    'path *=.*magic-mesh|git *=.*magic-mesh|/root/magic-mesh|/home/.*/magic-mesh' \
    Cargo.toml crates; then
    echo "verify-standalone-workspace: forbidden source-repository dependency" >&2
    exit 1
fi

cargo metadata --no-deps --locked --format-version 1 >/dev/null
echo "verify-standalone-workspace: admitted workspace boundary passed"
