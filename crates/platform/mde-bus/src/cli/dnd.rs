//! BUS-2.8.cli — `mde-bus dnd` operator verb for toggling +
//! inspecting the mesh-wide Do Not Disturb state.
//!
//! Three sub-verbs:
//!
//!   - `mde-bus dnd on`     — flip DND on; written to
//!     `<bus_root>/dnd.yaml` via atomic temp+rename. Mesh peers
//!     pick the change up through the GFS heal window (typically
//!     within 1 s) once BUS-2.8.watcher ships; until then, each
//!     `mde-bus publish` re-reads the file before routing.
//!   - `mde-bus dnd off`    — flip DND off (same write path).
//!   - `mde-bus dnd status` — print the current state in
//!     human-readable form (active/inactive + since-timestamp +
//!     peer-of-record).
//!
//! All three default to the operator's
//! `<XDG_DATA_HOME>/mde/bus/` directory; an explicit
//! `--bus-root <path>` override lets tests + integration smokes
//! point at a tmpdir without polluting the real state.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use chrono::TimeZone;
use clap::Subcommand;

use crate::dnd;

/// CLI sub-verbs for `mde-bus dnd`.
#[derive(Subcommand, Debug)]
pub enum DndOp {
    /// Flip DND on. Writes `<bus_root>/dnd.yaml` with active=true.
    On {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
    },
    /// Flip DND off. Writes `<bus_root>/dnd.yaml` with active=false.
    Off {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
    },
    /// Print the current DND state in human-readable form.
    Status {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Emit the state as a single-line JSON object suitable
        /// for piping to `jq`. Replaces the multi-line
        /// human-readable `DND: on/off\nSince: ...\nBy: ...`
        /// format with the underlying `DndState` JSON shape.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Toggle DND — flips the current state (on → off, off → on).
    /// Convenient when the operator knows they want "the other
    /// state" without typing it explicitly. Reads the current
    /// state, then writes the inverse with a fresh timestamp.
    Toggle {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
    },
}

fn resolve_bus_root(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

fn local_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// Write a new DND state with the given `active` value, stamped
/// with `now_unix_ms` + the local hostname. Returns the written
/// state so callers can echo it or assert on it in tests.
pub fn set_state(bus_root: &std::path::Path, active: bool) -> Result<dnd::DndState> {
    let now_ms = chrono::Local::now().timestamp_millis();
    // Preserve any BUS-6.7 fleet snoozes when toggling DND — they
    // share `dnd.yaml` but are independent of the global toggle, so
    // flipping DND on/off must not wipe an in-flight snooze.
    let existing = dnd::load_default(bus_root);
    let state = dnd::DndState {
        active,
        since_unix_ms: now_ms,
        set_by_peer: local_hostname(),
        snoozes: existing.snoozes,
    };
    dnd::save_default(bus_root, &state)
        .with_context(|| format!("save dnd.yaml to {}", bus_root.display()))?;
    Ok(state)
}

/// Format a [`DndState`] for human-readable terminal output.
/// Lines kept short so the verb fits the standard 80-col operator
/// terminal even with a long peer name.
#[must_use]
pub fn format_status(state: &dnd::DndState) -> String {
    let active_line = if state.active { "DND: on" } else { "DND: off" };
    if state.since_unix_ms == 0 && state.set_by_peer.is_empty() {
        return format!("{active_line}\n(never toggled; using default off)");
    }
    let when = chrono::Local
        .timestamp_millis_opt(state.since_unix_ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S %Z").to_string())
        .unwrap_or_else(|| format!("unix_ms={}", state.since_unix_ms));
    format!(
        "{active_line}\nSince: {when}\nBy: {peer}",
        peer = state.set_by_peer,
    )
}

/// `mde-bus dnd` dispatch — runs the sub-verb's logic + prints
/// the resulting state to stdout.
pub async fn run(op: DndOp) -> Result<()> {
    match op {
        DndOp::On { bus_root } => {
            let root = resolve_bus_root(bus_root)?;
            let state = set_state(&root, true)?;
            println!("{}", format_status(&state));
        }
        DndOp::Off { bus_root } => {
            let root = resolve_bus_root(bus_root)?;
            let state = set_state(&root, false)?;
            println!("{}", format_status(&state));
        }
        DndOp::Status { bus_root, json } => {
            let root = resolve_bus_root(bus_root)?;
            let state = dnd::load_default(&root);
            if json {
                let line = serde_json::to_string(&state)
                    .map_err(|err| anyhow!("serialize dnd state: {err}"))?;
                println!("{line}");
            } else {
                println!("{}", format_status(&state));
            }
        }
        DndOp::Toggle { bus_root } => {
            let root = resolve_bus_root(bus_root)?;
            let current = dnd::load_default(&root);
            let state = set_state(&root, !current.active)?;
            println!("{}", format_status(&state));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_state_on_writes_active_true() {
        let tmp = std::env::temp_dir().join(format!("mde-bus-dnd-cli-on-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let s = set_state(&tmp, true).unwrap();
        assert!(s.active);
        assert!(s.since_unix_ms > 0);
        assert!(!s.set_by_peer.is_empty());
        // Load-back round-trip.
        let loaded = dnd::load_default(&tmp);
        assert_eq!(loaded, s);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn set_state_off_writes_active_false() {
        let tmp = std::env::temp_dir().join(format!("mde-bus-dnd-cli-off-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // First flip on, then off — verifies the off path doesn't
        // just no-op when the prior state was on.
        set_state(&tmp, true).unwrap();
        let s = set_state(&tmp, false).unwrap();
        assert!(!s.active);
        assert!(s.since_unix_ms > 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn format_status_default_state_is_never_toggled() {
        let s = dnd::DndState::default();
        let out = format_status(&s);
        assert!(out.contains("DND: off"));
        assert!(out.contains("never toggled"));
    }

    #[test]
    fn format_status_active_state_includes_since_and_by() {
        let s = dnd::DndState {
            active: true,
            since_unix_ms: 1_700_000_000_000,
            set_by_peer: "fedora".to_string(),
            ..Default::default()
        };
        let out = format_status(&s);
        assert!(out.contains("DND: on"));
        assert!(out.contains("Since:"));
        assert!(out.contains("By: fedora"));
    }

    #[test]
    fn toggle_flips_off_to_on_then_back_to_off() {
        let tmp = std::env::temp_dir().join(format!("mde-bus-dnd-toggle-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Initial state is default off. Toggle once → on.
        let first = set_state(&tmp, !dnd::load_default(&tmp).active).unwrap();
        assert!(first.active);
        // Toggle again → off.
        let second = set_state(&tmp, !dnd::load_default(&tmp).active).unwrap();
        assert!(!second.active);
        // Third toggle → on again.
        let third = set_state(&tmp, !dnd::load_default(&tmp).active).unwrap();
        assert!(third.active);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn local_hostname_never_empty() {
        // Whatever path the helper takes, the result must be
        // non-empty — the fallback chain guarantees this.
        let h = local_hostname();
        assert!(!h.is_empty());
    }
}
