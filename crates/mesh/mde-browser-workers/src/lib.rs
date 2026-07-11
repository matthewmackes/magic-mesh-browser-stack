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
