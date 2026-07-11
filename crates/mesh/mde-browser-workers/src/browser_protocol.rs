//! BROWSER-DD-12 — Browser external-protocol handoff owner.
//!
//! The Browser shell refuses to navigate external schemes like `mailto:` and
//! `magnet:` and publishes `action/browser/protocol` instead. This worker owns
//! the daemon side of that handoff: it validates the Browser-origin payload,
//! records the accepted route, and emits a bounded event for the target surface
//! owner. It does not fake an Email composer or magnet/torrent transfer lane that
//! is not implemented yet.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Browser-owned external protocol handoff topic.
pub const ACTION_TOPIC: &str = "action/browser/protocol";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-protocol/";

/// Accepted protocol route event topic prefix for this node.
pub const RESULT_PREFIX: &str = "event/browser-protocol/";

/// Default poll cadence. External protocol handoffs are explicit user actions.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

const MAX_URL_CHARS: usize = 8_192;
const MAX_HOST_CHARS: usize = 128;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Parsed Browser external-protocol handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Browser host that published the request.
    pub host: String,
    /// External URL scheme.
    pub scheme: String,
    /// Target platform owner (`email` or `transfers`).
    pub target: String,
    /// Original external URL.
    pub url: String,
}

/// Retained status for this node's Browser protocol owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProtocolStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent accepted request id.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// External scheme from the most recent accepted request.
    pub last_scheme: Option<String>,
    /// Target owner from the most recent accepted request.
    pub last_target: Option<String>,
    /// Original URL from the most recent accepted request.
    pub last_url: Option<String>,
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

/// Daemon worker for Browser external-protocol handoffs.
pub struct BrowserProtocolWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    bus_root_override: Option<std::path::PathBuf>,
    status: ProtocolStatus,
}

impl BrowserProtocolWorker {
    /// Create a Browser protocol worker for one node.
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
            status: ProtocolStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_scheme: None,
                last_target: None,
                last_url: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                rejected: 0,
                last_routed_ms: None,
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
                tracing::debug!(target: "mackesd::browser_protocol", error = %e, "list_since failed");
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

    fn apply_request(&mut self, persist: &Persist, request: ProtocolRequest) {
        let now = self.now_ms();
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_scheme = Some(request.scheme.clone());
        self.status.last_target = Some(request.target.clone());
        self.status.last_url = Some(request.url.clone());
        self.status.state = "routed".to_owned();
        self.status.last_error = None;
        self.status.last_routed_ms = Some(now);
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

    fn publish_event(&self, persist: &Persist, request: &ProtocolRequest, routed_ms: u64) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_protocol_routed",
            "source": "browser_protocol",
            "node": self.node,
            "request_id": &request.id,
            "host": &request.host,
            "scheme": &request.scheme,
            "target": &request.target,
            "url": &request.url,
            "routed_ms": routed_ms,
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }
}

#[async_trait::async_trait]
impl Worker for BrowserProtocolWorker {
    fn name(&self) -> &'static str {
        "browser_protocol"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_protocol", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_protocol", error = %e, "persist open failed; worker idle");
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

fn parse_request(body: &str, id: &str) -> Result<ProtocolRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("browser-protocol JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_protocol_handoff") {
        return Err("wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("wrong source".to_owned());
    }
    let host = required_string(&v, "host", MAX_HOST_CHARS)?;
    if !valid_host(&host) {
        return Err("invalid host".to_owned());
    }
    let scheme = required_string(&v, "scheme", 16)?.to_ascii_lowercase();
    let target = required_string(&v, "target", 32)?.to_ascii_lowercase();
    if !valid_route(&scheme, &target) {
        return Err("unsupported protocol route".to_owned());
    }
    let url = required_string(&v, "url", MAX_URL_CHARS)?;
    if url_scheme(&url).as_deref() != Some(scheme.as_str()) {
        return Err("URL scheme does not match handoff scheme".to_owned());
    }

    Ok(ProtocolRequest {
        id: id.to_owned(),
        host,
        scheme,
        target,
        url,
    })
}

fn valid_route(scheme: &str, target: &str) -> bool {
    matches!(
        (scheme, target),
        ("mailto", "email") | ("magnet", "transfers")
    )
}

fn url_scheme(url: &str) -> Option<String> {
    let (scheme, rest) = url.trim().split_once(':')?;
    if scheme.is_empty() || rest.is_empty() {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
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

fn valid_host(host: &str) -> bool {
    host.bytes()
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

    fn protocol_body(scheme: &str, target: &str, url: &str) -> String {
        serde_json::json!({
            "op": "browser_protocol_handoff",
            "source": "browser",
            "host": "mesh-node-1",
            "scheme": scheme,
            "target": target,
            "url": url,
        })
        .to_string()
    }

    #[test]
    fn parse_request_accepts_browser_mailto_and_magnet_routes() {
        let mail = parse_request(
            &protocol_body("mailto", "email", "mailto:ops@example.test?subject=mesh"),
            "mail-1",
        )
        .expect("mailto route");
        assert_eq!(mail.scheme, "mailto");
        assert_eq!(mail.target, "email");
        assert_eq!(mail.host, "mesh-node-1");

        let magnet = parse_request(
            &protocol_body(
                "magnet",
                "transfers",
                "magnet:?xt=urn:btih:0123456789abcdef",
            ),
            "magnet-1",
        )
        .expect("magnet route");
        assert_eq!(magnet.scheme, "magnet");
        assert_eq!(magnet.target, "transfers");
    }

    #[test]
    fn parse_request_rejects_non_browser_or_mismatched_routes() {
        assert!(
            parse_request(r#"{"op":"browser_protocol_handoff","source":"cloud"}"#, "x").is_err()
        );
        assert!(
            parse_request(
                &protocol_body("mailto", "transfers", "mailto:ops@example.test"),
                "x"
            )
            .is_err(),
            "mailto routes only to email"
        );
        assert!(
            parse_request(
                &protocol_body("magnet", "transfers", "mailto:ops@example.test"),
                "x"
            )
            .is_err(),
            "declared scheme must match URL"
        );
        assert!(
            parse_request(
                &protocol_body("mailto", "email", "mailto:ops@example.test")
                    .replace("mesh-node-1", "../bad"),
                "x",
            )
            .is_err(),
            "host names stay identifier-like"
        );
    }

    #[test]
    fn apply_request_publishes_status_and_protocol_event() {
        let bus = tempfile::tempdir().expect("bus");
        let persist = Persist::open(bus.path().to_path_buf()).expect("persist");
        let mut worker =
            BrowserProtocolWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 123_456));
        let request = parse_request(
            &protocol_body("mailto", "email", "mailto:ops@example.test"),
            "req-1",
        )
        .expect("request");

        worker.apply_request(&persist, request);

        let status_body = persist
            .list_since("state/browser-protocol/node-a", None)
            .expect("list status")[0]
            .body
            .clone()
            .expect("status body");
        let status: ProtocolStatus = serde_json::from_str(&status_body).expect("status JSON");
        assert_eq!(status.state, "routed");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.last_request_id.as_deref(), Some("req-1"));
        assert_eq!(status.last_scheme.as_deref(), Some("mailto"));
        assert_eq!(status.last_target.as_deref(), Some("email"));

        let event_body = persist
            .list_since("event/browser-protocol/node-a", None)
            .expect("list event")[0]
            .body
            .clone()
            .expect("event body");
        let event: serde_json::Value = serde_json::from_str(&event_body).expect("event JSON");
        assert_eq!(event["op"], "browser_protocol_routed");
        assert_eq!(event["source"], "browser_protocol");
        assert_eq!(event["request_id"], "req-1");
        assert_eq!(event["host"], "mesh-node-1");
        assert_eq!(event["scheme"], "mailto");
        assert_eq!(event["target"], "email");
        assert_eq!(event["routed_ms"], 123_456);
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
                Some(&protocol_body(
                    "magnet",
                    "transfers",
                    "magnet:?xt=urn:btih:0123456789abcdef",
                )),
            )
            .expect("write good");
        let mut worker =
            BrowserProtocolWorker::new("node-a".to_owned()).with_now_fn(Arc::new(|| 777));

        worker.drain_requests(&persist);
        worker.drain_requests(&persist);

        assert_eq!(worker.status.rejected, 1);
        assert_eq!(worker.status.accepted, 1);
        assert_eq!(worker.status.state, "routed");
        let events = persist
            .list_since("event/browser-protocol/node-a", None)
            .expect("list events");
        assert_eq!(events.len(), 1, "cursor prevents replay");
    }
}
