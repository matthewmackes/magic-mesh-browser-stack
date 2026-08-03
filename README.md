# Magic Mesh Browser Stack

This repository is the intended standalone home for the legacy host Browser
stack extracted from Construct. Its provenance and source-to-destination map
are recorded in [`docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md`](docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md).

## Current build scope

The root workspace contains the dependency-complete wire, policy, preview
client, worker core, Bus, seal, mesh-type, and Browser worker crates. They
resolve only against crates.io and extracted standalone crates:

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
cargo clippy --workspace --lib --locked --offline -- -D warnings
cargo clippy -p mde-web-preview-client --all-targets --locked --offline -- -D warnings
```

Those checks cover the preserved Servo, CEF, sandbox, shell-side bridge, and
worker runtime source. They are compile/lint evidence only: a vendored CEF
payload and live guest/seat acceptance are separate gates.

## Why the workspace is intentionally split

The worker family and its shared platform crates are now part of the root
workspace. The client no longer points at the root-only `mde-egui` harness; it
uses the public `egui` data types directly. No placeholder crates were added.

The nested Servo, CEF, and sandbox manifests remain separate workspaces because
of their native/runtime constraints. Their committed locks and farm checks
resolve against the extracted wire crate and crates.io. Live runtime/image
acceptance remains open; this repository therefore proves the preserved
helper/client/worker build boundary, not production Chromium readiness.

## Provenance status

The history-bearing repository is published at
[`matthewmackes/magic-mesh-browser-stack`](https://github.com/matthewmackes/magic-mesh-browser-stack)
from commit `996d3d27cfc4c52776c2289a0069d92e2bede66d`. The root and native helper build boundaries have clean
clone/farm evidence; complete source cleanup and live guest cutover remain open
gates.
