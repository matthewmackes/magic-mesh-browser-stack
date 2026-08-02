# Legacy host Browser extraction provenance

This is the Phase 0 prerequisite for `WL-ARCH-008`. It records the current
host Browser inventory without deleting source or rewriting the live
`magic-mesh` history. The history-bearing standalone repository is published;
the later source removal and VM cutover remain open work.

## Immutable source snapshot

- Source repository: `https://github.com/matthewmackes/magic-mesh.git`
- Source commit used for this manifest: the `source_commit` recorded in
  [`manifest.tsv`](manifest.tsv) (the manifest is generated before this
  provenance commit, so later unrelated commits do not change its anchor).
- Source branch: recorded in `manifest.tsv`.
- Destination: `matthewmackes/magic-mesh-browser-stack`.
- Extraction method: `verify-browser-extraction.sh --write` enumerates tracked
  paths with `git ls-files`, adds scoped host-Browser signal matches, records
  each source blob SHA, and writes a sorted TSV path map. It also records a
  worktree blob SHA/state so concurrent edits in mixed/shared files remain
  visible without being mistaken for committed extraction history. `--check`
  compares that generated candidate set with every manifest row, verifies the
  immutable source blob, and verifies clean worktree bytes exactly. A dirty
  mixed/shared row is required to remain divergent from the source snapshot,
  while its recorded worktree hash remains an audit-time observation.
- Current safety posture: the standalone publication is recorded below; no
  source deletion or live-worktree history rewrite has been performed. The
  verifier rejects untracked Browser candidates and Browser paths changed since
  the anchored source snapshot.

The manifest has three classes:

| Class | Meaning | Destination convention |
| --- | --- | --- |
| `browser-owned` | The current host Browser implementation, helper, worker, runtime, policy, model, verification, or Browser-only asset. | `magic-mesh-browser-stack:<source path>` — preserve the relative path for the first history-bearing extraction. |
| `mixed-purpose` | A live file containing Browser seams plus non-Browser shell, daemon, package, KDC, transfer, or image behavior. | `split-in-magic-mesh-browser-stack:<source path>#browser-sections` — split the named Browser sections before host removal. |
| `shared` | A contract, crypto/storage dependency, legal file, or generic gate with Browser consumers and non-Browser callers. | `retain-in-magic-mesh:<source path>#shared-contract-or-reference` — keep the shared implementation; copy only the compatibility material required by the standalone repository. |

The manifest is intentionally path- and blob-specific. A new Browser signal in
the scoped source tree, a missing row, an unclassified row, an untracked path,
or a changed source blob fails closed. Dirty `browser-owned` paths fail closed;
dirty `mixed-purpose`/`shared` paths are recorded as `worktree_state=dirty` and
must be committed/reconciled before extraction.

## Standalone root status — 2026-08-02

Root workspace metadata now contains the dependency-complete
`crates/desktop/mde-web-wire` contract and the already-present pure
`crates/services/mde-adblock` engine. Its locked test is therefore a narrow
provenance/build sanity check, not full Browser-stack acceptance:

```text
cargo test --workspace --locked
```

The remaining extracted manifests are not admitted to the root workspace
because they still reference shared crates absent from this repository:
`mde-egui`, `mde-worker-core`, `mde-bus`, `mackes-mesh-types`, and `mde-seal`.
No placeholder implementations were added. The admitted root workspace has
locked test/clippy CI and is published at
`matthewmackes/magic-mesh-browser-stack` (publication commit `25c9e5bc`). Full
workspace buildability remains open until the omitted dependencies are
extracted or replaced with explicit standalone contracts.

## Current workspace, package, and process inventory

The inventory is derived from the current tree rather than a hand-maintained
filename guess. The main workspace and package edges are mixed in the root
`Cargo.toml`/`Cargo.lock`, `mde-shell-egui/Cargo.toml`, and `mackesd/Cargo.toml`:

- Browser application surface: `mde-shell-egui/src/web/**`, with shell route,
  `Surface::Browser`, Front Door, navigation, policy projection, transfer,
  KDC/MPRIS, and package seams in the mixed rows.
- Helper/workspace packages: `mde-web-preview` (Servo), `mde-web-cef` (CEF),
  `mde-web-sandbox`, `mde-web-wire`, `mde-web-preview-client`, and
  `mde-browser-workers`. CEF and Servo have separate lock/build roots today.
- Browser-only service/worker material: `mde-adblock` and the
  `mde-browser-workers` family; the Browser media transfer lane is under
  `mackesd` and is separately marked Browser-owned.
- Shared contracts: `mde-bookmarks`, `mde-bookmarks-egui`, `mde-seal`,
  `mde-worker-core`, and `mackes-mesh-types`; their non-Browser callers keep
  them in `magic-mesh` until a standalone compatibility layer exists.
- Host processes and installed helpers: `mde-web-preview`, `mde-web-cef`,
  `mde-web-cef-renderer`, `cef-verify`, plus the Browser verification,
  CEF/Widevine, SELinux, TTS/STT/translation, and model provisioning helpers.
- Package/payload identity: the `magic-mesh-browser` RPM variant and its
  Browser assets, systemd units, SELinux modules, and first-boot setup hooks
  are represented by the Browser-owned and mixed package rows.
- Preserved legal roots: `LICENSE` and `NOTICE` must be copied to the
  standalone repository with attribution intact; they are marked `shared`.

## Persistent Browser data locations

These are the locations and state classes found in the current host stack. They
are inventory only; this prerequisite does not read, copy, or delete user data.

| Location or source | State | Migration/provenance handling |
| --- | --- | --- |
| `$MDE_WORKGROUP_ROOT/browser/` (normally `/mnt/mesh-storage/browser/`) | Managed URL policy, safe-browsing hosts, custom filter rules, and Browser policy inputs. | Browser-owned policy data; back up and map explicitly before removal. |
| `$MDE_WORKGROUP_ROOT/browser-session-sync/<host>/latest.json` and `send-tab-outbox/` | Replicated startup/session snapshots and send-tab outbox records. | Portable session/download metadata only; preserve backups and report import results. |
| `/var/lib/mde/browser-session-sync/` and the configured Bus/workgroup data root | Local session-sync fallback and daemon Browser worker state. | Preserve legacy records; do not silently turn them into guest credentials. |
| `$XDG_DATA_HOME/mde/browser/` or `~/.local/share/mde/browser/` | Shell Browser captures, PDFs, scrape exports, offline-cache outputs, and Browser-local generated artifacts. | User data and downloads must survive package removal; destination/import remains explicit. |
| User-configured `Downloads` directory (fixtures use `/home/mm/Downloads`) | Completed Browser downloads and transfer-ledger destinations. | Never recursively delete; reconcile the transfer ledger and files independently. |
| `$XDG_CACHE_HOME` / `/var/cache/magic-mesh/{tts,stt,translate}` | Browser TTS/STT/translation voice/model cache and installed model assets. | Treat models as replaceable assets; record skipped/failed import rows. |
| `/usr/share/magic-mesh/{browser,tts,stt,translate}/` | Packaged Browser manifests, policy/runtime metadata, and active model paths. | Package assets move to the standalone repository; host uninstall must not remove user state. |
| `/opt/mde/cef` and `/tmp/.mde-web-{preview,cef}-root*` | CEF runtime plus disposable helper sandbox roots and shared-memory/socket activity. | Runtime is Browser-owned and disposable; never classify sandbox scratch as user profile data. |
| Browser worker Bus/workgroup topics (`state/browser-*`, `action/browser-*`, `adfilter/compiled/`) | Policy, passkey ceremony handoff, offline cache, security posture, session sync, media, share, and worker state. | Preserve sealed/private records; cookies, passwords, private passkeys, and sealed credentials are never silently exported to a guest. |

The current Servo helper intentionally has no persistent profile/history/home;
its private root is a disposable `/tmp/.mde-web-preview-root*` sandbox. CEF
runtime caches are likewise private/ephemeral under the helper's configured
runtime roots. This distinction is recorded so a later migration does not
invent a profile that the current source does not own.

## Focused test baseline

The source baseline to run before extraction/removal is the following focused
set. This document records the baseline targets; it does not claim they were
rerun by this manifest-only prerequisite.

```text
cargo test -p mde-shell-egui --lib
cargo test -p mde-web-preview-client --features testkit
cargo test --manifest-path crates/desktop/mde-web-wire/Cargo.toml --locked
cargo test --manifest-path crates/desktop/mde-web-preview/Cargo.toml --locked
cargo test --manifest-path crates/desktop/mde-web-cef/Cargo.toml --locked
cargo test -p mde-browser-workers --locked
cargo test -p mackesd --lib browser
install-helpers/verify-rpm-payload.sh payload crates/mesh/mackesd/Cargo.toml
```

The standalone repository must later repeat the helper/renderer/verifier,
worker, package, and clean-clone checks without a sibling `magic-mesh`
checkout. This manifest is the source-to-destination evidence needed to make
that extraction auditable.

## Remaining extraction and cutover actions

1. Review this manifest and the persistent-data inventory.
2. Complete the history-preserving extraction in this repository, including
   the omitted helper/worker/package dependencies, while preserving
   `LICENSE`/`NOTICE` and updating this provenance record.
3. Build/test the complete standalone repository from a clean clone, then
   remove the corresponding host Browser source from `magic-mesh`.
4. Finish the live Browser VM image, VDI attachment, guest Chromium rendering,
   input, audio/video, reconnect, performance, and six-node acceptance gates.
