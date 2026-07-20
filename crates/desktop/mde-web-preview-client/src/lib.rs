//! `mde-web-preview-client` — the shell-side IPC + shm texture bridge that drives
//! and displays the sandboxed Servo browser helper (BOOKMARKS-6).
//!
//! # The seam
//!
//! The web engine runs out-of-process in the workspace-excluded `mde-web-preview`
//! crate (BOOKMARKS-5): a hard-sandboxed Servo instance that renders offscreen and
//! publishes each finished frame into a single shared-memory region (its
//! `FrameChannel`, an `MWP1`-tagged, seqlock-synchronised memfd). This crate is
//! the SHELL's half of that seam:
//!
//! ```text
//!   shell  ── ControlMsg (load/reload/back/forward/resize/input) ─▶  helper
//!   shell  ◀─ EventMsg  (attach-fd/paint-ready/title/nav/crashed) ──  helper
//!                         │
//!             SCM_RIGHTS shm fd (once) ─▶ map READ-ONLY ─▶ FrameReader
//!                         │
//!   paint-ready ─▶ FrameReader::to_color_image ─▶ egui TextureHandle ─▶ paint
//! ```
//!
//! * [`wire`] is the typed, length-prefixed message set — the socket contract.
//! * [`frame`] mirrors the `MWP1` shm header layout (the shm wire contract) and
//!   maps the received fd **read-only**, taking a tear-free seqlock snapshot and
//!   producing an [`egui::ColorImage`].
//! * [`scm`] passes/receives the shm fd over the session socket via `SCM_RIGHTS`.
//! * [`input`] scales egui pointer input by `pixels_per_point` into the device
//!   pixels the helper renders in.
//! * [`session`] ties it together: [`WebSession`] owns one helper's socket +
//!   mapped frame, drains events on [`WebSession::poll`], hands a fresh frame to
//!   the shell **only on paint-ready** ([`WebSession::take_frame`]), forwards
//!   input + nav, and surfaces a helper death as a typed, isolated [`SessionState`].
//!
//! # What is real vs. gated (§7)
//!
//! The socket protocol, the shm read + texture-upload path, the input scaling, and
//! the crash detection are REAL and fold-tested headless against an in-process
//! fake helper (the [`testkit`] module) that publishes a genuine `MWP1` shm frame
//! and speaks the event protocol — no Servo, no GPU. The live spawn of the real
//! `mde-web-preview` binary is behind the `live-helper` feature and is honest-gated
//! to a GPU seat (it also needs the helper's `tab` mode taught to speak this
//! socket — the BOOKMARKS-5 follow-up), exactly as the VDI decoder crates gate
//! their live connect.

// Re-export the toolkit through the harness so the shell and this bridge resolve
// to exactly one egui (no cross-surface version skew, §4).
pub use mde_egui::egui;

pub mod filter;
pub mod frame;
pub mod input;
pub mod scm;
pub mod session;

// The socket wire contract now lives in its own crate (`mde-web-wire`) so the
// shell client and BOTH out-of-process helpers (`mde-web-preview` / Servo and
// `mde-web-cef` / Chromium) share ONE type identity, not three `#[path]`-included
// copies of one source file. Re-exported as `wire` so callers' `wire::…` paths
// (and this crate's own `crate::wire::…`) are unchanged.
pub use mde_web_wire as wire;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use filter::{
    resource_from_wire, resource_to_wire, ManagedUrlPolicy, RequestFilter, SafeBrowsingBlocklist,
};
pub use frame::{FrameReader, FrameSnapshot, PixelFormat, ReaderError};
pub use input::map_event;
pub use session::{
    BeforeUnloadDialog, CertError, JsDialog, LoginCaptureStatus, MediaMetadataStatus, NavState,
    PasskeyRequestStatus, PermissionRequest, ResourceRequestStatus, SessionState, WebSession,
};
pub use wire::{
    ControlMsg, CursorKind, EditCommand, EventMsg, InputEvent, MediaTransportAction, WireError,
};

// The ad-filter engine types the shell compiles a session's [`RequestFilter`]
// from (BOOKMARKS-7). Re-exported so the Browser surface + the live-helper spawn
// path resolve the SAME `mde_adblock` types this crate's seam speaks.
pub use mde_adblock::{
    confusable_reason, host_of, is_confusable_host, BlockTally, ConfusableReason, Decision, Engine,
    FilterListSource, FilterListStore, ResourceType,
};
