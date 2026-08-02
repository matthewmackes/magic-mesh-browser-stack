//! Mackes Bus — mesh-wide notification + clipboard pub/sub bus.
//!
//! v6.x BUS-1..7 epic. Locked 2026-05-25 via 104-Q poll across 26
//! rounds; full design lock at `docs/design/v6.x-mackes-bus.md`.
//!
//! This crate is the in-process library used by the `mde-bus` binary,
//! by the `mackesd::workers::bus_supervisor` subprocess supervisor,
//! and by the `mackesd::workers::clipboard_sync` capturer (which
//! publishes every clip on the bus). It exposes:
//!
//! * [`topic`] — slash-hierarchy topic names + MQTT-wildcard matcher
//!   (BUS-1.5).
//! * [`wildcard`] — the `+` / `#` wildcard match table itself, split
//!   so test surfaces stay focused (BUS-1.5).
//! * [`seed`] — first-run idempotent seed of the 12 curated default
//!   topics (BUS-1.6).
//! * [`template`] — Tera-backed message templating with mesh
//!   variables, `{{exec 'cmd'}}` shell exec, and `{{include 'path'}}`
//!   GFS file content (BUS-1.10).
//!
//! Later BUS-1.* tasks layer ntfy broker supervision (BUS-1.2),
//! mDNS-on-Nebula discovery (BUS-1.3), SQLite+file-tree persistence
//! (BUS-1.4), subscription manifest (BUS-1.7), the full CLI (BUS-1.8),
//! and the retention engine (BUS-1.9) on top of these primitives.

// P2 perf-12: the Bus wire/frame path (RPC framing, topic/template parsing, and the
// webhook receivers under `hooks/`) parses UNTRUSTED remote input. A stray
// `.unwrap()`/`.expect()` on that path is a remote-triggerable panic (DoS-adjacent),
// so deny both in non-test code. Test code keeps them for terse assertions.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod audit;
pub mod broker;
pub mod cli;
pub mod correlate;
pub mod discovery;
pub mod dnd;
/// WL-SEC-002 — cross-mesh federation runtime enforcement (grant-gated routing,
/// accept/revoke core, trust-cert install-on-accept, cross-mesh ingress spool).
pub mod federation;
pub mod hooks;
pub mod persist;
pub mod retention;
pub mod rpc;
pub mod seed;
pub mod subs;
pub mod surface;
pub mod template;
pub mod topic;
pub mod wildcard;

/// Default XDG-rooted base directory for the bus on-disk state:
/// `~/.local/share/mde/bus/`. Used by [`seed`] for the first-run
/// marker, by BUS-1.4 for the per-topic file tree, and by BUS-1.7 for
/// the subscription manifest.
///
/// Returns `None` when neither `$XDG_DATA_HOME` nor `$HOME` is set
/// (e.g. the daemon was launched from a context with no user home).
/// Callers should fall back to `/var/lib/mde/bus/` in that case.
///
/// **`MDE_BUS_ROOT` (SETUP-fix) takes precedence** — it pins a SHARED bus spool
/// so the root `mackesd` system daemon (responders) and the uid-1000 desktop
/// GUIs (workbench/applet) land on ONE bus. Without it, `dirs::data_dir()` is
/// per-HOME (`/root/.local/share/mde/bus` vs `/home/<u>/.local/share/mde/bus`),
/// so every workbench↔mackesd request/reply times out ("mesh service isn't
/// answering"). The RPM sets `MDE_BUS_ROOT=/run/mde-bus` for both the unit and
/// the user session (environment.d) over a sticky 1777 runtime dir.
#[must_use]
pub fn default_data_dir() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("MDE_BUS_ROOT") {
        return Some(std::path::PathBuf::from(root));
    }
    dirs::data_dir().map(|d| d.join("mde").join("bus"))
}

/// The canonical RPM-managed shared bus spool the system `mackesd` daemon
/// and its responders run on (mirrors `MDE_BUS_ROOT` in `mackesd.service` +
/// `environment.d`). A sticky 1777 runtime dir over a 0666 `index.sqlite`,
/// so the uid-1000 desktop GUIs can read/write it without being root.
pub const SYSTEM_BUS_ROOT: &str = "/run/mde-bus";

/// Bus spool a **desktop GUI client** (workbench / applet) should use to
/// reach the local `mackesd` responders. Resolution order:
///
///   1. `MDE_BUS_ROOT` when set (the explicit pin — honored everywhere);
///   2. else the live [`SYSTEM_BUS_ROOT`] when its `index.sqlite` exists —
///      i.e. a system `mackesd` is running on this box;
///   3. else the per-HOME [`default_data_dir`] (no system daemon — a dev
///      tree or a standalone GUI).
///
/// SUBAUDIT — why this exists over [`default_data_dir`]: `environment.d`
/// only seeds `MDE_BUS_ROOT` into sessions that *log in after* the RPM
/// drops the file. A Cosmic session already running at upgrade time keeps
/// the old (empty) env, so the GUI silently falls back to its per-HOME
/// spool while `mackesd` answers on `/run/mde-bus` — every request/reply
/// then times out as "mde host worker not responding". Preferring the live
/// system bus makes the GUI robust to that session-env staleness instead of
/// depending on a re-login. Daemon-side code keeps [`default_data_dir`]
/// (it always has the explicit env pin), and step 2 is gated on the file
/// existing so test/headless contexts (no system bus) still get the
/// per-HOME path.
#[must_use]
pub fn client_data_dir() -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os("MDE_BUS_ROOT") {
        return Some(std::path::PathBuf::from(root));
    }
    let system = std::path::Path::new(SYSTEM_BUS_ROOT);
    if system.join("index.sqlite").exists() {
        return Some(system.to_path_buf());
    }
    dirs::data_dir().map(|d| d.join("mde").join("bus"))
}
