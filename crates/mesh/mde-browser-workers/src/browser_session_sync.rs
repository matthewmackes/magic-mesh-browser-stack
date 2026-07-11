//! BROWSER-DD-7 — Browser session-sync owner.
//!
//! The Browser shell publishes `action/browser/session-sync` snapshots and
//! `action/browser/send-tab` handoffs for the state it owns. This worker is the
//! mesh-side owner for those streams: it drains the Bus, validates the Browser
//! payload shapes, persists the latest restore snapshot locally, mirrors the same
//! JSON into the Syncthing-backed workgroup root, and materializes send-tab
//! handoffs into a replicated outbox. Snapshot file bodies remain the exact
//! Browser snapshot shape so the startup-restore parser can consume them directly;
//! no wrapper envelope is inserted between sync and restore.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Browser-owned session snapshot action topic.
pub const ACTION_TOPIC: &str = "action/browser/session-sync";

/// Browser-owned send-tab action topic.
pub const SEND_TAB_TOPIC: &str = "action/browser/send-tab";

/// Existing KDE Connect phone-share verb used for phone-targeted send-tab delivery.
const ACTION_CONNECT_SHARE: &str = "action/connect/share";

/// Retained-latest status topic for this node.
pub const STATE_PREFIX: &str = "state/browser-session-sync/";

/// Share/local subdirectory holding per-host latest snapshots.
pub const SESSION_SYNC_SUBDIR: &str = "browser-session-sync";

/// Latest snapshot filename. Its body is the Browser snapshot JSON itself.
pub const LATEST_FILE: &str = "latest.json";

/// Share/local subdirectory holding durable send-tab handoff outbox records.
pub const SEND_TAB_OUTBOX_SUBDIR: &str = "browser-send-tab";

/// Default poll cadence. The Browser dedupes snapshots before publish; the worker
/// can poll frequently without a file-write storm.
pub const DEFAULT_TICK: Duration = Duration::from_secs(2);

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Published status for this node's browser session-sync owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionSyncStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// True when the latest local snapshot is mirrored to the shared root.
    pub syncing: bool,
    /// True when a valid local snapshot still needs a shared-root mirror.
    pub pending_local: bool,
    /// Browser host name from the most recent accepted snapshot.
    pub last_host: Option<String>,
    /// Local persist timestamp for the most recent accepted snapshot.
    pub last_snapshot_ms: Option<u64>,
    /// Shared-root mirror timestamp for the most recent successful mirror.
    pub last_mirror_ms: Option<u64>,
}

/// Worker that persists Browser session-sync snapshots for startup restore.
pub struct BrowserSessionSyncWorker {
    node: String,
    local_root: PathBuf,
    share_root: PathBuf,
    cursor: Option<String>,
    send_tab_cursor: Option<String>,
    last_host: Option<String>,
    last_snapshot_ms: Option<u64>,
    last_mirror_ms: Option<u64>,
    pending_local: bool,
    tick: Duration,
    now_fn: NowFn,
    share_gate: Option<Arc<AtomicBool>>,
    bus_root_override: Option<PathBuf>,
}

impl BrowserSessionSyncWorker {
    /// Create a Browser session-sync worker for one node and workgroup share.
    #[must_use]
    pub fn new(node: String, local_root: PathBuf, share_root: PathBuf) -> Self {
        Self {
            node,
            local_root,
            share_root,
            cursor: None,
            send_tab_cursor: None,
            last_host: None,
            last_snapshot_ms: None,
            last_mirror_ms: None,
            pending_local: false,
            tick: DEFAULT_TICK,
            now_fn: Arc::new(default_now),
            share_gate: None,
            bus_root_override: None,
        }
    }

    /// Override the worker polling interval.
    #[must_use]
    pub const fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Override the clock used for deterministic tests.
    #[must_use]
    pub fn with_now_fn(mut self, now: NowFn) -> Self {
        self.now_fn = now;
        self
    }

    /// Override shared-root availability with a test-controlled gate.
    #[must_use]
    pub fn with_share_gate(mut self, gate: Arc<AtomicBool>) -> Self {
        self.share_gate = Some(gate);
        self
    }

    /// Override the Bus root used by `Persist`.
    #[must_use]
    pub fn with_bus_root(mut self, root: PathBuf) -> Self {
        self.bus_root_override = Some(root);
        self
    }

    fn now_ms(&self) -> u64 {
        (self.now_fn)()
    }

    fn share_writable(&self) -> bool {
        self.share_gate.as_ref().map_or_else(
            || mackes_mesh_types::mesh_storage::shared_root_writable(&self.share_root),
            |g| g.load(Ordering::SeqCst),
        )
    }

    fn drain_snapshots(&mut self, persist: &Persist) {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_session_sync", error = %e, "list_since failed");
                return;
            }
        };
        for msg in msgs {
            self.cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_snapshot(&body) {
                Ok(snapshot) => self.apply_snapshot(snapshot, persist),
                Err(e) => {
                    tracing::warn!(
                        target: "mackesd::browser_session_sync",
                        ulid = %msg.ulid,
                        error = %e,
                        "discarding malformed browser session snapshot"
                    );
                }
            }
        }
    }

    fn drain_send_tabs(&mut self, persist: &Persist) {
        let msgs = match persist.list_since(SEND_TAB_TOPIC, self.send_tab_cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_session_sync", error = %e, "send-tab list_since failed");
                return;
            }
        };
        for msg in msgs {
            self.send_tab_cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_send_tab(&body, &msg.ulid) {
                Ok(handoff) => self.apply_send_tab(handoff, persist),
                Err(e) => {
                    tracing::warn!(
                        target: "mackesd::browser_session_sync",
                        ulid = %msg.ulid,
                        error = %e,
                        "discarding malformed browser send-tab handoff"
                    );
                }
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: BrowserSessionSnapshot, persist: &Persist) {
        let path = latest_path(&self.local_root, &snapshot.host);
        if let Err(e) = write_atomic(&path, &snapshot.body) {
            tracing::warn!(
                target: "mackesd::browser_session_sync",
                path = %path.display(),
                error = %e,
                "failed to persist local browser session snapshot"
            );
            return;
        }
        self.last_host = Some(snapshot.host.clone());
        self.last_snapshot_ms = Some(self.now_ms());
        self.pending_local = true;
        self.mirror_pending();
        self.publish_status(persist);
    }

    fn mirror_pending(&mut self) {
        if !self.share_writable() {
            return;
        }
        let Some(host) = self.last_host.clone() else {
            return;
        };
        let src = latest_path(&self.local_root, &host);
        let Ok(body) = std::fs::read_to_string(&src) else {
            return;
        };
        let dst = latest_path(&self.share_root, &host);
        if let Err(e) = write_atomic(&dst, &body) {
            tracing::debug!(
                target: "mackesd::browser_session_sync",
                path = %dst.display(),
                error = %e,
                "browser session snapshot mirror skipped"
            );
            return;
        }
        self.pending_local = false;
        self.last_mirror_ms = Some(self.now_ms());
    }

    fn apply_send_tab(&mut self, handoff: BrowserSendTabHandoff, persist: &Persist) {
        let path = send_tab_path(
            &self.local_root,
            &handoff.target,
            &handoff.target_id,
            &handoff.source_host,
            &handoff.id,
        );
        if let Err(e) = write_atomic(&path, &handoff.body) {
            tracing::warn!(
                target: "mackesd::browser_session_sync",
                path = %path.display(),
                error = %e,
                "failed to persist local browser send-tab handoff"
            );
            return;
        }
        self.publish_phone_send_tab(&handoff, persist);
        self.mirror_send_tab_outbox();
    }

    fn publish_phone_send_tab(&self, handoff: &BrowserSendTabHandoff, persist: &Persist) {
        if handoff.target != "phone" {
            return;
        }
        let body = serde_json::json!({
            "device_id": handoff.target_id,
            "url": handoff.url,
            "open": true,
            "source": "browser_send_tab",
            "source_host": handoff.source_host,
            "handoff_id": handoff.id,
        })
        .to_string();
        if let Err(e) = persist.write(ACTION_CONNECT_SHARE, Priority::Default, None, Some(&body)) {
            tracing::warn!(
                target: "mackesd::browser_session_sync",
                device = %handoff.target_id,
                error = %e,
                "failed to publish phone browser send-tab handoff"
            );
        }
    }

    fn mirror_send_tab_outbox(&self) {
        if !self.share_writable() {
            return;
        }
        for (rel, body) in local_outbox_entries(&self.local_root) {
            let dst = self.share_root.join(SEND_TAB_OUTBOX_SUBDIR).join(rel);
            if let Err(e) = write_atomic(&dst, &body) {
                tracing::debug!(
                    target: "mackesd::browser_session_sync",
                    path = %dst.display(),
                    error = %e,
                    "browser send-tab outbox mirror skipped"
                );
            }
        }
    }

    fn publish_status(&self, persist: &Persist) {
        let status = SessionSyncStatus {
            node: self.node.clone(),
            syncing: self.share_writable() && !self.pending_local,
            pending_local: self.pending_local,
            last_host: self.last_host.clone(),
            last_snapshot_ms: self.last_snapshot_ms,
            last_mirror_ms: self.last_mirror_ms,
        };
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&status) {
            let _ = persist.write(&topic, Priority::Min, None, Some(&body));
        }
    }
}

#[async_trait::async_trait]
impl Worker for BrowserSessionSyncWorker {
    fn name(&self) -> &'static str {
        "browser_session_sync"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_session_sync", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_session_sync", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.mirror_pending();
        self.mirror_send_tab_outbox();
        self.publish_status(&persist);
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.drain_snapshots(&persist);
                    self.drain_send_tabs(&persist);
                    self.mirror_pending();
                    self.mirror_send_tab_outbox();
                    self.publish_status(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        self.mirror_pending();
        self.mirror_send_tab_outbox();
        self.publish_status(&persist);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserSessionSnapshot {
    host: String,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserSendTabHandoff {
    id: String,
    target: String,
    target_id: String,
    source_host: String,
    url: String,
    body: String,
}

fn parse_snapshot(body: &str) -> Result<BrowserSessionSnapshot, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("snapshot JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_session_sync") {
        return Err("wrong op".to_owned());
    }
    if !v.get("settings").is_some_and(serde_json::Value::is_object) {
        return Err("missing settings object".to_owned());
    }
    if !v.get("tabs").is_some_and(serde_json::Value::is_array) {
        return Err("missing tabs array".to_owned());
    }
    let host = v
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "missing host".to_owned())?;
    let host = sanitize_host(host);
    if host.is_empty() {
        return Err("host has no safe path characters".to_owned());
    }
    let body = serde_json::to_string_pretty(&v).map_err(|e| format!("snapshot encode: {e}"))?;
    Ok(BrowserSessionSnapshot { host, body })
}

fn parse_send_tab(body: &str, id: &str) -> Result<BrowserSendTabHandoff, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("send-tab JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_send_tab") {
        return Err("wrong op".to_owned());
    }
    let target = v
        .get("target")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|target| matches!(*target, "node" | "phone"))
        .ok_or_else(|| "missing supported target".to_owned())?;
    let source_host = v
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "missing host".to_owned())?;
    let target_id = v
        .get("target_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|target_id| !target_id.is_empty())
        .unwrap_or(target);
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "missing url".to_owned())?;
    if !v
        .get("engine")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|engine| matches!(engine, "servo" | "cef"))
    {
        return Err("missing supported engine".to_owned());
    }
    let source_host = sanitize_host(source_host);
    if source_host.is_empty() {
        return Err("host has no safe path characters".to_owned());
    }
    let target_id = sanitize_host(target_id);
    if target_id.is_empty() {
        return Err("target_id has no safe path characters".to_owned());
    }
    let id = sanitize_host(id);
    if id.is_empty() {
        return Err("id has no safe path characters".to_owned());
    }
    let body = serde_json::to_string_pretty(&v).map_err(|e| format!("send-tab encode: {e}"))?;
    Ok(BrowserSendTabHandoff {
        id,
        target: target.to_owned(),
        target_id,
        source_host,
        url: url.to_owned(),
        body,
    })
}

fn sanitize_host(host: &str) -> String {
    host.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                Some(c)
            } else if c.is_ascii_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// Return the latest snapshot path under a root for a sanitized host name.
#[must_use]
pub fn latest_path(root: &Path, host: &str) -> PathBuf {
    root.join(SESSION_SYNC_SUBDIR)
        .join(sanitize_host(host))
        .join(LATEST_FILE)
}

/// Return the durable send-tab outbox path for a target class, source host, and id.
#[must_use]
pub fn send_tab_path(
    root: &Path,
    target: &str,
    target_id: &str,
    source_host: &str,
    id: &str,
) -> PathBuf {
    root.join(SEND_TAB_OUTBOX_SUBDIR)
        .join(sanitize_host(target))
        .join(sanitize_host(target_id))
        .join(sanitize_host(source_host))
        .join(format!("{}.json", sanitize_host(id)))
}

fn local_outbox_entries(root: &Path) -> Vec<(PathBuf, String)> {
    let base = root.join(SEND_TAB_OUTBOX_SUBDIR);
    let Ok(targets) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for target in targets.filter_map(Result::ok) {
        let target_path = target.path();
        if !target_path.is_dir() {
            continue;
        }
        let Ok(target_ids) = std::fs::read_dir(&target_path) else {
            continue;
        };
        for target_id in target_ids.filter_map(Result::ok) {
            let target_id_path = target_id.path();
            if !target_id_path.is_dir() {
                continue;
            }
            let Ok(hosts) = std::fs::read_dir(&target_id_path) else {
                continue;
            };
            for host in hosts.filter_map(Result::ok) {
                let host_path = host.path();
                if !host_path.is_dir() {
                    continue;
                }
                let Ok(files) = std::fs::read_dir(&host_path) else {
                    continue;
                };
                for file in files.filter_map(Result::ok) {
                    let path = file.path();
                    if path.extension().is_none_or(|ext| ext != "json") {
                        continue;
                    }
                    let Ok(body) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    if let Ok(rel) = path.strip_prefix(&base) {
                        out.push((rel.to_path_buf(), body));
                    }
                }
            }
        }
    }
    out
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// Resolve the local durable session-sync root for this host.
#[must_use]
pub fn resolve_local_root() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("/var/lib/mde/browser-session-sync"),
        |d| d.join("mde").join("browser-session-sync"),
    )
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(host: &str, url: &str) -> String {
        serde_json::json!({
            "op": "browser_session_sync",
            "source": "browser",
            "host": host,
            "active_index": 0,
            "settings": {
                "future_engine": "cef",
                "vertical_tabs": true,
                "page_zoom_percent": 110,
                "speed_dial": [{"label": "Ops", "url": "https://ops.mesh/", "hint": "Ops"}]
            },
            "tabs": [{"index": 0, "engine": "cef", "url": url}],
            "downloads": []
        })
        .to_string()
    }

    fn send_tab(target: &str, host: &str, url: &str) -> String {
        serde_json::json!({
            "op": "browser_send_tab",
            "target": target,
            "engine": "cef",
            "url": url,
            "title": "Example",
            "preview": "Example",
            "source": "browser",
            "host": host
        })
        .to_string()
    }

    fn send_tab_with_target_id(target: &str, target_id: &str, host: &str, url: &str) -> String {
        let mut v: serde_json::Value = serde_json::from_str(&send_tab(target, host, url)).unwrap();
        v["target_id"] = serde_json::json!(target_id);
        v["target_label"] = serde_json::json!(target_id);
        v.to_string()
    }

    #[test]
    fn parse_snapshot_preserves_the_startup_restore_shape() {
        let parsed = parse_snapshot(&snapshot("work station/1", "https://example.test/")).unwrap();
        assert_eq!(parsed.host, "work-station1");
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["op"], "browser_session_sync");
        assert_eq!(v["settings"]["speed_dial"][0]["label"], "Ops");
        assert_eq!(v["tabs"][0]["url"], "https://example.test/");
    }

    #[test]
    fn parse_snapshot_rejects_the_wrong_shape() {
        assert!(parse_snapshot("{}").is_err());
        assert!(
            parse_snapshot(r#"{"op":"browser_send_tab","settings":{},"tabs":[],"host":"h"}"#)
                .is_err()
        );
        assert!(
            parse_snapshot(r#"{"op":"browser_session_sync","settings":{},"host":"h"}"#).is_err()
        );
    }

    #[test]
    fn parse_send_tab_preserves_the_browser_handoff_shape() {
        let parsed = parse_send_tab(
            &send_tab("phone", "work station/1", "https://example.test/"),
            "01ABC",
        )
        .unwrap();
        assert_eq!(parsed.id, "01ABC");
        assert_eq!(parsed.target, "phone");
        assert_eq!(parsed.target_id, "phone");
        assert_eq!(parsed.source_host, "work-station1");
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["op"], "browser_send_tab");
        assert_eq!(v["target"], "phone");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://example.test/");
    }

    #[test]
    fn parse_send_tab_uses_concrete_target_id_when_present() {
        let parsed = parse_send_tab(
            &send_tab_with_target_id("node", "eagle seat/1", "node-a", "https://example.test/"),
            "01ABC",
        )
        .unwrap();
        assert_eq!(parsed.target, "node");
        assert_eq!(parsed.target_id, "eagle-seat1");
    }

    #[test]
    fn parse_send_tab_rejects_unrouteable_handoffs() {
        assert!(parse_send_tab("{}", "01").is_err());
        assert!(
            parse_send_tab(&send_tab("email", "node-a", "https://example.test/"), "01").is_err()
        );
        assert!(parse_send_tab(&send_tab("node", "node-a", ""), "01").is_err());
        assert!(
            parse_send_tab(
                r#"{"op":"browser_send_tab","target":"node","engine":"webkit","url":"https://example.test/","host":"node-a"}"#,
                "01"
            )
            .is_err()
        );
    }

    #[test]
    fn apply_snapshot_writes_local_and_mirrors_when_share_is_up() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(true));
        let mut worker = BrowserSessionSyncWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate)
        .with_now_fn(Arc::new(|| 42));
        let snap = parse_snapshot(&snapshot("node-a", "https://mesh.test/")).unwrap();

        worker.apply_snapshot(snap, &persist);

        let local_body = std::fs::read_to_string(latest_path(local.path(), "node-a")).unwrap();
        let share_body = std::fs::read_to_string(latest_path(share.path(), "node-a")).unwrap();
        assert_eq!(local_body, share_body);
        assert!(!worker.pending_local);
        assert_eq!(worker.last_mirror_ms, Some(42));
        let status = persist
            .list_since("state/browser-session-sync/node-a", None)
            .unwrap()
            .pop()
            .unwrap();
        let status: SessionSyncStatus =
            serde_json::from_str(status.body.as_deref().unwrap()).unwrap();
        assert!(status.syncing);
        assert_eq!(status.last_host.as_deref(), Some("node-a"));
    }

    #[test]
    fn apply_snapshot_keeps_local_pending_when_share_is_down() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let mut worker = BrowserSessionSyncWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate.clone());
        let snap = parse_snapshot(&snapshot("node-a", "https://mesh.test/")).unwrap();

        worker.apply_snapshot(snap, &persist);

        assert!(latest_path(local.path(), "node-a").is_file());
        assert!(!latest_path(share.path(), "node-a").exists());
        assert!(worker.pending_local);
        gate.store(true, Ordering::SeqCst);
        worker.mirror_pending();
        assert!(latest_path(share.path(), "node-a").is_file());
        assert!(!worker.pending_local);
    }

    #[test]
    fn apply_send_tab_writes_local_and_mirrors_when_share_is_up() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(true));
        let mut worker = BrowserSessionSyncWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate);
        let handoff = parse_send_tab(
            &send_tab("node", "node-a", "https://mesh.test/"),
            "01Handoff",
        )
        .unwrap();

        worker.apply_send_tab(handoff, &persist);

        let local_body = std::fs::read_to_string(send_tab_path(
            local.path(),
            "node",
            "node",
            "node-a",
            "01Handoff",
        ))
        .unwrap();
        let share_body = std::fs::read_to_string(send_tab_path(
            share.path(),
            "node",
            "node",
            "node-a",
            "01Handoff",
        ))
        .unwrap();
        assert_eq!(local_body, share_body);
        let v: serde_json::Value = serde_json::from_str(&share_body).unwrap();
        assert_eq!(v["url"], "https://mesh.test/");
        assert!(
            persist
                .list_since(ACTION_CONNECT_SHARE, None)
                .unwrap()
                .is_empty(),
            "node send-tab records do not publish KDE Connect phone shares"
        );
    }

    #[test]
    fn send_tab_outbox_mirrors_pending_local_entries_when_share_returns() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let mut worker = BrowserSessionSyncWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate.clone());
        let handoff = parse_send_tab(
            &send_tab_with_target_id("phone", "pixel-8", "node-a", "https://mesh.test/"),
            "01Phone",
        )
        .unwrap();

        worker.apply_send_tab(handoff, &persist);

        assert!(send_tab_path(local.path(), "phone", "pixel-8", "node-a", "01Phone").is_file());
        assert!(!send_tab_path(share.path(), "phone", "pixel-8", "node-a", "01Phone").exists());
        gate.store(true, Ordering::SeqCst);
        worker.mirror_send_tab_outbox();
        assert!(send_tab_path(share.path(), "phone", "pixel-8", "node-a", "01Phone").is_file());
    }

    #[test]
    fn phone_send_tab_publishes_the_existing_kde_connect_share_verb() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserSessionSyncWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        );
        let handoff = parse_send_tab(
            &send_tab_with_target_id("phone", "pixel-8", "node-a", "https://mesh.test/"),
            "01Phone",
        )
        .unwrap();

        worker.apply_send_tab(handoff, &persist);

        let msgs = persist.list_since(ACTION_CONNECT_SHARE, None).unwrap();
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("share body");
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["device_id"], "pixel-8");
        assert_eq!(v["url"], "https://mesh.test/");
        assert_eq!(v["open"], true);
        assert_eq!(v["source"], "browser_send_tab");
        assert_eq!(v["source_host"], "node-a");
        assert_eq!(v["handoff_id"], "01Phone");
    }
}
