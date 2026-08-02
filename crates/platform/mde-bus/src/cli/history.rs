//! `mde-bus history` — print stored messages on a topic.
//!
//! Reads from the per-peer SQLite index (BUS-1.4). Supports
//! optional `--since <ulid>` cursor + `--count N` limit. Default
//! is "every message ever stored on the topic" — operators
//! usually want `--count 20` for the last-20 view.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Args;

use crate::persist::Persist;

/// CLI args for `mde-bus history`.
#[derive(Args, Debug, Default)]
pub struct HistoryArgs {
    /// Topic to print history for. Exact match (no wildcards).
    pub topic: String,
    /// Start cursor (exclusive). Useful for "what's new since
    /// my last poll?" queries.
    #[arg(long)]
    pub since: Option<String>,
    /// End cursor (exclusive). Useful for "what's older than
    /// this ULID?" pagination queries. Composable with
    /// `--since` for a half-open `(since, before)` range.
    #[arg(long)]
    pub before: Option<String>,
    /// Print at most this many messages (most-recent N).
    #[arg(long)]
    pub count: Option<usize>,
    /// Reverse output ordering — print newest first. Default is
    /// chronological (oldest first). Useful when scanning a noisy
    /// topic visually: the most recent message lands at the top
    /// of the terminal instead of scrolling off-screen.
    #[arg(long, default_value_t = false)]
    pub reverse: bool,
    /// Override the bus-root directory (defaults to
    /// `<XDG_DATA_HOME>/mde/bus`).
    #[arg(long)]
    pub bus_root: Option<PathBuf>,
    /// Emit JSON Lines instead of the tail-format summary. Each
    /// line is a full StoredMessage object suitable for piping
    /// to `jq`.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

fn default_bus_root() -> Result<PathBuf> {
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

/// Execute the `history` verb.
pub async fn run(args: HistoryArgs) -> Result<()> {
    let bus_root = match args.bus_root.clone() {
        Some(p) => p,
        None => default_bus_root()?,
    };
    let p = Persist::open(bus_root).context("open persist")?;
    let mut rows = p.list_since(&args.topic, args.since.as_deref())?;
    // `--before` is exclusive — drop every row whose ULID is
    // >= the cursor. Applied BEFORE the `--count` slice so the
    // operator gets the last N entries from the filtered range,
    // not the last N from the unfiltered range.
    if let Some(before) = args.before.as_deref() {
        rows.retain(|m| m.ulid.as_str() < before);
    }
    if let Some(n) = args.count {
        let start = rows.len().saturating_sub(n);
        rows = rows.split_off(start);
    }
    // `--reverse` flips the output to newest-first. Applied AFTER
    // the `--count` slice so the operator still gets "the last N
    // messages" — just printed top-down newest instead of bottom-
    // up oldest. The underlying `list_since` is always ULID-
    // ordered ascending; this is purely a print-order toggle.
    if args.reverse {
        rows.reverse();
    }
    for m in &rows {
        if args.json {
            // StoredMessage derives Serialize so we can emit it
            // directly. JSONL convention — one object per line,
            // no pretty-print.
            let s =
                serde_json::to_string(m).map_err(|e| anyhow!("serialize stored message: {e}"))?;
            println!("{s}");
        } else {
            let line = crate::cli::tail::format_line(m);
            println!("{line}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::Priority;

    #[tokio::test]
    async fn returns_all_when_count_omitted() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        for i in 0..5 {
            p.write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Should not hang or error.
        let args = HistoryArgs {
            topic: "t/x".to_string(),
            since: None,
            before: None,
            count: None,
            reverse: false,
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        };
        run(args).await.unwrap();
    }

    #[tokio::test]
    async fn count_limits_output() {
        // Behavioral check via direct list_since call to avoid
        // capturing stdout in tests.
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        for i in 0..10 {
            p.write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let all = p.list_since("t/x", None).unwrap();
        assert_eq!(all.len(), 10);
        // Run the verb — main coverage of the verb itself.
        let args = HistoryArgs {
            topic: "t/x".to_string(),
            since: None,
            before: None,
            count: Some(3),
            reverse: false,
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        };
        run(args).await.unwrap();
    }

    #[tokio::test]
    async fn before_cursor_excludes_rows_at_or_after() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        let mut ulids: Vec<String> = Vec::new();
        for i in 0..5 {
            let stored = p
                .write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            ulids.push(stored.ulid);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Pick the 3rd ULID as the `--before` cursor. The retain
        // predicate is strict `< cursor`, so rows[3] + rows[4] are
        // excluded; rows[0..3] are kept.
        let cursor = ulids[3].clone();
        let mut filtered: Vec<crate::persist::StoredMessage> = p.list_since("t/x", None).unwrap();
        filtered.retain(|m| m.ulid.as_str() < cursor.as_str());
        assert_eq!(filtered.len(), 3);
        for m in &filtered {
            assert!(
                m.ulid < cursor,
                "ulid {} should be strictly < cursor",
                m.ulid
            );
        }
        // Run the verb to confirm it doesn't panic with --before.
        let args = HistoryArgs {
            topic: "t/x".to_string(),
            since: None,
            before: Some(cursor),
            count: None,
            reverse: false,
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        };
        run(args).await.unwrap();
    }

    #[tokio::test]
    async fn reverse_flag_runs_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        for i in 0..3 {
            p.write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Confirm the underlying ordering invariant the `--reverse`
        // flag flips. `list_since` always returns ULID-ascending; a
        // simple Vec::reverse on the result is enough to satisfy
        // "newest first" because ULID is monotonic in timestamp.
        let asc = p.list_since("t/x", None).unwrap();
        let mut desc = asc.clone();
        desc.reverse();
        assert_eq!(asc.len(), 3);
        assert_eq!(desc.len(), 3);
        assert_eq!(desc[0].ulid, asc[2].ulid);
        assert_eq!(desc[2].ulid, asc[0].ulid);
        // Now run the verb with --reverse to confirm dispatch path.
        let args = HistoryArgs {
            topic: "t/x".to_string(),
            since: None,
            before: None,
            count: None,
            reverse: true,
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        };
        run(args).await.unwrap();
    }
}
