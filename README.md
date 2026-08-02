# Magic Mesh Browser Stack

This repository is the intended standalone home for the legacy host Browser
stack extracted from Construct. Its provenance and source-to-destination map
are recorded in [`docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md`](docs/design/browser-stack-extraction/UPSTREAM-SOURCE.md).

## Current build scope

The root workspace currently contains two dependency-complete crates:
`mde-web-wire`, the pure-`std` length-prefixed socket contract, and
`mde-adblock`, the headless filter engine. Both are present in this checkout
and resolve only against crates.io dependencies:

```text
cargo test --workspace --locked
```

This command is an honest clean-clone check for the admitted root workspace.
The same boundary is enforced by
[`install-helpers/verify-standalone-workspace.sh`](install-helpers/verify-standalone-workspace.sh)
and `.github/workflows/standalone.yml`. The complete extracted stack is not
yet independently buildable, so this CI does not overclaim native helper
acceptance.

## Why the workspace is intentionally narrow

The extracted client and worker manifests still require shared crates that are
not present in this repository: `mde-egui`, `mde-worker-core`, `mde-bus`,
`mackes-mesh-types`, and `mde-seal`. The client also retains path edges into
those omitted crates. Adding placeholder crates or listing those packages in
the root workspace would fabricate a build and violate the extraction
contract.

The nested Servo, CEF, and sandbox manifests remain separate workspaces because
of their native/runtime constraints, but they also require the extracted wire
crate and a complete standalone dependency map before full-stack publication.
Until those dependencies and package boundaries are resolved, this repository
must not be treated as proof that the host Browser has been removed.

## Provenance status

The local candidate has not been published at
`matthewmackes/magic-mesh-browser-stack`. The target remote, a history-complete
extraction commit, full clean-clone build evidence, and live guest cutover
evidence remain open gates.
