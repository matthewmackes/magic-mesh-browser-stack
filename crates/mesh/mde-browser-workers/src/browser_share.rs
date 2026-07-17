//! BROWSER-DD-12 — Browser platform-share handoff owner.
//!
//! The Browser shell publishes `action/browser/share` for platform targets whose
//! delivery is owned outside the Browser surface. This worker validates those
//! Browser-origin handoffs, records the accepted route, and emits a bounded event
//! for downstream Peer/Email/QR owners. It does not fake peer delivery, Email
//! composition, or QR rendering.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

use crate::RetainedStatusPublisher;

/// Browser-owned platform-share handoff topic.
pub const ACTION_TOPIC: &str = "action/browser/share";

/// Existing KDE Connect phone-share verb used for phone-targeted Browser shares.
const ACTION_CONNECT_SHARE: &str = "action/connect/share";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-share/";

/// Accepted share route event topic prefix for this node.
pub const RESULT_PREFIX: &str = "event/browser-share/";

/// Default poll cadence. Browser share handoffs are explicit user actions.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

const MAX_URL_CHARS: usize = 8_192;
const MAX_TEXT_CHARS: usize = 512;
const MAX_HOST_CHARS: usize = 128;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Parsed Browser platform-share handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Browser host that published the request.
    pub host: String,
    /// Target platform owner (`peer`, `phone`, `email`, or `qr`).
    pub target: String,
    /// Concrete target id for owners that need it (`phone` -> KDE Connect device id).
    pub target_id: Option<String>,
    /// Page URL to share.
    pub url: String,
    /// Page title at share time.
    pub title: String,
    /// Short preview shown by downstream surfaces.
    pub preview: String,
}

/// Retained status for this node's Browser share owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShareStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent accepted request id.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Target owner from the most recent accepted request.
    pub last_target: Option<String>,
    /// Concrete downstream target id from the most recent accepted request.
    pub last_target_id: Option<String>,
    /// URL from the most recent accepted request.
    pub last_url: Option<String>,
    /// Preview from the most recent accepted request.
    pub last_preview: Option<String>,
    /// Outcome state: `idle`, `routed`, or `error`.
    pub state: String,
    /// Last validation error, if any.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Timestamp of the most recent accepted route.
    pub last_routed_ms: Option<u64>,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser platform-share handoffs.
pub struct BrowserShareWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    bus_root_override: Option<std::path::PathBuf>,
    status: ShareStatus,
    status_publisher: RetainedStatusPublisher,
}

impl BrowserShareWorker {
    /// Create a Browser share worker for one node.
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
            status: ShareStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_target: None,
                last_target_id: None,
                last_url: None,
                last_preview: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                rejected: 0,
                last_routed_ms: None,
                updated_ms,
            },
            status_publisher: RetainedStatusPublisher::new(),
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
                tracing::debug!(target: "mackesd::browser_share", error = %e, "list_since failed");
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

    fn apply_request(&mut self, persist: &Persist, request: ShareRequest) {
        let now = self.now_ms();
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_target = Some(request.target.clone());
        self.status.last_target_id = request.target_id.clone();
        self.status.last_url = Some(request.url.clone());
        self.status.last_preview = Some(request.preview.clone());
        self.status.state = "routed".to_owned();
        self.status.last_error = None;
        self.status.last_routed_ms = Some(now);
        self.status.updated_ms = now;
        self.publish_event(persist, &request, now);
        self.publish_phone_share(persist, &request);
        self.publish_status(persist);
    }

    fn publish_status(&mut self, persist: &Persist) {
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&self.status) {
            self.status_publisher
                .publish(persist, &topic, Priority::Min, body);
        }
    }

    fn publish_event(&self, persist: &Persist, request: &ShareRequest, routed_ms: u64) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": self.node,
            "request_id": &request.id,
            "host": &request.host,
            "target": &request.target,
            "target_id": &request.target_id,
            "url": &request.url,
            "title": &request.title,
            "preview": &request.preview,
            "routed_ms": routed_ms,
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }

    fn publish_phone_share(&self, persist: &Persist, request: &ShareRequest) {
        if request.target != "phone" {
            return;
        }
        let Some(device_id) = request.target_id.as_deref() else {
            return;
        };
        let body = serde_json::json!({
            "device_id": device_id,
            "url": request.url,
            "open": true,
            "source": "browser_share",
            "source_host": request.host,
            "handoff_id": request.id,
        })
        .to_string();
        if let Err(e) = persist.write(ACTION_CONNECT_SHARE, Priority::Default, None, Some(&body)) {
            tracing::warn!(
                target: "mackesd::browser_share",
                device = %device_id,
                error = %e,
                "failed to publish phone browser share handoff"
            );
        }
    }
}

#[async_trait::async_trait]
impl Worker for BrowserShareWorker {
    fn name(&self) -> &'static str {
        "browser_share"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_share", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_share", error = %e, "persist open failed; worker idle");
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

fn parse_request(body: &str, id: &str) -> Result<ShareRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("browser-share JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_share") {
        return Err("wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("wrong source".to_owned());
    }
    let host = required_string(&v, "host", MAX_HOST_CHARS)?;
    if !valid_host(&host) {
        return Err("invalid host".to_owned());
    }
    let target = required_string(&v, "target", 32)?.to_ascii_lowercase();
    if !matches!(target.as_str(), "peer" | "phone" | "email" | "qr") {
        return Err("unsupported share target".to_owned());
    }
    let target_id = optional_string(&v, "target_id", MAX_HOST_CHARS)?;
    if target == "phone" && target_id.is_empty() {
        return Err("phone share target_id is required".to_owned());
    }
    let target_id = if target_id.is_empty() {
        None
    } else if valid_host(&target_id) {
        Some(target_id)
    } else {
        return Err("invalid target_id".to_owned());
    };
    let url = required_string(&v, "url", MAX_URL_CHARS)?;
    if !valid_share_url(&url) {
        return Err("invalid share URL".to_owned());
    }
    let title = optional_string(&v, "title", MAX_TEXT_CHARS)?;
    let preview = required_string(&v, "preview", MAX_TEXT_CHARS)?;

    Ok(ShareRequest {
        id: id.to_owned(),
        host,
        target,
        target_id,
        url,
        title,
        preview,
    })
}

fn required_string(v: &serde_json::Value, key: &str, max_chars: usize) -> Result<String, String> {
    let value = v
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing {key}"))?
        .trim();
    if value.is_empty() {
        return Err(format!("empty {key}"));
    }
    if value.chars().count() > max_chars {
        return Err(format!("{key} is too long"));
    }
    Ok(value.to_owned())
}

fn optional_string(v: &serde_json::Value, key: &str, max_chars: usize) -> Result<String, String> {
    let Some(value) = v.get(key).and_then(serde_json::Value::as_str) else {
        return Ok(String::new());
    };
    let trimmed = value.trim();
    if trimmed.chars().count() > max_chars {
        return Err(format!("{key} is too long"));
    }
    Ok(trimmed.to_owned())
}

fn valid_host(host: &str) -> bool {
    host.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn valid_share_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && !rest.is_empty()
        && scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        && !url.chars().any(char::is_control)
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

    fn share_body(target: &str) -> String {
        share_body_with_target_id(target, None)
    }

    fn share_body_with_target_id(target: &str, target_id: Option<&str>) -> String {
        let mut body = serde_json::json!({
            "op": "browser_share",
            "source": "browser",
            "host": "mesh-node-1",
            "target": target,
            "url": "https://example.test/page",
            "title": "Example",
            "preview": "Example",
        });
        if let Some(target_id) = target_id {
            body["target_id"] = serde_json::json!(target_id);
            body["target_label"] = serde_json::json!(target_id);
        }
        body.to_string()
    }

    #[test]
    fn parse_request_accepts_browser_platform_share_targets() {
        for target in ["peer", "email", "qr"] {
            let request = parse_request(&share_body(target), "01H").expect("valid share");
            assert_eq!(request.id, "01H");
            assert_eq!(request.host, "mesh-node-1");
            assert_eq!(request.target, target);
            assert_eq!(request.url, "https://example.test/page");
            assert_eq!(request.title, "Example");
            assert_eq!(request.preview, "Example");
        }

        let phone = parse_request(&share_body_with_target_id("phone", Some("pixel-8")), "01P")
            .expect("phone share");
        assert_eq!(phone.target, "phone");
        assert_eq!(phone.target_id.as_deref(), Some("pixel-8"));
    }

    #[test]
    fn parse_request_rejects_non_browser_or_malformed_share_payloads() {
        assert!(parse_request(r#"{"op":"browser_share","source":"cloud"}"#, "x").is_err());
        assert!(parse_request(&share_body("phone"), "x").is_err());
        assert!(parse_request(&share_body_with_target_id("phone", Some("../bad")), "x").is_err());
        assert!(parse_request(&share_body("fax"), "x").is_err());
        assert!(parse_request(
            &share_body("email").replace("https://example.test/page", "not-a-url"),
            "x"
        )
        .is_err());
        assert!(
            parse_request(&share_body("peer").replace("mesh-node-1", "../bad"), "x").is_err(),
            "host names stay identifier-like"
        );
    }

    #[test]
    fn apply_request_publishes_status_and_share_event() {
        let bus = tempfile::tempdir().expect("bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("persist");
        let mut worker =
            BrowserShareWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 123_456));
        let request = parse_request(&share_body("email"), "req-1").expect("request");

        worker.apply_request(&persist, request);

        let status_body = persist
            .list_since("state/browser-share/node-a", None)
            .expect("list status")[0]
            .body
            .clone()
            .expect("status body");
        let status: ShareStatus = serde_json::from_str(&status_body).expect("status JSON");
        assert_eq!(status.state, "routed");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.last_request_id.as_deref(), Some("req-1"));
        assert_eq!(status.last_target.as_deref(), Some("email"));
        assert_eq!(status.last_target_id, None);
        assert_eq!(status.last_preview.as_deref(), Some("Example"));

        let event_body = persist
            .list_since("event/browser-share/node-a", None)
            .expect("list event")[0]
            .body
            .clone()
            .expect("event body");
        let event: serde_json::Value = serde_json::from_str(&event_body).expect("event JSON");
        assert_eq!(event["op"], "browser_share_routed");
        assert_eq!(event["source"], "browser_share");
        assert_eq!(event["request_id"], "req-1");
        assert_eq!(event["host"], "mesh-node-1");
        assert_eq!(event["target"], "email");
        assert!(event["target_id"].is_null());
        assert_eq!(event["url"], "https://example.test/page");
        assert_eq!(event["routed_ms"], 123_456);
    }

    #[test]
    fn phone_share_publishes_the_existing_kde_connect_share_verb() {
        let bus = tempfile::tempdir().expect("bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("persist");
        let mut worker =
            BrowserShareWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 123_456));
        let request = parse_request(
            &share_body_with_target_id("phone", Some("pixel-8")),
            "req-phone",
        )
        .expect("request");

        worker.apply_request(&persist, request);

        let msgs = persist.list_since(ACTION_CONNECT_SHARE, None).unwrap();
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("connect share body");
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(v["device_id"], "pixel-8");
        assert_eq!(v["url"], "https://example.test/page");
        assert_eq!(v["open"], true);
        assert_eq!(v["source"], "browser_share");
        assert_eq!(v["source_host"], "mesh-node-1");
        assert_eq!(v["handoff_id"], "req-phone");
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
            .write(
                ACTION_TOPIC,
                Priority::Default,
                None,
                Some(&share_body("qr")),
            )
            .expect("write good");
        let mut worker = BrowserShareWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 777));

        worker.drain_requests(&persist);
        worker.drain_requests(&persist);

        assert_eq!(worker.status.rejected, 1);
        assert_eq!(worker.status.accepted, 1);
        assert_eq!(worker.status.state, "routed");
        let events = persist
            .list_since("event/browser-share/node-a", None)
            .expect("list events");
        assert_eq!(events.len(), 1, "cursor prevents replay");
    }
}
