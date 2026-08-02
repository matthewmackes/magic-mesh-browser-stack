//! `mde-bus tail` — follow messages on a topic by polling the
//! SQLite index since a cursor.
//!
//! Two flavors:
//!
//! - **Follow mode** (default): poll every 250 ms for new
//!   messages on the topic + print each as a one-line summary.
//!   Exits on Ctrl-C.
//! - **Last-N mode** (`--count N`): print the last N messages
//!   immediately + exit. No polling.
//!
//! Topic argument supports MQTT-style wildcards (`+` single-
//! level, `#` multi-level) — the matcher walks every topic the
//! registry knows about and tails the union.
//!
//! Output format: `<ulid>  <topic>  <priority>  <title or body>`
//! — a glanceable line that pipes into `grep` cleanly.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;

use crate::persist::{Persist, StoredMessage};

/// CLI args for `mde-bus tail`.
#[derive(Args, Debug, Default)]
pub struct TailArgs {
    /// Topic or wildcard pattern (e.g. `fleet/+` or `gh/#`).
    pub pattern: String,
    /// Start from a specific ULID cursor (exclusive). Defaults
    /// to the latest message at start time.
    #[arg(long)]
    pub since: Option<String>,
    /// Print the last N messages then exit (skip polling).
    #[arg(long)]
    pub count: Option<usize>,
    /// Poll interval in milliseconds (defaults to 250 ms).
    #[arg(long, default_value = "250")]
    pub interval_ms: u64,
    /// Override the bus-root directory (defaults to
    /// `<XDG_DATA_HOME>/mde/bus`). Mainly for tests.
    #[arg(long)]
    pub bus_root: Option<PathBuf>,
    /// Emit JSON Lines instead of the human-readable summary.
    /// Each line is a full StoredMessage object suitable for
    /// piping to `jq`.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Format one stored message as either the human-readable tail
/// summary or a JSONL line, gated on `json`. Centralised here so
/// the two emit-sites (count-mode pre-loop + follow-mode poll
/// loop) stay in sync.
#[must_use]
pub fn format_emit(msg: &StoredMessage, json: bool) -> String {
    if json {
        serde_json::to_string(msg).unwrap_or_else(|_| format_line(msg))
    } else {
        format_line(msg)
    }
}

/// Format one stored message as a single-line tail entry.
pub fn format_line(msg: &StoredMessage) -> String {
    let summary = msg
        .title
        .as_deref()
        .or(msg.body.as_deref())
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("");
    // BUS-2.7 — surface action buttons inline so an operator tailing the
    // bus sees them (the notification UI renders them as clickable buttons).
    let actions = if msg.actions.is_empty() {
        String::new()
    } else {
        let list = msg
            .actions
            .iter()
            .map(|a| format!("{}→{}", a.label, a.url))
            .collect::<Vec<_>>()
            .join(", ");
        format!("  [{list}]")
    };
    // BUS-2.7.d — show the parent ULID when this message is a threaded
    // reply, so an operator tailing the bus sees the thread linkage.
    let reply = match &msg.reply_to {
        Some(parent) => format!("  ↳{parent}"),
        None => String::new(),
    };
    format!(
        "{}  {}  {}  {}{}{}",
        msg.ulid, msg.topic, msg.priority, summary, reply, actions
    )
}

/// Resolve the pattern against the topic registry, returning
/// every concrete topic name we should tail.
///
/// For patterns without wildcards, returns `[pattern]` directly
/// — even if the topic isn't registered yet, so subsequent
/// publishes still surface.
pub fn expand_pattern(pattern: &str, all_topics: &[String]) -> Vec<String> {
    if !pattern.contains('+') && !pattern.contains('#') {
        return vec![pattern.to_string()];
    }
    all_topics
        .iter()
        .filter(|t| crate::wildcard::matches(pattern, t))
        .cloned()
        .collect()
}

/// Resolve the default bus root from the env (XDG fallback).
fn default_bus_root() -> Result<PathBuf> {
    crate::default_data_dir().ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --bus-root"))
}

/// Discover every topic the index has seen so far. Used to
/// expand wildcard patterns. Falls back to an empty list when
/// the index is empty (which is fine — the matcher then has
/// nothing to expand and the verb tails nothing until publishes
/// land).
fn discovered_topics(p: &Persist) -> Result<Vec<String>> {
    p.list_topics().context("discover topics")
}

/// Execute the tail verb.
pub async fn run(args: TailArgs) -> Result<()> {
    let bus_root = match args.bus_root.clone() {
        Some(p) => p,
        None => default_bus_root()?,
    };
    let persist = Persist::open(bus_root).context("open persist")?;

    // `--count` mode: print last N from each topic + exit.
    if let Some(n) = args.count {
        let topics = expand_pattern(&args.pattern, &discovered_topics(&persist)?);
        let mut all: Vec<StoredMessage> = Vec::new();
        for t in &topics {
            all.extend(persist.list_since(t, None)?);
        }
        all.sort_by(|a, b| a.ulid.cmp(&b.ulid));
        let start = all.len().saturating_sub(n);
        for m in &all[start..] {
            println!("{}", format_emit(m, args.json));
        }
        return Ok(());
    }

    // Follow mode: initial cursor + poll loop.
    let initial_topics = discovered_topics(&persist)?;
    let topics = expand_pattern(&args.pattern, &initial_topics);
    let mut cursor = args.since.clone();
    if cursor.is_none() {
        // Default cursor: latest ULID at start time so we don't
        // re-print existing history.
        let mut latest = String::new();
        for t in &topics {
            for m in persist.list_since(t, None)? {
                if m.ulid > latest {
                    latest = m.ulid.clone();
                }
            }
        }
        if !latest.is_empty() {
            cursor = Some(latest);
        }
    }

    let interval = Duration::from_millis(args.interval_ms);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                return Ok(());
            }
            _ = ticker.tick() => {
                // Re-discover topics every tick so newly-published
                // wildcards expand to fresh hits.
                let topics_now = expand_pattern(&args.pattern, &discovered_topics(&persist)?);
                let mut new_max = cursor.clone();
                let mut new_rows: Vec<StoredMessage> = Vec::new();
                for t in &topics_now {
                    let rows = persist.list_since(t, cursor.as_deref())?;
                    for row in rows {
                        if Some(&row.ulid) > new_max.as_ref() {
                            new_max = Some(row.ulid.clone());
                        }
                        new_rows.push(row);
                    }
                }
                new_rows.sort_by(|a, b| a.ulid.cmp(&b.ulid));
                for m in new_rows {
                    println!("{}", format_emit(&m, args.json));
                }
                cursor = new_max;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::Priority;

    #[test]
    fn format_line_uses_title_first() {
        let m = StoredMessage {
            ulid: "01ABCDE".to_string(),
            topic: "t/x".to_string(),
            priority: "high".to_string(),
            title: Some("Title".to_string()),
            body: Some("Body".to_string()),
            ts_unix_ms: 0,
            file_path: "t/x/01ABCDE.json".to_string(),
            actions: Vec::new(),
            reply_to: None,
        };
        let line = format_line(&m);
        assert!(line.contains("01ABCDE"));
        assert!(line.contains("t/x"));
        assert!(line.contains("high"));
        assert!(line.contains("Title"));
        assert!(!line.contains("Body")); // Title wins
    }

    #[test]
    fn format_line_falls_back_to_body() {
        let m = StoredMessage {
            ulid: "01ABCDE".to_string(),
            topic: "t/x".to_string(),
            priority: "high".to_string(),
            title: None,
            body: Some("the body line\nsecond line".to_string()),
            ts_unix_ms: 0,
            file_path: "t/x/01ABCDE.json".to_string(),
            actions: Vec::new(),
            reply_to: None,
        };
        let line = format_line(&m);
        assert!(line.contains("the body line"));
        assert!(!line.contains("second line"));
    }

    #[test]
    fn format_line_appends_actions() {
        let m = StoredMessage {
            ulid: "01ACT".to_string(),
            topic: "t/x".to_string(),
            priority: "default".to_string(),
            title: Some("Conflict".to_string()),
            body: None,
            ts_unix_ms: 0,
            file_path: "t/x/01ACT.json".to_string(),
            actions: vec![crate::persist::Action {
                label: "Resolve".to_string(),
                url: "mde://files/resolve".to_string(),
            }],
            reply_to: None,
        };
        let line = format_line(&m);
        assert!(line.contains("Conflict"));
        assert!(line.contains("Resolve→mde://files/resolve"));
    }

    #[test]
    fn format_line_shows_reply_to() {
        // BUS-2.7.d — a threaded reply renders its parent ULID inline.
        let m = StoredMessage {
            ulid: "01REPLY".to_string(),
            topic: "fleet/announce".to_string(),
            priority: "default".to_string(),
            title: Some("re: status".to_string()),
            body: None,
            ts_unix_ms: 0,
            file_path: "fleet/announce/01REPLY.json".to_string(),
            actions: Vec::new(),
            reply_to: Some("01PARENT".to_string()),
        };
        let line = format_line(&m);
        assert!(line.contains("↳01PARENT"), "reply linkage shown: {line}");
    }

    #[test]
    fn format_emit_json_round_trips_reply_to() {
        // BUS-2.7.d — reply_to survives the --json serialize/deserialize.
        let m = StoredMessage {
            ulid: "01J0RPL".to_string(),
            topic: "fleet/announce".to_string(),
            priority: "default".to_string(),
            title: Some("re".to_string()),
            body: None,
            ts_unix_ms: 1,
            file_path: "fleet/announce/01J0RPL.json".to_string(),
            actions: Vec::new(),
            reply_to: Some("01PARENT".to_string()),
        };
        let parsed: StoredMessage = serde_json::from_str(&format_emit(&m, true)).unwrap();
        assert_eq!(parsed.reply_to.as_deref(), Some("01PARENT"));
        assert_eq!(parsed, m);
    }

    #[test]
    fn format_emit_json_round_trips_full_envelope() {
        let m = StoredMessage {
            ulid: "01J0AAA".to_string(),
            topic: "fleet/announce".to_string(),
            priority: "high".to_string(),
            title: Some("hello".to_string()),
            body: Some("body".to_string()),
            ts_unix_ms: 1_700_000_000_000,
            file_path: "fleet/announce/01J0AAA.json".to_string(),
            actions: Vec::new(),
            reply_to: None,
        };
        let json = format_emit(&m, true);
        let parsed: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn format_emit_tsv_matches_format_line() {
        let m = StoredMessage {
            ulid: "01J0AAA".to_string(),
            topic: "fleet/announce".to_string(),
            priority: "high".to_string(),
            title: Some("hello".to_string()),
            body: None,
            ts_unix_ms: 0,
            file_path: "p".to_string(),
            actions: Vec::new(),
            reply_to: None,
        };
        assert_eq!(format_emit(&m, false), format_line(&m));
    }

    #[test]
    fn expand_pattern_without_wildcard_returns_pattern() {
        let topics = vec!["a/b".to_string(), "c/d".to_string()];
        assert_eq!(expand_pattern("a/b", &topics), vec!["a/b".to_string()]);
        // Even if the topic isn't in the registry — publishes
        // will land there.
        assert_eq!(expand_pattern("z/z", &topics), vec!["z/z".to_string()]);
    }

    #[test]
    fn expand_pattern_with_wildcard_filters_registry() {
        let topics = vec![
            "gh/push".to_string(),
            "gh/pr".to_string(),
            "gitea/push".to_string(),
        ];
        let mut got = expand_pattern("gh/#", &topics);
        got.sort();
        assert_eq!(got, vec!["gh/pr".to_string(), "gh/push".to_string()]);

        let mut got = expand_pattern("+/push", &topics);
        got.sort();
        assert_eq!(got, vec!["gh/push".to_string(), "gitea/push".to_string()]);
    }

    #[tokio::test]
    async fn count_mode_prints_last_n_and_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        for i in 0..5 {
            p.write("t/x", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let args = TailArgs {
            pattern: "t/x".to_string(),
            since: None,
            count: Some(3),
            interval_ms: 250,
            bus_root: Some(tmp.path().to_path_buf()),
            json: false,
        };
        // Should not hang.
        let res = tokio::time::timeout(Duration::from_secs(2), run(args)).await;
        assert!(res.is_ok());
    }
}
