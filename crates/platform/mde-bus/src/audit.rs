//! BUS-7.1 + EPIC-BUS-EXT-AUDIT-BUS (Q28) — the publish audit trail.
//!
//! Every publish through [`crate::persist::Persist::write`] emits one
//! metadata-only [`AuditEntry`] to the `audit/<peer>` Bus topic
//! (`<peer>` = the publishing peer's identity). The record carries
//! just the metadata an operator + security audit needs — "who / when
//! / what topic / what priority / which ULID" — never the message
//! body. The body lives in the topic file tree (BUS-1.4) where the
//! audit can re-fetch when needed.
//!
//! **Migrated from per-day JSONL to a Bus topic (Q28, 2026-05-28):**
//! the audit trail used to be `<bus_root>/audit/<YYYY-MM-DD>.jsonl`
//! files. It now rides the Bus itself — one uniform substrate — so
//! cross-peer audit visibility is trivial (every peer subscribes to
//! `audit/+` via the default `#` manifest, and the GFS-replicated
//! per-peer trees converge). Properties:
//!
//! - **`priority = min`** — audit records are the lowest-noise class.
//! - **`retention = forever`** — the retention reaper exempts
//!   `audit/*` topics regardless of priority
//!   (see [`crate::retention::run_pass_at`]).
//! - **Cycle-guarded** — `Persist::write` does NOT audit a write whose
//!   topic is already under `audit/` (else infinite recursion).
//! - **Best-effort** — a failed audit emit never fails the original
//!   publish; the message is already durably stored.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One audit-log entry. Metadata only — never the body. This is the
/// JSON body of each `audit/<peer>` Bus message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// Publisher identity. Defaults to the peer hostname for
    /// daemon-originated publishes; webhook handlers pass the
    /// adapter name (`github`, `gitea`, etc.); CLI publishes
    /// pass `cli:<hostname>` so audits distinguish operator
    /// commands from daemon traffic.
    pub publisher: String,
    /// ISO-8601 (UTC) timestamp of the publish.
    pub ts_iso: String,
    /// Topic the message landed on.
    pub topic: String,
    /// Priority — `min` / `default` / `high` / `urgent`.
    pub priority: String,
    /// ULID of the message in the file tree + index.
    pub ulid: String,
}

/// Errors reading the audit trail.
#[derive(Debug, Error)]
pub enum AuditError {
    /// File-system / index error.
    #[error("io: {0}")]
    Io(String),
    /// JSON serialization error (should never happen — the
    /// type is plain JSON-compatible).
    #[error("json: {0}")]
    Json(String),
}

/// Read every audit entry across all `audit/<peer>` topics under
/// `bus_root`, oldest-first (by ISO timestamp). Decodes each
/// `audit/*` Bus message body back into an [`AuditEntry`]. Per-message
/// decode failures are skipped + logged (one malformed record
/// shouldn't blow away the whole read). Returns `Ok(vec![])` when
/// there's no audit topic yet (pre-first-publish).
///
/// Replaces the retired JSONL reader: the audit trail is now a Bus
/// topic, so the read path goes through the per-peer index.
///
/// # Errors
/// [`AuditError::Io`] when the index can't be opened or queried.
pub fn read_entries_from_bus(bus_root: &std::path::Path) -> Result<Vec<AuditEntry>, AuditError> {
    let persist = crate::persist::Persist::open(bus_root.to_path_buf())
        .map_err(|e| AuditError::Io(format!("open index: {e}")))?;
    let topics = persist
        .list_topics()
        .map_err(|e| AuditError::Io(format!("list_topics: {e}")))?;
    let mut out = Vec::new();
    for topic in topics {
        if !topic.starts_with(crate::persist::AUDIT_TOPIC_PREFIX) {
            continue;
        }
        let msgs = persist
            .list_since(&topic, None)
            .map_err(|e| AuditError::Io(format!("list_since {topic}: {e}")))?;
        for m in msgs {
            let Some(body) = m.body else { continue };
            match serde_json::from_str::<AuditEntry>(&body) {
                Ok(entry) => out.push(entry),
                Err(e) => tracing::warn!(
                    target: "mde_bus::audit",
                    ulid = %m.ulid,
                    error = %e,
                    "skipping malformed audit record"
                ),
            }
        }
    }
    // Oldest-first by ISO timestamp (sort-friendly). Matches the
    // chronological order the old JSONL reader produced.
    out.sort_by(|a, b| a.ts_iso.cmp(&b.ts_iso));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::Priority;
    use crate::persist::Persist;

    #[test]
    fn audit_entry_round_trips_through_json() {
        let e = AuditEntry {
            publisher: "peer-a".into(),
            ts_iso: "2026-05-28T12:00:00+00:00".into(),
            topic: "mesh/foo".into(),
            priority: "default".into(),
            ulid: "01ABC".into(),
        };
        let raw = serde_json::to_string(&e).unwrap();
        let back: AuditEntry = serde_json::from_str(&raw).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn read_entries_empty_when_no_audit_topic() {
        let tmp = tempfile::tempdir().unwrap();
        let entries = read_entries_from_bus(tmp.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn read_entries_decodes_audit_messages_from_the_bus() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        // A regular publish auto-emits an audit record to audit/<peer>
        // (the persist write-path does this). Publish two messages.
        p.write("mesh/alpha", Priority::Default, None, Some("a"))
            .unwrap();
        p.write("mon/beta", Priority::High, None, Some("b"))
            .unwrap();
        // The audit trail now has two records (one per publish),
        // readable back as AuditEntry with the ORIGINAL topics.
        let entries = read_entries_from_bus(tmp.path()).unwrap();
        assert_eq!(entries.len(), 2, "one audit record per publish");
        let topics: std::collections::BTreeSet<&str> =
            entries.iter().map(|e| e.topic.as_str()).collect();
        assert!(topics.contains("mesh/alpha"));
        assert!(topics.contains("mon/beta"));
        // The audit records themselves were NOT re-audited (cycle
        // guard) — else we'd see audit/<peer> entries too.
        assert!(!entries.iter().any(|e| e.topic.starts_with("audit/")));
    }
}
