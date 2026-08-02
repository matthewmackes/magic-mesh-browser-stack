//! BUS-1.9 — message retention + GFS quota enforcement.
//!
//! Per `docs/design/v6.x-mackes-bus.md` §8:
//!
//! - **Per-priority TTL**:
//!     - `urgent`  — forever (never deleted)
//!     - `high`    — 30 days
//!     - `default` — 7 days
//!     - `min`     — 24 hours
//!   Operators can override per-topic via `policy.yaml` (deferred
//!   to BUS-7.3 — the default policy ships hardcoded here).
//! - **`audit/*` is BOUNDED, not exempt** (AUDIT-RUN-CAP-1): the audit
//!   trail rides the bus on the `/run` tmpfs, so each pass prunes the
//!   oldest audit records once the lane exceeds [`AUDIT_RUN_CAP_BYTES`],
//!   keeping only the most-recent window resident. The old
//!   retention=forever exemption let audit fill a small `/run` to 100%
//!   and wedge the node (lh1, 2026-06-24). The §8 long-term hash-chain
//!   audit is a durable-disk concern (not yet persisted there).
//! - **GFS quota** — soft 500 MB, hard 2 GB. Soft breach
//!   publishes a `bus/sys/quota` warning at `default` priority;
//!   hard breach AFAICT halts new publishes (BUS-1.4 write path
//!   doesn't enforce yet — surfaced in [`QuotaReport`] for the
//!   operator alert path).
//!
//! The retention loop spawns as a tokio task next to the broker
//! supervisor and runs one GC pass per hour. Each pass walks the
//! SQLite index by `ts_unix_ms`, finds messages past their
//! priority's TTL, deletes both the on-disk JSON file and the
//! index row, and finally sums disk usage to compare against
//! the quota.
//!
//! Pure helpers — `policy_ttl`, `ttl_cutoff_unix_ms`,
//! `find_expired_in_index`, `disk_usage_bytes` — are exposed
//! for tests so the loop can be exercised deterministically
//! with a stub clock.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use thiserror::Error;

use crate::persist::Persist;

/// TTL per priority class. Operators can override per-topic in
/// BUS-7.3's policy.yaml (deferred); the defaults here match
/// the design-doc lock.
pub const DEFAULT_TTL_MIN_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_TTL_DEFAULT_SECS: u64 = 7 * 24 * 60 * 60;
pub const DEFAULT_TTL_HIGH_SECS: u64 = 30 * 24 * 60 * 60;
/// EFF-47 — ephemeral RPC topics (`reply/<ulid>` + `action/…`).
/// Interactive request/response pairs are garbage minutes after
/// the exchange; without this class they sat at the `default`
/// 7-day TTL and accumulated one row + file per RPC. One hour
/// comfortably covers any in-flight poll/retry window.
pub const DEFAULT_TTL_EPHEMERAL_SECS: u64 = 60 * 60;

/// GFS quota thresholds. These are the conservative **tmpfs-safe** fallback
/// defaults: the bus spool lives on `/run` (tmpfs), and a lighthouse `/run` is
/// as small as ~190 MB, so the old 500 MB soft / 2 GB hard could never fire
/// before ENOSPC bricked the node (BULLETPROOF-1, found live 2026-06-16 — both
/// lighthouses filled `/run` to 100% on the unreaped `audit/*` lane). mackesd
/// overrides these with a filesystem-relative cap at spawn; the fixed default
/// stays small enough that it alone keeps the smallest tmpfs from filling.
pub const DEFAULT_QUOTA_SOFT_BYTES: u64 = 96 * 1024 * 1024;
pub const DEFAULT_QUOTA_HARD_BYTES: u64 = 144 * 1024 * 1024;

/// AUDIT-RUN-CAP-1 — per-topic byte cap for the `audit/*` lane on `/run`.
///
/// The audit trail rides the bus on the `/run` tmpfs (see [`crate::audit`]).
/// It was previously **retention=forever / exempt** — neither the per-priority
/// TTL reap nor the hard-cap evictor would shed it until every non-audit byte
/// was already gone. On a small lighthouse `/run` (~190 MB) that exemption was
/// the wedge: a `get-state` poll grew `audit/<peer>` to 79 MB and filled `/run`
/// to 100% before the df-pressure evictor could ever reach the audit lane
/// (lh1, 2026-06-24; Eagle, same week, hit 479 MB / 122k records). So the
/// `/run` copy of the audit trail is now **bounded**, not exempt: each pass
/// prunes the OLDEST `audit/*` records once the lane exceeds this cap, and the
/// hard-cap / df-pressure evictor can shed audit too (it is no longer skipped).
///
/// 8 MiB keeps a generous most-recent audit window resident on `/run` — the
/// only window an operator needs live for incident triage — while staying a
/// small fraction of even the smallest tmpfs, so audit alone can never fill it.
///
/// SECURITY NOTE (§8): this bounds ONLY the volatile `/run` copy. The §8
/// long-term hash-chain audit is a DURABLE-DISK concern and is NOT yet
/// persisted there — the trail today lives solely on this tmpfs and does not
/// survive reboot (see [`crate::audit`]). Bounding the `/run` copy does not
/// regress that, because the long-term store was never the tmpfs copy; when the
/// durable §8 store lands, the most-recent window kept here is the live tail of
/// it, not the system of record. Until then, an operator who needs history
/// beyond this window must read it before it rotates off `/run`.
pub const AUDIT_RUN_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// AUDIT-RUN-CAP-1 — entry-count backstop for the `audit/*` lane, paired with
/// [`AUDIT_RUN_CAP_BYTES`]. The byte cap measures spool *file* bytes, but the
/// SQLite index ALSO duplicates each record (BUS-RETENTION-1) and is itself a
/// real tmpfs consumer, and a record whose spool file is missing/0-length (a
/// partial write, or a tmpfs that lost spool files but kept the index)
/// contributes 0 to the byte sum. Without a second bound, such records would
/// accumulate index rows without ever tripping the byte cap — re-opening the
/// unbounded-index wedge that filled a 391 MB `/run` (v10.0.18). So the prune
/// ALSO keeps at most this many of the most-recent records REGARDLESS of their
/// on-disk size: every record costs exactly one toward this budget, so the lane
/// can never stall on a 0-byte read.
///
/// BUS-RUN-INODE-1 — lowered from 20k to 2k. Each retained record is a separate
/// `<ulid>.json` FILE, and the `/run` tmpfs the bus lives on has a FIXED inode
/// budget unrelated to its byte size (a ~3.9 GB DO-lighthouse `/run` has only
/// ~819,200 inodes). At 20k the byte cap was the binding limit (~200 B records ⇒
/// well under 8 MiB), but `entries × topic_count` FILES can exhaust inodes long
/// before any byte cap trips — which is exactly how `/run/mde-bus` reached
/// ~754,000 files / ~390 MB on both lighthouses (12.1.0 release), wedging
/// mackesd's systemd namespace setup (`status=226/NAMESPACE`). 2k records is
/// still a generous most-recent window for incident triage while cutting the
/// per-topic file (inode) footprint an order of magnitude. Whichever bound is
/// hit first wins (the prune keeps the SMALLER window); the global
/// [`DEFAULT_MAX_SPOOL_FILES`] guard backstops the aggregate across all topics.
pub const AUDIT_RUN_CAP_ENTRIES: usize = 2_000;

/// BROKER-RESILIENCE-1 — per-topic byte cap for EVERY non-audit topic on `/run`.
///
/// The audit lane already has its own ([`AUDIT_RUN_CAP_BYTES`]); this is the
/// matching bound for every OTHER topic. The TTL reap shortest class is the
/// `min` 24 h / ephemeral 1 h window, and the global quota / fs-pressure
/// evictor only fires under whole-`/run` pressure — so between those, a single
/// high-frequency topic can balloon for a long time. When the broker is down
/// that is exactly what happens: the alert/state sources keep publishing
/// (persist-only) at their normal 2 s cadence, and the live wedge showed
/// `state/boot-readiness` at 25 MB, `audit/*` at 19 MB, `compute/*` at 13 MB —
/// ≈77 MB of a 190 MB `/run` — with NO broker ever draining them. A down broker
/// must NEVER be able to fill `/run`, so each pass also bounds every topic's
/// most-recent window to this cap, independent of the broker, the global quota,
/// or fs pressure. Conservative on purpose: 8 MiB per topic keeps a generous
/// live window for triage while staying a small fraction of even the smallest
/// tmpfs, so no one topic alone can fill it. Whichever of the byte/entry bound
/// is hit first wins (the prune keeps the smaller window).
pub const TOPIC_RUN_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// BROKER-RESILIENCE-1 — entry-count backstop for the per-topic cap, the exact
/// analogue of [`AUDIT_RUN_CAP_ENTRIES`] for non-audit topics: a record whose
/// spool file is missing/0-length contributes 0 to the byte sum, so without a
/// count bound the index rows (which duplicate each body — BUS-RETENTION-1)
/// could grow unbounded while the byte cap never trips. Counting one-per-record
/// can never stall on a 0-byte read.
///
/// BUS-RUN-INODE-1 — lowered from 20k to 2k for the same reason as
/// [`AUDIT_RUN_CAP_ENTRIES`]: each entry is one `<ulid>.json` FILE, and the byte
/// cap does not bound inodes. With ~40 active non-audit topics, a 20k per-topic
/// entry cap alone admitted ~800k files — enough to exhaust the ~819,200-inode
/// `/run` tmpfs on both lighthouses (12.1.0) while every byte cap read "under
/// budget". 2k keeps a generous per-topic window; the global
/// [`DEFAULT_MAX_SPOOL_FILES`] guard bounds the aggregate regardless of topic
/// count.
pub const TOPIC_RUN_CAP_ENTRIES: usize = 2_000;

/// BUS-RUN-FULL-1-dfguard — filesystem-pressure trip point. The bus's own
/// quota (`quota_hard_bytes`) only bounds the bus's *own* footprint; it says
/// nothing about the rest of the `/run` tmpfs the bus shares with dnf locks, the
/// journal, systemd runtime state, etc. On a small lighthouse `/run` (~190 MB) a
/// co-tenant writer can push the *filesystem* to 90%+ while the bus is still
/// comfortably under its own hard cap — and a full `/run` breaks runtime locks
/// (dnf, the bus's own WAL) and wedges the node (the v10.0.18 roll failure
/// class). So when the actual filesystem holding `bus_root` crosses this fill
/// ratio, `run_pass_at` lowers the *effective* hard cap below the bus's current
/// footprint and lets the existing oldest-first evictor emergency-prune the
/// spool, handing headroom back to the OS rather than wedging on a small `/run`.
/// 85% leaves enough slack that the prune lands before ENOSPC.
pub const FS_PRESSURE_FILL_PCT: u64 = 85;

/// BUS-RUN-INODE-1 — global cap on the NUMBER of spool message files (inodes)
/// the bus may keep on `/run`, independent of their byte size.
///
/// Every OTHER retention bound — the per-priority TTL, the per-topic
/// [`TOPIC_RUN_CAP_BYTES`] / [`AUDIT_RUN_CAP_BYTES`] caps, the hard-cap
/// ([`RetentionPolicy::quota_hard_bytes`]) and fs-pressure evictors — is measured
/// in BYTES, and mackesd sizes the byte hard cap relative to the *byte* size of
/// the hosting `/run` (`total/2`). But a tmpfs has a FIXED, SEPARATE inode budget
/// that is unrelated to its byte size: a ~3.9 GB DO-lighthouse `/run` has only
/// ~819,200 inodes. The bus writes one small `<ulid>.json` file per message, and
/// the per-topic entry cap only bounds files WITHIN a topic — so
/// `entries_per_topic × topic_count` files accrete with no AGGREGATE bound. That
/// is exactly how `/run/mde-bus` reached ~754,000 files / ~390 MB on both
/// lighthouses during the 12.1.0 release: the byte hard cap (~1.95 GB) never
/// fired because bytes were low, but the ~819,200 inodes were exhausted, and
/// mackesd then failed its systemd namespace setup (`status=226/NAMESPACE`) — a
/// hard node wedge the byte-only guards could not see coming.
///
/// So each pass ALSO bounds the total spool file count: once it exceeds the cap
/// the oldest files (across all topics, `audit/*` shed last) are evicted until
/// back under, exactly the oldest-first policy the byte hard cap uses — just
/// counting inodes instead of bytes. This fixed default is the tmpfs-safe
/// fallback used by the standalone `mde-bus` daemon + tests; mackesd overrides it
/// relative to the filesystem's actual inode total at spawn (a quarter of
/// `df --output=itotal`), so a small (~190 MB) and a large (~3.9 GB) `/run` are
/// both bounded well below their inode budget. `0` disables the guard.
pub const DEFAULT_MAX_SPOOL_FILES: u64 = 50_000;

/// Default GC pass cadence. BUS-RUN-FULL-1 (2026-06-18): the old hourly pass let
/// high-ingest nodes refill `/run` (tmpfs) from the quota cap to 100% between
/// passes — a live .13 spool reached 389 MB (double the ~195 MB cap) and blocked
/// dnf. The quota eviction (BULLETPROOF-1, oldest-first) only bounds the spool if
/// it runs often enough to keep pace with ingest, so the pass is now every 2 min:
/// ingest per window (a few MB) stays far under the cap headroom, so `/run` never
/// fills. Still far faster than the shortest TTL.
pub const DEFAULT_PASS_INTERVAL: Duration = Duration::from_secs(120);

/// Resolved retention policy. Construct via [`RetentionPolicy::default()`]
/// for the design-locked defaults.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Seconds before `min`-priority messages are GC'd.
    pub ttl_min_secs: u64,
    /// Seconds before `default`-priority messages are GC'd.
    pub ttl_default_secs: u64,
    /// Seconds before `high`-priority messages are GC'd.
    pub ttl_high_secs: u64,
    /// EFF-47 — seconds before ephemeral RPC topics (`reply/*`,
    /// `action/*`) are GC'd regardless of priority class.
    pub ttl_ephemeral_secs: u64,
    /// Soft quota — exceeding this publishes `bus/sys/quota`
    /// warning (default priority).
    pub quota_soft_bytes: u64,
    /// Hard quota — surfaced in [`QuotaReport::hard_exceeded`]
    /// so the operator + downstream alerting can react.
    pub quota_hard_bytes: u64,
    /// AUDIT-RUN-CAP-1 — per-topic byte cap for the `audit/*` lane on `/run`.
    /// Audit is BOUNDED, not exempt: each pass prunes the oldest audit records
    /// once the lane exceeds this, keeping the most-recent window resident on
    /// the tmpfs without letting audit grow unbounded and wedge the node.
    pub audit_cap_bytes: u64,
    /// AUDIT-RUN-CAP-1 — entry-count backstop for the `audit/*` lane. The prune
    /// keeps at most this many of the most-recent audit records regardless of
    /// their on-disk size, so a 0-byte/missing-file read can never stall the
    /// byte cap and let index rows grow unbounded. See [`AUDIT_RUN_CAP_ENTRIES`].
    pub audit_cap_entries: usize,
    /// BROKER-RESILIENCE-1 — per-topic byte cap for EVERY non-audit topic. Each
    /// pass bounds every topic's most-recent window to this size so no single
    /// topic can grow unbounded between TTL/quota passes — the bound that makes
    /// a down broker unable to fill `/run`. See [`TOPIC_RUN_CAP_BYTES`].
    pub topic_cap_bytes: u64,
    /// BROKER-RESILIENCE-1 — entry-count backstop for the per-topic cap (the
    /// analogue of `audit_cap_entries` for non-audit topics, so a 0-byte read
    /// can't stall the byte cap). See [`TOPIC_RUN_CAP_ENTRIES`].
    pub topic_cap_entries: usize,
    /// BUS-RUN-INODE-1 — global cap on the total number of spool message files
    /// (inodes) across ALL topics. The byte caps above do not bound inodes, and a
    /// tmpfs's inode budget is fixed + unrelated to its byte size — so without
    /// this the spool can exhaust `/run`'s inodes far under any byte cap (the
    /// ~754k-file lighthouse wedge). Each pass evicts the oldest files once the
    /// count exceeds this, mirroring the byte hard-cap's oldest-first policy.
    /// `0` disables the guard. See [`DEFAULT_MAX_SPOOL_FILES`].
    pub max_spool_files: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            ttl_min_secs: DEFAULT_TTL_MIN_SECS,
            ttl_default_secs: DEFAULT_TTL_DEFAULT_SECS,
            ttl_high_secs: DEFAULT_TTL_HIGH_SECS,
            ttl_ephemeral_secs: DEFAULT_TTL_EPHEMERAL_SECS,
            quota_soft_bytes: DEFAULT_QUOTA_SOFT_BYTES,
            quota_hard_bytes: DEFAULT_QUOTA_HARD_BYTES,
            audit_cap_bytes: AUDIT_RUN_CAP_BYTES,
            audit_cap_entries: AUDIT_RUN_CAP_ENTRIES,
            topic_cap_bytes: TOPIC_RUN_CAP_BYTES,
            topic_cap_entries: TOPIC_RUN_CAP_ENTRIES,
            max_spool_files: DEFAULT_MAX_SPOOL_FILES,
        }
    }
}

impl RetentionPolicy {
    /// TTL in seconds for a given priority. `urgent` returns
    /// `None` (never expires).
    #[must_use]
    pub fn ttl_secs(&self, priority: &str) -> Option<u64> {
        match priority {
            "min" => Some(self.ttl_min_secs),
            "default" => Some(self.ttl_default_secs),
            "high" => Some(self.ttl_high_secs),
            "urgent" => None,
            _ => Some(self.ttl_default_secs),
        }
    }
}

/// Result of one retention pass.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    /// Files (and matching index rows) removed by TTL expiry this pass.
    pub removed: usize,
    /// AUDIT-RUN-CAP-1 — oldest `audit/*` records pruned this pass because the
    /// audit lane exceeded its per-topic `/run` cap ([`AUDIT_RUN_CAP_BYTES`]).
    /// The audit trail is BOUNDED on the tmpfs, not exempt: the most-recent
    /// window stays resident, everything older rotates off. Distinct from
    /// `removed` (TTL expiry) and `evicted` (hard-cap / fs-pressure).
    pub audit_pruned: usize,
    /// BROKER-RESILIENCE-1 — records pruned this pass because a non-audit topic
    /// exceeded its per-topic `/run` cap ([`TOPIC_RUN_CAP_BYTES`] /
    /// [`TOPIC_RUN_CAP_ENTRIES`]). Each topic's most-recent window stays
    /// resident; everything older rotates off — so a single high-frequency
    /// topic (an alert source publishing persist-only to a down broker) can
    /// never grow unbounded and fill the tmpfs. Distinct from `audit_pruned`
    /// (the audit lane), `removed` (TTL expiry), and `evicted` (hard-cap).
    pub topic_pruned: usize,
    /// BULLETPROOF-1 — messages evicted by the hard-cap safety valve
    /// (oldest-first, regardless of TTL) because the spool exceeded the
    /// hard quota. Distinct from `removed` so the log shows when the bus
    /// is shedding live data to stay off ENOSPC.
    pub evicted: usize,
    /// BUS-RUN-INODE-1 — message FILES evicted this pass by the global spool
    /// file-count (inode) guard, because the total spool file count exceeded
    /// [`RetentionPolicy::max_spool_files`]. Oldest-first across all topics
    /// (`audit/*` shed last), mirroring `evicted`. Distinct from the byte-based
    /// `evicted`: this fires on INODE exhaustion (many small files) while bytes
    /// are still under the byte hard cap — the /run-inode wedge the byte guards
    /// could not see (both lighthouses, 12.1.0).
    pub inode_evicted: usize,
    /// BUS-RUN-INODE-1 — true when the filesystem holding `bus_root` was at/over
    /// [`FS_PRESSURE_FILL_PCT`] on INODES this pass, so the effective file cap was
    /// lowered below the bus's own file count to emergency-shed inodes (the inode
    /// analogue of `fs_pressure`). Distinct from `inode_evicted > 0`, which also
    /// fires on a plain bus-only file-cap breach.
    pub inode_pressure: bool,
    /// Total disk bytes the bus occupies after the pass — the remaining spool
    /// `*.json` files PLUS the index DB itself (`index.sqlite{,-wal,-shm}`,
    /// BUS-RETENTION-1). This is the true `/run` footprint the quota guards.
    pub bytes_after: u64,
    /// BUS-RUN-FULL-1-dfguard — true when the filesystem holding `bus_root` was
    /// at/over [`FS_PRESSURE_FILL_PCT`] this pass, so the effective hard cap was
    /// lowered below the bus's own footprint to emergency-prune the spool (the
    /// bus shedding its own data to keep a near-full `/run` off ENOSPC, even
    /// though the bus's bytes alone were under the configured hard cap). Distinct
    /// from `evicted > 0`, which also fires on a plain bus-only hard-cap breach.
    pub fs_pressure: bool,
    /// Quota state post-pass.
    pub quota: QuotaReport,
}

/// Quota breach status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuotaReport {
    /// True when bytes_after > soft_quota.
    pub soft_exceeded: bool,
    /// True when bytes_after > hard_quota.
    pub hard_exceeded: bool,
}

/// Errors a retention pass can surface.
#[derive(Debug, Error)]
pub enum RetentionError {
    /// SQLite failure (query, delete, etc.).
    #[error("sql: {0}")]
    Sql(String),
    /// File-system failure (delete, walk, etc.).
    #[error("io: {0}")]
    Io(String),
}

/// Compute the unix-ms cutoff below which messages of `priority`
/// are eligible for deletion. Pure helper exposed for tests.
#[must_use]
pub fn ttl_cutoff_unix_ms(
    policy: &RetentionPolicy,
    priority: &str,
    now_unix_ms: i64,
) -> Option<i64> {
    let secs = policy.ttl_secs(priority)?;
    let cutoff = now_unix_ms - i64::try_from(secs * 1000).unwrap_or(i64::MAX);
    Some(cutoff)
}

/// Walk the file tree under `root` and sum the bytes of every
/// `<ulid>.json` it contains. Excludes the SQLite database
/// itself + any `.tmp` files. Pure helper for tests.
pub fn disk_usage_bytes(root: &Path) -> Result<u64, RetentionError> {
    fn walk(dir: &Path, acc: &mut u64) -> Result<(), RetentionError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| RetentionError::Io(format!("readdir {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| RetentionError::Io(format!("readdir entry: {e}")))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("index.sqlite") || name.ends_with(".tmp") {
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let ft = entry
                .file_type()
                .map_err(|e| RetentionError::Io(format!("file_type {}: {e}", path.display())))?;
            if ft.is_dir() {
                walk(&path, acc)?;
            } else if ft.is_file() && name.ends_with(".json") {
                let meta = entry
                    .metadata()
                    .map_err(|e| RetentionError::Io(format!("metadata {}: {e}", path.display())))?;
                *acc += meta.len();
            }
        }
        Ok(())
    }
    let mut acc: u64 = 0;
    walk(root, &mut acc)?;
    Ok(acc)
}

/// BUS-RUN-INODE-1 — the NUMBER of spool `<ulid>.json` message files under
/// `root` (i.e. the inodes the bus spool consumes on the tmpfs). The inode
/// analogue of [`disk_usage_bytes`]: same walk + same exclusions (the index DB,
/// `.tmp` scratch, hidden files), but counting files instead of summing bytes.
/// This is the footprint the global file-count guard bounds — a tmpfs's inode
/// budget is fixed + independent of its byte size, so a spool can exhaust inodes
/// while every byte cap reads "under budget" (the /run-inode lighthouse wedge).
/// Pure helper, exposed for tests.
pub fn spool_file_count(root: &Path) -> Result<u64, RetentionError> {
    fn walk(dir: &Path, acc: &mut u64) -> Result<(), RetentionError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| RetentionError::Io(format!("readdir {}: {e}", dir.display())))?;
        for entry in entries {
            let entry = entry.map_err(|e| RetentionError::Io(format!("readdir entry: {e}")))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("index.sqlite") || name.ends_with(".tmp") {
                continue;
            }
            if name.starts_with('.') {
                continue;
            }
            let ft = entry
                .file_type()
                .map_err(|e| RetentionError::Io(format!("file_type {}: {e}", path.display())))?;
            if ft.is_dir() {
                walk(&path, acc)?;
            } else if ft.is_file() && name.ends_with(".json") {
                *acc += 1;
            }
        }
        Ok(())
    }
    let mut acc: u64 = 0;
    walk(root, &mut acc)?;
    Ok(acc)
}

/// BUS-RETENTION-1 — on-disk bytes of the index DB itself (`index.sqlite` plus
/// its `-wal`/`-shm` sidecars). [`disk_usage_bytes`] deliberately excludes these
/// (it sums only spool `*.json`), but the index is a real consumer of the tmpfs
/// that backs the bus — it duplicates each message's title+body — so the quota
/// footprint MUST include it. A 121 MB `index.sqlite` filled a 391 MB `/run`
/// while the spool-only accounting reported "under budget" (found in the
/// v10.0.18 fleet roll, 2026-06-19). Missing files contribute 0.
#[must_use]
pub fn index_db_bytes(root: &Path) -> u64 {
    ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"]
        .iter()
        .filter_map(|n| std::fs::metadata(root.join(n)).ok())
        .map(|m| m.len())
        .sum()
}

/// BUS-RUN-FULL-1-dfguard — the actual fill state of the filesystem hosting
/// `bus_root`, as `(total_bytes, avail_bytes)`. Unlike [`disk_usage_bytes`] +
/// [`index_db_bytes`] (which measure only the *bus's own* footprint), this is the
/// whole shared `/run` tmpfs including every co-tenant writer (dnf locks, the
/// journal, systemd runtime state). `df -B1 --output=size,avail` mirrors the
/// existing mackesd probes; the workspace forbids `unsafe_code`, so a direct
/// `statvfs(2)` via libc is off the table and the coreutils `df` (on every Fedora
/// install) is the no-unsafe read. Returns `None` if `df` is missing or its
/// output is unparseable — the caller then degrades to the plain bus-only cap.
#[must_use]
pub fn filesystem_total_avail_bytes(path: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("df")
        .arg("-B1")
        .arg("--output=size,avail")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Line 1 is the header ("1B-blocks  Avail"); line 2 is "<size> <avail>".
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cols = text.lines().nth(1)?.split_whitespace();
    let total = cols.next()?.parse::<u64>().ok()?;
    let avail = cols.next()?.parse::<u64>().ok()?;
    Some((total, avail))
}

/// BUS-RUN-INODE-1 — the INODE fill of the filesystem holding `path`, as
/// `(itotal, iavail)`. The inode analogue of [`filesystem_total_avail_bytes`]:
/// `df --output=itotal,iavail` reads the tmpfs's inode budget + free inodes,
/// which are FIXED + independent of the byte size, so a spool can run the
/// filesystem out of inodes while it is nowhere near full on bytes (the ~754k
/// file / ~390 MB lighthouse wedge). mackesd uses `itotal` to size the global
/// [`RetentionPolicy::max_spool_files`] cap; the pass uses `iavail` for the inode
/// analogue of the fs-pressure guard. `None` if `df` is missing/unparseable (or
/// the filesystem does not report inodes, e.g. reports `-`), in which case the
/// caller degrades to the fixed configured cap.
#[must_use]
pub fn filesystem_total_avail_inodes(path: &Path) -> Option<(u64, u64)> {
    let out = std::process::Command::new("df")
        .arg("--output=itotal,iavail")
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Line 1 is the header ("Inodes IUsed ..."); line 2 is "<itotal> <iavail>".
    let text = String::from_utf8_lossy(&out.stdout);
    let mut cols = text.lines().nth(1)?.split_whitespace();
    let itotal = cols.next()?.parse::<u64>().ok()?;
    let iavail = cols.next()?.parse::<u64>().ok()?;
    Some((itotal, iavail))
}

/// BUS-RUN-FULL-1-dfguard — pure threshold helper. Given the configured bus hard
/// cap, the bus's current footprint, and the *whole filesystem's* `(total,
/// avail)`, decide the **effective** hard cap to feed the oldest-first evictor and
/// whether the filesystem is under pressure.
///
/// When the filesystem fill ratio `(total - avail) / total` is at/over
/// [`FS_PRESSURE_FILL_PCT`], the bus must shed regardless of its own cap: we
/// compute how many bytes must be freed to bring the filesystem back to the
/// trip point (`reclaim`) and lower the effective hard cap to
/// `footprint - reclaim` (never below 0, never above the configured cap). The
/// existing evictor, which already drives the footprint down toward the soft cap
/// when it exceeds the hard cap, then emergency-prunes the spool. Only the bus's
/// own bytes can be reclaimed, so the achievable floor is naturally bounded — but
/// every byte the bus gives back is headroom the OS regains.
///
/// With no pressure (or `total == 0`, an unreadable probe) the effective cap is
/// just the configured hard cap and `pressure` is false — identical to the
/// pre-dfguard behavior.
#[must_use]
pub fn fs_pressure_hard_cap(
    configured_hard_bytes: u64,
    footprint_bytes: u64,
    fs_total_bytes: u64,
    fs_avail_bytes: u64,
) -> (u64, bool) {
    if fs_total_bytes == 0 {
        return (configured_hard_bytes, false);
    }
    let used = fs_total_bytes.saturating_sub(fs_avail_bytes);
    // Integer-only fill check: used/total >= PCT/100  ⇔  used*100 >= total*PCT.
    // u128 so a multi-GB workstation tmpfs can't overflow the multiply.
    let over =
        u128::from(used) * 100 >= u128::from(fs_total_bytes) * u128::from(FS_PRESSURE_FILL_PCT);
    if !over {
        return (configured_hard_bytes, false);
    }
    // Bytes that must leave the filesystem to drop back to exactly the trip
    // point: used - total*PCT/100.
    let target_used =
        u64::try_from(u128::from(fs_total_bytes) * u128::from(FS_PRESSURE_FILL_PCT) / 100)
            .unwrap_or(fs_total_bytes);
    let reclaim = used.saturating_sub(target_used);
    // Lower the cap below the current footprint by `reclaim` so the evictor sheds
    // at least that much — clamped to [0, configured] so pressure never *raises*
    // the cap and an over-large reclaim just drives the cap to 0 (shed all
    // sheddable spool).
    let effective = footprint_bytes
        .saturating_sub(reclaim)
        .min(configured_hard_bytes);
    (effective, true)
}

/// BUS-RUN-INODE-1 — pure threshold helper, the INODE analogue of
/// [`fs_pressure_hard_cap`]. Given the configured bus file cap, the bus's current
/// spool file count, and the *whole filesystem's* `(itotal, iavail)`, decide the
/// **effective** file cap to feed the oldest-first file evictor and whether the
/// filesystem is under INODE pressure.
///
/// When the filesystem inode fill `(itotal - iavail) / itotal` is at/over
/// [`FS_PRESSURE_FILL_PCT`], the bus must shed files regardless of its own cap:
/// compute how many inodes must be freed to bring the filesystem back to the trip
/// point (`reclaim`) and lower the effective cap to `files - reclaim` (never below
/// 0, never above the configured cap). Only the bus's own files can be reclaimed,
/// so the achievable floor is naturally bounded — but every inode the bus gives
/// back is headroom the OS regains. With no pressure (or `itotal == 0`, an
/// unreadable/inode-less probe) the effective cap is just the configured cap and
/// `pressure` is false.
#[must_use]
pub fn inode_pressure_file_cap(
    configured_file_cap: u64,
    footprint_files: u64,
    fs_total_inodes: u64,
    fs_avail_inodes: u64,
) -> (u64, bool) {
    if fs_total_inodes == 0 {
        return (configured_file_cap, false);
    }
    let used = fs_total_inodes.saturating_sub(fs_avail_inodes);
    // Integer-only fill check: used/itotal >= PCT/100 ⇔ used*100 >= itotal*PCT.
    let over =
        u128::from(used) * 100 >= u128::from(fs_total_inodes) * u128::from(FS_PRESSURE_FILL_PCT);
    if !over {
        return (configured_file_cap, false);
    }
    let target_used =
        u64::try_from(u128::from(fs_total_inodes) * u128::from(FS_PRESSURE_FILL_PCT) / 100)
            .unwrap_or(fs_total_inodes);
    let reclaim = used.saturating_sub(target_used);
    let effective = footprint_files
        .saturating_sub(reclaim)
        .min(configured_file_cap);
    (effective, true)
}

/// BUS-RETENTION-1 — return disk that the index DB freed (by the row deletes in
/// a pass) to the OS. SQLite frees pages internally on `DELETE` but never shrinks
/// the file on its own, so without this the index only ever grows — a live
/// 121 MB `index.sqlite` (which duplicates message title+body) filled a 391 MB
/// `/run` while the spool was capped (found in the v10.0.18 fleet roll). A full
/// `VACUUM` is the only reliable reclaim: `incremental_vacuum` returns ~0 pages
/// in WAL mode (verified). VACUUM rewrites the DB **in place** (same inode →
/// never the [[BUS-INODE-ORPHAN]] unlink/recreate trap) and needs ~the DB size
/// in temp space, which the preceding TTL-reap/eviction just freed on the tmpfs.
///
/// Guarded by `freelist_count` so idle passes (nothing deleted → nothing to
/// reclaim) skip the VACUUM entirely; combined with the 120 s GC cadence the DB
/// stays small, so each VACUUM is sub-second and safe alongside the live broker
/// (own connection, 5 s `busy_timeout`). Every step is best-effort — a
/// BUSY/ENOSPC failure is ignored and retried next pass.
fn compact_index(conn: &Connection) {
    // Return checkpointed WAL pages to the OS (no-op outside WAL mode).
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    // Only VACUUM when there's a non-trivial amount to reclaim (> 64 pages ≈
    // 256 KiB), so idle passes don't rewrite the DB for nothing.
    let freelist: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(0);
    if freelist > 64 {
        let _ = conn.execute_batch("VACUUM;");
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

/// Open the per-peer index that lives at `<bus_root>/index.sqlite`.
fn open_index(bus_root: &Path) -> Result<Connection, RetentionError> {
    let p = bus_root.join("index.sqlite");
    let conn = Connection::open(&p)
        .map_err(|e| RetentionError::Sql(format!("open {}: {e}", p.display())))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| RetentionError::Sql(format!("busy_timeout: {e}")))?;
    Ok(conn)
}

/// One GC pass: find every message past its priority's TTL,
/// remove the on-disk JSON file, delete the index row, then
/// compute the post-pass disk usage + quota state.
///
/// `now_unix_ms` is injected for testability — production
/// callers pass `current_unix_ms()`.
///
/// BUS-RUN-FULL-1-dfguard — also reads the *actual* fill of the filesystem
/// holding `bus_root` ([`filesystem_total_avail_bytes`]); when `/run` is over
/// [`FS_PRESSURE_FILL_PCT`] the effective hard cap is lowered below the bus's own
/// footprint so the existing oldest-first evictor emergency-prunes the spool
/// (handing headroom back to the OS) instead of wedging the node on a small,
/// co-tenant-filled `/run`. See [`fs_pressure_hard_cap`].
///
/// # Errors
/// [`RetentionError::Sql`] or [`RetentionError::Io`] depending
/// on which layer fails.
pub fn run_pass_at(
    policy: &RetentionPolicy,
    bus_root: &Path,
    now_unix_ms: i64,
) -> Result<PassReport, RetentionError> {
    // Probe the real filesystem fill once; `None` (df missing/unparseable)
    // degrades to the plain bus-only cap. BUS-RUN-INODE-1 — probe the INODE fill
    // too, so the global file-count guard also honors whole-`/run` inode pressure.
    let fs = filesystem_total_avail_bytes(bus_root);
    let inodes = filesystem_total_avail_inodes(bus_root);
    run_pass_at_inner(policy, bus_root, now_unix_ms, fs, inodes)
}

/// BUS-RUN-FULL-1-dfguard / BUS-RUN-INODE-1 — the body of [`run_pass_at`] with the
/// filesystem byte `(total, avail)` AND inode `(itotal, iavail)` readings
/// injected, so both dfguard paths are unit-testable without a controllable real
/// `/run`. `fs == None` / `inodes == None` means "probe unavailable" and
/// reproduces the pre-dfguard behavior exactly for that dimension.
fn run_pass_at_inner(
    policy: &RetentionPolicy,
    bus_root: &Path,
    now_unix_ms: i64,
    fs: Option<(u64, u64)>,
    inodes: Option<(u64, u64)>,
) -> Result<PassReport, RetentionError> {
    let conn = open_index(bus_root)?;

    // Build cutoff per priority class and collect rows to delete.
    // EPIC-BUS-EXT-AUDIT-BUS (Q28) — `audit/*` topics are
    // retention=forever (the audit trail must never be reaped, even
    // though audit records are `min` priority); exclude them from the
    // victim query regardless of TTL.
    let audit_like = format!("{}%", crate::persist::AUDIT_TOPIC_PREFIX);
    let mut victims: Vec<(String, String)> = Vec::new(); // (ulid, file_path)
    for priority in ["min", "default", "high"] {
        let Some(cutoff) = ttl_cutoff_unix_ms(policy, priority, now_unix_ms) else {
            continue;
        };
        let mut stmt = conn
            .prepare(
                "SELECT ulid, file_path FROM messages \
                 WHERE priority = ?1 AND ts_unix_ms < ?2 AND topic NOT LIKE ?3",
            )
            .map_err(|e| RetentionError::Sql(format!("prepare: {e}")))?;
        let rows = stmt
            .query_map(params![priority, cutoff, audit_like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| RetentionError::Sql(format!("query: {e}")))?;
        for r in rows {
            victims.push(r.map_err(|e| RetentionError::Sql(format!("decode: {e}")))?);
        }
    }

    // EFF-47 — ephemeral RPC topics (`reply/<ulid>` request-response
    // pairs + the `action/…` requests that produced them) reap on the
    // short ephemeral TTL regardless of priority class. Without this,
    // every interactive RPC accumulated a row + file for the full
    // 7-day default TTL. `audit/*` can't match either prefix, but the
    // exclusion is kept for defense in depth.
    {
        let cutoff =
            now_unix_ms - i64::try_from(policy.ttl_ephemeral_secs * 1000).unwrap_or(i64::MAX);
        let mut stmt = conn
            .prepare(
                "SELECT ulid, file_path FROM messages \
                 WHERE (topic LIKE 'reply/%' OR topic LIKE 'action/%') \
                   AND ts_unix_ms < ?1 AND topic NOT LIKE ?2",
            )
            .map_err(|e| RetentionError::Sql(format!("prepare ephemeral: {e}")))?;
        let rows = stmt
            .query_map(params![cutoff, audit_like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| RetentionError::Sql(format!("query ephemeral: {e}")))?;
        for r in rows {
            let v = r.map_err(|e| RetentionError::Sql(format!("decode ephemeral: {e}")))?;
            // The per-priority pass may already hold this victim
            // (a reply older than its priority TTL) — dedupe so the
            // delete loop doesn't double-count.
            if !victims.iter().any(|(u, _)| u == &v.0) {
                victims.push(v);
            }
        }
    }

    // Delete file + row for each victim. File first so an
    // index-only orphan is the worst-case failure mode (rather
    // than a row pointing at a deleted file, which the
    // divergence detector then has to clean up).
    let mut removed = 0_usize;
    for (ulid, rel_path) in &victims {
        let abs = bus_root.join(rel_path);
        if abs.exists() {
            std::fs::remove_file(&abs)
                .map_err(|e| RetentionError::Io(format!("rm {}: {e}", abs.display())))?;
        }
        conn.execute("DELETE FROM messages WHERE ulid = ?1", params![ulid])
            .map_err(|e| RetentionError::Sql(format!("delete row {ulid}: {e}")))?;
        removed += 1;
    }

    // AUDIT-RUN-CAP-1 — bound the `audit/*` lane on `/run`. The TTL reap above
    // deliberately skips `audit/*` (so the audit window isn't governed by the
    // `min` 24 h class), but audit is no longer EXEMPT: prune the oldest audit
    // records whenever the lane exceeds its per-topic cap, keeping the
    // most-recent window resident on the tmpfs. This runs every pass — it is the
    // primary bound on audit growth, so the lane can never fill a small `/run`
    // before the hard-cap / fs-pressure evictor would even look at it (the lh1 +
    // Eagle 2026-06-24 wedge). The §8 long-term hash-chain audit belongs on
    // durable disk (not yet persisted there — see [`AUDIT_RUN_CAP_BYTES`]).
    let audit_pruned = prune_audit_over_cap(
        &conn,
        bus_root,
        policy.audit_cap_bytes,
        policy.audit_cap_entries,
    )?;

    // BROKER-RESILIENCE-1 — bound EVERY non-audit topic to its most-recent
    // `/run` window. The TTL reap above only sheds messages past their class's
    // TTL and the hard-cap evictor only fires under whole-`/run` pressure, so
    // between them a single high-frequency topic can balloon — which is exactly
    // what a down broker causes (the alert/state sources keep publishing
    // persist-only). This runs every pass and keeps each topic's window itself
    // bounded, so no one topic can ever fill the tmpfs regardless of broker
    // state. `audit/*` is excluded — it has its own (independent) cap above.
    let topic_pruned = prune_topics_over_cap(
        &conn,
        bus_root,
        policy.topic_cap_bytes,
        policy.topic_cap_entries,
    )?;

    // BUS-RETENTION-1 — reclaim the DB pages the deletes above freed BEFORE we
    // measure/evict, so the footprint reflects a compacted index (not its stale
    // high-water mark) and we don't over-evict spool to compensate for a bloated
    // DB that's about to shrink.
    compact_index(&conn);

    // BULLETPROOF-1 — hard-cap safety valve. The TTL reap skips `audit/*` and
    // the per-topic audit cap above already bounds the audit lane to its
    // most-recent `/run` window — so audit is no longer the unbounded-growth
    // wedge it once was. But the evictor must STILL be able to shed audit under
    // df-pressure: a co-tenant can fill a small `/run` even with audit already
    // capped, and on a 190 MB tmpfs that is the difference between a prune and a
    // node wedge. So if we're still over the HARD cap after the reap + audit
    // prune, evict oldest-first until back under the SOFT cap. Non-audit goes
    // first; `audit/*` is shed after it (it is bounded, but NOT skipped — a full
    // `/run` is strictly worse than losing more of the volatile tmpfs audit tail
    // that does not survive reboot and is not the §8 durable system of record).
    //
    // BUS-RETENTION-1 — the footprint counts the spool files AND the index DB
    // itself (`index_db_bytes`): the DB is a real tmpfs consumer (it duplicates
    // title+body), so excluding it let a 121 MB index fill a 391 MB `/run` while
    // the cap reported "under budget". Eviction can only shed spool files, so it
    // targets `soft - db` to bring spool + (compacted) DB back under the soft cap.
    let mut bytes_after = disk_usage_bytes(bus_root)? + index_db_bytes(bus_root);

    // BUS-RUN-FULL-1-dfguard — fold the *whole filesystem's* fill into the cap.
    // When `/run` is over FS_PRESSURE_FILL_PCT the effective hard cap drops below
    // the bus's current footprint (so eviction fires even if the bus alone is
    // under its configured cap), and the soft target — the level eviction sheds
    // down to — drops by the same relief so the spool actually gives the OS that
    // headroom back. With no pressure both equal the configured caps and the
    // behavior is identical to before.
    let (effective_hard, fs_pressure) = match fs {
        Some((total, avail)) => {
            fs_pressure_hard_cap(policy.quota_hard_bytes, bytes_after, total, avail)
        }
        None => (policy.quota_hard_bytes, false),
    };
    // The eviction landing zone (target total footprint) is the configured soft
    // cap, but never ABOVE the effective hard cap: under fs pressure the effective
    // hard cap sits just below the current footprint (footprint - reclaim), so the
    // evictor sheds exactly the filesystem overshoot — not the whole spool. When
    // the bus is ALSO over its own soft cap, the configured soft cap is the lower
    // (more aggressive) target and wins. With no pressure this is just the
    // configured soft cap, identical to the pre-dfguard behavior.
    let effective_soft = policy.quota_soft_bytes.min(effective_hard);

    let mut evicted = 0_usize;
    if bytes_after > effective_hard {
        let spool = disk_usage_bytes(bus_root)?;
        // The index duplicates each message's title+body, so evicting a message
        // frees its spool file AND (after compaction) its DB rows — roughly
        // (1 + db/spool)× the file size from the TOTAL footprint. eviction only
        // tracks spool bytes, so target the spool level whose total lands at the
        // soft cap: spool_target = spool * soft / total. (u128 to avoid overflow
        // at multi-GB workstation tmpfs sizes.)
        let spool_target = if bytes_after > 0 {
            u64::try_from(u128::from(spool) * u128::from(effective_soft) / u128::from(bytes_after))
                .unwrap_or(effective_soft)
        } else {
            0
        };
        evicted = evict_oldest_to_cap(&conn, bus_root, spool, spool_target)?;
        // Reclaim again — eviction deleted more rows; then re-measure the true
        // footprint (spool + compacted DB).
        compact_index(&conn);
        bytes_after = disk_usage_bytes(bus_root)? + index_db_bytes(bus_root);
    }

    // BUS-RUN-INODE-1 — bound the total spool FILE (inode) count, the dimension
    // NO byte cap covers. A tmpfs has a FIXED inode budget unrelated to its byte
    // size (a ~3.9 GB DO-lighthouse `/run` has only ~819,200 inodes) and the byte
    // hard cap is sized to the byte total, so the spool can exhaust inodes while
    // far under the byte cap — exactly how `/run/mde-bus` reached ~754,000 files /
    // ~390 MB on both lighthouses (12.1.0), wedging mackesd's namespace setup
    // (`status=226/NAMESPACE`). This runs AFTER the byte eviction (so a byte-heavy
    // pass has already shed) and, once the spool file count exceeds the cap,
    // evicts oldest-first across all topics until back under a soft target — the
    // same oldest-first, `audit/*`-last policy the byte hard cap uses, counting
    // inodes instead of bytes. `max_spool_files == 0` disables the guard.
    let mut inode_evicted = 0_usize;
    let mut inode_pressure = false;
    if policy.max_spool_files > 0 {
        let files = spool_file_count(bus_root)?;
        // Fold whole-`/run` inode pressure into the cap (inode analogue of the
        // byte dfguard): under inode pressure the effective cap drops below the
        // current file count so we shed the overshoot even when the bus alone is
        // under its configured file cap. `None` → configured cap, no pressure.
        let (effective_file_cap, pressure) = match inodes {
            Some((itotal, iavail)) => {
                inode_pressure_file_cap(policy.max_spool_files, files, itotal, iavail)
            }
            None => (policy.max_spool_files, false),
        };
        inode_pressure = pressure;
        if files > effective_file_cap {
            // Shed to ~3/4 of the effective cap so the guard doesn't re-fire every
            // pass on a steady-state busy node (the inode analogue of the byte
            // soft/hard gap — hysteresis).
            let file_target = effective_file_cap.saturating_sub(effective_file_cap / 4);
            inode_evicted = evict_oldest_files_to_cap(&conn, bus_root, files, file_target)?;
            compact_index(&conn);
            bytes_after = disk_usage_bytes(bus_root)? + index_db_bytes(bus_root);
        }
    }

    let quota = QuotaReport {
        soft_exceeded: bytes_after > policy.quota_soft_bytes,
        hard_exceeded: bytes_after > policy.quota_hard_bytes,
    };
    Ok(PassReport {
        removed,
        audit_pruned,
        topic_pruned,
        evicted,
        inode_evicted,
        inode_pressure,
        bytes_after,
        fs_pressure,
        quota,
    })
}

/// BULLETPROOF-1 — evict oldest messages (by `ts_unix_ms`) until on-disk usage
/// drops from `start_bytes` to at most `soft_bytes`. Two phases: non-`audit/*`
/// first, then `audit/*` only if shedding all eligible non-audit still leaves
/// us over the cap. Tracks the running total by subtracting each file's size
/// (no full re-walk per delete). Returns the number of messages evicted.
fn evict_oldest_to_cap(
    conn: &rusqlite::Connection,
    bus_root: &Path,
    start_bytes: u64,
    soft_bytes: u64,
) -> Result<usize, RetentionError> {
    let audit_like = format!("{}%", crate::persist::AUDIT_TOPIC_PREFIX);
    let mut running = start_bytes;
    let mut evicted = 0_usize;
    // Phase 1: non-audit oldest-first. Phase 2: audit oldest-first.
    for sql in [
        "SELECT ulid, file_path FROM messages WHERE topic NOT LIKE ?1 ORDER BY ts_unix_ms ASC",
        "SELECT ulid, file_path FROM messages WHERE topic LIKE ?1 ORDER BY ts_unix_ms ASC",
    ] {
        if running <= soft_bytes {
            break;
        }
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| RetentionError::Sql(format!("evict prepare: {e}")))?;
        let rows = stmt
            .query_map(params![audit_like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| RetentionError::Sql(format!("evict query: {e}")))?;
        for r in rows {
            if running <= soft_bytes {
                break;
            }
            let (ulid, rel_path) =
                r.map_err(|e| RetentionError::Sql(format!("evict decode: {e}")))?;
            let abs = bus_root.join(&rel_path);
            let sz = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
            if abs.exists() {
                std::fs::remove_file(&abs)
                    .map_err(|e| RetentionError::Io(format!("evict rm {}: {e}", abs.display())))?;
            }
            conn.execute("DELETE FROM messages WHERE ulid = ?1", params![ulid])
                .map_err(|e| RetentionError::Sql(format!("evict row {ulid}: {e}")))?;
            running = running.saturating_sub(sz);
            evicted += 1;
        }
    }
    Ok(evicted)
}

/// BUS-RUN-INODE-1 — the inode analogue of [`evict_oldest_to_cap`]: evict oldest
/// messages (by `ts_unix_ms`) until the spool FILE COUNT drops from `start_files`
/// to at most `target_files`. Same two phases (non-`audit/*` first, then
/// `audit/*` only if shedding all non-audit still leaves us over) and the same
/// oldest-first policy, but the running total is a FILE COUNT: each delete that
/// actually removed a file on disk decrements it by one (a stale row whose file
/// was already gone frees no inode, so it doesn't count toward the target — but
/// the row is still deleted to keep the index in step). Returns the number of
/// messages evicted.
fn evict_oldest_files_to_cap(
    conn: &rusqlite::Connection,
    bus_root: &Path,
    start_files: u64,
    target_files: u64,
) -> Result<usize, RetentionError> {
    let audit_like = format!("{}%", crate::persist::AUDIT_TOPIC_PREFIX);
    let mut running = start_files;
    let mut evicted = 0_usize;
    // Phase 1: non-audit oldest-first. Phase 2: audit oldest-first.
    for sql in [
        "SELECT ulid, file_path FROM messages WHERE topic NOT LIKE ?1 ORDER BY ts_unix_ms ASC",
        "SELECT ulid, file_path FROM messages WHERE topic LIKE ?1 ORDER BY ts_unix_ms ASC",
    ] {
        if running <= target_files {
            break;
        }
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| RetentionError::Sql(format!("inode-evict prepare: {e}")))?;
        let rows = stmt
            .query_map(params![audit_like], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| RetentionError::Sql(format!("inode-evict query: {e}")))?;
        for r in rows {
            if running <= target_files {
                break;
            }
            let (ulid, rel_path) =
                r.map_err(|e| RetentionError::Sql(format!("inode-evict decode: {e}")))?;
            let abs = bus_root.join(&rel_path);
            let freed_inode = abs.exists();
            if freed_inode {
                std::fs::remove_file(&abs).map_err(|e| {
                    RetentionError::Io(format!("inode-evict rm {}: {e}", abs.display()))
                })?;
            }
            conn.execute("DELETE FROM messages WHERE ulid = ?1", params![ulid])
                .map_err(|e| RetentionError::Sql(format!("inode-evict row {ulid}: {e}")))?;
            if freed_inode {
                running = running.saturating_sub(1);
            }
            evicted += 1;
        }
    }
    Ok(evicted)
}

/// Delete one message's spool file (if present) then its index row. File first
/// so an index-only orphan is the worst-case failure mode (a row pointing at a
/// deleted file is what the divergence detector then has to clean up). Shared by
/// the audit-cap prune; the TTL reap + hard-cap evict loops keep their own
/// inline copies (they track a running byte total as they go).
fn delete_message_file_and_row(
    conn: &rusqlite::Connection,
    abs: &Path,
    ulid: &str,
) -> Result<(), RetentionError> {
    if abs.exists() {
        std::fs::remove_file(abs)
            .map_err(|e| RetentionError::Io(format!("rm {}: {e}", abs.display())))?;
    }
    conn.execute("DELETE FROM messages WHERE ulid = ?1", params![ulid])
        .map_err(|e| RetentionError::Sql(format!("delete row {ulid}: {e}")))?;
    Ok(())
}

/// AUDIT-RUN-CAP-1 — prune the OLDEST `audit/*` records until the audit lane is
/// bounded to its most-recent window: at most `cap_bytes` of on-disk spool AND
/// at most `cap_entries` records (whichever is the smaller window wins). The
/// most-recent records stay resident on `/run`; everything older rotates off.
/// Returns the number of audit records pruned.
///
/// This is what makes `audit/*` BOUNDED rather than exempt: unlike the TTL reap
/// (which still skips `audit/*` so the audit window isn't governed by the `min`
/// 24 h class) and unlike the hard-cap evictor (which only fires under quota /
/// fs pressure), this runs every pass and keeps the audit lane itself from ever
/// growing unbounded on the tmpfs.
///
/// Two bounds, because the byte cap alone is not robust:
/// - **`cap_bytes`** sums each record's spool *file* size (the index has no size
///   column), mirroring [`disk_usage_bytes`]. This is the binding limit in the
///   normal case where files are present.
/// - **`cap_entries`** is the backstop: a record whose file is missing/0-length
///   contributes 0 to the byte sum, so a byte-only cap could never trip while
///   index rows (which duplicate each body — BUS-RETENTION-1) grow unbounded.
///   Counting one-per-record can never stall on a 0-byte read.
///
/// Ordering is `ts_unix_ms DESC, ulid DESC`: ULIDs are monotonic +
/// lexicographically time-ordered, so the secondary key makes the kept window
/// fully deterministic even when a burst of audit records shares the same
/// millisecond (the lh1 `get-state`-poll pattern) — without it, *which* of the
/// equal-timestamp records survived at the boundary was unspecified by SQLite.
///
/// Always keeps at least the single most-recent record (the first row is kept
/// before either bound is consulted), so a degenerate `cap_bytes == 0` /
/// `cap_entries == 0` config bounds the lane to one record rather than wiping
/// the audit trail entirely.
fn prune_audit_over_cap(
    conn: &rusqlite::Connection,
    bus_root: &Path,
    cap_bytes: u64,
    cap_entries: usize,
) -> Result<usize, RetentionError> {
    let audit_like = format!("{}%", crate::persist::AUDIT_TOPIC_PREFIX);
    // Newest-first (deterministic tiebreak on the unique ulid) so we keep the
    // most-recent window and prune from the tail.
    let mut stmt = conn
        .prepare(
            "SELECT ulid, file_path FROM messages \
             WHERE topic LIKE ?1 ORDER BY ts_unix_ms DESC, ulid DESC",
        )
        .map_err(|e| RetentionError::Sql(format!("audit-cap prepare: {e}")))?;
    let rows = stmt
        .query_map(params![audit_like], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| RetentionError::Sql(format!("audit-cap query: {e}")))?;

    let mut kept_bytes: u64 = 0;
    let mut kept_count: usize = 0;
    let mut victims: Vec<(String, PathBuf)> = Vec::new();
    for r in rows {
        let (ulid, rel_path) =
            r.map_err(|e| RetentionError::Sql(format!("audit-cap decode: {e}")))?;
        let abs = bus_root.join(&rel_path);
        let sz = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
        // Keep this record if it is the first (always retain the newest) OR it
        // still fits BOTH bounds. Either bound being reached starts the prune of
        // this and every older record. `kept_count == 0` guarantees forward
        // progress even when both caps are 0 — the lane floors at one record,
        // never zero.
        let within_bounds = kept_count == 0
            || (kept_bytes.saturating_add(sz) <= cap_bytes && kept_count < cap_entries);
        if within_bounds {
            kept_bytes = kept_bytes.saturating_add(sz);
            kept_count += 1;
        } else {
            // Past a cap: this record (and everything older) is pruned.
            victims.push((ulid, abs));
        }
    }
    // Release the statement's borrow before the delete loop reuses `conn`.
    drop(stmt);

    let mut pruned = 0_usize;
    for (ulid, abs) in &victims {
        delete_message_file_and_row(conn, abs, ulid)?;
        pruned += 1;
    }
    Ok(pruned)
}

/// BROKER-RESILIENCE-1 — bound EVERY non-audit topic to its most-recent `/run`
/// window: for each distinct topic, keep at most `cap_bytes` of on-disk spool
/// AND at most `cap_entries` records (whichever is the smaller window wins),
/// pruning the OLDEST records of that topic past the bound. Returns the total
/// records pruned across all topics.
///
/// This is the per-topic analogue of [`prune_audit_over_cap`] for the rest of
/// the bus. It runs every pass and is the bound that makes a down broker unable
/// to fill `/run`: the alert/state sources keep publishing persist-only when the
/// broker is absent, and without a per-topic cap a single one of them
/// (`state/boot-readiness` at the live 2 s cadence) balloons unbounded between
/// the TTL reap and the whole-`/run` evictor. `audit/*` is excluded — it has its
/// own independent cap ([`prune_audit_over_cap`]) so it isn't double-bounded.
///
/// Ordering + the always-keep-the-newest floor mirror [`prune_audit_over_cap`]:
/// `ts_unix_ms DESC, ulid DESC` makes the kept window deterministic even within
/// a same-millisecond burst, and the first record of each topic is retained
/// before either bound is consulted, so a degenerate `cap == 0` bounds a topic
/// to one record rather than wiping it.
fn prune_topics_over_cap(
    conn: &rusqlite::Connection,
    bus_root: &Path,
    cap_bytes: u64,
    cap_entries: usize,
) -> Result<usize, RetentionError> {
    let audit_like = format!("{}%", crate::persist::AUDIT_TOPIC_PREFIX);
    // Distinct non-audit topics. Each is bounded independently below.
    let topics: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT topic FROM messages WHERE topic NOT LIKE ?1")
            .map_err(|e| RetentionError::Sql(format!("topic-cap topics prepare: {e}")))?;
        let rows = stmt
            .query_map(params![audit_like], |r| r.get::<_, String>(0))
            .map_err(|e| RetentionError::Sql(format!("topic-cap topics query: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| RetentionError::Sql(format!("topic-cap topics decode: {e}")))?);
        }
        out
    };

    let mut total_pruned = 0_usize;
    for topic in &topics {
        // Newest-first (deterministic tiebreak on the unique ulid) so we keep
        // the most-recent window and prune from the tail of THIS topic.
        let victims: Vec<(String, PathBuf)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT ulid, file_path FROM messages \
                     WHERE topic = ?1 ORDER BY ts_unix_ms DESC, ulid DESC",
                )
                .map_err(|e| RetentionError::Sql(format!("topic-cap prepare: {e}")))?;
            let rows = stmt
                .query_map(params![topic], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| RetentionError::Sql(format!("topic-cap query: {e}")))?;

            let mut kept_bytes: u64 = 0;
            let mut kept_count: usize = 0;
            let mut victims: Vec<(String, PathBuf)> = Vec::new();
            for r in rows {
                let (ulid, rel_path) =
                    r.map_err(|e| RetentionError::Sql(format!("topic-cap decode: {e}")))?;
                let abs = bus_root.join(&rel_path);
                let sz = std::fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                // Keep the newest unconditionally (forward-progress floor), else
                // keep while BOTH bounds still hold; past either, this record and
                // every older one of this topic is pruned.
                let within_bounds = kept_count == 0
                    || (kept_bytes.saturating_add(sz) <= cap_bytes && kept_count < cap_entries);
                if within_bounds {
                    kept_bytes = kept_bytes.saturating_add(sz);
                    kept_count += 1;
                } else {
                    victims.push((ulid, abs));
                }
            }
            victims
        };
        for (ulid, abs) in &victims {
            delete_message_file_and_row(conn, abs, ulid)?;
            total_pruned += 1;
        }
    }
    Ok(total_pruned)
}

/// Convenience: today's clock as unix-ms.
pub fn current_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Long-lived retention loop. Wakes on a `tokio::time::interval`
/// tick, calls [`run_pass_at`] with the current clock, and
/// publishes a `bus/sys/quota` warning when the soft quota is
/// exceeded.
///
/// Spawns one task; exit on shutdown signal (operator-cancelled
/// via `shutdown_rx`).
pub async fn run_loop(
    policy: RetentionPolicy,
    bus_root: PathBuf,
    interval: Duration,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately — skip it to give the daemon
    // a clean second-stage startup window before the first GC.
    ticker.tick().await;
    let mut last_soft_warn = false;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => return,
            _ = ticker.tick() => {
                match run_pass_at(&policy, &bus_root, current_unix_ms()) {
                    Ok(report) => {
                        tracing::info!(
                            target: "mde_bus::retention",
                            removed = report.removed,
                            audit_pruned = report.audit_pruned,
                            topic_pruned = report.topic_pruned,
                            evicted = report.evicted,
                            inode_evicted = report.inode_evicted,
                            bytes_after = report.bytes_after,
                            fs_pressure = report.fs_pressure,
                            inode_pressure = report.inode_pressure,
                            soft_exceeded = report.quota.soft_exceeded,
                            hard_exceeded = report.quota.hard_exceeded,
                            "retention pass complete"
                        );
                        if report.topic_pruned > 0 {
                            tracing::info!(
                                target: "mde_bus::retention",
                                topic_pruned = report.topic_pruned,
                                "BROKER-RESILIENCE-1: a topic was over its per-topic /run cap — pruned oldest records, kept the most-recent window (a down broker can't fill /run)"
                            );
                        }
                        if report.audit_pruned > 0 {
                            tracing::info!(
                                target: "mde_bus::retention",
                                audit_pruned = report.audit_pruned,
                                "AUDIT-RUN-CAP-1: audit lane over its /run cap — pruned oldest audit records, kept the most-recent window"
                            );
                        }
                        if report.evicted > 0 {
                            tracing::warn!(
                                target: "mde_bus::retention",
                                evicted = report.evicted,
                                bytes_after = report.bytes_after,
                                fs_pressure = report.fs_pressure,
                                "BULLETPROOF-1: hard-cap reached — evicted oldest messages to keep the bus tmpfs off ENOSPC"
                            );
                        }
                        if report.inode_evicted > 0 {
                            tracing::warn!(
                                target: "mde_bus::retention",
                                inode_evicted = report.inode_evicted,
                                inode_pressure = report.inode_pressure,
                                "BUS-RUN-INODE-1: spool file-count cap reached — evicted oldest files to keep /run off inode-exhaustion (the 754k-file lighthouse wedge)"
                            );
                        }
                        if report.fs_pressure {
                            tracing::warn!(
                                target: "mde_bus::retention",
                                bytes_after = report.bytes_after,
                                "BUS-RUN-FULL-1-dfguard: filesystem over the fill threshold — lowered the effective hard cap to emergency-prune the spool and hand /run headroom back to the OS"
                            );
                        }
                        if report.quota.soft_exceeded && !last_soft_warn {
                            // Publish a warning to bus/sys/quota.
                            // Best-effort — failure to publish is
                            // logged but doesn't break the loop.
                            if let Err(e) = publish_quota_warning(
                                &bus_root,
                                report.bytes_after,
                                policy.quota_soft_bytes,
                                policy.quota_hard_bytes,
                            ) {
                                tracing::warn!(
                                    target: "mde_bus::retention",
                                    error = %e,
                                    "failed to publish quota warning"
                                );
                            }
                            last_soft_warn = true;
                        } else if !report.quota.soft_exceeded {
                            last_soft_warn = false;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "mde_bus::retention",
                            error = %e,
                            "retention pass failed"
                        );
                    }
                }
            }
        }
    }
}

/// Publish a `bus/sys/quota` warning into the per-peer Persist
/// + on-disk tree. Pure sync helper — the retention loop calls
/// this from inside the tokio task.
fn publish_quota_warning(
    bus_root: &Path,
    bytes_used: u64,
    soft_limit: u64,
    hard_limit: u64,
) -> Result<(), RetentionError> {
    let p = Persist::open(bus_root.to_path_buf())
        .map_err(|e| RetentionError::Io(format!("open persist: {e}")))?;
    let title = format!(
        "Bus storage at {} MB / {} MB soft limit",
        bytes_used / 1024 / 1024,
        soft_limit / 1024 / 1024
    );
    let body = format!(
        "Mackes Bus on-disk store has exceeded its soft quota.\n\
         Used: {} MB\n\
         Soft limit: {} MB\n\
         Hard limit: {} MB\n\n\
         Retention is running but messages outlive their TTL when \
         the quota is over. Consider lowering per-topic TTL or \
         retiring noisy topics.",
        bytes_used / 1024 / 1024,
        soft_limit / 1024 / 1024,
        hard_limit / 1024 / 1024,
    );
    p.write(
        "bus/sys/quota",
        crate::hooks::config::Priority::Default,
        Some(&title),
        Some(&body),
    )
    .map_err(|e| RetentionError::Io(format!("publish quota warning: {e}")))?;
    Ok(())
}

/// BUS-RETENTION-2 — publish a `/run`-low warning to the **`mackesd::alert`**
/// lane so it surfaces in the Notification Hub (unlike `bus/sys/quota`, which is
/// not an alert lane). The bus spool lives on a small `/run` tmpfs; a near-full
/// `/run` breaks runtime locks (dnf, the bus's own WAL) — the failure class that
/// blocked the v10.0.18 fleet roll. The caller (mackesd, which can `df`) detects
/// the headroom and calls this on the transition into the low state.
/// `Priority::High` → the Hub renders it as a Warning.
///
/// # Errors
/// [`RetentionError::Io`] if the Persist open or write fails.
pub fn publish_run_low_warning(
    bus_root: &Path,
    avail_mb: u64,
    total_mb: u64,
) -> Result<(), RetentionError> {
    let p = Persist::open(bus_root.to_path_buf())
        .map_err(|e| RetentionError::Io(format!("open persist: {e}")))?;
    let pct = if total_mb > 0 {
        avail_mb * 100 / total_mb
    } else {
        0
    };
    let title = format!("Low /run space — {avail_mb} MB free ({pct}%)");
    let body = format!(
        "The filesystem backing the message bus ({}) is nearly full: \
         {avail_mb} MB free of {total_mb} MB ({pct}%).\n\n\
         A full /run breaks runtime locks (dnf, the bus index WAL) and can wedge \
         the node. The bus retention GC is shedding to recover; if this persists, \
         free /run space or check for a runaway writer.",
        bus_root.display(),
    );
    p.write(
        "mackesd::alert",
        crate::hooks::config::Priority::High,
        Some(&title),
        Some(&body),
    )
    .map_err(|e| RetentionError::Io(format!("publish run-low warning: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::Priority;

    fn open_tmp_with(messages: &[(&str, Priority, i64)]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let bus_root = tmp.path().to_path_buf();
        let p = Persist::open(bus_root.clone()).unwrap();
        // Insert with controlled ts_unix_ms by writing then
        // back-dating via direct SQLite UPDATE — keeps the test
        // hermetic against wall-clock drift.
        for (topic, prio, ts) in messages {
            let m = p.write(topic, *prio, None, Some("body")).unwrap();
            let conn = Connection::open(bus_root.join("index.sqlite")).unwrap();
            conn.execute(
                "UPDATE messages SET ts_unix_ms = ?1 WHERE ulid = ?2",
                params![ts, m.ulid],
            )
            .unwrap();
        }
        // EPIC-BUS-EXT-AUDIT-BUS: each write() above also emitted an
        // audit/<peer> record. Purge them (row + files) so the
        // retention fixture contains exactly the seeded messages —
        // retention behavior is what's under test here, not the audit
        // doubling (audit/* is retention-exempt anyway, exercised in
        // its own test below).
        let conn = Connection::open(bus_root.join("index.sqlite")).unwrap();
        conn.execute("DELETE FROM messages WHERE topic LIKE 'audit/%'", [])
            .unwrap();
        let _ = std::fs::remove_dir_all(bus_root.join("audit"));
        (tmp, bus_root)
    }

    #[test]
    fn ttl_secs_returns_per_priority_defaults() {
        let p = RetentionPolicy::default();
        assert_eq!(p.ttl_secs("min"), Some(DEFAULT_TTL_MIN_SECS));
        assert_eq!(p.ttl_secs("default"), Some(DEFAULT_TTL_DEFAULT_SECS));
        assert_eq!(p.ttl_secs("high"), Some(DEFAULT_TTL_HIGH_SECS));
        assert_eq!(p.ttl_secs("urgent"), None);
        // Unknown priorities fall back to `default`.
        assert_eq!(p.ttl_secs("garbage"), Some(DEFAULT_TTL_DEFAULT_SECS));
    }

    #[test]
    fn ttl_cutoff_subtracts_correctly() {
        let p = RetentionPolicy::default();
        // 1_000_000_000_000 ms = 2001-09-09T01:46:40Z
        let now = 1_000_000_000_000_i64;
        let cutoff_default = ttl_cutoff_unix_ms(&p, "default", now).unwrap();
        // default TTL = 7 days = 604800 seconds = 604_800_000 ms
        assert_eq!(now - cutoff_default, 604_800_000);
    }

    #[test]
    fn urgent_messages_never_expire() {
        let now = 1_000_000_000_000_i64;
        // 20 years ago — still urgent, still kept.
        let twenty_years = 20_i64 * 365 * 24 * 60 * 60 * 1000;
        let ancient = now - twenty_years;
        let (_tmp, root) = open_tmp_with(&[("t", Priority::Urgent, ancient)]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 0);
    }

    #[test]
    fn min_messages_expire_after_24h() {
        let now = 1_000_000_000_000_i64;
        let day_ago_plus = now - (25_i64 * 60 * 60 * 1000); // 25h ago
        let recent = now - (1_i64 * 60 * 60 * 1000); // 1h ago
        let (_tmp, root) = open_tmp_with(&[
            ("t/old", Priority::Min, day_ago_plus),
            ("t/new", Priority::Min, recent),
        ]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 1);
        // The recent message is still indexed.
        let p = Persist::open(root.clone()).unwrap();
        assert_eq!(p.count().unwrap(), 1);
    }

    #[test]
    fn audit_topics_not_ttl_reaped_but_bounded_by_cap() {
        // AUDIT-RUN-CAP-1 — audit/* is NOT governed by the per-priority TTL
        // (so an old-but-within-cap audit window survives a reap), but it is no
        // longer retention=forever/exempt: it is BOUNDED by the per-topic cap
        // (asserted in `audit_lane_pruned_to_cap_keeps_most_recent` below). Here
        // we prove the TTL reap alone still leaves a small audit window intact
        // while reaping the same-age non-audit message — with a cap far above
        // the tiny audit footprint so the cap doesn't fire.
        let now = 1_000_000_000_000_i64;
        let ten_days = now - (10 * 24 * 60 * 60 * 1000);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let p = Persist::open(root.clone()).unwrap();
        // audit/peerx is cycle-guarded (no further audit); the regular
        // write also emits an audit/<peer> record.
        p.write("audit/peerx", Priority::Min, None, Some("{}"))
            .unwrap();
        p.write("t/regular", Priority::Min, None, Some("body"))
            .unwrap();
        // Back-date everything to 10 days ago — well past the 24h min TTL.
        let conn = Connection::open(root.join("index.sqlite")).unwrap();
        conn.execute("UPDATE messages SET ts_unix_ms = ?1", params![ten_days])
            .unwrap();
        drop(conn);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        // Only the non-audit message is TTL-reaped; the tiny audit window is
        // under the 8 MiB cap so the audit prune doesn't fire either.
        assert_eq!(report.removed, 1, "only the non-audit message reaped");
        assert_eq!(report.audit_pruned, 0, "tiny audit window is under the cap");
        let p = Persist::open(root.clone()).unwrap();
        assert!(
            !p.list_since("audit/peerx", None).unwrap().is_empty(),
            "audit/peerx within the cap survives the TTL reap"
        );
    }

    #[test]
    fn audit_lane_pruned_to_cap_keeps_most_recent() {
        // AUDIT-RUN-CAP-1 — the decisive regression for the lh1/Eagle 2026-06-24
        // wedge: an over-cap audit lane MUST be pruned (it is bounded, NOT
        // exempt), and the MOST-RECENT window must be what survives.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        // 8 audit records, 256 KB each = 2 MB of audit, ascending ts so the
        // first is the oldest. Cap at 1 MB → only the newest ~4 survive.
        let kb = 1024;
        let mut ulids = Vec::new();
        for i in 0..8 {
            ulids.push(write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now - (8 - i) * 1000,
                256 * kb,
            ));
        }
        let policy = RetentionPolicy {
            audit_cap_bytes: 1024 * 1024, // 1 MB cap over a ~2 MB audit lane
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert!(
            report.audit_pruned >= 1,
            "over-cap audit lane must be pruned, not exempt"
        );
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("audit/peerx", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        // Oldest pruned, newest kept (most-recent window stays on /run).
        assert!(
            !remaining.contains(&ulids[0]),
            "oldest audit record pruned over the cap"
        );
        assert!(
            remaining.contains(ulids.last().unwrap()),
            "most-recent audit record survives"
        );
        // The kept audit lane is at/under the cap (within one record's slack).
        let p = Persist::open(root.clone()).unwrap();
        let audit_bytes: u64 = p
            .list_since("audit/peerx", None)
            .unwrap()
            .iter()
            .map(|m| {
                std::fs::metadata(root.join(&m.file_path))
                    .map(|x| x.len())
                    .unwrap_or(0)
            })
            .sum();
        assert!(
            audit_bytes <= policy.audit_cap_bytes + 256 * kb as u64,
            "audit lane bounded to ~cap, was {audit_bytes}"
        );
    }

    #[test]
    fn audit_lane_under_cap_is_untouched() {
        // A small audit lane (well under the cap) is never pruned — the
        // most-recent window stays fully resident.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        for i in 0..3 {
            write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now - i * 1000,
                4 * 1024,
            );
        }
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.audit_pruned, 0, "under-cap audit lane untouched");
        let p = Persist::open(root.clone()).unwrap();
        assert_eq!(
            p.list_since("audit/peerx", None).unwrap().len(),
            3,
            "all three audit records survive under the cap"
        );
    }

    #[test]
    fn audit_entry_count_backstop_bounds_a_zero_byte_lane() {
        // AUDIT-RUN-CAP-1 — the entry-count backstop. If the byte cap is huge
        // (or the spool files read as 0 bytes), the lane must STILL be bounded by
        // record count so index rows can't grow unbounded — the metadata-zero
        // stall that would re-open the v10.0.18 unbounded-index wedge. Here the
        // byte cap is effectively infinite, so ONLY the entry cap can fire.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        for i in 0..10 {
            write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now - (10 - i) * 1000,
                64,
            );
        }
        let policy = RetentionPolicy {
            audit_cap_bytes: u64::MAX, // byte cap can never bind
            audit_cap_entries: 4,      // keep only the 4 most-recent records
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert_eq!(report.audit_pruned, 6, "entry cap sheds the 6 oldest");
        let p = Persist::open(root.clone()).unwrap();
        assert_eq!(
            p.list_since("audit/peerx", None).unwrap().len(),
            4,
            "entry-count backstop bounds the lane regardless of bytes"
        );
    }

    #[test]
    fn topic_lane_pruned_to_cap_keeps_most_recent() {
        // BROKER-RESILIENCE-1 — the decisive regression for the down-broker
        // spool wedge: a non-audit topic over its per-topic cap MUST be pruned
        // to its most-recent window. Models a single alert source (here
        // `state/boot-readiness`) publishing persist-only at its 2 s cadence
        // with NO broker draining it — the live 25 MB-on-a-190 MB-/run case.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        let kb = 1024;
        // 8 records, 256 KB each = 2 MB on one topic, ascending ts (first oldest).
        let mut ulids = Vec::new();
        for i in 0..8 {
            ulids.push(write_sized(
                &root,
                "state/boot-readiness",
                // Use High so the TTL reap (30 d) never fires — the per-topic
                // cap must be the ONLY thing that sheds these.
                Priority::High,
                now - (8 - i) * 1000,
                256 * kb,
            ));
        }
        purge_audit(&root); // isolate the per-topic (non-audit) cap behavior
        let policy = RetentionPolicy {
            topic_cap_bytes: 1024 * 1024, // 1 MB cap over the ~2 MB topic
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert_eq!(report.removed, 0, "no TTL expiry — High survives 30 d");
        assert!(
            report.topic_pruned >= 1,
            "over-cap topic must be pruned to its most-recent window"
        );
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("state/boot-readiness", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(
            !remaining.contains(&ulids[0]),
            "oldest record pruned over the cap"
        );
        assert!(
            remaining.contains(ulids.last().unwrap()),
            "most-recent record survives"
        );
        let topic_bytes: u64 = p
            .list_since("state/boot-readiness", None)
            .unwrap()
            .iter()
            .map(|m| {
                std::fs::metadata(root.join(&m.file_path))
                    .map(|x| x.len())
                    .unwrap_or(0)
            })
            .sum();
        assert!(
            topic_bytes <= policy.topic_cap_bytes + 256 * kb as u64,
            "topic bounded to ~cap, was {topic_bytes}"
        );
    }

    #[test]
    fn topic_cap_does_not_touch_audit_lane() {
        // BROKER-RESILIENCE-1 — the per-topic cap excludes `audit/*` (it has its
        // own independent cap), so an audit lane under the audit cap is NEVER
        // pruned by the topic pass even when the topic cap is tiny.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        for i in 0..6 {
            write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now - (6 - i) * 1000,
                4 * 1024,
            );
        }
        let policy = RetentionPolicy {
            topic_cap_bytes: 0, // would shed everything IF it touched audit
            topic_cap_entries: 0,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert_eq!(
            report.topic_pruned, 0,
            "per-topic cap must not touch the audit lane"
        );
        let p = Persist::open(root.clone()).unwrap();
        assert_eq!(
            p.list_since("audit/peerx", None).unwrap().len(),
            6,
            "audit lane untouched by the per-topic cap (under its own audit cap)"
        );
    }

    #[test]
    fn sustained_publish_with_no_broker_stays_bounded() {
        // BROKER-RESILIENCE-1 — the end-to-end property: under sustained
        // publishes to a topic with NO broker (persist-only), repeated retention
        // passes keep the topic's `/run` footprint bounded to ~the cap, never
        // growing unbounded. We publish in waves and run a pass after each.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let kb = 1024;
        let cap = 512 * kb as u64;
        let policy = RetentionPolicy {
            topic_cap_bytes: cap,
            ..RetentionPolicy::default()
        };
        let mut ts = 1_000_000_000_000_i64;
        for _wave in 0..5 {
            // Each wave: 10 × 128 KB = 1.25 MB onto one topic (well over the cap).
            for _ in 0..10 {
                ts += 1000;
                write_sized(&root, "compute/event", Priority::High, ts, 128 * kb);
            }
            purge_audit(&root);
            let report = run_pass_at(&policy, &root, ts).unwrap();
            let _ = report;
            // After every pass the topic's spool footprint is bounded to ~cap.
            let p = Persist::open(root.clone()).unwrap();
            let topic_bytes: u64 = p
                .list_since("compute/event", None)
                .unwrap()
                .iter()
                .map(|m| {
                    std::fs::metadata(root.join(&m.file_path))
                        .map(|x| x.len())
                        .unwrap_or(0)
                })
                .sum();
            assert!(
                topic_bytes <= cap + 128 * kb as u64,
                "sustained no-broker publishes must stay bounded (~cap); was {topic_bytes}"
            );
        }
    }

    #[test]
    fn audit_zero_cap_floors_at_one_record_not_zero() {
        // AUDIT-RUN-CAP-1 — a degenerate cap (0 bytes / 0 entries) must NOT wipe
        // the audit trail; the lane floors at the single most-recent record so
        // the newest audit event is always retained.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        let mut ulids = Vec::new();
        for i in 0..5 {
            ulids.push(write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now - (5 - i) * 1000,
                128,
            ));
        }
        let policy = RetentionPolicy {
            audit_cap_bytes: 0,
            audit_cap_entries: 0,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert_eq!(report.audit_pruned, 4, "all but the newest pruned");
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("audit/peerx", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert_eq!(
            remaining.len(),
            1,
            "exactly the newest record survives a 0 cap"
        );
        assert!(
            remaining.contains(ulids.last().unwrap()),
            "the surviving record is the most-recent one"
        );
    }

    #[test]
    fn audit_prune_is_deterministic_under_same_ms_burst() {
        // AUDIT-RUN-CAP-1 — the lh1 pattern: a burst of audit records sharing the
        // SAME ts_unix_ms. The `ulid DESC` secondary sort must make the kept
        // window deterministic — the lexicographically-greatest (newest) ULIDs
        // survive — not whatever order SQLite happens to return for the tie.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        // All 8 share the SAME timestamp; ULIDs are monotonic so write order ==
        // ulid order. 256 KB each → ~2 MB; cap 1 MB keeps the newest few.
        let mut ulids = Vec::new();
        for _ in 0..8 {
            ulids.push(write_sized(
                &root,
                "audit/peerx",
                Priority::Min,
                now,
                256 * 1024,
            ));
        }
        let policy = RetentionPolicy {
            audit_cap_bytes: 1024 * 1024,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert!(
            report.audit_pruned >= 1,
            "over-cap lane pruned even under a ts tie"
        );
        let p = Persist::open(root.clone()).unwrap();
        let remaining: std::collections::HashSet<String> = p
            .list_since("audit/peerx", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        // The newest (greatest ULID) is kept; the oldest (least ULID) is pruned —
        // deterministically, despite every record sharing ts_unix_ms.
        assert!(
            remaining.contains(ulids.last().unwrap()),
            "newest ULID survives the tie"
        );
        assert!(
            !remaining.contains(&ulids[0]),
            "oldest ULID pruned under the tie"
        );
    }

    #[test]
    fn ephemeral_rpc_topics_reap_after_one_hour() {
        // EFF-47 — reply/* + action/* reap on the 1h ephemeral TTL
        // while a same-age default-priority normal topic survives its
        // 7-day class TTL.
        let now = 1_000_000_000_000_i64;
        let two_hours_ago = now - (2_i64 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[
            ("reply/01OLDULID", Priority::Default, two_hours_ago),
            (
                "action/fleet/list-revisions",
                Priority::Default,
                two_hours_ago,
            ),
            ("t/normal", Priority::Default, two_hours_ago),
        ]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 2, "both RPC topics reaped, normal survives");
        let p = Persist::open(root.clone()).unwrap();
        assert!(p.list_since("reply/01OLDULID", None).unwrap().is_empty());
        assert!(p
            .list_since("action/fleet/list-revisions", None)
            .unwrap()
            .is_empty());
        assert!(!p.list_since("t/normal", None).unwrap().is_empty());
    }

    #[test]
    fn fresh_rpc_topics_survive_the_ephemeral_ttl() {
        // An in-flight RPC (30 min old) must NOT be reaped.
        let now = 1_000_000_000_000_i64;
        let half_hour_ago = now - (30_i64 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[("reply/01FRESH", Priority::Default, half_hour_ago)]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 0);
    }

    #[test]
    fn ephemeral_pass_dedupes_against_priority_pass() {
        // A reply/* row older than BOTH its priority TTL and the
        // ephemeral TTL is removed exactly once.
        let now = 1_000_000_000_000_i64;
        let ten_days_ago = now - (10_i64 * 24 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[("reply/01ANCIENT", Priority::Min, ten_days_ago)]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(
            report.removed, 1,
            "single removal despite matching both passes"
        );
    }

    #[test]
    fn default_messages_expire_after_7d() {
        let now = 1_000_000_000_000_i64;
        let eight_days = now - (8_i64 * 24 * 60 * 60 * 1000);
        let three_days = now - (3_i64 * 24 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[
            ("t/old", Priority::Default, eight_days),
            ("t/new", Priority::Default, three_days),
        ]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn high_messages_expire_after_30d() {
        let now = 1_000_000_000_000_i64;
        let thirty_one = now - (31_i64 * 24 * 60 * 60 * 1000);
        let ten = now - (10_i64 * 24 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[
            ("t/old", Priority::High, thirty_one),
            ("t/new", Priority::High, ten),
        ]);
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn pass_removes_both_file_and_row() {
        let now = 1_000_000_000_000_i64;
        let two_days = now - (2 * 24 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[("t/x", Priority::Min, two_days)]);
        // Sanity: file is on disk.
        let p = Persist::open(root.clone()).unwrap();
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 1);
        // File AND row gone.
        assert_eq!(p.count().unwrap(), 0);
        assert!(p.detect_divergence().unwrap().is_clean());
    }

    #[test]
    fn disk_usage_sums_json_files() {
        let (_tmp, root) = open_tmp_with(&[("t/x", Priority::Min, 1)]);
        let bytes = disk_usage_bytes(&root).unwrap();
        // The one message's JSON is at least a few bytes long.
        assert!(bytes > 50, "expected non-trivial bytes, got {bytes}");
    }

    #[test]
    fn quota_soft_breach_surfaces_in_report() {
        // Soft breach that is NOT a hard breach: advisory only, no eviction
        // (the hard-cap valve fires only above the hard quota — BULLETPROOF-1).
        let policy = RetentionPolicy {
            quota_soft_bytes: 1,                // 1 byte — guaranteed exceeded
            quota_hard_bytes: 10 * 1024 * 1024, // 10 MB — not reached by one small msg
            ..RetentionPolicy::default()
        };
        let (_tmp, root) = open_tmp_with(&[("t/x", Priority::Urgent, 1)]);
        let report = run_pass_at(&policy, &root, 100).unwrap();
        assert!(report.quota.soft_exceeded, "soft quota surfaces");
        assert!(!report.quota.hard_exceeded, "below hard quota");
        assert_eq!(report.evicted, 0, "no eviction below the hard cap");
    }

    #[test]
    fn unknown_priority_falls_back_to_default_ttl() {
        let now = 1_000_000_000_000_i64;
        let eight_days = now - (8 * 24 * 60 * 60 * 1000);
        let (_tmp, root) = open_tmp_with(&[("t/x", Priority::Default, eight_days)]);
        // Hack: write a row with an unknown priority string by
        // bypassing the typed write path.
        let conn = Connection::open(root.join("index.sqlite")).unwrap();
        conn.execute("UPDATE messages SET priority = 'wat'", [])
            .unwrap();
        // The run_pass only walks min/default/high — 'wat' isn't
        // any of those, so it survives. This documents the
        // safety semantics: unknown priorities are immortal
        // until the operator backfills them.
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 0);
    }

    // BULLETPROOF-1 — hard-cap eviction safety valve.

    /// Seed `topic` with a `body_len`-byte body at `ts`, returning its ULID.
    fn write_sized(root: &Path, topic: &str, prio: Priority, ts: i64, body_len: usize) -> String {
        let p = Persist::open(root.to_path_buf()).unwrap();
        let body = "x".repeat(body_len);
        let m = p.write(topic, prio, None, Some(&body)).unwrap();
        let conn = Connection::open(root.join("index.sqlite")).unwrap();
        conn.execute(
            "UPDATE messages SET ts_unix_ms = ?1 WHERE ulid = ?2",
            params![ts, m.ulid],
        )
        .unwrap();
        m.ulid
    }

    fn purge_audit(root: &Path) {
        let conn = Connection::open(root.join("index.sqlite")).unwrap();
        conn.execute("DELETE FROM messages WHERE topic LIKE 'audit/%'", [])
            .unwrap();
        let _ = std::fs::remove_dir_all(root.join("audit"));
    }

    #[test]
    fn hard_cap_evicts_oldest_first_until_under_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Six 1 MB messages (6 MB) on a 3 MB-hard / 2 MB-soft policy.
        let mb = 1024 * 1024;
        let now = 1_000_000_000_000_i64;
        let mut ulids = Vec::new();
        for i in 0..6 {
            // Recent, ascending ts so High's 30d TTL never reaps them — the
            // hard-cap valve is the only thing that should fire.
            ulids.push(write_sized(
                &root,
                "t/bulk",
                Priority::High,
                now - (6 - i) * 1000,
                mb,
            ));
        }
        purge_audit(&root);
        // BUS-RETENTION-1 — the footprint counts the index DB too (it duplicates
        // bodies, ~1 MB/msg here), so the total ≈ 2× the spool. Caps are sized so
        // a few oldest messages are shed while the newest survives + the result
        // lands under the hard cap (soft=2 MB would round to "shed everything"
        // once the DB is counted).
        let policy = RetentionPolicy {
            quota_soft_bytes: 5 * mb as u64,
            quota_hard_bytes: 6 * mb as u64,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert_eq!(report.removed, 0, "no TTL expiry — High survives 30d");
        assert!(report.evicted >= 1, "hard cap must shed oldest");
        assert!(
            report.bytes_after <= policy.quota_hard_bytes,
            "post-eviction must be under the hard cap, was {}",
            report.bytes_after
        );
        // The newest message (highest ts) must survive; the oldest must be gone.
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("t/bulk", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(remaining.contains(ulids.last().unwrap()), "newest survives");
        assert!(!remaining.contains(&ulids[0]), "oldest evicted");
    }

    #[test]
    fn hard_cap_sheds_non_audit_before_audit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mb = 1024 * 1024;
        let now = 1_000_000_000_000_i64;
        // 4 non-audit (1 MB each) + 2 audit (1 MB each), all recent so nothing
        // TTL-reaps. Eviction is phase-ordered (non-audit first) regardless of
        // ts, so shedding the non-audit lane alone gets us under the cap.
        let mut non_audit = Vec::new();
        for i in 0..4 {
            non_audit.push(write_sized(
                &root,
                "t/data",
                Priority::High,
                now - (6 - i) * 1000,
                mb,
            ));
        }
        let audit_a = write_sized(&root, "audit/peerx", Priority::Min, now - 2000, mb);
        let audit_b = write_sized(&root, "audit/peerx", Priority::Min, now - 1000, mb);
        // BUS-RETENTION-1 — the footprint now also counts the index DB (which
        // duplicates bodies, ~1 MB/msg here), so the synthetic total ≈ 2× the
        // spool. Caps are sized so shedding the 4 non-audit messages alone brings
        // the total under the soft cap, leaving both audit records intact.
        let policy = RetentionPolicy {
            quota_soft_bytes: 5 * mb as u64,
            quota_hard_bytes: 6 * mb as u64,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at(&policy, &root, now).unwrap();
        assert!(report.evicted >= 1);
        // Audit is shed last: both audit records must survive because evicting
        // the non-audit lane alone gets us under the cap.
        let p = Persist::open(root.clone()).unwrap();
        let audit_left: Vec<String> = p
            .list_since("audit/peerx", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(
            audit_left.contains(&audit_a) && audit_left.contains(&audit_b),
            "audit/* must be preserved when non-audit eviction suffices"
        );
        assert!(
            p.list_since("t/data", None).unwrap().len() < non_audit.len(),
            "non-audit lane shed first"
        );
    }

    #[test]
    fn under_hard_cap_evicts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_sized(&root, "t/small", Priority::High, 1, 1024);
        purge_audit(&root);
        // Default tmpfs-safe policy (96/144 MB) is far above a 1 KB spool.
        let report = run_pass_at(&RetentionPolicy::default(), &root, 1_000_000_000_000).unwrap();
        assert_eq!(report.evicted, 0);
    }

    // BUS-RETENTION-1 — the index DB must count toward the footprint + shrink.

    #[test]
    fn footprint_includes_index_db() {
        // A message with a sizable body bloats the index (it duplicates
        // title+body), so `index_db_bytes` is non-trivial and `run_pass_at`'s
        // `bytes_after` must exceed the spool-only `disk_usage_bytes`.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_sized(&root, "t/keep", Priority::Urgent, 1, 64 * 1024);
        purge_audit(&root);
        let db = index_db_bytes(&root);
        assert!(
            db > 0,
            "index DB must contribute to the footprint, got {db}"
        );
        let report = run_pass_at(&RetentionPolicy::default(), &root, 100).unwrap();
        let spool = disk_usage_bytes(&root).unwrap();
        assert!(
            report.bytes_after >= spool + index_db_bytes(&root),
            "bytes_after ({}) must include the index DB on top of the spool ({spool})",
            report.bytes_after,
        );
        assert!(report.bytes_after > spool, "DB bytes are counted");
    }

    #[test]
    fn pass_reclaims_index_db_after_bulk_delete() {
        // Born INCREMENTAL (the migration sets auto_vacuum) so a pass that
        // deletes expired rows returns their pages to the OS — the index file
        // must SHRINK, not just free pages internally (the v10.0.18 failure:
        // a 121 MB index that never gave space back filled /run).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        let long_ago = now - (10 * 24 * 60 * 60 * 1000); // 10 days — past min/default TTL
                                                         // 40 expired min-priority messages with 32 KB bodies each → a fat index.
        for i in 0..40 {
            write_sized(
                &root,
                &format!("t/old{i}"),
                Priority::Min,
                long_ago,
                32 * 1024,
            );
        }
        purge_audit(&root);
        let db_before = index_db_bytes(&root);
        assert!(
            db_before > 256 * 1024,
            "index should be fat pre-reap, got {db_before}"
        );
        let report = run_pass_at(&RetentionPolicy::default(), &root, now).unwrap();
        assert_eq!(report.removed, 40, "all 40 expired messages reaped");
        let db_after = index_db_bytes(&root);
        assert!(
            db_after < db_before / 2,
            "index DB must shrink after the reap: before={db_before} after={db_after}",
        );
    }

    #[test]
    fn run_low_warning_lands_on_the_hub_alert_lane() {
        // BUS-RETENTION-2 — the /run-low warning must publish to `mackesd::alert`
        // (a Hub alert lane) so the operator actually sees it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        publish_run_low_warning(&root, 24, 190).unwrap();
        let p = Persist::open(root.clone()).unwrap();
        let msgs = p.list_since("mackesd::alert", None).unwrap();
        assert_eq!(msgs.len(), 1, "exactly one run-low alert published");
        let m = &msgs[0];
        assert!(
            m.title.as_deref().unwrap_or("").contains("Low /run"),
            "title names the condition"
        );
        // High priority → the Hub classifies it as a Warning.
        assert_eq!(m.priority, "high");
    }

    #[test]
    fn default_quota_is_tmpfs_safe() {
        // Regression: the old 500 MB/2 GB defaults exceeded a ~190 MB
        // lighthouse /run, so the cap could never fire before ENOSPC.
        assert!(DEFAULT_QUOTA_HARD_BYTES < 190 * 1024 * 1024);
        assert!(DEFAULT_QUOTA_SOFT_BYTES < DEFAULT_QUOTA_HARD_BYTES);
    }

    #[test]
    fn soak_steady_state_footprint_stays_flat_under_sustained_traffic() {
        // BUS-RETENTION-1 acceptance ("a soak test shows steady-state size flat
        // under sustained traffic"): under repeated publish bursts that each
        // exceed the cap, the post-GC footprint (spool + index.sqlite) must reach
        // a FLAT plateau — it must NOT ratchet upward as cumulative writes grow.
        // That ratchet was the live v10.0.18 failure: a 121 MB index that never
        // returned freed pages to the OS filled a 391 MB /run. The decisive check
        // is plateau, not an exact cap: with working reclaim the footprint at a
        // late round ≈ a mid round; if the DB never reclaimed, the footprint would
        // grow roughly with total writes (late round ≈ 2× the mid round here), so
        // the < 1.5× band below fails closed on a reclaim regression.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // Caps with headroom over SQLite's working-set floor so the run is about
        // steady-state behavior, not the evictor's exact landing point (that is
        // covered by `hard_cap_evicts_oldest_first_until_under_soft`).
        let policy = RetentionPolicy {
            quota_soft_bytes: 256 * 1024,
            quota_hard_bytes: 384 * 1024,
            ..RetentionPolicy::default()
        };

        let now = 1_000_000_000_000_i64;
        let rounds = 12_i64;
        let mut footprints = Vec::new();
        for round in 0..rounds {
            // A burst of sized Urgent messages each round. Urgent never
            // TTL-expires, so ONLY the hard-cap evictor + DB reclaim bound the
            // footprint — exactly the steady-state mechanism under test. `ts =
            // now + round` makes earlier rounds the oldest, so eviction sheds them
            // first (oldest-traffic-out, like a real spool under load).
            for i in 0..60 {
                write_sized(
                    &root,
                    &format!("soak/r{round}/m{i}"),
                    Priority::Urgent,
                    now + round,
                    2 * 1024,
                );
            }
            purge_audit(&root);
            let report = run_pass_at(&policy, &root, now + round).unwrap();
            assert!(report.bytes_after > 0, "round {round}: empty footprint");
            footprints.push(report.bytes_after);
        }

        // By the mid rounds the cumulative writes far exceed the cap, so eviction
        // + reclaim are fully engaged and the footprint has plateaued. Compare a
        // mid round to the last: a flat steady state stays within a tight band; a
        // reclaim regression would have crept upward with total writes.
        let mid = footprints[(rounds / 2) as usize];
        let last = *footprints.last().unwrap();
        assert!(
            last <= mid + mid / 2,
            "footprint ratcheted upward across the soak (DB reclaim regressed): \
             mid={mid} last={last} series={footprints:?}",
        );
        // And the plateau is in the neighborhood of the cap, not a runaway — this
        // catches "eviction never engaged" (footprint would track total writes).
        assert!(
            last <= policy.quota_hard_bytes * 2,
            "plateau {last} is far above the cap {} — eviction did not bound the bus",
            policy.quota_hard_bytes,
        );
    }

    // BUS-RUN-FULL-1-dfguard — filesystem-pressure guard.

    #[test]
    fn fs_pressure_below_threshold_keeps_configured_cap() {
        // 80% full (< 85% trip) → effective cap == configured, no pressure.
        let total = 100;
        let avail = 20; // used = 80 → 80%
        let (cap, pressure) = fs_pressure_hard_cap(1000, 500, total, avail);
        assert_eq!(cap, 1000, "below threshold: cap unchanged");
        assert!(!pressure, "below threshold: no pressure");
    }

    #[test]
    fn fs_pressure_at_threshold_lowers_cap_below_footprint() {
        // Exactly 85% full → pressure trips. With used == target (85%) the
        // reclaim is 0, so the cap equals the footprint (clamped to configured):
        // any growth from here evicts.
        let total = 100;
        let avail = 15; // used = 85 → exactly 85%
        let footprint = 40;
        let (cap, pressure) = fs_pressure_hard_cap(1000, footprint, total, avail);
        assert!(pressure, "at threshold: pressure trips");
        assert_eq!(
            cap, footprint,
            "reclaim 0 at the trip point → cap == footprint"
        );
    }

    #[test]
    fn fs_pressure_over_threshold_reclaims_the_overshoot() {
        // 95% full on a 100-byte fs, 15% over the 85% trip → reclaim = 95 - 85 =
        // 10 bytes. A 50-byte footprint must be capped at 50 - 10 = 40 so the
        // evictor sheds ~10 bytes of spool back to the OS.
        let total = 100;
        let avail = 5; // used = 95
        let (cap, pressure) = fs_pressure_hard_cap(1000, 50, total, avail);
        assert!(pressure);
        assert_eq!(cap, 40, "cap = footprint - (used - 85% target)");
    }

    #[test]
    fn fs_pressure_clamps_cap_to_configured_and_zero() {
        // Reclaim larger than the footprint drives the cap to 0 (shed all
        // sheddable spool), never negative.
        let total = 100;
        let avail = 0; // 100% full → reclaim = 15
        let (cap, pressure) = fs_pressure_hard_cap(1000, 5, total, avail);
        assert!(pressure);
        assert_eq!(cap, 0, "reclaim > footprint → cap floors at 0");

        // Pressure never RAISES the cap above the configured hard cap: a tiny
        // reclaim on a footprint far above the configured cap still clamps down.
        let (cap2, pressure2) = fs_pressure_hard_cap(30, 1000, 100, 10); // 90% full
        assert!(pressure2);
        assert!(
            cap2 <= 30,
            "effective cap never exceeds configured, got {cap2}"
        );
    }

    #[test]
    fn fs_pressure_unreadable_probe_is_noop() {
        // total == 0 models an unreadable/missing `df` → degrade to the plain cap.
        let (cap, pressure) = fs_pressure_hard_cap(1000, 999, 0, 0);
        assert_eq!(cap, 1000);
        assert!(!pressure);
    }

    // BUS-RUN-INODE-1 — the global spool file-count (inode) guard. These are the
    // decisive regressions for the ~754k-file / ~390 MB `/run` wedge on both
    // lighthouses (12.1.0): the byte hard cap never fired (bytes were low) but the
    // fixed tmpfs inode budget was exhausted, wedging mackesd's namespace setup.

    #[test]
    fn inode_pressure_below_threshold_keeps_configured_cap() {
        // 50% of inodes used → no inode pressure → configured cap unchanged.
        let itotal = 1000;
        let iavail = 500;
        let (cap, pressure) = inode_pressure_file_cap(1000, 500, itotal, iavail);
        assert_eq!(cap, 1000);
        assert!(!pressure);
    }

    #[test]
    fn inode_pressure_over_threshold_reclaims_the_overshoot() {
        // 95% of inodes used on a 1000-inode fs → trip point is 85% (850 used), so
        // reclaim = 950 - 850 = 100 files. Effective cap = footprint - reclaim.
        let itotal = 1000;
        let iavail = 50; // 95% full
        let (cap, pressure) = inode_pressure_file_cap(10_000, 900, itotal, iavail);
        assert!(pressure, "inode pressure must be flagged");
        assert_eq!(cap, 800, "footprint 900 - reclaim 100 = 800");
    }

    #[test]
    fn inode_pressure_clamps_cap_to_configured_and_zero() {
        // Reclaim larger than the footprint drives the cap to 0, never negative.
        let (cap, pressure) = inode_pressure_file_cap(1000, 5, 100, 0); // 100% full
        assert!(pressure);
        assert_eq!(cap, 0, "reclaim > footprint → cap floors at 0");
        // Pressure never RAISES the cap above the configured file cap.
        let (cap2, pressure2) = inode_pressure_file_cap(30, 1000, 100, 10); // 90% full
        assert!(pressure2);
        assert!(
            cap2 <= 30,
            "effective cap never exceeds configured, got {cap2}"
        );
    }

    #[test]
    fn inode_pressure_unreadable_probe_is_noop() {
        // itotal == 0 models a `df` with no inode reporting → degrade to the cap.
        let (cap, pressure) = inode_pressure_file_cap(1000, 999, 0, 0);
        assert_eq!(cap, 1000);
        assert!(!pressure);
    }

    #[test]
    fn spool_file_count_counts_only_message_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        for _ in 0..5 {
            write_sized(&root, "t/x", Priority::High, 1, 16);
        }
        purge_audit(&root);
        // 5 message files; the index.sqlite{,-wal,-shm} + any `.tmp` are excluded.
        assert_eq!(spool_file_count(&root).unwrap(), 5);
    }

    #[test]
    fn inode_cap_evicts_oldest_first_until_under_target_newest_kept() {
        // THE bound proof: write M > cap message files, run a pass with a small
        // `max_spool_files`, and assert the spool is bounded to ≤ cap files with
        // the NEWEST surviving and the OLDEST evicted. Byte caps are left at their
        // huge defaults so ONLY the inode guard can fire.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        // 12 tiny recent High messages (no TTL reap, trivial bytes) on one topic,
        // ascending ts so index 0 is the oldest.
        let mut ulids = Vec::new();
        for i in 0..12 {
            ulids.push(write_sized(
                &root,
                "t/tick",
                Priority::High,
                now - (12 - i) * 1000,
                16,
            ));
        }
        purge_audit(&root);
        assert_eq!(
            spool_file_count(&root).unwrap(),
            12,
            "12 files before the pass"
        );
        // Cap at 4 files. Target after eviction is 4 - 4/4 = 3 (the hysteresis
        // floor), so the pass sheds the 9 oldest and keeps the 3 newest.
        let policy = RetentionPolicy {
            max_spool_files: 4,
            ..RetentionPolicy::default()
        };
        // `inodes = None` isolates the CONFIGURED file cap from any coincidental
        // inode pressure on the build host's tmp filesystem (hermetic).
        let report = run_pass_at_inner(&policy, &root, now, None, None).unwrap();
        assert_eq!(report.removed, 0, "no TTL expiry — High survives 30d");
        assert_eq!(report.evicted, 0, "bytes are trivial — no byte eviction");
        assert!(report.inode_evicted >= 1, "inode guard must shed files");
        // The bound holds: spool file count is at/under the cap.
        let files_after = spool_file_count(&root).unwrap();
        assert!(
            files_after <= policy.max_spool_files,
            "spool must be bounded to ≤ {} files, was {files_after}",
            policy.max_spool_files
        );
        // Oldest-first: the newest survives, the oldest is gone.
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("t/tick", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(remaining.contains(ulids.last().unwrap()), "newest survives");
        assert!(!remaining.contains(&ulids[0]), "oldest evicted");
    }

    #[test]
    fn inode_cap_sheds_non_audit_before_audit() {
        // Mirror `hard_cap_sheds_non_audit_before_audit` for the inode guard: with
        // a file cap that shedding the non-audit lane alone satisfies, `audit/*`
        // files must survive (audit is shed LAST).
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        let mut non_audit = Vec::new();
        for i in 0..8 {
            non_audit.push(write_sized(
                &root,
                "t/data",
                Priority::High,
                now - (10 - i) * 1000,
                16,
            ));
        }
        // Each t/data write auto-emits an `audit/<host>` record (real wall-clock
        // ts). Purge that auto-audit lane, THEN write the audit/peerx records we
        // assert on — audit/* writes are cycle-guarded so they don't re-audit.
        purge_audit(&root);
        let audit_a = write_sized(&root, "audit/peerx", Priority::Min, now - 2000, 16);
        let audit_b = write_sized(&root, "audit/peerx", Priority::Min, now - 1000, 16);
        // 10 files total (8 t/data + 2 audit/peerx). Cap at 6 → target 6 - 6/4 = 5.
        // Shedding 5 of the 8 non-audit files alone lands at 5, so audit is never
        // reached. `inodes = None` keeps it hermetic vs. host inode pressure.
        let policy = RetentionPolicy {
            max_spool_files: 6,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at_inner(&policy, &root, now, None, None).unwrap();
        assert!(report.inode_evicted >= 1);
        let p = Persist::open(root.clone()).unwrap();
        let audit_left: Vec<String> = p
            .list_since("audit/peerx", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(
            audit_left.contains(&audit_a) && audit_left.contains(&audit_b),
            "audit/* must be preserved when non-audit inode eviction suffices"
        );
        assert!(
            p.list_since("t/data", None).unwrap().len() < non_audit.len(),
            "non-audit lane shed first"
        );
    }

    #[test]
    fn under_inode_cap_evicts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_sized(&root, "t/small", Priority::High, 1, 16);
        purge_audit(&root);
        // Default file cap (50k) is far above a 1-file spool. `inodes = None`
        // keeps it hermetic against any coincidental host inode pressure.
        let report = run_pass_at_inner(
            &RetentionPolicy::default(),
            &root,
            1_000_000_000_000,
            None,
            None,
        )
        .unwrap();
        assert_eq!(report.inode_evicted, 0);
    }

    #[test]
    fn max_spool_files_zero_disables_the_inode_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        for i in 0..10 {
            write_sized(&root, "t/tick", Priority::High, now - (10 - i) * 1000, 16);
        }
        purge_audit(&root);
        // Cap 0 disables the guard → no inode eviction even well "over" any cap.
        // (The `max_spool_files > 0` gate short-circuits before the file walk.)
        let policy = RetentionPolicy {
            max_spool_files: 0,
            ..RetentionPolicy::default()
        };
        let report = run_pass_at_inner(&policy, &root, now, None, None).unwrap();
        assert_eq!(report.inode_evicted, 0, "cap 0 disables the inode guard");
        assert_eq!(spool_file_count(&root).unwrap(), 10, "nothing shed");
    }

    #[test]
    fn inode_dfguard_emergency_sheds_when_fs_inodes_full_though_under_own_cap() {
        // The inode analogue of `dfguard_emergency_prunes_when_fs_full_though_bus_
        // under_cap`: the bus is FAR under its own (default 50k) file cap, but the
        // whole `/run` is 87% inode-full — so the inode dfguard must lower the
        // effective cap below the bus's file count and shed the overshoot anyway.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let now = 1_000_000_000_000_i64;
        let mut ulids = Vec::new();
        for i in 0..10 {
            ulids.push(write_sized(
                &root,
                "t/tick",
                Priority::High,
                now - (10 - i) * 1000,
                16,
            ));
        }
        purge_audit(&root);
        // Default caps → the bus is under both its byte and (50k) file caps.
        let policy = RetentionPolicy::default();
        // Inject a 87%-inode-full filesystem: itotal=100, iavail=13 → used=87,
        // trip point 85 → reclaim=2. footprint=10 files → effective cap=8 → target
        // 8 - 8/4 = 6, so the 4 oldest files are shed, the 6 newest kept.
        let report = run_pass_at_inner(&policy, &root, now, None, Some((100, 13))).unwrap();
        assert!(report.inode_pressure, "inode pressure must be flagged");
        assert!(
            report.inode_evicted >= 1,
            "inode dfguard must shed despite the bus being under its own file cap"
        );
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("t/tick", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(
            !remaining.contains(&ulids[0]),
            "oldest evicted under inode pressure"
        );
        assert!(remaining.contains(ulids.last().unwrap()), "newest survives");
    }

    #[test]
    fn dfguard_emergency_prunes_when_fs_full_though_bus_under_cap() {
        // The decisive end-to-end case: the bus's OWN footprint is comfortably
        // under its configured hard cap, but the shared filesystem is 95% full
        // (a co-tenant filled /run). The dfguard must still evict oldest-first to
        // hand the OS headroom back, rather than leaving the node to wedge.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let mb = 1024 * 1024;
        let now = 1_000_000_000_000_i64;
        let mut ulids = Vec::new();
        for i in 0..6 {
            // Recent High messages → no TTL reap; only the dfguard can shed them.
            ulids.push(write_sized(
                &root,
                "t/bulk",
                Priority::High,
                now - (6 - i) * 1000,
                mb,
            ));
        }
        purge_audit(&root);
        // Caps far above the ~6 MB spool+DB footprint, so the bus-only hard-cap
        // valve would NEVER fire on its own.
        let policy = RetentionPolicy {
            quota_soft_bytes: 500 * mb as u64,
            quota_hard_bytes: 600 * mb as u64,
            ..RetentionPolicy::default()
        };
        // Inject a 95%-full filesystem (10 MB total, 0.5 MB avail). 95% - 85% =
        // 10% of 10 MB = 1 MB to reclaim → at least one 1 MB message is shed.
        let fs = Some((10 * mb as u64, mb as u64 / 2));
        // No inode-pressure injection (`None`) — the default file cap (50k) won't
        // fire on 6 files, so this test isolates the BYTE dfguard.
        let report = run_pass_at_inner(&policy, &root, now, fs, None).unwrap();
        assert!(report.fs_pressure, "fs pressure must be flagged");
        assert!(
            report.evicted >= 1,
            "dfguard must shed despite the bus being under its own cap"
        );
        // Oldest-first: the very first message is gone; the newest survives.
        let p = Persist::open(root.clone()).unwrap();
        let remaining: Vec<String> = p
            .list_since("t/bulk", None)
            .unwrap()
            .into_iter()
            .map(|m| m.ulid)
            .collect();
        assert!(
            !remaining.contains(&ulids[0]),
            "oldest evicted under fs pressure"
        );
        assert!(remaining.contains(ulids.last().unwrap()), "newest survives");
    }

    #[test]
    fn dfguard_no_pressure_leaves_bus_only_behavior_unchanged() {
        // With the filesystem at 50% full, the dfguard is inert: a bus under its
        // own hard cap evicts nothing and `fs_pressure` is false — byte-for-byte
        // the pre-dfguard path.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_sized(&root, "t/small", Priority::High, 1, 1024);
        purge_audit(&root);
        let mb = 1024 * 1024;
        let fs = Some((100 * mb as u64, 50 * mb as u64)); // 50% full
        let report = run_pass_at_inner(
            &RetentionPolicy::default(),
            &root,
            1_000_000_000_000,
            fs,
            None,
        )
        .unwrap();
        assert!(!report.fs_pressure);
        assert_eq!(report.evicted, 0);
        assert_eq!(report.inode_evicted, 0);
    }

    #[test]
    fn run_pass_at_reaches_the_real_df_probe_without_panicking() {
        // The wired entrypoint `run_pass_at` (called from mackesd's GC thread)
        // must actually probe the real filesystem via `df`. This proves that read
        // is reachable and its output parses without panicking — and that a tiny
        // spool under the default tmpfs-safe caps does not evict. We deliberately
        // do NOT assert on `fs_pressure`: a build host's tmp could legitimately be
        // over the fill threshold, so that bit is exercised deterministically by
        // the injected-fs tests above; here we only require the probe path runs.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        write_sized(&root, "t/x", Priority::High, 1, 1024);
        purge_audit(&root);
        let report = run_pass_at(&RetentionPolicy::default(), &root, 1_000_000_000_000).unwrap();
        // A 1 KB spool is far under the 144 MB default hard cap; only a genuinely
        // full filesystem could force eviction, which a tempdir-on-a-real-disk is
        // not, so nothing should be shed.
        assert_eq!(
            report.evicted, 0,
            "tiny spool under default caps evicts nothing"
        );
    }

    #[test]
    fn filesystem_probe_parses_a_real_path() {
        // The `df` reader returns a sane (total, avail) for the temp dir's real
        // filesystem: total > 0 and avail <= total. Guards against a column-order
        // or parse regression in the `--output=size,avail` read.
        let tmp = tempfile::tempdir().unwrap();
        if let Some((total, avail)) = filesystem_total_avail_bytes(tmp.path()) {
            assert!(total > 0, "real filesystem reports a non-zero size");
            assert!(
                avail <= total,
                "avail ({avail}) must not exceed total ({total})"
            );
        }
        // `None` (no `df`) is an acceptable degraded outcome — the guard then
        // falls back to the bus-only cap — so we don't fail the test on it.
    }
}
