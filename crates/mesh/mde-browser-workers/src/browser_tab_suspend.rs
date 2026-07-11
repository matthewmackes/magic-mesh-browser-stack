//! BROWSER-DD-12 — Browser idle-tab suspend owner.
//!
//! The Browser shell owns the UI decision and stops the inactive helper locally,
//! then publishes `action/browser/tab-suspend`. This worker owns the daemon side
//! of that handoff: it validates the Browser-origin request, records the accepted
//! suspend in retained state, and emits a bounded event for fleet/diagnostic
//! consumers. It does not fabricate deeper engine hibernation that the helper
//! process model does not expose.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Browser-owned idle-tab suspend request topic.
pub const ACTION_TOPIC: &str = "action/browser/tab-suspend";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-tab-suspend/";

/// Accepted suspend event topic prefix for this node.
pub const RESULT_PREFIX: &str = "event/browser-tab-suspend/";

/// Default poll cadence. Idle suspend handoffs are sparse and shell-driven.
pub const DEFAULT_TICK: Duration = Duration::from_secs(2);

const MAX_URL_CHARS: usize = 4_096;
const MAX_TITLE_CHARS: usize = 512;
const MAX_HOST_CHARS: usize = 128;
const MAX_IDLE_AFTER_MS: u64 = 24 * 60 * 60 * 1000;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Parsed Browser idle-tab suspend request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSuspendRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Browser host that published the request.
    pub host: String,
    /// Tab index in the Browser shell that was suspended.
    pub tab_index: u64,
    /// Browser engine wire label.
    pub engine: String,
    /// Page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Idle timeout that caused the shell suspend, in milliseconds.
    pub idle_after_ms: u64,
}

/// Retained status for this node's Browser idle-tab suspend owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TabSuspendStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent request id, if any request was accepted.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Page URL from the most recent accepted request.
    pub last_url: Option<String>,
    /// Browser engine from the most recent accepted request.
    pub last_engine: Option<String>,
    /// Tab index from the most recent accepted request.
    pub last_tab_index: Option<u64>,
    /// Idle timeout from the most recent accepted request.
    pub last_idle_after_ms: Option<u64>,
    /// Outcome state: `idle`, `suspended`, or `error`.
    pub state: String,
    /// Last validation error, if any.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Timestamp of the most recent accepted request.
    pub last_suspend_ms: Option<u64>,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser idle-tab suspend handoffs.
pub struct BrowserTabSuspendWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    bus_root_override: Option<std::path::PathBuf>,
    status: TabSuspendStatus,
}

impl BrowserTabSuspendWorker {
    /// Create a Browser idle-tab suspend worker for one node.
    #[must_use]
    pub fn new(node: String) -> Self {
        let now_fn: NowFn = Arc::new(default_now);
        let updated_ms = now_fn();
        Self {
            node: node.clone(),
            cursor: None,
            tick: DEFAULT_TICK,
            now_fn,
            bus_root_override: None,
            status: TabSuspendStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_url: None,
                last_engine: None,
                last_tab_index: None,
                last_idle_after_ms: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                rejected: 0,
                last_suspend_ms: None,
                updated_ms,
            },
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

    /// Override the Bus root used by `Persist`.
    #[must_use]
    pub fn with_bus_root(mut self, root: std::path::PathBuf) -> Self {
        self.bus_root_override = Some(root);
        self
    }

    fn now_ms(&self) -> u64 {
        (self.now_fn)()
    }

    fn drain_requests(&mut self, persist: &Persist) {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_tab_suspend", error = %e, "list_since failed");
                return;
            }
        };
        for msg in msgs {
            self.cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_request(&body, &msg.ulid) {
                Ok(request) => self.apply_request(persist, request),
                Err(e) => {
                    self.status.rejected = self.status.rejected.saturating_add(1);
                    self.status.state = "error".to_owned();
                    self.status.last_error = Some(e);
                    self.status.updated_ms = self.now_ms();
                    self.publish_status(persist);
                }
            }
        }
    }

    fn apply_request(&mut self, persist: &Persist, request: TabSuspendRequest) {
        let now = self.now_ms();
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_url = Some(request.url.clone());
        self.status.last_engine = Some(request.engine.clone());
        self.status.last_tab_index = Some(request.tab_index);
        self.status.last_idle_after_ms = Some(request.idle_after_ms);
        self.status.state = "suspended".to_owned();
        self.status.last_error = None;
        self.status.last_suspend_ms = Some(now);
        self.status.updated_ms = now;
        self.publish_event(persist, &request, now);
        self.publish_status(persist);
    }

    fn publish_status(&self, persist: &Persist) {
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&self.status) {
            let _ = persist.write(&topic, Priority::Min, None, Some(&body));
        }
    }

    fn publish_event(&self, persist: &Persist, request: &TabSuspendRequest, suspended_ms: u64) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_tab_suspended",
            "source": "browser_tab_suspend",
            "node": self.node,
            "request_id": &request.id,
            "host": &request.host,
            "tab_index": request.tab_index,
            "engine": &request.engine,
            "url": &request.url,
            "title": &request.title,
            "idle_after_ms": request.idle_after_ms,
            "suspended_ms": suspended_ms,
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }
}

#[async_trait::async_trait]
impl Worker for BrowserTabSuspendWorker {
    fn name(&self) -> &'static str {
        "browser_tab_suspend"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_tab_suspend", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_tab_suspend", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.publish_status(&persist);
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.drain_requests(&persist);
                    self.publish_status(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        self.publish_status(&persist);
        Ok(())
    }
}

fn parse_request(body: &str, id: &str) -> Result<TabSuspendRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("tab-suspend JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_tab_suspend") {
        return Err("wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("wrong source".to_owned());
    }
    let host = required_string(&v, "host", MAX_HOST_CHARS)?;
    if !valid_host(&host) {
        return Err("invalid host".to_owned());
    }
    let engine = required_string(&v, "engine", 16)?;
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("unsupported engine".to_owned());
    }
    let url = required_string(&v, "url", MAX_URL_CHARS)?;
    if url.trim().is_empty() {
        return Err("empty URL".to_owned());
    }
    let title = optional_string(&v, "title", MAX_TITLE_CHARS)?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing tab_index".to_owned())?;
    let idle_after_ms = v
        .get("idle_after_ms")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing idle_after_ms".to_owned())?;
    if idle_after_ms == 0 || idle_after_ms > MAX_IDLE_AFTER_MS {
        return Err("invalid idle_after_ms".to_owned());
    }

    Ok(TabSuspendRequest {
        id: id.to_owned(),
        host,
        tab_index,
        engine,
        url,
        title,
        idle_after_ms,
    })
}

fn required_string(v: &serde_json::Value, key: &str, max_chars: usize) -> Result<String, String> {
    let value = v
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing {key}"))?;
    bounded_string(value, key, max_chars)
}

fn optional_string(v: &serde_json::Value, key: &str, max_chars: usize) -> Result<String, String> {
    let Some(value) = v.get(key).and_then(serde_json::Value::as_str) else {
        return Ok(String::new());
    };
    bounded_string(value, key, max_chars)
}

fn bounded_string(value: &str, key: &str, max_chars: usize) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.chars().count() > max_chars {
        return Err(format!("{key} is too long"));
    }
    Ok(trimmed.to_owned())
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mde_bus::hooks::config::Priority;
    use mde_bus::persist::Persist;

    use super::*;

    fn suspend_body() -> String {
        serde_json::json!({
            "op": "browser_tab_suspend",
            "source": "browser",
            "host": "mesh-node-1",
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/page",
            "title": "Example",
            "idle_after_ms": 1_800_000_u64,
        })
        .to_string()
    }

    #[test]
    fn parse_request_accepts_browser_idle_suspend_payload() {
        let request = parse_request(&suspend_body(), "01H").expect("valid request");

        assert_eq!(request.id, "01H");
        assert_eq!(request.host, "mesh-node-1");
        assert_eq!(request.tab_index, 2);
        assert_eq!(request.engine, "cef");
        assert_eq!(request.url, "https://example.test/page");
        assert_eq!(request.title, "Example");
        assert_eq!(request.idle_after_ms, 1_800_000);
    }

    #[test]
    fn parse_request_rejects_non_browser_or_malformed_payloads() {
        assert!(parse_request(r#"{"op":"browser_tab_suspend","source":"cloud"}"#, "x").is_err());
        assert!(parse_request(&suspend_body().replace("\"cef\"", "\"webkit\""), "x").is_err());
        assert!(
            parse_request(&suspend_body().replace("1800000", "0"), "x").is_err(),
            "zero idle timeout is not a real idle policy"
        );
        assert!(
            parse_request(&suspend_body().replace("mesh-node-1", "../bad"), "x").is_err(),
            "host names stay identifier-like"
        );
    }

    #[test]
    fn apply_request_publishes_status_and_suspend_event() {
        let bus = tempfile::tempdir().expect("bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("persist");
        let mut worker =
            BrowserTabSuspendWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 123_456));
        let request = parse_request(&suspend_body(), "req-1").expect("request");

        worker.apply_request(&persist, request);

        let status_body = persist
            .list_since("state/browser-tab-suspend/node-a", None)
            .expect("list status")[0]
            .body
            .clone()
            .expect("status body");
        let status: TabSuspendStatus = serde_json::from_str(&status_body).expect("status JSON");
        assert_eq!(status.state, "suspended");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.last_request_id.as_deref(), Some("req-1"));
        assert_eq!(status.last_tab_index, Some(2));
        assert_eq!(status.last_suspend_ms, Some(123_456));

        let event_body = persist
            .list_since("event/browser-tab-suspend/node-a", None)
            .expect("list event")[0]
            .body
            .clone()
            .expect("event body");
        let event: serde_json::Value = serde_json::from_str(&event_body).expect("event JSON");
        assert_eq!(event["op"], "browser_tab_suspended");
        assert_eq!(event["source"], "browser_tab_suspend");
        assert_eq!(event["request_id"], "req-1");
        assert_eq!(event["host"], "mesh-node-1");
        assert_eq!(event["engine"], "cef");
        assert_eq!(event["suspended_ms"], 123_456);
    }

    #[test]
    fn drain_requests_tracks_rejections_and_does_not_replay() {
        let bus = tempfile::tempdir().expect("bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("persist");
        persist
            .write(
                ACTION_TOPIC,
                Priority::Default,
                None,
                Some(r#"{"op":"wrong"}"#),
            )
            .expect("write bad");
        persist
            .write(ACTION_TOPIC, Priority::Default, None, Some(&suspend_body()))
            .expect("write good");
        let mut worker =
            BrowserTabSuspendWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 777));

        worker.drain_requests(&persist);
        worker.drain_requests(&persist);

        assert_eq!(worker.status.rejected, 1);
        assert_eq!(worker.status.accepted, 1);
        assert_eq!(worker.status.state, "suspended");
        let events = persist
            .list_since("event/browser-tab-suspend/node-a", None)
            .expect("list events");
        assert_eq!(events.len(), 1, "cursor prevents replay");
    }
}
