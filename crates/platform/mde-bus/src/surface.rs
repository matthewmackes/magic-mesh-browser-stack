//! BUS-2.1 — priority → surface mapping.
//!
//! Per `docs/design/v6.x-mackes-bus.md` §6, every Bus message
//! has a `priority` field that drives which on-screen surfaces
//! the operator's UI lights up:
//!
//! | priority  | surfaces                                                                 |
//! |----------:|--------------------------------------------------------------------------|
//! | `min`     | silent log only (history available, no UI)                                |
//! | `default` | tray icon + Dock breadcrumb badge                                         |
//! | `high`    | status-zone slide-up strip + sound + persistent until ack                 |
//! | `urgent`  | Theater takeover + wallpaper stripe + phone push (KDC2 + ntfy app)        |
//!
//! This module owns the *dispatch table*: given a [`Priority`]
//! and a [`Surfaces`] trait implementation, it calls the right
//! sequence of surface methods. The Iced surfaces in
//! `crates/mde-portal/` + `crates/mde-popover/` + the phone-
//! push path in `crates/mde-kdc-proto/` impl the trait — this
//! crate stays GUI-free.
//!
//! Pure dispatcher → unit-testable with a stub `Surfaces` impl
//! that records every call. No tokio, no Iced, no Wayland —
//! just an enum + trait + match.

use std::sync::{Arc, Mutex};

use crate::hooks::config::Priority;
use crate::persist::StoredMessage;

/// The full set of UI surfaces a Bus message can light up.
/// Each method takes the [`StoredMessage`] so implementations
/// can render title + body + ULID + topic without re-fetching.
///
/// All methods are sync — the dispatcher doesn't `await`. Real
/// implementations spawn tokio tasks internally when they need
/// async work; the dispatcher fires-and-forgets.
pub trait Surfaces: Send + Sync {
    /// `min` priority — silent log only. Typically a no-op in
    /// production (the message is already in `Persist`); the
    /// hook lets tests assert that no other surface fired.
    fn log_silent(&self, msg: &StoredMessage);

    /// `default` priority — show in the tray drop-down +
    /// increment the Dock breadcrumb badge.
    fn tray_and_badge(&self, msg: &StoredMessage);

    /// `high` priority — open the status-zone slide-up strip
    /// with this message + play the alert sound once.
    fn status_strip_and_sound(&self, msg: &StoredMessage);

    /// `urgent` priority — Theater takeover (full-screen
    /// layer-shell overlay), paint a wallpaper stripe, AND
    /// push to the operator's paired phone via KDC2 + ntfy app.
    fn theater_wallpaper_phone(&self, msg: &StoredMessage);
}

/// Dispatch the message to the right surface(s) based on its
/// priority. The priority string comes from
/// [`StoredMessage::priority`] (lowercase: `min` / `default` /
/// `high` / `urgent`). Unknown priorities fall back to `default`
/// — same safety semantics as the retention engine.
pub fn dispatch(msg: &StoredMessage, surfaces: &dyn Surfaces) {
    let p = parse_priority(&msg.priority);
    match p {
        Priority::Min => surfaces.log_silent(msg),
        Priority::Default => surfaces.tray_and_badge(msg),
        Priority::High => surfaces.status_strip_and_sound(msg),
        Priority::Urgent => surfaces.theater_wallpaper_phone(msg),
    }
}

/// BUS-2.8 — dispatch with DND/quiet-hour suppression applied.
/// When `crate::dnd::is_suppressed` returns true, the message is
/// log-silent regardless of priority (the persist + audit paths
/// upstream already wrote the message; this just gates routing
/// to display surfaces). When suppression is false, falls
/// through to the standard priority dispatch.
///
/// `tags` is the message's tag list (typically `[
/// "priority=high", "source=fleet", ...]`) including any
/// `override=dnd` bypass marker.
///
/// BUS-6.7 — a fleet-wide topic snooze (in `state.snoozes`) also
/// suppresses routing: if any unexpired snooze pattern matches the
/// message topic, the message is log-silent. The message's own
/// `ts_unix_ms` serves as "now" for the expiry check — it was just
/// written, so its timestamp is the dispatch instant. The
/// `override=dnd` bypass still wins over a snooze, matching the DND
/// emergency-bypass contract.
pub fn dispatch_with_dnd(
    msg: &StoredMessage,
    state: &crate::dnd::DndState,
    topic_hours: crate::dnd::TopicQuietHours,
    tags: &[&str],
    now_local_seconds: u32,
    surfaces: &dyn Surfaces,
) {
    if crate::dnd::is_suppressed(state, topic_hours, tags, now_local_seconds) {
        surfaces.log_silent(msg);
        return;
    }
    if !tags.contains(&"override=dnd")
        && crate::dnd::is_snoozed(&state.snoozes, &msg.topic, msg.ts_unix_ms)
    {
        surfaces.log_silent(msg);
        return;
    }
    dispatch(msg, surfaces);
}

/// Parse the lowercase priority string stored in the index
/// back into the [`Priority`] enum. Unknown → `Default`.
#[must_use]
pub fn parse_priority(s: &str) -> Priority {
    match s {
        "min" => Priority::Min,
        "default" => Priority::Default,
        "high" => Priority::High,
        "urgent" => Priority::Urgent,
        _ => Priority::Default,
    }
}

/// Log-only no-op surface implementation. Used as the daemon's
/// default until the BUS-2.2..2.8 Iced surfaces land — every
/// dispatched message just logs through `tracing` with the
/// surface name + ULID + topic. Production GUIs replace this
/// at startup.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogOnlySurfaces;

impl Surfaces for LogOnlySurfaces {
    fn log_silent(&self, msg: &StoredMessage) {
        tracing::debug!(
            target: "mde_bus::surface",
            surface = "silent_log",
            ulid = %msg.ulid,
            topic = %msg.topic,
            "dispatch"
        );
    }
    fn tray_and_badge(&self, msg: &StoredMessage) {
        tracing::info!(
            target: "mde_bus::surface",
            surface = "tray_and_badge",
            ulid = %msg.ulid,
            topic = %msg.topic,
            "dispatch (default-priority; tray + badge pending Iced surface impl)"
        );
    }
    fn status_strip_and_sound(&self, msg: &StoredMessage) {
        tracing::info!(
            target: "mde_bus::surface",
            surface = "status_strip_and_sound",
            ulid = %msg.ulid,
            topic = %msg.topic,
            "dispatch (high-priority; strip + sound pending Iced surface impl)"
        );
    }
    fn theater_wallpaper_phone(&self, msg: &StoredMessage) {
        tracing::warn!(
            target: "mde_bus::surface",
            surface = "theater_wallpaper_phone",
            ulid = %msg.ulid,
            topic = %msg.topic,
            "dispatch (urgent-priority; theater + wallpaper + phone pending Iced surface impl)"
        );
    }
}

/// Recording stub for tests. Counts the number of times each
/// surface fired so tests can snapshot-assert the dispatch
/// table without mocking the full Iced layer.
#[derive(Debug, Clone, Default)]
pub struct RecordingSurfaces {
    inner: Arc<Mutex<RecordingState>>,
}

#[derive(Debug, Default)]
struct RecordingState {
    pub log_silent: Vec<String>,
    pub tray_and_badge: Vec<String>,
    pub status_strip_and_sound: Vec<String>,
    pub theater_wallpaper_phone: Vec<String>,
}

impl RecordingSurfaces {
    /// Construct a fresh recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot accessors — each returns the list of ULIDs that
    /// fired through that surface, in call order.
    #[must_use]
    pub fn log_silent_ulids(&self) -> Vec<String> {
        self.lock().log_silent.clone()
    }
    #[must_use]
    pub fn tray_and_badge_ulids(&self) -> Vec<String> {
        self.lock().tray_and_badge.clone()
    }
    #[must_use]
    pub fn status_strip_and_sound_ulids(&self) -> Vec<String> {
        self.lock().status_strip_and_sound.clone()
    }
    #[must_use]
    pub fn theater_wallpaper_phone_ulids(&self) -> Vec<String> {
        self.lock().theater_wallpaper_phone.clone()
    }

    /// Lock the recording state, recovering the guard if a prior holder
    /// panicked (poison) rather than cascading the panic (E8.3 — no `unwrap`).
    fn lock(&self) -> std::sync::MutexGuard<'_, RecordingState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Surfaces for RecordingSurfaces {
    fn log_silent(&self, msg: &StoredMessage) {
        self.lock().log_silent.push(msg.ulid.clone());
    }
    fn tray_and_badge(&self, msg: &StoredMessage) {
        self.lock().tray_and_badge.push(msg.ulid.clone());
    }
    fn status_strip_and_sound(&self, msg: &StoredMessage) {
        self.lock().status_strip_and_sound.push(msg.ulid.clone());
    }
    fn theater_wallpaper_phone(&self, msg: &StoredMessage) {
        self.lock().theater_wallpaper_phone.push(msg.ulid.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(ulid: &str, priority: &str) -> StoredMessage {
        StoredMessage {
            ulid: ulid.to_string(),
            topic: "t".to_string(),
            priority: priority.to_string(),
            title: None,
            body: Some("b".to_string()),
            ts_unix_ms: 0,
            file_path: format!("t/{ulid}.json"),
            actions: Vec::new(),
            reply_to: None,
        }
    }

    #[test]
    fn parse_priority_normalises_to_default_on_unknown() {
        assert_eq!(parse_priority("min"), Priority::Min);
        assert_eq!(parse_priority("default"), Priority::Default);
        assert_eq!(parse_priority("high"), Priority::High);
        assert_eq!(parse_priority("urgent"), Priority::Urgent);
        // Unknown / typo / future-priority strings: safe to
        // fall back to `default` so they still surface.
        assert_eq!(parse_priority("garbage"), Priority::Default);
        assert_eq!(parse_priority(""), Priority::Default);
    }

    #[test]
    fn min_only_fires_log_silent() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("u1", "min"), &s);
        assert_eq!(s.log_silent_ulids(), vec!["u1".to_string()]);
        assert!(s.tray_and_badge_ulids().is_empty());
        assert!(s.status_strip_and_sound_ulids().is_empty());
        assert!(s.theater_wallpaper_phone_ulids().is_empty());
    }

    #[test]
    fn default_only_fires_tray_and_badge() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("u2", "default"), &s);
        assert!(s.log_silent_ulids().is_empty());
        assert_eq!(s.tray_and_badge_ulids(), vec!["u2".to_string()]);
        assert!(s.status_strip_and_sound_ulids().is_empty());
        assert!(s.theater_wallpaper_phone_ulids().is_empty());
    }

    #[test]
    fn high_only_fires_status_strip() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("u3", "high"), &s);
        assert!(s.log_silent_ulids().is_empty());
        assert!(s.tray_and_badge_ulids().is_empty());
        assert_eq!(s.status_strip_and_sound_ulids(), vec!["u3".to_string()]);
        assert!(s.theater_wallpaper_phone_ulids().is_empty());
    }

    #[test]
    fn urgent_only_fires_theater_wallpaper_phone() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("u4", "urgent"), &s);
        assert!(s.log_silent_ulids().is_empty());
        assert!(s.tray_and_badge_ulids().is_empty());
        assert!(s.status_strip_and_sound_ulids().is_empty());
        assert_eq!(s.theater_wallpaper_phone_ulids(), vec!["u4".to_string()]);
    }

    #[test]
    fn dispatch_table_snapshot_in_call_order() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("a", "min"), &s);
        dispatch(&msg("b", "default"), &s);
        dispatch(&msg("c", "high"), &s);
        dispatch(&msg("d", "urgent"), &s);
        dispatch(&msg("e", "default"), &s);
        assert_eq!(s.log_silent_ulids(), vec!["a"]);
        assert_eq!(s.tray_and_badge_ulids(), vec!["b", "e"]);
        assert_eq!(s.status_strip_and_sound_ulids(), vec!["c"]);
        assert_eq!(s.theater_wallpaper_phone_ulids(), vec!["d"]);
    }

    #[test]
    fn unknown_priority_falls_back_to_default_surfaces() {
        let s = RecordingSurfaces::new();
        dispatch(&msg("u5", "garbage"), &s);
        assert_eq!(s.tray_and_badge_ulids(), vec!["u5".to_string()]);
        assert!(s.log_silent_ulids().is_empty());
    }

    // ── BUS-6.7 snooze dispatch gate ────────────────────────────────

    fn msg_on(ulid: &str, priority: &str, topic: &str, ts_ms: i64) -> StoredMessage {
        StoredMessage {
            ulid: ulid.to_string(),
            topic: topic.to_string(),
            priority: priority.to_string(),
            title: None,
            body: Some("b".to_string()),
            ts_unix_ms: ts_ms,
            file_path: format!("{topic}/{ulid}.json"),
            actions: Vec::new(),
            reply_to: None,
        }
    }

    fn snoozed_state(topic: &str, until: i64) -> crate::dnd::DndState {
        crate::dnd::DndState {
            snoozes: vec![crate::dnd::TopicSnooze {
                topic: topic.to_string(),
                until_unix_ms: until,
                set_by_peer: "peerA".to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn snoozed_topic_is_log_silent() {
        let now = 1_000_000_000_000_i64;
        let s = RecordingSurfaces::new();
        let state = snoozed_state("fleet/sec", now + 60_000);
        // A high-priority message that would normally hit the status
        // strip is silenced because fleet/sec is snoozed.
        dispatch_with_dnd(
            &msg_on("u1", "high", "fleet/sec", now),
            &state,
            crate::dnd::TopicQuietHours::default(),
            &[],
            0,
            &s,
        );
        assert_eq!(s.log_silent_ulids(), vec!["u1".to_string()]);
        assert!(s.status_strip_and_sound_ulids().is_empty());
    }

    #[test]
    fn non_snoozed_topic_dispatches_normally() {
        let now = 1_000_000_000_000_i64;
        let s = RecordingSurfaces::new();
        let state = snoozed_state("fleet/sec", now + 60_000);
        // Different topic → not snoozed → normal dispatch.
        dispatch_with_dnd(
            &msg_on("u2", "high", "fleet/announce", now),
            &state,
            crate::dnd::TopicQuietHours::default(),
            &[],
            0,
            &s,
        );
        assert_eq!(s.status_strip_and_sound_ulids(), vec!["u2".to_string()]);
        assert!(s.log_silent_ulids().is_empty());
    }

    #[test]
    fn expired_snooze_dispatches_normally() {
        let now = 1_000_000_000_000_i64;
        let s = RecordingSurfaces::new();
        // Snooze expired 1 ms before the message ts → no suppression.
        let state = snoozed_state("fleet/sec", now - 1);
        dispatch_with_dnd(
            &msg_on("u3", "high", "fleet/sec", now),
            &state,
            crate::dnd::TopicQuietHours::default(),
            &[],
            0,
            &s,
        );
        assert_eq!(s.status_strip_and_sound_ulids(), vec!["u3".to_string()]);
    }

    #[test]
    fn override_dnd_tag_bypasses_snooze() {
        let now = 1_000_000_000_000_i64;
        let s = RecordingSurfaces::new();
        let state = snoozed_state("fleet/sec", now + 60_000);
        // override=dnd is the emergency bypass — it beats a snooze too.
        dispatch_with_dnd(
            &msg_on("u4", "urgent", "fleet/sec", now),
            &state,
            crate::dnd::TopicQuietHours::default(),
            &["override=dnd"],
            0,
            &s,
        );
        assert_eq!(s.theater_wallpaper_phone_ulids(), vec!["u4".to_string()]);
        assert!(s.log_silent_ulids().is_empty());
    }
}
