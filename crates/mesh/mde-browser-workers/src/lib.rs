//! Per-seat browser workers for the `mackesd` supervisor.
//!
//! arch-7 (2026-07-11) extracted these out of the `mackesd` fleet
//! control-plane bin crate: a per-SEAT browser concern (passkeys, offline
//! cache, read-aloud/voice/translate, external-protocol/share, tab-suspend,
//! session-sync, browser policy) had been mixed into the control-plane
//! daemon — a layering violation and a large compile-coupling.
//!
//! Each worker implements the [`mde_worker_core::Worker`] trait and is driven
//! by `mackesd`'s in-process supervisor. `mackesd` depends on this crate and
//! re-exports the modules under `workers::` so its supervisor spawn sites (and
//! the arch-5 census / drift-guard) reach them under the same
//! `workers::browser_*` paths as before.

#![forbid(unsafe_code)]

// BROWSER-DD-6 — Browser passkey/WebAuthn ceremony owner. Drains strict
// action/browser/passkey handoffs, persists pending challenges locally, mirrors
// them into the Syncthing-backed workgroup root, owns the software platform-
// authenticator key store (sealing platform passkey private keys via the shared
// `mde_seal::{seal_bytes, unseal_bytes}` primitives), and publishes honest
// pending/created/asserted/error state without minting fake credentials.
// arch-7 (2026-07-11): the 11th and final browser worker to leave the mackesd
// control-plane crate — unblocked by extracting the seal primitives into
// `mde-seal`, so it no longer depends back on the daemon.
pub mod browser_passkeys;
// BROWSER-DD-12 — Browser external-protocol owner. Drains
// action/browser/protocol handoffs for external schemes Browser refused to
// navigate, validates mailto/email and magnet/transfers routes, and publishes
// retained route status/events without faking the downstream surface.
pub mod browser_protocol;
// BROWSER-DD-11 — Browser read-aloud/TTS owner. Drains bounded
// action/browser/read-aloud page-text requests, invokes a locally configured
// offline TTS command when present, and publishes honest unavailable/error/spoken
// state for the shell.
pub mod browser_read_aloud;
// BROWSER-DD-12 — Browser CEF security-update status owner. Watches the
// packaged fast-update manifest and active CEF runtime, publishing an honest
// current/missing/mismatch posture for the independent browser-engine update path.
pub mod browser_security_update;
// BROWSER-DD-12 — Browser platform-share owner. Drains
// action/browser/share handoffs, validates Peer/Email/QR routes, and publishes
// retained route status/events without faking downstream delivery.
pub mod browser_share;
// BROWSER-DD-12 — Browser idle-tab suspend owner. Drains shell-published
// action/browser/tab-suspend handoffs after the shell has stopped the inactive
// helper, validates the payload, and publishes retained suspend status/events.
pub mod browser_tab_suspend;
// BROWSER-DD-12 — Browser private offline/mesh translation owner. Drains
// action/browser/translate, invokes a locally configured offline/mesh translation
// command when present, emits bounded translation events, and publishes honest
// unavailable/error/translated state for the shell.
pub mod browser_translate;
// BROWSER-DD-11 — Browser voice-command/dictation STT owner. Drains
// action/browser/voice-command, invokes a locally configured offline STT/capture
// command when present, emits bounded transcript events, and publishes honest
// unavailable/error/transcribed state for the shell.
pub mod browser_voice_command;
// BROWSER-DD-12 — Browser offline/mesh cache owner. Drains explicit Browser
// cache snapshots, validates private offline/mesh payloads, writes local durable
// records, and mirrors them into the Syncthing-backed workgroup root. Uses the
// shared `mackes_mesh_types::shared_root_writable` guard for the share seam.
pub mod browser_offline_cache;
// BROWSER-DD-7 — Browser session-sync owner. Drains the Browser's
// action/browser/session-sync snapshots, validates the restore-compatible JSON
// shape, persists the latest local copy, and mirrors it to the Syncthing-backed
// workgroup root as browser-session-sync/<host>/latest.json.
pub mod browser_session_sync;
// BOOKMARKS-8 — the mesh-wide browser/ad-blocker POLICY worker. Replicates the
// operator-authored fleet policy doc over the encrypted share, folds it for
// THIS node's role (the role NAME is passed into the worker by the daemon),
// and enforces at the browser launch/spawn seam. Uses the shared
// `mackes_mesh_types::shared_root_writable` guard for the share seam.
pub mod browser_policy;
