# Magic Mesh Browser Stack

This repository is the intended standalone home for the legacy host Browser
stack extracted from Construct. Its provenance and source-to-destination map
are recorded in [`docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md`](docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md).

## Current build scope

The root workspace contains three dependency-complete crates: `mde-web-wire`,
the pure-`std` length-prefixed socket contract; `mde-adblock`, the headless
filter engine; and `mde-web-preview-client`, the shell-side IPC, shared-memory
frame, input, and crash-state bridge. They resolve only against crates.io and
the extracted wire crate:

```text
cargo test --workspace --locked
```

This command is the clean-clone check for the admitted root workspace. The same
boundary is enforced by
[`install-helpers/verify-standalone-workspace.sh`](install-helpers/verify-standalone-workspace.sh)
and `.github/workflows/standalone.yml`.

The native helper roots are also checked independently on the build farm:

```text
cargo check --manifest-path crates/desktop/mde-web-sandbox/Cargo.toml --locked --offline
cargo check --manifest-path crates/desktop/mde-web-cef/Cargo.toml --locked --offline
cargo check --manifest-path crates/desktop/mde-web-preview/Cargo.toml --locked --offline
cargo clippy --manifest-path crates/desktop/mde-web-preview-client/Cargo.toml --all-targets --locked --offline -- -D warnings
```

Those checks cover the preserved Servo, CEF, sandbox, and shell-side bridge
source. They are compile/lint evidence only: a vendored CEF payload and live
guest/seat acceptance are separate gates.

## Why the workspace is intentionally split

The extracted worker manifest still requires shared platform crates that are
not present in this repository: `mde-worker-core`, `mde-bus`,
`mackes-mesh-types`, and `mde-seal`. It remains outside the root workspace
until those contracts are extracted. The client no longer points at the
root-only `mde-egui` harness; it uses the public `egui` data types directly.
No placeholder crates were added.

The nested Servo, CEF, and sandbox manifests remain separate workspaces because
of their native/runtime constraints. Their committed locks and farm checks
resolve against the extracted wire crate and crates.io. The worker family and
live runtime/image acceptance remain open; this repository therefore proves
the preserved helper/client build boundary, not production Chromium readiness.

## Provenance status

The history-bearing repository is published at
[`matthewmackes/magic-mesh-browser-stack`](https://github.com/matthewmackes/magic-mesh-browser-stack)
from commit `db37bc4a`. The root and native helper build boundaries have clean
clone/farm evidence; worker extraction, complete source cleanup, and live
guest cutover remain open gates.
