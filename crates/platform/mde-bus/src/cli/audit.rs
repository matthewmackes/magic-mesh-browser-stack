//! `mde-bus audit` — inspect the per-peer publish audit log
//! (BUS-7.1). Read-only operator-facing inspection of the
//! `<bus_root>/audit/<YYYY-MM-DD>.jsonl` files.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;

use crate::audit;

/// CLI sub-verbs for `mde-bus audit`.
#[derive(Subcommand, Debug)]
pub enum AuditOp {
    /// Print audit entries from oldest to newest. Optional
    /// filters narrow the output before `--tail` truncates.
    List {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Print only the last N entries (most recent). 0 = all.
        #[arg(long, default_value_t = 0)]
        tail: usize,
        /// Filter: only entries whose publisher matches this
        /// string exactly. e.g. `--publisher github` shows only
        /// webhook publishes via the GitHub adapter.
        #[arg(long)]
        publisher: Option<String>,
        /// Filter: only entries whose topic matches this MQTT-
        /// style pattern (`+` single-level, `#` multi-level).
        /// e.g. `--topic 'mon/#'` shows every monitoring publish.
        #[arg(long)]
        topic: Option<String>,
        /// Filter: only entries at this priority (`min` /
        /// `default` / `high` / `urgent`). Case-sensitive lower.
        #[arg(long)]
        priority: Option<String>,
        /// Filter: keep only entries whose ISO timestamp's date
        /// portion is >= this value. Expected form is `YYYY-MM-DD`
        /// (10 chars). Pure string prefix comparison — ISO dates
        /// are sort-friendly so this works without parsing.
        #[arg(long)]
        since_date: Option<String>,
        /// Filter: keep only entries whose ISO timestamp's date
        /// portion is <= this value. Same `YYYY-MM-DD` form +
        /// inclusive semantics as `--since-date`. Composable with
        /// `--since-date` for an explicit date range.
        #[arg(long)]
        until_date: Option<String>,
        /// Emit JSON Lines instead of TSV — one JSON object per
        /// audit entry, suitable for piping to `jq` or other
        /// JSON-aware tooling. Each line is a complete
        /// `AuditEntry` per the serde Serialize derive.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the count of audit entries matching the same set of
    /// filters as `list`. Useful for monitoring + dashboards
    /// where the full list would be too noisy ("how many urgent
    /// publishes happened today?"). Default output is a bare
    /// decimal integer; `--json` switches to a JSON object so
    /// `jq` pipes can stay symmetric with `list --json`.
    Count {
        /// Override the bus_root path.
        #[arg(long)]
        bus_root: Option<PathBuf>,
        /// Filter: only entries whose publisher matches this
        /// string exactly.
        #[arg(long)]
        publisher: Option<String>,
        /// Filter: only entries whose topic matches this MQTT-
        /// style pattern.
        #[arg(long)]
        topic: Option<String>,
        /// Filter: only entries at this priority.
        #[arg(long)]
        priority: Option<String>,
        /// Filter: keep only entries whose ISO timestamp's date
        /// portion is >= this value.
        #[arg(long)]
        since_date: Option<String>,
        /// Filter: keep only entries whose ISO timestamp's date
        /// portion is <= this value.
        #[arg(long)]
        until_date: Option<String>,
        /// Emit a JSON object `{"count": N}` instead of the bare
        /// integer. Symmetric with `list --json` for jq pipelines.
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

/// Pure-fn — apply the five CLI filters (publisher / topic /
/// priority / since_date / until_date) to a flat slice of audit
/// entries. Each filter is `Option<&str>`; `None` means "don't
/// filter on this field." The returned Vec contains references
/// into the input slice — no allocation per kept entry.
///
/// `since_date` and `until_date` expect `YYYY-MM-DD` strings;
/// the predicate is a pure string prefix comparison against
/// `e.ts_iso[..10]` since ISO 8601 dates are sort-friendly.
#[must_use]
pub fn apply_filters<'a>(
    entries: &'a [audit::AuditEntry],
    publisher: Option<&str>,
    topic_pattern: Option<&str>,
    priority: Option<&str>,
    since_date: Option<&str>,
    until_date: Option<&str>,
) -> Vec<&'a audit::AuditEntry> {
    entries
        .iter()
        .filter(|e| publisher.is_none_or(|p| e.publisher == p))
        .filter(|e| topic_pattern.is_none_or(|pat| crate::wildcard::matches(pat, &e.topic)))
        .filter(|e| priority.is_none_or(|p| e.priority == p))
        .filter(|e| since_date.is_none_or(|d| e.ts_iso.len() >= 10 && &e.ts_iso[..10] >= d))
        .filter(|e| until_date.is_none_or(|d| e.ts_iso.len() >= 10 && &e.ts_iso[..10] <= d))
        .collect()
}

/// Execute the `audit` verb. Read-only.
pub fn run(op: AuditOp) -> Result<()> {
    match op {
        AuditOp::List {
            bus_root,
            tail,
            publisher,
            topic,
            priority,
            since_date,
            until_date,
            json,
        } => {
            let root = resolve_bus_root(bus_root)?;
            let entries = audit::read_entries_from_bus(&root)
                .with_context(|| format!("read audit at {}", root.display()))?;
            let filtered = apply_filters(
                &entries,
                publisher.as_deref(),
                topic.as_deref(),
                priority.as_deref(),
                since_date.as_deref(),
                until_date.as_deref(),
            );
            let slice: &[_] = if tail == 0 || tail >= filtered.len() {
                &filtered[..]
            } else {
                &filtered[filtered.len() - tail..]
            };
            for e in slice {
                if json {
                    // jq-pipe-friendly JSONL. serde_json::to_string
                    // produces a single line (no pretty-print)
                    // matching the JSONL convention of one object
                    // per line. Serialize failure is theoretically
                    // impossible for AuditEntry's flat string +
                    // String shape, but guard it as an io::Error
                    // for symmetry with the TSV path.
                    let line = serde_json::to_string(e)
                        .map_err(|err| anyhow!("serialize audit entry: {err}"))?;
                    println!("{line}");
                } else {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        e.ts_iso, e.publisher, e.topic, e.priority, e.ulid,
                    );
                }
            }
        }
        AuditOp::Count {
            bus_root,
            publisher,
            topic,
            priority,
            since_date,
            until_date,
            json,
        } => {
            let root = resolve_bus_root(bus_root)?;
            let entries = audit::read_entries_from_bus(&root)
                .with_context(|| format!("read audit at {}", root.display()))?;
            let filtered = apply_filters(
                &entries,
                publisher.as_deref(),
                topic.as_deref(),
                priority.as_deref(),
                since_date.as_deref(),
                until_date.as_deref(),
            );
            let n = filtered.len();
            if json {
                println!("{{\"count\":{n}}}");
            } else {
                println!("{n}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(publisher: &str, topic: &str, priority: &str, ulid: &str) -> audit::AuditEntry {
        entry_at("2026-05-27T12:00:00Z", publisher, topic, priority, ulid)
    }

    fn entry_at(
        ts_iso: &str,
        publisher: &str,
        topic: &str,
        priority: &str,
        ulid: &str,
    ) -> audit::AuditEntry {
        audit::AuditEntry {
            publisher: publisher.to_string(),
            ts_iso: ts_iso.to_string(),
            topic: topic.to_string(),
            priority: priority.to_string(),
            ulid: ulid.to_string(),
        }
    }

    #[test]
    fn list_with_missing_dir_returns_ok_empty() {
        let tmp =
            std::env::temp_dir().join(format!("mde-bus-audit-cli-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let r = run(AuditOp::List {
            bus_root: Some(tmp.clone()),
            tail: 0,
            publisher: None,
            topic: None,
            priority: None,
            since_date: None,
            until_date: None,
            json: false,
        });
        assert!(r.is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filter_publisher_keeps_only_matching() {
        let entries = vec![
            entry("github", "fleet/announce", "default", "u1"),
            entry("fedora", "fleet/announce", "default", "u2"),
            entry("github", "mon/cpu", "high", "u3"),
        ];
        let kept = apply_filters(&entries, Some("github"), None, None, None, None);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ulid, "u1");
        assert_eq!(kept[1].ulid, "u3");
    }

    #[test]
    fn filter_topic_wildcard_keeps_only_matches() {
        let entries = vec![
            entry("fedora", "mon/cpu", "default", "u1"),
            entry("fedora", "mon/disk", "default", "u2"),
            entry("fedora", "fleet/announce", "default", "u3"),
        ];
        let kept = apply_filters(&entries, None, Some("mon/#"), None, None, None);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ulid, "u1");
        assert_eq!(kept[1].ulid, "u2");
    }

    #[test]
    fn filter_priority_keeps_only_exact_match() {
        let entries = vec![
            entry("fedora", "t", "default", "u1"),
            entry("fedora", "t", "high", "u2"),
            entry("fedora", "t", "urgent", "u3"),
        ];
        let kept = apply_filters(&entries, None, None, Some("urgent"), None, None);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].ulid, "u3");
    }

    #[test]
    fn filter_chain_combines_three_predicates() {
        let entries = vec![
            entry("github", "mon/cpu", "default", "u1"),
            entry("github", "mon/cpu", "high", "u2"),
            entry("fedora", "mon/cpu", "high", "u3"),
            entry("github", "fleet/announce", "high", "u4"),
        ];
        // Want github + mon/* + high → u2 only.
        let kept = apply_filters(
            &entries,
            Some("github"),
            Some("mon/#"),
            Some("high"),
            None,
            None,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].ulid, "u2");
    }

    #[test]
    fn filter_no_predicates_returns_everything() {
        let entries = vec![
            entry("a", "t1", "default", "u1"),
            entry("b", "t2", "high", "u2"),
        ];
        let kept = apply_filters(&entries, None, None, None, None, None);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn filter_since_date_keeps_only_at_or_after() {
        let entries = vec![
            entry_at("2026-05-24T08:00:00Z", "p", "t", "default", "u1"),
            entry_at("2026-05-25T12:00:00Z", "p", "t", "default", "u2"),
            entry_at("2026-05-26T20:00:00Z", "p", "t", "default", "u3"),
        ];
        let kept = apply_filters(&entries, None, None, None, Some("2026-05-25"), None);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ulid, "u2");
        assert_eq!(kept[1].ulid, "u3");
    }

    #[test]
    fn filter_until_date_keeps_only_at_or_before() {
        let entries = vec![
            entry_at("2026-05-24T08:00:00Z", "p", "t", "default", "u1"),
            entry_at("2026-05-25T12:00:00Z", "p", "t", "default", "u2"),
            entry_at("2026-05-26T20:00:00Z", "p", "t", "default", "u3"),
        ];
        let kept = apply_filters(&entries, None, None, None, None, Some("2026-05-25"));
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].ulid, "u1");
        assert_eq!(kept[1].ulid, "u2");
    }

    #[test]
    fn filter_date_range_combines_since_and_until() {
        let entries = vec![
            entry_at("2026-05-23T08:00:00Z", "p", "t", "default", "u1"),
            entry_at("2026-05-24T08:00:00Z", "p", "t", "default", "u2"),
            entry_at("2026-05-25T12:00:00Z", "p", "t", "default", "u3"),
            entry_at("2026-05-26T20:00:00Z", "p", "t", "default", "u4"),
            entry_at("2026-05-27T01:00:00Z", "p", "t", "default", "u5"),
        ];
        let kept = apply_filters(
            &entries,
            None,
            None,
            None,
            Some("2026-05-24"),
            Some("2026-05-26"),
        );
        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].ulid, "u2");
        assert_eq!(kept[2].ulid, "u4");
    }

    #[test]
    fn count_verb_runs_on_empty_dir() {
        let tmp =
            std::env::temp_dir().join(format!("mde-bus-audit-cli-count-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Default (no filters, plain output) — should just print 0.
        let r = run(AuditOp::Count {
            bus_root: Some(tmp.clone()),
            publisher: None,
            topic: None,
            priority: None,
            since_date: None,
            until_date: None,
            json: false,
        });
        assert!(r.is_ok());
        // JSON path too.
        let r = run(AuditOp::Count {
            bus_root: Some(tmp.clone()),
            publisher: None,
            topic: None,
            priority: None,
            since_date: None,
            until_date: None,
            json: true,
        });
        assert!(r.is_ok());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filter_date_with_malformed_ts_drops_entry() {
        // ts_iso shorter than 10 chars → string-prefix check
        // can't compare safely, so the predicate drops the row.
        // Operators relying on date filters won't see corrupt rows.
        let entries = vec![
            entry_at("short", "p", "t", "default", "u1"),
            entry_at("2026-05-25T12:00:00Z", "p", "t", "default", "u2"),
        ];
        let kept = apply_filters(&entries, None, None, None, Some("2026-05-20"), None);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].ulid, "u2");
    }
}
