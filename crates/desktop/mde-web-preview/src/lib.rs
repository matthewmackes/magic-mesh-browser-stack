//! `mde-web-preview` — the out-of-process, OS-sandboxed Servo browser helper
//! (BOOKMARKS-5; design `docs/design/mesh-bookmarks.md`).
//!
//! The shell (BOOKMARKS-4's `Surface::Bookmarks`) spawns this binary as a
//! separate, hard-sandboxed process per browser session. It embeds the
//! interactive Servo web engine ([`engine`]) behind a layered OS sandbox
//! ([`sandbox`]) and publishes rendered frames into a shared-memory channel
//! ([`shm`]) that BOOKMARKS-6 maps into an egui texture. Security defaults —
//! zero telemetry, a generic UA, no persistent history, cookies cleared on
//! close, denied sensitive permissions — are real, and persistence is
//! additionally impossible by construction (the sandbox gives the process no
//! writable `$HOME`/keys/data).
//!
//! Scope (this crate): the binary + the Servo embedding + the OS sandbox + the
//! shm frame-emit seam + the headless "about:blank -> a frame arrives on the shm
//! channel" test. The shell-side shm/IPC bridge and input forwarding are
//! BOOKMARKS-6.

pub mod engine;
pub mod sandbox;
pub mod shm;

pub use engine::{secure_preferences, Engine, GENERIC_USER_AGENT};
pub use sandbox::SandboxPolicy;
pub use shm::{FrameChannel, FrameView, PixelFormat};
