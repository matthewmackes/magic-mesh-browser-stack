//! `mde-bus mute` — manage per-peer topic mute patterns in
//! `~/.local/share/mde/bus/subs.yaml`.
//!
//! Mute patterns silence a topic even when it matches the
//! subscribe list. Useful for narrowing noisy sources without
//! unsubscribing entirely.
//!
//! Three sub-verbs (mirror the `sub` verb shape so operators
//! don't have to learn two layouts): `add`, `remove`, `list`.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use chrono::TimeZone as _;
use clap::Subcommand;

use crate::subs::{self, SubsManifest};

/// CLI sub-verbs for `mde-bus mute`.
#[derive(Subcommand, Debug)]
pub enum MuteOp {
    /// Add a mute pattern. With `--duration`, sets a fleet-wide
    /// timed snooze in the GFS-replicated `dnd.yaml` (BUS-6.7)
    /// instead of a local `subs.yaml` mute — every peer honors it
    /// until it expires + auto-unmutes.
    Add {
        /// Topic or wildcard pattern to mute.
        topic: String,
        /// Override the manifest path (per-peer mute mode).
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// BUS-6.7 — fleet-wide snooze duration (`90s`/`30m`/`1h`/
        /// `2d`). When set, the mute is written to `dnd.yaml` (fleet-
        /// wide, auto-expiring) rather than the local `subs.yaml`.
        #[arg(long)]
        duration: Option<String>,
        /// Override the bus-root dir (fleet-snooze mode — where
        /// `dnd.yaml` lives). Defaults to `<XDG_DATA_HOME>/mde/bus`.
        #[arg(long)]
        bus_root: Option<PathBuf>,
    },
    /// Remove a mute pattern. With `--bus-root`, also clears any
    /// matching fleet-wide snooze from `dnd.yaml` (BUS-6.7 early
    /// unmute, before the duration expires).
    Remove {
        /// Topic or wildcard pattern to unmute.
        topic: String,
        /// Override the manifest path.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// BUS-6.7 — when set, also drops any fleet snooze whose
        /// topic equals `topic` from `dnd.yaml`.
        #[arg(long)]
        bus_root: Option<PathBuf>,
    },
    /// Print the current mute list.
    List {
        /// Override the manifest path.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Filter the printed list to topics matching this
        /// MQTT-style pattern (`+` single-level, `#` multi-
        /// level). Symmetry with `sub list --pattern`.
        #[arg(long)]
        pattern: Option<String>,
        /// Emit JSON Lines instead of plain-text. Each line is a
        /// JSON-quoted topic string suitable for piping to `jq`.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the count of mute patterns (optionally filtered by
    /// an MQTT-style pattern). Symmetric with `sub count`.
    Count {
        /// Override the manifest path.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Filter count to mutes matching this MQTT-style pattern.
        #[arg(long)]
        pattern: Option<String>,
        /// Emit `{"count":N}` instead of the bare integer.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

fn resolve_manifest_path(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = arg {
        return Ok(p);
    }
    subs::default_per_peer_path()
        .ok_or_else(|| anyhow!("no $HOME / $XDG_DATA_HOME — pass --manifest"))
}

fn read_or_default(path: &std::path::Path) -> Result<SubsManifest> {
    if !path.exists() {
        return Ok(SubsManifest::default());
    }
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    SubsManifest::parse_yaml(&body).with_context(|| format!("parse {}", path.display()))
}

fn write_atomic(path: &std::path::Path, m: &SubsManifest) -> Result<()> {
    let body = m.to_yaml().context("encode subs.yaml")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, body.as_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
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

/// BUS-6.7 — set (or refresh) a fleet-wide snooze for `topic` in
/// `dnd.yaml`. Prunes expired entries + replaces any existing
/// snooze for the same exact topic so re-running extends rather
/// than duplicates. Returns the computed expiry (ms) so callers +
/// tests can assert on it.
pub fn set_snooze(
    bus_root: &std::path::Path,
    topic: &str,
    duration_secs: i64,
    now_unix_ms: i64,
) -> Result<i64> {
    let until = now_unix_ms + duration_secs * 1000;
    let mut state = crate::dnd::load_default(bus_root);
    state.snoozes = crate::dnd::prune_expired_snoozes(state.snoozes, now_unix_ms);
    state.snoozes.retain(|s| s.topic != topic);
    state.snoozes.push(crate::dnd::TopicSnooze {
        topic: topic.to_string(),
        until_unix_ms: until,
        set_by_peer: local_hostname(),
    });
    crate::dnd::save_default(bus_root, &state)
        .with_context(|| format!("write {}/dnd.yaml", bus_root.display()))?;
    Ok(until)
}

/// BUS-6.7 — drop any fleet snooze whose topic equals `topic`.
/// Returns `true` when an entry was removed. Also prunes expired
/// entries while it's writing.
pub fn clear_snooze(bus_root: &std::path::Path, topic: &str, now_unix_ms: i64) -> Result<bool> {
    let mut state = crate::dnd::load_default(bus_root);
    let before = state.snoozes.len();
    state.snoozes = crate::dnd::prune_expired_snoozes(state.snoozes, now_unix_ms);
    state.snoozes.retain(|s| s.topic != topic);
    let removed = state.snoozes.len() != before;
    if removed {
        crate::dnd::save_default(bus_root, &state)
            .with_context(|| format!("write {}/dnd.yaml", bus_root.display()))?;
    }
    Ok(removed)
}

/// Execute the `mute` verb.
pub async fn run(op: MuteOp) -> Result<()> {
    match op {
        MuteOp::Add {
            topic,
            manifest,
            duration,
            bus_root,
        } => {
            if let Some(dur) = duration {
                // BUS-6.7 — fleet-wide timed snooze path.
                let secs = crate::dnd::parse_duration_secs(&dur).ok_or_else(|| {
                    anyhow!("invalid --duration {dur:?} (expected NNs/NNm/NNh/NNd)")
                })?;
                let root = resolve_bus_root(bus_root)?;
                let now = chrono::Local::now().timestamp_millis();
                let until = set_snooze(&root, &topic, secs, now)?;
                let until_local = chrono::Local
                    .timestamp_millis_opt(until)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S %z").to_string())
                    .unwrap_or_else(|| format!("{until} ms"));
                println!("snoozed fleet-wide: {topic} until {until_local}");
            } else {
                let path = resolve_manifest_path(manifest)?;
                let mut m = read_or_default(&path)?;
                if !m.mute.iter().any(|t| t == &topic) {
                    m.mute.push(topic.clone());
                    m.mute.sort();
                    m.mute.dedup();
                    write_atomic(&path, &m)?;
                    println!("muted: {topic}");
                } else {
                    println!("already muted: {topic}");
                }
            }
        }
        MuteOp::Remove {
            topic,
            manifest,
            bus_root,
        } => {
            let path = resolve_manifest_path(manifest)?;
            let mut m = read_or_default(&path)?;
            let before = m.mute.len();
            m.mute.retain(|t| t != &topic);
            if m.mute.len() != before {
                write_atomic(&path, &m)?;
                println!("unmuted: {topic}");
            } else {
                println!("not muted: {topic}");
            }
            // BUS-6.7 — early-clear a fleet snooze when --bus-root
            // is supplied (auto-expiry handles the no-flag case).
            if let Some(root) = bus_root {
                let now = chrono::Local::now().timestamp_millis();
                if clear_snooze(&root, &topic, now)? {
                    println!("snooze cleared fleet-wide: {topic}");
                }
            }
        }
        MuteOp::Count {
            manifest,
            pattern,
            json,
        } => {
            let path = resolve_manifest_path(manifest)?;
            let m = read_or_default(&path)?;
            let n = if let Some(p) = pattern.as_deref() {
                m.mute
                    .iter()
                    .filter(|t| crate::wildcard::matches(p, t))
                    .count()
            } else {
                m.mute.len()
            };
            if json {
                println!("{{\"count\":{n}}}");
            } else {
                println!("{n}");
            }
        }
        MuteOp::List {
            manifest,
            pattern,
            json,
        } => {
            let path = resolve_manifest_path(manifest)?;
            let m = read_or_default(&path)?;
            for t in &m.mute {
                if let Some(p) = pattern.as_deref() {
                    if !crate::wildcard::matches(p, t) {
                        continue;
                    }
                }
                if json {
                    // JSON-encoded string per line — guarantees
                    // proper quoting of topics with special chars.
                    let s = serde_json::to_string(t).unwrap_or_else(|_| format!("{t:?}"));
                    println!("{s}");
                } else {
                    println!("{t}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_manifest() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("subs.yaml");
        (tmp, path)
    }

    #[tokio::test]
    async fn add_inserts_into_mute() {
        let (_tmp, path) = tmp_manifest();
        run(MuteOp::Add {
            topic: "noisy/+".to_string(),
            manifest: Some(path.clone()),
            duration: None,
            bus_root: None,
        })
        .await
        .unwrap();
        let m = SubsManifest::parse_yaml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(m.mute.iter().any(|t| t == "noisy/+"));
    }

    #[tokio::test]
    async fn remove_drops_from_mute() {
        let (_tmp, path) = tmp_manifest();
        run(MuteOp::Add {
            topic: "x".to_string(),
            manifest: Some(path.clone()),
            duration: None,
            bus_root: None,
        })
        .await
        .unwrap();
        run(MuteOp::Remove {
            topic: "x".to_string(),
            manifest: Some(path.clone()),
            bus_root: None,
        })
        .await
        .unwrap();
        let m = SubsManifest::parse_yaml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(!m.mute.iter().any(|t| t == "x"));
    }

    #[tokio::test]
    async fn add_is_idempotent() {
        let (_tmp, path) = tmp_manifest();
        for _ in 0..3 {
            run(MuteOp::Add {
                topic: "x".to_string(),
                manifest: Some(path.clone()),
                duration: None,
                bus_root: None,
            })
            .await
            .unwrap();
        }
        let m = SubsManifest::parse_yaml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(m.mute.iter().filter(|t| *t == "x").count(), 1);
    }

    #[tokio::test]
    async fn count_returns_mute_count() {
        let (_tmp, path) = tmp_manifest();
        run(MuteOp::Count {
            manifest: Some(path.clone()),
            pattern: None,
            json: false,
        })
        .await
        .unwrap();
        for t in ["noisy/foo", "noisy/bar", "quiet/spam"] {
            run(MuteOp::Add {
                topic: t.to_string(),
                manifest: Some(path.clone()),
                duration: None,
                bus_root: None,
            })
            .await
            .unwrap();
        }
        run(MuteOp::Count {
            manifest: Some(path.clone()),
            pattern: None,
            json: true,
        })
        .await
        .unwrap();
        run(MuteOp::Count {
            manifest: Some(path),
            pattern: Some("noisy/+".to_string()),
            json: false,
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn list_pattern_filters_mute_topics() {
        let (_tmp, path) = tmp_manifest();
        for t in ["noisy/foo", "noisy/bar", "quiet/spam"] {
            run(MuteOp::Add {
                topic: t.to_string(),
                manifest: Some(path.clone()),
                duration: None,
                bus_root: None,
            })
            .await
            .unwrap();
        }
        let m = SubsManifest::parse_yaml(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let matched: Vec<&String> = m
            .mute
            .iter()
            .filter(|t| crate::wildcard::matches("noisy/+", t))
            .collect();
        assert_eq!(matched.len(), 2);
        // Dispatch exercise — verifies the verb runs with --pattern.
        run(MuteOp::List {
            manifest: Some(path),
            pattern: Some("noisy/+".to_string()),
            json: false,
        })
        .await
        .unwrap();
    }

    // ── BUS-6.7 fleet-snooze tests ──────────────────────────────────────

    #[test]
    fn set_snooze_writes_entry_with_expiry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = 1_000_000_000_000_i64;
        let until = set_snooze(root, "fleet/sec", 3600, now).unwrap();
        assert_eq!(until, now + 3_600_000);
        let state = crate::dnd::load_default(root);
        assert_eq!(state.snoozes.len(), 1);
        assert_eq!(state.snoozes[0].topic, "fleet/sec");
        assert_eq!(state.snoozes[0].until_unix_ms, until);
        assert!(!state.snoozes[0].set_by_peer.is_empty());
    }

    #[test]
    fn set_snooze_replaces_same_topic_not_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = 1_000_000_000_000_i64;
        set_snooze(root, "fleet/sec", 60, now).unwrap();
        let until2 = set_snooze(root, "fleet/sec", 3600, now + 1000).unwrap();
        let state = crate::dnd::load_default(root);
        // Same topic → one entry, refreshed to the later expiry.
        assert_eq!(state.snoozes.len(), 1);
        assert_eq!(state.snoozes[0].until_unix_ms, until2);
    }

    #[test]
    fn set_snooze_prunes_expired_on_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = 1_000_000_000_000_i64;
        // Seed a snooze that's already long expired by hand.
        let mut state = crate::dnd::DndState::default();
        state.snoozes.push(crate::dnd::TopicSnooze {
            topic: "old/topic".to_string(),
            until_unix_ms: now - 5_000,
            set_by_peer: "peerA".to_string(),
        });
        crate::dnd::save_default(root, &state).unwrap();
        // A new snooze write prunes the expired one.
        set_snooze(root, "fleet/sec", 60, now).unwrap();
        let reloaded = crate::dnd::load_default(root);
        assert_eq!(reloaded.snoozes.len(), 1);
        assert_eq!(reloaded.snoozes[0].topic, "fleet/sec");
    }

    #[test]
    fn clear_snooze_removes_matching_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let now = 1_000_000_000_000_i64;
        set_snooze(root, "fleet/sec", 3600, now).unwrap();
        assert!(clear_snooze(root, "fleet/sec", now).unwrap());
        assert!(crate::dnd::load_default(root).snoozes.is_empty());
        // Clearing a topic with no snooze returns false.
        assert!(!clear_snooze(root, "fleet/sec", now).unwrap());
    }

    #[tokio::test]
    async fn add_with_duration_writes_fleet_snooze() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        run(MuteOp::Add {
            topic: "fleet/sec".to_string(),
            manifest: None,
            duration: Some("1h".to_string()),
            bus_root: Some(root.clone()),
        })
        .await
        .unwrap();
        let state = crate::dnd::load_default(&root);
        assert_eq!(state.snoozes.len(), 1);
        assert_eq!(state.snoozes[0].topic, "fleet/sec");
        // Fleet snooze must NOT have written a per-peer subs.yaml mute.
        assert!(!root.join("subs.yaml").exists());
    }

    #[tokio::test]
    async fn add_with_bad_duration_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let r = run(MuteOp::Add {
            topic: "fleet/sec".to_string(),
            manifest: None,
            duration: Some("banana".to_string()),
            bus_root: Some(root),
        })
        .await;
        assert!(r.is_err());
    }
}
