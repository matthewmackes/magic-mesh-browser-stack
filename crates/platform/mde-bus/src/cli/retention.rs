//! `mde-bus retention` — operator-facing diagnostics on the
//! BUS-1.9 retention engine.
//!
//! Read-only — never deletes / never rewrites. The `pass` sub-
//! verb is intentionally NOT shipped here; retention GC runs
//! inside the daemon's tick loop, not as a one-shot CLI gesture.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use crate::retention::{
    disk_usage_bytes, RetentionPolicy, DEFAULT_QUOTA_HARD_BYTES, DEFAULT_QUOTA_SOFT_BYTES,
};

/// CLI sub-verbs for `mde-bus retention`.
#[derive(Subcommand, Debug)]
pub enum RetentionOp {
    /// Print the resolved retention policy + current disk usage.
    /// Reports per-priority TTLs (`min` / `default` / `high` —
    /// `urgent` is forever), soft + hard GFS quotas, and the
    /// `<bus_root>` byte total relative to those quotas. Useful
    /// for operators wondering "why hasn't this message expired
    /// yet" or "are we close to the quota."
    Status {
        /// Override the bus-root directory.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Emit a JSON object with every field instead of the
        /// human-readable summary.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn resolve_bus_root(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

fn format_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.1} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.1} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

fn format_ttl(secs: u64) -> String {
    let hours = secs / 3600;
    if hours >= 24 {
        let days = hours / 24;
        format!("{days}d")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{secs}s")
    }
}

/// Execute the `retention` verb. Read-only.
pub fn run(op: RetentionOp) -> Result<()> {
    match op {
        RetentionOp::Status { bus_root, json } => {
            let root = resolve_bus_root(bus_root)?;
            let policy = RetentionPolicy::default();
            let used = disk_usage_bytes(&root)
                .with_context(|| format!("scan disk usage at {}", root.display()))?;
            if json {
                let val = serde_json::json!({
                    "ttl_min_secs": policy.ttl_min_secs,
                    "ttl_default_secs": policy.ttl_default_secs,
                    "ttl_high_secs": policy.ttl_high_secs,
                    "ttl_urgent": "forever",
                    "quota_soft_bytes": DEFAULT_QUOTA_SOFT_BYTES,
                    "quota_hard_bytes": DEFAULT_QUOTA_HARD_BYTES,
                    "used_bytes": used,
                    "bus_root": root.display().to_string(),
                });
                println!("{val}");
            } else {
                println!("Retention policy:");
                println!("  min     TTL: {}", format_ttl(policy.ttl_min_secs));
                println!("  default TTL: {}", format_ttl(policy.ttl_default_secs));
                println!("  high    TTL: {}", format_ttl(policy.ttl_high_secs));
                println!("  urgent  TTL: forever (never auto-expired)");
                println!();
                println!("Disk quota:");
                println!("  soft: {}", format_bytes(DEFAULT_QUOTA_SOFT_BYTES));
                println!("  hard: {}", format_bytes(DEFAULT_QUOTA_HARD_BYTES));
                println!("  used: {}", format_bytes(used));
                if used >= DEFAULT_QUOTA_HARD_BYTES {
                    println!(
                        "  *** OVER HARD QUOTA *** ({}%)",
                        used * 100 / DEFAULT_QUOTA_HARD_BYTES
                    );
                } else if used >= DEFAULT_QUOTA_SOFT_BYTES {
                    println!(
                        "  ! over soft quota ({}%)",
                        used * 100 / DEFAULT_QUOTA_SOFT_BYTES
                    );
                }
                println!();
                println!("bus_root: {}", root.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_runs_on_empty_bus_root() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(RetentionOp::Status {
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn status_json_runs_on_empty_bus_root() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run(RetentionOp::Status {
            bus_root: Some(tmp.path().to_path_buf()),
            json: true,
        });
        assert!(r.is_ok());
    }

    #[test]
    fn format_bytes_handles_all_orders() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn format_ttl_handles_all_units() {
        assert_eq!(format_ttl(60), "60s");
        assert_eq!(format_ttl(3600), "1h");
        assert_eq!(format_ttl(86_400), "1d");
        assert_eq!(format_ttl(7 * 86_400), "7d");
        assert_eq!(format_ttl(30 * 86_400), "30d");
    }
}
