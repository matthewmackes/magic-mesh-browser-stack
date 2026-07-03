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

pub mod frame;
pub mod input;
pub mod scm;
pub mod session;
pub mod wire;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use frame::{FrameReader, FrameSnapshot, PixelFormat, ReaderError};
pub use input::map_event;
pub use session::{NavState, SessionState, WebSession};
pub use wire::{ControlMsg, EventMsg, InputEvent, WireError};
