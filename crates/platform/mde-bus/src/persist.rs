//! BUS-1.4 — message persistence: per-topic JSON file tree +
//! per-peer SQLite index.
//!
//! Per `docs/design/v6.x-mackes-bus.md` §8:
//!
//! - **Authoritative store**: `<bus_root>/<topic-path>/<ulid>.json`.
//!   The full message body lives here. The directory tree is
//!   inotify-friendly + lives on the GFS mesh-home so every peer
//!   sees every message.
//! - **Queryable index**: per-peer `<bus_root>/index.sqlite`.
//!   Stores enough to answer tail / history / retention queries
//!   without walking the file tree. NOT on GFS — SQLite plus
//!   networked FS is a known footgun (lock-stealing,
//!   journal-replay edge cases). Each peer maintains its own
//!   index against the shared file tree.
//!
//! `Persist::write` is the single entry point: it generates a
//! ULID, writes the JSON file atomically (temp + rename), inserts
//! the index row, and returns the [`StoredMessage`] snapshot.
//!
//! `Persist::list_since` answers replay + tail queries — the
//! `(topic, ulid)` SQLite index makes it an index-range scan.
//!
//! `Persist::detect_divergence` is the safety net for the
//! "index says X exists, file tree doesn't (or vice-versa)" case
//! — typically caused by an external process dropping a file
//! into the tree without going through `write`, or by a crash
//! between file-write and index-insert.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use crate::hooks::config::Priority;

/// SQL schema applied on `open` — embedded so the binary doesn't
/// need a separate file at runtime.
const SCHEMA: &str = include_str!("../migrations/0001_init.sql");

/// Process-global MONOTONIC ULID generator for message ids. The `(topic,
/// ulid)` cursor scan (`list_since`, `latest_ulid`, the music daemon's
/// `seed_cursors_at_tail`) relies on ULID order reflecting WRITE order, but
/// `Ulid::new()` is timestamp + random — two writes in the same millisecond
/// can invert, so a consumer could skip or reorder same-ms messages. This
/// generator guarantees strictly increasing ULIDs across all topics in the
/// process (same-ms ties increment the random tail; a backwards clock reuses
/// the higher previous timestamp). `Generator::new()` is `const`, so no lazy
/// init is needed.
static ULID_GEN: std::sync::Mutex<ulid::Generator> = std::sync::Mutex::new(ulid::Generator::new());

/// Mint the next monotonic ULID. Falls back to `Ulid::new()` only on the
/// astronomically-unlikely same-millisecond 2^80 overflow.
fn next_ulid() -> Ulid {
    ULID_GEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .generate()
        .unwrap_or_else(|_| Ulid::new())
}

/// BOOT-REC-3 — true only for a definitive SQLite **read-only** failure (the
/// boot-race latch), NOT busy/locked contention. Gates the self-heal recreate
/// so transient concurrency never deletes the live index.
fn is_readonly_err(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ReadOnly
    )
}

/// Default `bus_root` path. Matches BUS-1.7 + BUS-1.6 conventions.
pub const DEFAULT_BUS_ROOT: &str = "~/.local/share/mde/bus";

/// EPIC-BUS-EXT-AUDIT-BUS (Q28) — topic-prefix for the per-peer audit
/// stream. Every publish emits a metadata-only audit record to
/// `audit/<peer>`; messages already under this prefix are NOT
/// re-audited (the cycle guard in [`Persist::write`]).
pub const AUDIT_TOPIC_PREFIX: &str = "audit/";

/// BUS-AUDIT-FLOOD — whether a published topic warrants a metadata audit
/// record (§8: "security events are hash-chain audited"). Audit records are
/// EXEMPT from retention, so auditing must be reserved for control-plane
/// **mutations + events** — never the high-frequency observational classes:
/// `audit/*` (recursion guard), `state/*` status broadcasts, `reply/*`
/// responses, and read-only query verbs (`get-*`, `list-*`, `peer-states`,
/// `*-stats`, `mesh/directory`). Without this, a 2s music `get-state` poll
/// grew `audit/<peer>` to 122k records / 479M on a 3.9G tmpfs `/run` (Eagle,
/// 2026-06-24). Every mutation (play/enroll/revoke/provision/config/role/…)
/// still audits — only reads/observations are skipped.
#[must_use]
pub fn is_auditable(topic: &str) -> bool {
    !topic.starts_with(AUDIT_TOPIC_PREFIX)
        && !topic.starts_with("state/")
        && !topic.starts_with("reply/")
        && !topic.contains("/get-")
        && !topic.contains("/list-")
        && !topic.ends_with("/list")
        && !topic.contains("peer-states")
        && !topic.ends_with("-stats")
        && topic != "action/mesh/directory"
}

/// BUS-2.7 — a single notification action button: a `label` the
/// notification surface renders, and a `url` (typically `mde://…`)
/// dispatched via `mde-open` when the operator clicks it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Action {
    /// Button label the notification surface renders (e.g. "Resolve").
    pub label: String,
    /// Target URL — an `mde://…` deep-link dispatched through `mde-open`,
    /// or an `http(s)://` link opened in the browser.
    pub url: String,
}

/// One row of the index + the on-disk file pointer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredMessage {
    /// ULID — 26-char Crockford base32. Acts as the primary key
    /// + the timestamp-sortable cursor for `list_since`.
    pub ulid: String,
    /// Topic path. Used by every query and is the dir-name in
    /// the file tree.
    pub topic: String,
    /// Lowercase priority string (`min` / `default` / `high` /
    /// `urgent`). Kept as a string in the SQLite row so the
    /// schema doesn't need to know the [`Priority`] enum's
    /// in-Rust representation.
    pub priority: String,
    /// Optional title — typically the rendered `X-Title` for
    /// webhook publishes.
    pub title: Option<String>,
    /// Optional body — the message payload.
    pub body: Option<String>,
    /// Unix ms timestamp at write time. Used by retention scans
    /// (BUS-1.9) to find messages past TTL.
    pub ts_unix_ms: i64,
    /// Path relative to `bus_root`. The on-disk JSON file lives
    /// at `bus_root.join(file_path)`.
    pub file_path: String,
    /// BUS-2.7 — optional action buttons (≤5, `v6.x-mackes-bus.md` §9).
    /// `#[serde(default)]` keeps pre-2.7 messages (no field on disk)
    /// deserializing to an empty vec — no migration, full backward-compat.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// BUS-2.7.d — ULID of the message this is a reply to, when set
    /// (`mde-bus publish --reply-to <ulid>`). `None` for top-level
    /// messages. `#[serde(default)]` keeps pre-2.7.d files (no field on
    /// disk) deserializing to `None` — backward-compat, no migration.
    /// Threaded views (BUS-6.1) key off this; like `actions` it lives in
    /// the on-disk JSON only, never the SQLite index.
    #[serde(default)]
    pub reply_to: Option<String>,
}

/// Errors surfaced by [`Persist`] operations.
#[derive(Debug, Error)]
pub enum PersistError {
    /// File-system error (mkdir, write, rename, etc.).
    #[error("io: {0}")]
    Io(String),
    /// SQLite error (open, exec, query, etc.).
    #[error("sql: {0}")]
    Sql(String),
    /// JSON serialize / deserialize error.
    #[error("json: {0}")]
    Json(String),
    /// Topic name rejected — empty / contains `..` / leading
    /// `/` / etc. Mirrors `topic::Topic::validate` shape but
    /// kept local so persist doesn't import topic.
    #[error("invalid topic name: {0}")]
    BadTopic(String),
}

/// Per-peer persistence handle. Cheap to construct (one SQLite
/// open + a schema PRAGMA + idempotent CREATE TABLE); keep one
/// handle per daemon and share via `Arc` to taskwriting paths.
#[derive(Debug)]
pub struct Persist {
    bus_root: PathBuf,
    conn: Connection,
    /// BUS-INODE-ORPHAN-1 — the inode of `index.sqlite` at open time. A
    /// background self-heal recreate (unlink + new file) changes it; a
    /// long-running consumer detects the swap via [`Persist::reopen_if_index_changed`]
    /// and reopens instead of being stranded on the deleted inode.
    index_inode: Option<u64>,
}

/// perf-3 — process-wide set of `index.sqlite` paths this process has already
/// fully initialized (schema exec + shared-spool chmod + the BOOT-REC-3
/// write-probe). `Persist::open` is called from ~89 mde-shell-egui render-path
/// sites several times a second; after the FIRST open of a given path in this
/// process the init work is redundant (the schema CREATEs are `IF NOT EXISTS`,
/// WAL journal-mode persists in the DB header, and the read-only boot latch is
/// a one-shot cold-boot race), so later opens fast-path. `Mutex<Option<_>>` so
/// the static is `const`-constructible without a lazy-init crate.
static INITIALIZED_PATHS: std::sync::Mutex<Option<std::collections::HashSet<PathBuf>>> =
    std::sync::Mutex::new(None);

/// perf-3 — record that this process has fully initialized `db_path`.
fn mark_initialized(db_path: &Path) {
    INITIALIZED_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(db_path.to_path_buf());
}

/// perf-3 — whether this process has already fully initialized `db_path`.
fn is_initialized(db_path: &Path) -> bool {
    INITIALIZED_PATHS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|s| s.contains(db_path))
}

impl Persist {
    /// Open (or create) the per-peer index + ensure the bus
    /// root exists. Safe to call repeatedly — schema CREATEs
    /// are `IF NOT EXISTS` and the WAL pragma is idempotent.
    ///
    /// # Errors
    /// Returns [`PersistError::Io`] when the root can't be
    /// mkdir'd or [`PersistError::Sql`] when opening the
    /// database or running the schema fails.
    pub fn open(bus_root: PathBuf) -> Result<Self, PersistError> {
        let db_path = bus_root.join("index.sqlite");
        // perf-3 fast path — after this process has fully initialized `db_path`
        // once, skip the redundant schema exec + shared-spool chmod + BOOT-REC-3
        // write-probe and hand back a cheap bare connection. Guarded on the DB
        // file still existing (a vanished DB must fall through so the schema is
        // recreated, not opened as an empty un-schema'd file). The guard is
        // PROCESS-wide, so every distinct process still runs the full first-open
        // init once — preserving the cross-uid chmod + boot-race self-heal
        // semantics; only intra-process repeat opens are fast-pathed.
        if is_initialized(&db_path) {
            if let Some(index_inode) = file_inode(&db_path) {
                let conn = Self::open_conn_bare(&db_path)?;
                return Ok(Self {
                    bus_root,
                    conn,
                    index_inode: Some(index_inode),
                });
            }
        }
        std::fs::create_dir_all(&bus_root)
            .map_err(|e| PersistError::Io(format!("mkdir {}: {e}", bus_root.display())))?;
        let mut conn = Self::open_conn(&db_path)?;
        // SETUP-fix — a SHARED spool (MDE_BUS_ROOT) is read+written by both the
        // root mackesd daemon AND the uid-1000 desktop GUIs, so the spool dir +
        // sqlite must be cross-uid writable (else the user's request/reply +
        // index writes are denied → "mesh service isn't answering").
        relax_shared(&bus_root, true);
        relax_db(&bus_root);
        // BOOT-REC-3 — a cold-boot cross-uid create race (the desktop session
        // creates the WAL index before the daemon, or before tmpfiles sets the
        // spool perms) can leave the DB latched UNWRITABLE: every write then
        // returns "attempt to write a readonly database", so every RPC reply
        // fails and the workbench shows "mesh service unreachable / no peers"
        // until a manual clear. The index lives on /run (tmpfs, ephemeral — the
        // durable directory/heartbeat data is on QNM-Shared), so self-heal by
        // recreating it when the probe reports a TRUE read-only DB. We must NOT
        // recreate on lock/busy contention (many processes — daemon, GUIs, CLI —
        // open Persist concurrently): busy is transient + absorbed by
        // busy_timeout, and recreating on it would delete the live DB out from
        // under everyone. So gate strictly on the ReadOnly error code.
        if matches!(Self::write_probe(&conn), Err(e) if is_readonly_err(&e)) {
            // BUS-INODE-ORPHAN-1 — the index latched read-only (boot-race
            // cross-uid create). Prefer an IN-PLACE perm fix over the destructive
            // unlink+recreate: a recreate swaps the inode and strands every OTHER
            // live consumer (mackesd workers, the workbench, musicd) on the
            // now-deleted file, which is the "daemon not responding after long
            // uptime" wedge. Chmod the shared spool's db + sidecars writable and
            // re-probe; the latch usually clears with no swap.
            relax_db(&bus_root);
            conn = Self::open_conn(&db_path)?;
            if matches!(Self::write_probe(&conn), Err(e) if is_readonly_err(&e)) {
                // Still read-only. Only recreate if WE own the file (or are root):
                // otherwise a lower-priv consumer (e.g. a uid-1000 GUI) would
                // unlink the root daemon's live index out from under it (the
                // shared spool dir is 0777, so the unlink would otherwise succeed
                // and orphan the daemon). A non-owner logs loudly + carries on.
                if owns_or_root(&db_path) {
                    drop(conn);
                    for suffix in ["", "-wal", "-shm"] {
                        let _ = std::fs::remove_file(format!("{}{suffix}", db_path.display()));
                    }
                    conn = Self::open_conn(&db_path)?;
                    relax_db(&bus_root);
                    tracing::warn!(
                        target: "mde_bus::persist",
                        db = %db_path.display(),
                        pid = std::process::id(),
                        "bus index was read-only on open (boot-race) — recreated it (BOOT-REC-3 / BUS-INODE-ORPHAN-1; we own it)",
                    );
                } else {
                    tracing::error!(
                        target: "mde_bus::persist",
                        db = %db_path.display(),
                        pid = std::process::id(),
                        "bus index is read-only and NOT owned by this process — refusing to recreate (would strand the owning daemon); fix the shared-spool perms instead",
                    );
                }
            } else {
                tracing::warn!(
                    target: "mde_bus::persist",
                    db = %db_path.display(),
                    pid = std::process::id(),
                    "bus index was read-only on open — fixed perms in place, no inode swap (BUS-INODE-ORPHAN-1)",
                );
            }
        }
        let index_inode = file_inode(&db_path);
        // perf-3 — mark this path initialized so subsequent opens fast-path.
        mark_initialized(&db_path);
        Ok(Self {
            bus_root,
            conn,
            index_inode,
        })
    }

    /// BUS-INODE-ORPHAN-1 — the inode of the `index.sqlite` this handle has open
    /// (captured at open / last reopen). `None` if it can't be stat'd.
    #[must_use]
    pub fn index_inode(&self) -> Option<u64> {
        self.index_inode
    }

    /// BUS-INODE-ORPHAN-1 — if another process recreated `index.sqlite` (the
    /// BOOT-REC-3 unlink + new file = a new inode), this handle is stranded on
    /// the DELETED inode and stops seeing new writes — the "daemon not responding
    /// after long uptime" wedge. Detect the swap (cheap stat) and reopen the
    /// connection so a long-running consumer follows the live DB. Returns `true`
    /// if it reopened. Lifts the MUSIC-WEDGE-2 pattern into the shared crate so
    /// every consumer (mackesd workers, GUI surfaces, mde-musicd) is covered.
    pub fn reopen_if_index_changed(&mut self) -> bool {
        let db_path = self.bus_root.join("index.sqlite");
        let live = file_inode(&db_path);
        if live.is_some() && live != self.index_inode {
            if let Ok(conn) = Self::open_conn(&db_path) {
                self.conn = conn;
                self.index_inode = live;
                tracing::warn!(
                    target: "mde_bus::persist",
                    db = %db_path.display(),
                    "bus index inode changed under us — reopened the store (BUS-INODE-ORPHAN-1)",
                );
                return true;
            }
        }
        false
    }

    /// Open the SQLite index with the standard busy-timeout + schema. Split out
    /// so [`Self::open`]'s BOOT-REC-3 self-heal can reopen after recreating it.
    fn open_conn(db_path: &std::path::Path) -> Result<Connection, PersistError> {
        let conn = Connection::open(db_path)
            .map_err(|e| PersistError::Sql(format!("open {}: {e}", db_path.display())))?;
        // 5s busy_timeout absorbs short-lived contention (the SubsWatcher mtime
        // poller + retention pass + webhook publishes can all touch the DB).
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| PersistError::Sql(format!("busy_timeout: {e}")))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| PersistError::Sql(format!("schema: {e}")))?;
        Ok(conn)
    }

    /// perf-3 — open the SQLite index WITHOUT the schema exec, for the fast path
    /// in [`Self::open`] once the schema is known-applied for this path in this
    /// process. WAL journal-mode is persisted in the DB header (it survives
    /// across connections), so a bare open still gets WAL; we set the same
    /// per-connection `busy_timeout` + `synchronous = NORMAL` as [`Self::open_conn`]
    /// so a fast-path write behaves identically. Skips the `CREATE TABLE/INDEX
    /// IF NOT EXISTS` + WAL pragma + write-probe that dominate open cost.
    fn open_conn_bare(db_path: &std::path::Path) -> Result<Connection, PersistError> {
        let conn = Connection::open(db_path)
            .map_err(|e| PersistError::Sql(format!("open {}: {e}", db_path.display())))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| PersistError::Sql(format!("busy_timeout: {e}")))?;
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(|e| PersistError::Sql(format!("synchronous: {e}")))?;
        Ok(conn)
    }

    /// BOOT-REC-3 write-probe: a NON-contending write that surfaces a latched
    /// read-only DB at open time. Rewrites `PRAGMA user_version` to its current
    /// value (a real header write, no value change, no schema/table contention),
    /// so concurrent opens don't fight over a scratch table. `Ok(())` ⇒ the
    /// index accepts writes; a ReadOnly error ⇒ the boot-race latch.
    fn write_probe(conn: &Connection) -> Result<(), rusqlite::Error> {
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        conn.execute_batch(&format!("PRAGMA user_version = {v};"))
    }

    /// Append a new message: write the on-disk JSON file
    /// atomically, then insert the index row. Returns the
    /// [`StoredMessage`] so callers can pass it forward to
    /// downstream consumers (e.g., the ntfy publisher).
    ///
    /// # Errors
    /// - [`PersistError::BadTopic`] when `topic` fails the
    ///   validation rules (empty / leading `/` / `..` / double
    ///   `/`).
    /// - [`PersistError::Io`] on mkdir / write / rename failure.
    /// - [`PersistError::Json`] when serialization fails (should
    ///   not happen — the type is plain JSON-compatible).
    /// - [`PersistError::Sql`] when the INSERT fails.
    pub fn write(
        &self,
        topic: &str,
        priority: Priority,
        title: Option<&str>,
        body: Option<&str>,
    ) -> Result<StoredMessage, PersistError> {
        self.write_with_actions(topic, priority, title, body, &[])
    }

    /// BUS-2.7 — append a notification carrying optional action buttons
    /// (≤5, `v6.x-mackes-bus.md` §9). The `actions` persist in the on-disk
    /// JSON; the SQLite index stays a query layer. Empty `actions` is
    /// equivalent to [`Persist::write`].
    pub fn write_with_actions(
        &self,
        topic: &str,
        priority: Priority,
        title: Option<&str>,
        body: Option<&str>,
        actions: &[Action],
    ) -> Result<StoredMessage, PersistError> {
        self.write_full(topic, priority, title, body, actions, None)
    }

    /// BUS-2.7.d — full-envelope writer: action buttons AND an optional
    /// `reply_to` parent ULID (threaded replies; BUS-6.1 renders threads).
    /// [`Persist::write`] + [`Persist::write_with_actions`] are thin
    /// wrappers so the 40+ existing call sites stay untouched (the additive
    /// pivot from 2.7.a). `reply_to` persists in the on-disk JSON only.
    pub fn write_full(
        &self,
        topic: &str,
        priority: Priority,
        title: Option<&str>,
        body: Option<&str>,
        actions: &[Action],
        reply_to: Option<&str>,
    ) -> Result<StoredMessage, PersistError> {
        validate_topic(topic)?;

        // Monotonic ULID (see ULID_GEN) so the (topic, ulid) cursor scan
        // reflects write order even for same-millisecond writes.
        let ulid = next_ulid().to_string();

        let topic_dir = self.bus_root.join(topic);
        std::fs::create_dir_all(&topic_dir)
            .map_err(|e| PersistError::Io(format!("mkdir {}: {e}", topic_dir.display())))?;
        // SETUP-fix / AUDIT-MESH-16 — relax EVERY component of the topic path,
        // not just the leaf. `create_dir_all` makes intermediate dirs with the
        // creator's umask (root → 0755), so a parent like `reply/` created by
        // the root daemon would block a different-uid responder (e.g. mde-musicd
        // running as the desktop user) from creating its `reply/<ulid>` subdir —
        // its replies were silently dropped (`let _ = persist.write`), so every
        // music RPC timed out. Relaxing the whole chain to 0777 lets any mesh
        // uid write into the shared spool.
        relax_topic_chain(&self.bus_root, topic);

        let file_name = format!("{ulid}.json");
        let abs_path = topic_dir.join(&file_name);
        // file_path is the topic-tree-relative pointer used by
        // detect_divergence + by external consumers.
        let rel_path = format!("{topic}/{file_name}");

        let ts_unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        let msg = StoredMessage {
            ulid: ulid.clone(),
            topic: topic.to_string(),
            priority: priority_str(priority).to_string(),
            title: title.map(String::from),
            body: body.map(String::from),
            ts_unix_ms,
            file_path: rel_path,
            actions: actions.to_vec(),
            reply_to: reply_to.map(String::from),
        };

        // Write JSON atomically. tmp-then-rename so a crash mid-
        // write leaves the directory clean.
        let json = serde_json::to_string_pretty(&msg)
            .map_err(|e| PersistError::Json(format!("encode {ulid}: {e}")))?;
        let tmp = abs_path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())
            .map_err(|e| PersistError::Io(format!("write {}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &abs_path).map_err(|e| {
            PersistError::Io(format!(
                "rename {} → {}: {e}",
                tmp.display(),
                abs_path.display()
            ))
        })?;
        relax_shared(&abs_path, false); // SETUP-fix: shared-spool cross-uid reads

        // Index INSERT. If this fails after the file write, the
        // file lingers on disk and detect_divergence will surface
        // it on the next audit — that's the documented recovery
        // mode (we don't want to delete the authoritative copy
        // because of an index hiccup).
        self.conn
            .execute(
                "INSERT INTO messages (ulid, topic, priority, title, body, ts_unix_ms, file_path) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    msg.ulid,
                    msg.topic,
                    msg.priority,
                    msg.title,
                    msg.body,
                    msg.ts_unix_ms,
                    msg.file_path
                ],
            )
            .map_err(|e| PersistError::Sql(format!("insert {ulid}: {e}")))?;
        relax_db(&self.bus_root); // SETUP-fix: the INSERT (re)created sqlite -wal/-shm

        // BUS-7.1 + EPIC-BUS-EXT-AUDIT-BUS (Q28) — emit a metadata-only
        // audit record to the `audit/<peer>` Bus topic. **Cycle guard:**
        // audit records are NOT themselves audited. Best-effort: a failed
        // audit emit logs + carries on (the original message is already
        // durably stored) and never fails the caller's write.
        //
        // BUS-AUDIT-FLOOD fix (live on Eagle, 2026-06-24): audit only
        // control-plane MUTATIONS + events (§8 — "security events are
        // hash-chain audited"). Observational `state/*` broadcasts, read-only
        // query verbs (`get-*`/`list-*`/`peer-states`/`*-stats`/`mesh/directory`)
        // and their `reply/*` carry NO security value — and the `audit/<peer>`
        // topic is EXEMPT from retention (see retention.rs), so auditing a
        // chatty poller (music `get-state` @2s + the notify-center) grew it
        // unbounded to 122k records / 479M on the 3.9G tmpfs `/run`, tripping
        // the bus-full alert. Skipping the read/observational classes keeps the
        // audit to real security/control events while every mutation is still
        // recorded.
        if is_auditable(topic) {
            let pid = publisher_id();
            let entry = crate::audit::AuditEntry {
                publisher: pid.clone(),
                ts_iso: chrono::Utc::now().to_rfc3339(),
                topic: msg.topic.clone(),
                priority: msg.priority.clone(),
                ulid: msg.ulid.clone(),
            };
            match serde_json::to_string(&entry) {
                Ok(audit_body) => {
                    let audit_topic = format!("{AUDIT_TOPIC_PREFIX}{pid}");
                    // Recursive write — the guard above stops it from
                    // re-auditing itself. Always `min` priority.
                    if let Err(e) = self.write(&audit_topic, Priority::Min, None, Some(&audit_body))
                    {
                        tracing::warn!(
                            target: "mde_bus::persist",
                            error = %e,
                            ulid = %msg.ulid,
                            "audit emit failed — message persisted but audit gap"
                        );
                    }
                }
                Err(e) => tracing::warn!(
                    target: "mde_bus::persist",
                    error = %e,
                    "audit entry encode failed — audit gap"
                ),
            }
        }

        Ok(msg)
    }

    /// Return messages on `topic`, optionally starting after a
    /// `since_ulid` cursor (exclusive). Results are ordered by
    /// ULID ascending, which matches insertion order because
    /// ULIDs embed the write timestamp.
    ///
    /// `topic` is matched exactly — wildcard matching is the
    /// caller's responsibility (use `crate::wildcard::matches`
    /// to expand `+` / `#` patterns into a list of topics).
    ///
    /// # Errors
    /// [`PersistError::Sql`] on query or row-decode failure.
    /// The newest (max) ULID currently stored on `topic`, or `None` if the
    /// topic has no messages. Used to seed a poll cursor at the current tail so a
    /// restarting consumer skips the historical backlog and only handles NEW
    /// messages (a stale request must not replay on restart). Cheap — a single
    /// `ORDER BY ulid DESC LIMIT 1` index probe, not a full scan.
    pub fn latest_ulid(&self, topic: &str) -> Result<Option<String>, PersistError> {
        self.conn
            .query_row(
                "SELECT ulid FROM messages WHERE topic = ?1 ORDER BY ulid DESC LIMIT 1",
                params![topic],
                |row| row.get::<_, String>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(PersistError::Sql(format!("latest_ulid: {other}"))),
            })
    }

    pub fn list_since(
        &self,
        topic: &str,
        since_ulid: Option<&str>,
    ) -> Result<Vec<StoredMessage>, PersistError> {
        let mut out = Vec::new();
        if let Some(s) = since_ulid {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT ulid, topic, priority, title, body, ts_unix_ms, file_path \
                     FROM messages WHERE topic = ?1 AND ulid > ?2 ORDER BY ulid",
                )
                .map_err(|e| PersistError::Sql(format!("prepare list_since: {e}")))?;
            let rows = stmt
                .query_map(params![topic, s], row_to_message)
                .map_err(|e| PersistError::Sql(format!("query list_since: {e}")))?;
            for r in rows {
                out.push(r.map_err(|e| PersistError::Sql(format!("decode: {e}")))?);
            }
        } else {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT ulid, topic, priority, title, body, ts_unix_ms, file_path \
                     FROM messages WHERE topic = ?1 ORDER BY ulid",
                )
                .map_err(|e| PersistError::Sql(format!("prepare list_all: {e}")))?;
            let rows = stmt
                .query_map(params![topic], row_to_message)
                .map_err(|e| PersistError::Sql(format!("query list_all: {e}")))?;
            for r in rows {
                out.push(r.map_err(|e| PersistError::Sql(format!("decode: {e}")))?);
            }
        }
        Ok(out)
    }

    /// perf-4 — the newest message on `topic`, or `None` when the topic has no
    /// messages. Returns the SAME row that `list_since(topic, None).last()`
    /// would (ULID order == write order), but as a bounded `ORDER BY ulid DESC
    /// LIMIT 1` index probe instead of loading the whole retained history (up to
    /// ~20k rows / ~8 MiB per topic) just to discard all but the tail. This is
    /// the latest-wins read the shell render path wants.
    ///
    /// # Errors
    /// [`PersistError::Sql`] on query or row-decode failure.
    pub fn read_latest(&self, topic: &str) -> Result<Option<StoredMessage>, PersistError> {
        Ok(self.read_tail(topic, 1)?.pop())
    }

    /// perf-4 — the newest `n` messages on `topic`, returned OLDEST-first — the
    /// same rows + order as the tail of `list_since(topic, None)`, but bounded by
    /// `n` via `ORDER BY ulid DESC LIMIT n` so a chatty retained topic isn't
    /// fully loaded. `n == 0` yields an empty vec.
    ///
    /// # Errors
    /// [`PersistError::Sql`] on query or row-decode failure.
    pub fn read_tail(&self, topic: &str, n: usize) -> Result<Vec<StoredMessage>, PersistError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ulid, topic, priority, title, body, ts_unix_ms, file_path \
                 FROM messages WHERE topic = ?1 ORDER BY ulid DESC LIMIT ?2",
            )
            .map_err(|e| PersistError::Sql(format!("prepare read_tail: {e}")))?;
        let rows = stmt
            .query_map(
                params![topic, i64::try_from(n).unwrap_or(i64::MAX)],
                row_to_message,
            )
            .map_err(|e| PersistError::Sql(format!("query read_tail: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| PersistError::Sql(format!("decode: {e}")))?);
        }
        // The DESC query yields newest-first; reverse to OLDEST-first so callers
        // see `list_since`'s ascending order and `read_latest` finds the newest
        // as the LAST element (matching the old `.last()`).
        out.reverse();
        Ok(out)
    }

    /// Return every topic that has at least one indexed
    /// message. Used by `mde-bus tail` to expand wildcard
    /// patterns against the known-topic set.
    ///
    /// # Errors
    /// [`PersistError::Sql`] on query failure.
    pub fn list_topics(&self) -> Result<Vec<String>, PersistError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT topic FROM messages ORDER BY topic")
            .map_err(|e| PersistError::Sql(format!("prepare list_topics: {e}")))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| PersistError::Sql(format!("query list_topics: {e}")))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| PersistError::Sql(format!("decode: {e}")))?);
        }
        Ok(out)
    }

    /// Total message count — useful for tests + the
    /// `mde-bus history --count` verb (BUS-1.8 will wire).
    ///
    /// # Errors
    /// [`PersistError::Sql`] on query failure.
    pub fn count(&self) -> Result<i64, PersistError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .map_err(|e| PersistError::Sql(format!("count: {e}")))?;
        Ok(n)
    }

    /// Walk the file tree under `bus_root` and compare against
    /// the SQLite index. Reports:
    ///
    /// - **files_without_rows**: JSON files on disk with no
    ///   matching index entry. Typically created by an external
    ///   process (or a crash between rename + INSERT). The audit
    ///   pass can either back-fill the index or quarantine the
    ///   file.
    /// - **rows_without_files**: index rows whose JSON file is
    ///   gone. Either an external `rm`, a retention pass that
    ///   forgot to delete the row, or filesystem corruption.
    ///
    /// # Errors
    /// [`PersistError::Io`] on walk failure, [`PersistError::Sql`]
    /// on query failure.
    pub fn detect_divergence(&self) -> Result<DivergenceReport, PersistError> {
        // Collect every file_path in the index into a HashSet.
        let mut stmt = self
            .conn
            .prepare("SELECT file_path FROM messages")
            .map_err(|e| PersistError::Sql(format!("prepare divergence: {e}")))?;
        let mut indexed: std::collections::HashSet<String> = std::collections::HashSet::new();
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| PersistError::Sql(format!("query divergence: {e}")))?;
        for r in rows {
            indexed.insert(r.map_err(|e| PersistError::Sql(format!("decode: {e}")))?);
        }

        // Walk the file tree. The Bus root can also host client-side state files
        // (`settings-nav.json`, `tmux/state.json`, etc.) when the shared root is
        // injected into desktop processes. Those are not Bus messages; a message
        // file must have a ULID filename and a StoredMessage envelope that points
        // back at the same relative path.
        let mut json_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        walk_json_files(&self.bus_root, &self.bus_root, &mut json_files)?;
        let mut on_disk: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut invalid_message_files = Vec::new();
        let mut ignored_non_message_files = Vec::new();
        for rel in json_files {
            match read_indexable_message(&self.bus_root, &rel)? {
                MessageFile::Message(_) => {
                    on_disk.insert(rel);
                }
                MessageFile::Invalid(reason) => {
                    invalid_message_files.push(DivergenceFileProblem { path: rel, reason });
                }
                MessageFile::NonMessage => ignored_non_message_files.push(rel),
            }
        }

        let mut files_without_rows: Vec<String> = on_disk.difference(&indexed).cloned().collect();
        let mut rows_without_files: Vec<String> = indexed.difference(&on_disk).cloned().collect();
        files_without_rows.sort();
        rows_without_files.sort();
        invalid_message_files.sort_by(|a, b| a.path.cmp(&b.path));
        ignored_non_message_files.sort();

        Ok(DivergenceReport {
            files_without_rows,
            rows_without_files,
            invalid_message_files,
            ignored_non_message_files,
        })
    }

    /// Repair safe divergence between the authoritative JSON file tree and the
    /// local SQLite index.
    ///
    /// Valid message files without rows are re-indexed exactly as-is. Rows whose
    /// file is missing are reported by default; when `prune_missing_rows` is
    /// explicit, the stale index rows are exported under `.repair/` before being
    /// removed from the query index.
    pub fn repair_divergence(
        &self,
        dry_run: bool,
        prune_missing_rows: bool,
    ) -> Result<DivergenceRepairReport, PersistError> {
        let report = self.detect_divergence()?;
        let mut repair = DivergenceRepairReport {
            dry_run,
            rows_without_files: report.rows_without_files.clone(),
            invalid_message_files: report.invalid_message_files.clone(),
            ignored_non_message_files: report.ignored_non_message_files.clone(),
            ..DivergenceRepairReport::default()
        };

        if dry_run {
            repair.files_would_reindex = report.files_without_rows;
            if prune_missing_rows {
                repair.rows_would_prune = repair.rows_without_files.clone();
            }
            return Ok(repair);
        }

        for rel in &report.files_without_rows {
            let MessageFile::Message(msg) = read_indexable_message(&self.bus_root, rel)? else {
                return Err(PersistError::Io(format!(
                    "message file classification changed during repair: {rel}"
                )));
            };
            if self.insert_existing_message(&msg)? {
                repair.files_reindexed.push(rel.clone());
            }
        }
        if !repair.files_reindexed.is_empty() {
            relax_db(&self.bus_root);
        }

        if prune_missing_rows && !repair.rows_without_files.is_empty() {
            let missing = self.load_rows_by_file_paths(&repair.rows_without_files)?;
            if !missing.is_empty() {
                repair.missing_rows_export = Some(export_missing_rows(&self.bus_root, &missing)?);
            }
            self.delete_rows_by_file_paths(&repair.rows_without_files)?;
            repair.rows_pruned = std::mem::take(&mut repair.rows_without_files);
            relax_db(&self.bus_root);
        }

        Ok(repair)
    }

    fn insert_existing_message(&self, msg: &StoredMessage) -> Result<bool, PersistError> {
        let changed = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO messages (ulid, topic, priority, title, body, ts_unix_ms, file_path) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    &msg.ulid,
                    &msg.topic,
                    &msg.priority,
                    &msg.title,
                    &msg.body,
                    msg.ts_unix_ms,
                    &msg.file_path
                ],
            )
            .map_err(|e| PersistError::Sql(format!("reindex {}: {e}", msg.file_path)))?;
        if changed > 0 {
            return Ok(true);
        }

        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT file_path FROM messages WHERE ulid = ?1",
                params![&msg.ulid],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(PersistError::Sql(format!(
                    "check reindex conflict {}: {other}",
                    msg.file_path
                ))),
            })?;
        if existing.as_deref() == Some(msg.file_path.as_str()) {
            return Ok(false);
        }
        Err(PersistError::Sql(format!(
            "reindex {}: ULID {} already indexes {:?}",
            msg.file_path, msg.ulid, existing
        )))
    }

    fn load_rows_by_file_paths(
        &self,
        paths: &[String],
    ) -> Result<Vec<StoredMessage>, PersistError> {
        let mut out = Vec::new();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ulid, topic, priority, title, body, ts_unix_ms, file_path \
                 FROM messages WHERE file_path = ?1",
            )
            .map_err(|e| PersistError::Sql(format!("prepare missing-row export: {e}")))?;
        for path in paths {
            match stmt.query_row(params![path], row_to_message) {
                Ok(msg) => out.push(msg),
                Err(rusqlite::Error::QueryReturnedNoRows) => {}
                Err(e) => return Err(PersistError::Sql(format!("load missing row {path}: {e}"))),
            }
        }
        Ok(out)
    }

    fn delete_rows_by_file_paths(&self, paths: &[String]) -> Result<(), PersistError> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM messages WHERE file_path = ?1")
            .map_err(|e| PersistError::Sql(format!("prepare missing-row prune: {e}")))?;
        for path in paths {
            stmt.execute(params![path])
                .map_err(|e| PersistError::Sql(format!("prune missing row {path}: {e}")))?;
        }
        Ok(())
    }

    /// Test-only accessor for the bus root.
    #[cfg(test)]
    pub fn bus_root(&self) -> &Path {
        &self.bus_root
    }
}

/// Output of [`Persist::detect_divergence`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DivergenceReport {
    /// Relative paths (under `bus_root`) of JSON files that
    /// exist on disk but have no SQLite row.
    pub files_without_rows: Vec<String>,
    /// Relative paths (under `bus_root`) of SQLite rows whose
    /// JSON file is gone.
    pub rows_without_files: Vec<String>,
    /// ULID-named JSON files that look like Bus messages but cannot be safely
    /// re-indexed because their envelope is malformed or inconsistent.
    pub invalid_message_files: Vec<DivergenceFileProblem>,
    /// JSON files under the shared root that are not Bus message files. These
    /// are informational only and do not make the persistence layer divergent.
    pub ignored_non_message_files: Vec<String>,
}

impl DivergenceReport {
    /// Convenience — `true` when both sets are empty.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.files_without_rows.is_empty()
            && self.rows_without_files.is_empty()
            && self.invalid_message_files.is_empty()
    }
}

/// A Bus-message-shaped JSON file that cannot be trusted for repair.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DivergenceFileProblem {
    /// Relative path under the Bus root.
    pub path: String,
    /// Operator-facing reason the file was excluded from repair.
    pub reason: String,
}

/// Output of [`Persist::repair_divergence`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DivergenceRepairReport {
    /// True when no writes were attempted.
    pub dry_run: bool,
    /// Valid message files actually inserted into the SQLite index.
    pub files_reindexed: Vec<String>,
    /// Valid message files that would be inserted during a dry run.
    pub files_would_reindex: Vec<String>,
    /// Stale rows still present because `prune_missing_rows` was not requested.
    pub rows_without_files: Vec<String>,
    /// Stale rows removed from the SQLite index after being exported.
    pub rows_pruned: Vec<String>,
    /// Stale rows that would be pruned during a dry run.
    pub rows_would_prune: Vec<String>,
    /// Relative `.repair/...json` export path for pruned rows.
    pub missing_rows_export: Option<String>,
    /// Message-shaped files that were not safe to index.
    pub invalid_message_files: Vec<DivergenceFileProblem>,
    /// Non-message JSON files ignored by the verifier.
    pub ignored_non_message_files: Vec<String>,
}

impl DivergenceRepairReport {
    /// True when operator follow-up is still required after this repair pass.
    #[must_use]
    pub fn has_unresolved(&self) -> bool {
        !self.rows_without_files.is_empty() || !self.invalid_message_files.is_empty()
    }
}

/// Best-effort publisher identifier for audit entries. Reads
/// `$HOSTNAME` env, then `/proc/sys/kernel/hostname`, then
/// falls back to the literal `"mde-bus"` (same fallback chain
/// the mDNS discovery hostname uses for symmetry).
/// SETUP-fix — when `MDE_BUS_ROOT` pins a SHARED spool (the root mackesd daemon
/// and the uid-1000 desktop GUIs use ONE bus), relax a freshly-created bus path
/// to cross-uid writable: `0o777` dirs / `0o666` files. No-op on the per-user
/// default spool (no env). The bus carries mesh **control** messages, not
/// secrets (those live in the CA + QNM-Shared), so a world-rw runtime spool on a
/// single-operator node is an accepted trade — see docs/design/magic-setup-wizard.md.
#[cfg(unix)]
fn relax_shared(path: &std::path::Path, dir: bool) {
    if std::env::var_os("MDE_BUS_ROOT").is_none() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let mode = if dir { 0o777 } else { 0o666 };
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn relax_shared(_path: &std::path::Path, _dir: bool) {}

/// AUDIT-MESH-16 — relax every directory component of `topic` under `bus_root`
/// to 0777 (not just the leaf), so a topic like `reply/<ulid>` makes BOTH the
/// `reply/` parent and the `<ulid>` leaf writable by any mesh uid. Without this,
/// the intermediate dir created by the first writer (often the root daemon at
/// umask 022 → 0755) blocks a different-uid responder from writing its replies.
fn relax_topic_chain(bus_root: &std::path::Path, topic: &str) {
    let mut dir = bus_root.to_path_buf();
    for comp in topic.split('/').filter(|c| !c.is_empty()) {
        dir.push(comp);
        relax_shared(&dir, true);
    }
}

/// BUS-INODE-ORPHAN-1 — the inode of `path`, or `None` if it can't be stat'd.
/// Used to detect a self-heal recreate (unlink + new file = new inode) so a live
/// consumer can reopen rather than stay stranded on the deleted file.
#[cfg(unix)]
fn file_inode(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}
#[cfg(not(unix))]
fn file_inode(_path: &std::path::Path) -> Option<u64> {
    None
}

/// BUS-INODE-ORPHAN-1 — whether this process may safely recreate `path`: true
/// iff we own it (or are root). A self-chmod to the file's CURRENT mode succeeds
/// only for the owner or a CAP_FOWNER (root) process — a portable ownership probe
/// with no libc dependency. Gates the destructive recreate so a non-owner (e.g. a
/// uid-1000 GUI) never unlinks a root-owned live index. A vanished file → true
/// (a recreate is then safe / needed).
#[cfg(unix)]
fn owns_or_root(path: &std::path::Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => std::fs::set_permissions(path, m.permissions()).is_ok(),
        Err(_) => true,
    }
}
#[cfg(not(unix))]
fn owns_or_root(_path: &std::path::Path) -> bool {
    true
}

/// Relax the sqlite index + its WAL/SHM sidecars (created lazily by sqlite) for
/// the shared spool, so both uids can write the index.
fn relax_db(bus_root: &std::path::Path) {
    relax_shared(&bus_root.join("index.sqlite"), false);
    relax_shared(&bus_root.join("index.sqlite-wal"), false);
    relax_shared(&bus_root.join("index.sqlite-shm"), false);
}

fn publisher_id() -> String {
    if let Ok(v) = std::env::var("HOSTNAME") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Ok(body) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let t = body.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    "mde-bus".to_string()
}

fn priority_str(p: Priority) -> &'static str {
    match p {
        Priority::Min => "min",
        Priority::Default => "default",
        Priority::High => "high",
        Priority::Urgent => "urgent",
    }
}

fn validate_topic(topic: &str) -> Result<(), PersistError> {
    if topic.is_empty() {
        return Err(PersistError::BadTopic("empty".to_string()));
    }
    if topic.starts_with('/') || topic.ends_with('/') {
        return Err(PersistError::BadTopic(format!(
            "leading/trailing slash: {topic}"
        )));
    }
    if topic.contains("..") {
        return Err(PersistError::BadTopic(format!(
            "path-escape attempt: {topic}"
        )));
    }
    if topic.contains("//") {
        return Err(PersistError::BadTopic(format!("double slash: {topic}")));
    }
    // Wildcard chars are publish-illegal (they're query-only).
    if topic.contains('+') || topic.contains('#') {
        return Err(PersistError::BadTopic(format!(
            "wildcards in publish topic: {topic}"
        )));
    }
    Ok(())
}

enum MessageFile {
    Message(StoredMessage),
    Invalid(String),
    NonMessage,
}

fn read_indexable_message(root: &Path, rel: &str) -> Result<MessageFile, PersistError> {
    let path = Path::new(rel);
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return Ok(MessageFile::NonMessage);
    };
    if stem.parse::<Ulid>().is_err() {
        return Ok(MessageFile::NonMessage);
    }

    let abs = root.join(rel);
    let raw = std::fs::read_to_string(&abs)
        .map_err(|e| PersistError::Io(format!("read {}: {e}", abs.display())))?;
    let msg = match serde_json::from_str::<StoredMessage>(&raw) {
        Ok(msg) => msg,
        Err(e) => return Ok(MessageFile::Invalid(format!("decode StoredMessage: {e}"))),
    };

    if msg.ulid != stem {
        return Ok(MessageFile::Invalid(format!(
            "filename ULID {stem} does not match envelope ULID {}",
            msg.ulid
        )));
    }
    if msg.file_path != rel {
        return Ok(MessageFile::Invalid(format!(
            "envelope file_path {} does not match path {rel}",
            msg.file_path
        )));
    }
    if let Err(e) = validate_topic(&msg.topic) {
        return Ok(MessageFile::Invalid(format!("invalid topic: {e}")));
    }
    let expected = format!("{}/{}.json", msg.topic, msg.ulid);
    if expected != rel {
        return Ok(MessageFile::Invalid(format!(
            "topic/ULID imply {expected}, not {rel}"
        )));
    }

    Ok(MessageFile::Message(msg))
}

fn export_missing_rows(root: &Path, rows: &[StoredMessage]) -> Result<String, PersistError> {
    let repair_dir = root.join(".repair");
    std::fs::create_dir_all(&repair_dir)
        .map_err(|e| PersistError::Io(format!("mkdir {}: {e}", repair_dir.display())))?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let rel = format!(".repair/persist-missing-rows-{ts}.json");
    let path = root.join(&rel);
    let body = serde_json::to_string_pretty(rows)
        .map_err(|e| PersistError::Json(format!("encode missing-row export: {e}")))?;
    std::fs::write(&path, body.as_bytes())
        .map_err(|e| PersistError::Io(format!("write {}: {e}", path.display())))?;
    relax_shared(&path, false);
    Ok(rel)
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        ulid: row.get(0)?,
        topic: row.get(1)?,
        priority: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        ts_unix_ms: row.get(5)?,
        file_path: row.get(6)?,
        // BUS-2.7: the SQLite index is a query layer and does not carry
        // actions; the full message (incl. actions) lives in the on-disk
        // JSON at `file_path`, which consumers read when they render.
        actions: Vec::new(),
        // BUS-2.7.d: `reply_to` is likewise on-disk-JSON only — not indexed.
        reply_to: None,
    })
}

/// Recursively walk `dir`, accumulating relative `<topic>/<ulid>.json`
/// paths into `out`. Skips `index.sqlite*` (the DB itself) + any
/// hidden files (entries starting with `.`).
fn walk_json_files(
    base: &Path,
    dir: &Path,
    out: &mut std::collections::HashSet<String>,
) -> Result<(), PersistError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| PersistError::Io(format!("readdir {}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| PersistError::Io(format!("readdir entry: {e}")))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip the index DB itself + tmp files.
        if name.starts_with("index.sqlite") || name.ends_with(".tmp") {
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let ft = entry
            .file_type()
            .map_err(|e| PersistError::Io(format!("file_type {}: {e}", path.display())))?;
        if ft.is_dir() {
            walk_json_files(base, &path, out)?;
        } else if ft.is_file() && name.ends_with(".json") {
            let rel = path
                .strip_prefix(base)
                .map_err(|_| PersistError::Io(format!("strip_prefix {}", path.display())))?
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (tempfile::TempDir, Persist) {
        let tmp = tempfile::tempdir().unwrap();
        let p = Persist::open(tmp.path().to_path_buf()).unwrap();
        (tmp, p)
    }

    fn stored_message(topic: &str, ulid: &str, body: &str) -> StoredMessage {
        StoredMessage {
            ulid: ulid.to_owned(),
            topic: topic.to_owned(),
            priority: "default".to_owned(),
            title: None,
            body: Some(body.to_owned()),
            ts_unix_ms: 1_784_349_068_806,
            file_path: format!("{topic}/{ulid}.json"),
            actions: Vec::new(),
            reply_to: None,
        }
    }

    #[test]
    fn latest_ulid_returns_newest_or_none() {
        let (_tmp, p) = open_tmp();
        // Empty topic → None (consumer starts with no cursor).
        assert_eq!(p.latest_ulid("action/music/list-radio").unwrap(), None);
        let a = p
            .write(
                "action/music/list-radio",
                Priority::Default,
                None,
                Some("1"),
            )
            .unwrap();
        let b = p
            .write(
                "action/music/list-radio",
                Priority::Default,
                None,
                Some("2"),
            )
            .unwrap();
        // Newest (max ULID) wins, and ULIDs are monotonic so b > a.
        assert!(b.ulid > a.ulid);
        assert_eq!(
            p.latest_ulid("action/music/list-radio").unwrap(),
            Some(b.ulid)
        );
        // A different topic is unaffected.
        assert_eq!(p.latest_ulid("action/music/play").unwrap(), None);
    }

    #[test]
    fn write_probe_distinguishes_readonly_from_writable() {
        // BOOT-REC-3: the probe must FAIL on a read-only DB (the latched
        // boot-race state) and PASS on a writable one — that verdict is what
        // drives the self-heal recreate in `open`.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("probe.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();
        let ro =
            Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(
            Persist::write_probe(&ro).is_err(),
            "read-only DB must fail the probe"
        );
        let rw = Connection::open(&db).unwrap();
        assert!(
            Persist::write_probe(&rw).is_ok(),
            "writable DB must pass the probe"
        );
    }

    #[test]
    fn open_self_heals_after_recreate_and_stays_writable() {
        // A reopened bus accepts writes (probe + recreate path is idempotent).
        let tmp = tempfile::tempdir().unwrap();
        {
            let p = Persist::open(tmp.path().to_path_buf()).unwrap();
            p.write("test/a", Priority::Default, None, Some("x"))
                .unwrap();
        }
        let p2 = Persist::open(tmp.path().to_path_buf()).unwrap();
        // Must still accept writes after reopen (no false recreate / no latch).
        p2.write("test/b", Priority::Default, None, Some("y"))
            .unwrap();
    }

    #[test]
    fn reopen_if_index_changed_follows_a_recreate() {
        // BUS-INODE-ORPHAN-1 — a consumer stranded on a recreated (new-inode)
        // index detects the swap and reopens, then sees writes made by the
        // process that recreated it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bus");
        let mut a = Persist::open(root.clone()).unwrap();
        let first = a.index_inode();
        assert!(first.is_some());
        // Nothing changed yet → no reopen.
        assert!(!a.reopen_if_index_changed());

        // Another process recreates the index (unlink + fresh open = new inode).
        let db = root.join("index.sqlite");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        let b = Persist::open(root.clone()).unwrap();

        // `a` is now on the deleted inode; reopen detects + follows the live DB.
        assert!(a.reopen_if_index_changed());
        assert_ne!(a.index_inode(), first);
        // After reopening, `a` sees a write `b` makes to the live index.
        b.write("t/x", Priority::Default, None, Some("hi")).unwrap();
        assert_eq!(a.list_since("t/x", None).unwrap().len(), 1);
    }

    #[test]
    fn owns_or_root_true_for_our_own_file() {
        // BUS-INODE-ORPHAN-1 — the ownership probe is true for a file we created
        // (we own it), which is what gates the safe-to-recreate path.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("mine.sqlite");
        std::fs::write(&f, b"x").unwrap();
        assert!(owns_or_root(&f));
        // A missing file → true (a recreate is then safe / needed).
        assert!(owns_or_root(&tmp.path().join("gone.sqlite")));
    }

    #[test]
    fn open_creates_db_and_root() {
        let (tmp, _p) = open_tmp();
        assert!(tmp.path().join("index.sqlite").exists());
    }

    #[test]
    fn open_fast_paths_after_first_init() {
        // perf-3 — the first open of a path runs full init and records it in the
        // process-wide guard; subsequent opens take the fast path (bare
        // connection, no schema/write-probe) yet still return a usable handle.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bus");
        let db = root.join("index.sqlite");
        assert!(
            !is_initialized(&db),
            "path is not initialized before the first open"
        );
        let p1 = Persist::open(root.clone()).unwrap();
        p1.write("t/x", Priority::Default, None, Some("a")).unwrap();
        drop(p1);
        // First open populated the guard.
        assert!(
            is_initialized(&db),
            "the guard is populated after the first open"
        );
        // Second open fast-paths and is still fully usable for reads AND writes.
        let p2 = Persist::open(root.clone()).unwrap();
        assert_eq!(p2.list_since("t/x", None).unwrap().len(), 1);
        p2.write("t/y", Priority::Default, None, Some("b")).unwrap();
        assert_eq!(p2.list_since("t/y", None).unwrap().len(), 1);
    }

    #[test]
    fn open_falls_back_to_slow_path_when_db_vanishes() {
        // perf-3 — the fast path is gated on the DB file still existing: if the
        // index is unlinked out from under a marked path, the next open must
        // slow-path (recreate + schema), not open an empty un-schema'd file.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("bus");
        let db = root.join("index.sqlite");
        let p1 = Persist::open(root.clone()).unwrap();
        p1.write("t/x", Priority::Default, None, Some("a")).unwrap();
        drop(p1);
        assert!(is_initialized(&db));
        // Nuke the index (+ sidecars) — the guard still says "initialized".
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        // Next open must recreate a schema'd DB and accept writes.
        let p2 = Persist::open(root.clone()).unwrap();
        p2.write("t/z", Priority::Default, None, Some("c")).unwrap();
        assert_eq!(p2.list_since("t/z", None).unwrap().len(), 1);
    }

    #[test]
    fn read_latest_matches_list_since_last() {
        // perf-4 — read_latest returns the SAME row as loading-all-then-.last()
        // on a multi-row topic, and read_tail returns the same tail slice.
        let (_tmp, p) = open_tmp();
        // Empty topic → None, exactly like list_since(...).last().
        assert!(p.read_latest("t/x").unwrap().is_none());
        assert!(p.read_tail("t/x", 3).unwrap().is_empty());
        for i in 0..5 {
            p.write("t/x", Priority::Default, None, Some(&format!("m{i}")))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let all = p.list_since("t/x", None).unwrap();
        let expected_last = all.last().cloned().unwrap();
        let got = p.read_latest("t/x").unwrap().unwrap();
        assert_eq!(got, expected_last, "read_latest == list_since(None).last()");
        // read_tail(n) == the last n of list_since, oldest-first.
        let tail = p.read_tail("t/x", 3).unwrap();
        assert_eq!(tail, all[all.len() - 3..].to_vec());
        // n larger than the row count yields the whole topic (no panic).
        assert_eq!(p.read_tail("t/x", 99).unwrap(), all);
        // read_latest is topic-scoped.
        assert!(p.read_latest("t/other").unwrap().is_none());
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = Persist::open(tmp.path().to_path_buf()).unwrap();
        p1.write("test/x", Priority::Default, Some("t"), Some("b"))
            .unwrap();
        drop(p1);
        let p2 = Persist::open(tmp.path().to_path_buf()).unwrap();
        // 2 = the test/x message + its audit/<peer> record.
        assert_eq!(p2.count().unwrap(), 2);
    }

    #[test]
    fn write_creates_file_and_row() {
        let (tmp, p) = open_tmp();
        let msg = p
            .write(
                "fleet/announce",
                Priority::High,
                Some("Hello"),
                Some("Body line"),
            )
            .unwrap();
        // File exists on disk.
        let abs = tmp.path().join(&msg.file_path);
        assert!(abs.exists(), "file missing: {}", abs.display());
        // Row exists in DB. EPIC-BUS-EXT-AUDIT-BUS: each publish also
        // emits one audit record to audit/<peer>, so the index holds
        // 2 rows (the message + its audit record).
        assert_eq!(p.count().unwrap(), 2);
        // File content round-trips.
        let json = std::fs::read_to_string(&abs).unwrap();
        let decoded: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.ulid, msg.ulid);
        assert_eq!(decoded.topic, "fleet/announce");
        assert_eq!(decoded.priority, "high");
        assert_eq!(decoded.title.as_deref(), Some("Hello"));
    }

    #[test]
    fn list_since_returns_ulid_order() {
        let (_tmp, p) = open_tmp();
        let mut ulids = Vec::new();
        for i in 0..5 {
            let m = p
                .write("t/x", Priority::Default, None, Some(&format!("msg {i}")))
                .unwrap();
            ulids.push(m.ulid);
            // Tiny sleep to ensure timestamp progression — ULIDs
            // monotonically increase even within a millisecond,
            // but we want assert against deterministic order.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let rows = p.list_since("t/x", None).unwrap();
        assert_eq!(rows.len(), 5);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.ulid, ulids[i]);
        }
    }

    #[test]
    fn list_since_with_cursor_excludes_earlier() {
        let (_tmp, p) = open_tmp();
        let mut ulids = Vec::new();
        for i in 0..5 {
            let m = p
                .write("t/x", Priority::Default, None, Some(&format!("{i}")))
                .unwrap();
            ulids.push(m.ulid);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let rows = p.list_since("t/x", Some(&ulids[2])).unwrap();
        // Strictly after ulids[2] → ulids[3] + ulids[4].
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ulid, ulids[3]);
        assert_eq!(rows[1].ulid, ulids[4]);
    }

    #[test]
    fn list_since_filters_by_topic() {
        let (_tmp, p) = open_tmp();
        p.write("a", Priority::Default, None, Some("x")).unwrap();
        p.write("b", Priority::Default, None, Some("y")).unwrap();
        p.write("a", Priority::Default, None, Some("z")).unwrap();
        assert_eq!(p.list_since("a", None).unwrap().len(), 2);
        assert_eq!(p.list_since("b", None).unwrap().len(), 1);
        assert_eq!(p.list_since("nonexistent", None).unwrap().len(), 0);
    }

    #[test]
    fn dotted_unenrolled_hostname_topics_write_and_list() {
        let (_tmp, p) = open_tmp();
        p.write(
            "audit/localhost.localdomain",
            Priority::Min,
            None,
            Some("{}"),
        )
        .unwrap();
        let topics = p.list_topics().unwrap();
        assert!(
            topics.iter().any(|t| t == "audit/localhost.localdomain"),
            "fresh Fedora's default dotted hostname must be a listable topic"
        );
    }

    #[test]
    fn topic_validation_rejects_bad_inputs() {
        let (_tmp, p) = open_tmp();
        assert!(matches!(
            p.write("", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
        assert!(matches!(
            p.write("/leading", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
        assert!(matches!(
            p.write("trailing/", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
        assert!(matches!(
            p.write("../escape", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
        assert!(matches!(
            p.write("double//slash", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
        assert!(matches!(
            p.write("wild/+/card", Priority::Default, None, None),
            Err(PersistError::BadTopic(_))
        ));
    }

    #[test]
    fn write_full_persists_reply_to_in_json() {
        // BUS-2.7.d — write_full sets reply_to on the envelope; it round-
        // trips through the on-disk JSON, and the plain `write` wrapper
        // leaves it None.
        let (_tmp, p) = open_tmp();
        let reply = p
            .write_full(
                "t/x",
                Priority::Default,
                None,
                Some("re: hi"),
                &[],
                Some("01PARENTULID"),
            )
            .unwrap();
        assert_eq!(reply.reply_to.as_deref(), Some("01PARENTULID"));
        let raw = std::fs::read_to_string(p.bus_root().join(&reply.file_path)).unwrap();
        let parsed: StoredMessage = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed.reply_to.as_deref(), Some("01PARENTULID"));
        let top = p
            .write("t/x", Priority::Default, None, Some("top"))
            .unwrap();
        assert_eq!(top.reply_to, None, "the plain wrapper sets no reply_to");
    }

    #[test]
    fn stored_message_deserializes_without_reply_to_field() {
        // BUS-2.7.d — pre-2.7.d on-disk JSON (no reply_to key) loads as None.
        let raw = r#"{"ulid":"01X","topic":"t","priority":"default","title":null,"body":"b","ts_unix_ms":0,"file_path":"t/01X.json","actions":[]}"#;
        let parsed: StoredMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.reply_to, None);
    }

    #[test]
    fn divergence_detects_orphan_file() {
        let (_tmp, p) = open_tmp();
        let msg = p
            .write("t/x", Priority::Default, None, Some("real"))
            .unwrap();
        // Plant an orphan JSON in the topic dir.
        let orphan_ulid = "01KXSQW3G6PYVGSXPB2QEQW1XK";
        let orphan_msg = stored_message("t/x", orphan_ulid, "orphan");
        let topic_dir = p.bus_root().join("t/x");
        let orphan = topic_dir.join(format!("{orphan_ulid}.json"));
        std::fs::write(&orphan, serde_json::to_string_pretty(&orphan_msg).unwrap()).unwrap();
        let report = p.detect_divergence().unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.rows_without_files, Vec::<String>::new());
        assert_eq!(report.files_without_rows.len(), 1);
        assert!(report.files_without_rows[0].contains(orphan_ulid));
        // The real message is still indexed.
        let _ = msg;
    }

    #[test]
    fn divergence_ignores_non_message_json_files() {
        let (_tmp, p) = open_tmp();
        std::fs::write(
            p.bus_root().join("settings-nav.json"),
            r#"{"group":"mesh_system","section":"network"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(p.bus_root().join("tmux")).unwrap();
        std::fs::write(
            p.bus_root().join("tmux/state.json"),
            r#"{"last_session":"main"}"#,
        )
        .unwrap();

        let report = p.detect_divergence().unwrap();
        assert!(report.is_clean(), "non-message state is informational");
        assert_eq!(
            report.ignored_non_message_files,
            vec!["settings-nav.json".to_owned(), "tmux/state.json".to_owned()]
        );
    }

    #[test]
    fn divergence_flags_invalid_ulid_named_message_file() {
        let (_tmp, p) = open_tmp();
        let bad_ulid = "01KXSQW3G6PYVGSXPB2QEQW1XM";
        let dir = p.bus_root().join("state/bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{bad_ulid}.json")), "{ not json").unwrap();

        let report = p.detect_divergence().unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.files_without_rows, Vec::<String>::new());
        assert_eq!(report.rows_without_files, Vec::<String>::new());
        assert_eq!(report.invalid_message_files.len(), 1);
        assert_eq!(
            report.invalid_message_files[0].path,
            format!("state/bad/{bad_ulid}.json")
        );
    }

    #[test]
    fn repair_reindexes_valid_message_file_without_rewriting_body() {
        let (_tmp, p) = open_tmp();
        let orphan_ulid = "01KXSQW3G6PYVGSXPB2QEQW1XN";
        let orphan_msg = stored_message("state/bookmarks/sync", orphan_ulid, "orphan-body");
        let orphan_path = p.bus_root().join(&orphan_msg.file_path);
        std::fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
        let before = serde_json::to_string_pretty(&orphan_msg).unwrap();
        std::fs::write(&orphan_path, &before).unwrap();

        let repair = p.repair_divergence(false, false).unwrap();
        assert_eq!(repair.files_reindexed, vec![orphan_msg.file_path.clone()]);
        assert!(!repair.has_unresolved());
        assert_eq!(std::fs::read_to_string(&orphan_path).unwrap(), before);
        assert!(p.detect_divergence().unwrap().is_clean());
        assert_eq!(
            p.read_latest("state/bookmarks/sync").unwrap().unwrap().ulid,
            orphan_ulid
        );
    }

    #[test]
    fn repair_reindex_is_idempotent_when_a_row_appears_during_repair() {
        let (_tmp, p) = open_tmp();
        let orphan_ulid = "01KXSQW3G6PYVGSXPB2QEQW1XP";
        let orphan_msg = stored_message("state/bookmarks/sync", orphan_ulid, "orphan-body");
        assert!(p.insert_existing_message(&orphan_msg).unwrap());
        assert!(
            !p.insert_existing_message(&orphan_msg).unwrap(),
            "a same-path row inserted by another process is already repaired"
        );
    }

    #[test]
    fn repair_reports_missing_rows_without_fabricating_files() {
        let (_tmp, p) = open_tmp();
        let msg = p
            .write(
                "state/browser-custom-filter-rules-source/test",
                Priority::Default,
                None,
                Some("gone"),
            )
            .unwrap();
        std::fs::remove_file(p.bus_root().join(&msg.file_path)).unwrap();

        let repair = p.repair_divergence(false, false).unwrap();
        assert_eq!(repair.rows_without_files, vec![msg.file_path.clone()]);
        assert!(repair.has_unresolved());
        assert!(!p.bus_root().join(&msg.file_path).exists());
    }

    #[test]
    fn repair_prunes_missing_rows_only_when_explicit_and_exports_first() {
        let (_tmp, p) = open_tmp();
        let msg = p
            .write(
                "state/browser-custom-filter-rules-source/test",
                Priority::Default,
                None,
                Some("gone"),
            )
            .unwrap();
        std::fs::remove_file(p.bus_root().join(&msg.file_path)).unwrap();

        let repair = p.repair_divergence(false, true).unwrap();
        assert_eq!(repair.rows_pruned, vec![msg.file_path.clone()]);
        let export = repair.missing_rows_export.expect("export path");
        assert!(export.starts_with(".repair/persist-missing-rows-"));
        let exported = std::fs::read_to_string(p.bus_root().join(export)).unwrap();
        assert!(exported.contains(&msg.file_path));
        assert!(p.detect_divergence().unwrap().is_clean());
    }

    #[test]
    fn divergence_detects_missing_file() {
        let (_tmp, p) = open_tmp();
        let msg = p
            .write("t/x", Priority::Default, None, Some("real"))
            .unwrap();
        let abs = p.bus_root().join(&msg.file_path);
        std::fs::remove_file(&abs).unwrap();
        let report = p.detect_divergence().unwrap();
        assert!(!report.is_clean());
        assert_eq!(report.files_without_rows, Vec::<String>::new());
        assert_eq!(report.rows_without_files, vec![msg.file_path]);
    }

    #[test]
    fn divergence_clean_when_index_matches_tree() {
        let (_tmp, p) = open_tmp();
        for _ in 0..3 {
            p.write("t/x", Priority::Default, None, Some("m")).unwrap();
        }
        let report = p.detect_divergence().unwrap();
        assert!(report.is_clean(), "expected clean: {report:?}");
    }

    #[test]
    fn ten_thousand_message_replay() {
        let (_tmp, p) = open_tmp();
        for i in 0..10_000 {
            p.write("load/test", Priority::Default, None, Some(&i.to_string()))
                .unwrap();
        }
        // 20_000 = 10k load/test messages + 10k audit/<peer> records
        // (one per publish). The per-topic count is unaffected — audit
        // records land on a different topic.
        assert_eq!(p.count().unwrap(), 20_000);
        let rows = p.list_since("load/test", None).unwrap();
        assert_eq!(rows.len(), 10_000);
        // ULIDs are monotonically increasing within a process.
        for w in rows.windows(2) {
            assert!(w[0].ulid < w[1].ulid, "ULID order broke: {w:?}");
        }
    }

    #[test]
    fn is_auditable_skips_observational_reads_but_keeps_mutations() {
        // The flood classes observed live on Eagle (2026-06-24) must NOT audit:
        for noisy in [
            "audit/UNIT-EAGLE",   // recursion guard
            "state/voice/status", // observational broadcast
            "state/boot-readiness",
            "reply/abc",              // query response
            "action/music/get-state", // the dominant poller (~36% of the flood)
            "action/music/list-starred",
            "action/music/list-frequent",
            "action/music/peer-states",
            "action/music/library-stats",
            "action/clipboard/list",
            "action/mesh/directory",
        ] {
            assert!(
                !super::is_auditable(noisy),
                "{noisy} must NOT be audited (read/observational)"
            );
        }
        // Control-plane mutations + events MUST still be audited (§8):
        for sec in [
            "action/enroll/accept",
            "action/ca/revoke",
            "action/provision/spawn",
            "action/music/play",
            "action/role/pin",
            "event/security/alert",
            "action/connect/expose",
        ] {
            assert!(
                super::is_auditable(sec),
                "{sec} must still be audited (mutation/event)"
            );
        }
    }
}
