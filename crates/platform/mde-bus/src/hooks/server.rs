//! axum HTTP listener for inbound webhooks.
//!
//! Bind contract per BUS-3.1 design lock:
//!
//! - Listens on `<overlay_ip>:8444` exclusively. The bind is the
//!   security boundary — the kernel rejects underlay connections
//!   before they reach the application, so "Nebula source-IP
//!   only" authentication is enforced at the socket layer with
//!   zero in-process middleware.
//! - One route: `POST /hooks/:adapter` (axum path param). The
//!   adapter name selects the per-source extractor + rule list.
//!
//! Response codes:
//!
//! | Status | Meaning                                           |
//! |-------:|---------------------------------------------------|
//! | `202`  | Rule matched + publish forwarded to ntfy          |
//! | `204`  | Adapter accepted shape, no rule fired (no-op)     |
//! | `400`  | Body wasn't valid JSON / adapter rejected shape   |
//! | `404`  | Adapter not in `bus-hooks.yaml`                   |
//! | `422`  | Rule matched but a template render failed         |
//! | `502`  | Local ntfy broker not reachable / non-2xx         |
//!
//! All bodies are operator-facing JSON `{"error": "..."}` shapes
//! so `journalctl -u mde-bus` shows the failure in one line.
//!
//! Pre-spawn skip semantics mirror [`crate::broker`]: if the
//! overlay-IP publish file is missing (pre-enrollment peer) or
//! the config path resolution fails (no `$HOME`), the listener
//! returns [`ListenerOutcome::Skipped`] with a structured reason.
//! The outer daemon logs once and moves on; the next supervisor
//! tick re-evaluates.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{json, Value};
use thiserror::Error;
use tokio::task::JoinHandle;

use super::config::{ConfigError, HooksConfig};
use super::generic::GenericAdapter;
use super::gitea::GiteaAdapter;
use super::github::GitHubAdapter;
use super::home_assistant::HomeAssistantAdapter;
use super::matcher::{match_request, Adapter, MatchError};
use super::nut::NutAdapter;
use super::publisher::{publish_to_ntfy, PublisherError};
use super::sonarr::SonarrAdapter;
use crate::persist::Persist;
use crate::surface::{dispatch_with_dnd, LogOnlySurfaces};

/// Default port. Mirrored from `super::DEFAULT_LISTEN_PORT` so
/// other modules don't have to know the constant lives one level
/// up.
pub use super::DEFAULT_LISTEN_PORT;

/// Server configuration.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// Path to `bus-hooks.yaml` (operator-edited). When the file
    /// is missing, the listener still spawns with an empty config
    /// so the operator can drop a file in place without a daemon
    /// restart.
    pub config_path: PathBuf,
    /// Port to bind (defaults to 8444).
    pub listen_port: u16,
    /// Where to forward published messages (typically
    /// `format!("http://{overlay_ip}:8443")`).
    pub broker_base: String,
    /// Optional persistence root (`~/.local/share/mde/bus`).
    /// When set, every successful match → publish also writes
    /// to the per-topic file tree + SQLite index (BUS-1.4).
    /// `None` keeps the listener stateless (useful for tests).
    pub bus_root: Option<PathBuf>,
    /// BROKER-RESILIENCE-1 — persist-only mode: skip the outbound ntfy POST
    /// entirely (persist + audit only). Set when the broker is KNOWN absent —
    /// the supervisor evaluated [`crate::broker::evaluate_prereqs`] and got a
    /// `Skip` (no overlay IP, `ntfy` not installed, template missing). A
    /// known-down broker means every POST would fail anyway, so we must NOT
    /// keep attempting them: each failed POST eats a connect-timeout window
    /// AND the matched message is already durably persisted (the `persist`
    /// write runs first), so the spool + audit carry it to peers once the
    /// broker comes up. Attempting the doomed outbound was the live wedge —
    /// it drove the spool growth + the watchdog-starving blocking. Mirrors
    /// the `mde-bus publish --no-broker` contract.
    pub persist_only: bool,
}

impl ListenerConfig {
    /// New config with the canonical defaults (broker reachable: the
    /// listener forwards matched publishes to ntfy on `<overlay_ip>:8443`).
    #[must_use]
    pub fn for_overlay_ip(overlay_ip: &str) -> Self {
        Self {
            config_path: super::default_config_path()
                .unwrap_or_else(|| PathBuf::from("/var/lib/mackesd/bus-hooks.yaml")),
            listen_port: DEFAULT_LISTEN_PORT,
            broker_base: format!("http://{overlay_ip}:8443"),
            bus_root: crate::default_data_dir(),
            persist_only: false,
        }
    }

    /// BROKER-RESILIENCE-1 — same as [`Self::for_overlay_ip`] but in
    /// persist-only mode: the listener still binds + ingests webhooks and
    /// persists every match, but skips the outbound ntfy POST. Used when the
    /// broker is KNOWN absent (the supervisor's `evaluate_prereqs` returned a
    /// `Skip`), so the listener never wastes a connect-timeout window per
    /// request on a doomed POST — the persisted message rides the spool/audit
    /// to peers once the broker is back.
    #[must_use]
    pub fn persist_only_for_overlay_ip(overlay_ip: &str) -> Self {
        Self {
            persist_only: true,
            ..Self::for_overlay_ip(overlay_ip)
        }
    }
}

/// Why the listener didn't spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerSkipReason {
    /// `$HOME` / `$XDG_CONFIG_HOME` resolution failed — no
    /// `bus-hooks.yaml` to seed against.
    NoConfigPath,
    /// `tokio::net::TcpListener::bind` returned an error (port
    /// in use, permission denied, etc.). Carries the OS error
    /// string for the log.
    BindFailed(String),
}

impl std::fmt::Display for ListenerSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConfigPath => {
                write!(f, "no $HOME / $XDG_CONFIG_HOME — can't resolve config path")
            }
            Self::BindFailed(e) => write!(f, "bind failed: {e}"),
        }
    }
}

/// Result of [`run_listener`] — either the supervisor task
/// handle (cancelled on shutdown) or a structured skip reason.
#[derive(Debug)]
pub enum ListenerOutcome {
    /// Listener is up. The caller owns the join handle + the
    /// shutdown sender; dropping the sender triggers a graceful
    /// exit.
    Running {
        /// Task driving `axum::serve`.
        task: JoinHandle<()>,
        /// Drop or send `true` to stop the listener.
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        /// Local socket the listener bound to — useful for tests
        /// that bind on `:0` and need to know the assigned port.
        local_addr: SocketAddr,
    },
    /// Listener wasn't started — see reason.
    Skipped(ListenerSkipReason),
}

/// Internal per-request state.
struct ListenerState {
    config_path: PathBuf,
    broker_base: String,
    http_client: reqwest::Client,
    /// Adapters registered at startup. Indexed by name to keep
    /// lookup O(log N); names match the keys in `bus-hooks.yaml`.
    adapters: std::collections::BTreeMap<String, Box<dyn Adapter>>,
    /// BUS-1.4 persistence. `None` skips per-request writes (used
    /// by tests that don't care about the index + by future
    /// stateless modes). When `Some`, every matched publish lands
    /// in the per-topic file tree + SQLite index BEFORE the
    /// outbound ntfy POST so a transient broker failure doesn't
    /// lose the message.
    persist: Option<std::sync::Mutex<Persist>>,
    /// BUS-2.8 — bus_root path for the DND state lookup. `None`
    /// when persistence is disabled (tests); the dispatch then
    /// uses `DndState::default()` (DND off) as the fallback.
    bus_root: Option<PathBuf>,
    /// BROKER-RESILIENCE-1 — when true, `handle_hook` skips the outbound ntfy
    /// POST (persist + audit only). Set when the broker is known absent so a
    /// doomed POST never blocks the request or grows the spool via retry.
    persist_only: bool,
}

/// BUS-2.8 — current local time as seconds-of-day [0, 86_400).
/// Used by `dispatch_with_dnd` to evaluate per-topic quiet-hour
/// windows. Reads the operator's local timezone via chrono::Local.
fn current_local_seconds_of_day() -> u32 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    now.hour() * 3600 + now.minute() * 60 + now.second()
}

/// Start the webhook listener.
///
/// Binds on `<overlay_ip>:<listen_port>` and spawns the axum
/// serving loop on a tokio task. The returned outcome carries
/// either the join handle + shutdown sender or a skip reason.
///
/// # Errors
/// Returns `Err` only for unexpected failures *after* the bind
/// succeeded (e.g. join-handle creation). Bind failures are
/// reported as [`ListenerSkipReason::BindFailed`] so the caller
/// can degrade gracefully rather than crashing the daemon.
pub async fn run_listener(
    overlay_ip: IpAddr,
    cfg: ListenerConfig,
) -> anyhow::Result<ListenerOutcome> {
    let bind_addr = SocketAddr::new(overlay_ip, cfg.listen_port);
    let listener = match tokio::net::TcpListener::bind(bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            return Ok(ListenerOutcome::Skipped(ListenerSkipReason::BindFailed(
                e.to_string(),
            )));
        }
    };
    let local_addr = listener.local_addr()?;

    let persist = match cfg.bus_root.as_ref() {
        Some(root) => match Persist::open(root.clone()) {
            Ok(p) => Some(std::sync::Mutex::new(p)),
            Err(e) => {
                tracing::warn!(
                    target: "mde_bus::hooks",
                    error = %e,
                    "persistence layer skipped — webhooks publish but won't index"
                );
                None
            }
        },
        None => None,
    };

    let state = ListenerState {
        config_path: cfg.config_path,
        broker_base: cfg.broker_base,
        // BROKER-RESILIENCE-1 — a publish to a missing/dead broker must fail
        // FAST (connect + request timeouts), not hang and starve the watchdog.
        // The shared constructor applies both; see `publisher::broker_client`.
        http_client: super::publisher::broker_client(),
        adapters: register_builtin_adapters(),
        persist,
        bus_root: cfg.bus_root,
        persist_only: cfg.persist_only,
    };
    let state = Arc::new(state);

    let app = Router::new()
        .route("/hooks/:adapter", post(handle_hook))
        .with_state(state);

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            // Wake on shutdown_tx drop OR true-flip.
            let _ = shutdown_rx.changed().await;
        });
        if let Err(e) = serve.await {
            tracing::warn!(
                target: "mde_bus::hooks",
                error = %e,
                "axum serve loop exited with error"
            );
        }
    });

    Ok(ListenerOutcome::Running {
        task,
        shutdown_tx,
        local_addr,
    })
}

/// Register every built-in adapter. The list lives here so each
/// BUS-3.N adapter just adds one line at the matching task.
fn register_builtin_adapters() -> std::collections::BTreeMap<String, Box<dyn Adapter>> {
    let mut map: std::collections::BTreeMap<String, Box<dyn Adapter>> =
        std::collections::BTreeMap::new();
    map.insert("github".to_string(), Box::new(GitHubAdapter));
    map.insert("gitea".to_string(), Box::new(GiteaAdapter));
    map.insert("sonarr".to_string(), Box::new(SonarrAdapter));
    map.insert("nut".to_string(), Box::new(NutAdapter));
    map.insert("home_assistant".to_string(), Box::new(HomeAssistantAdapter));
    map.insert("generic".to_string(), Box::new(GenericAdapter));
    map
}

/// axum handler — runs the matcher, calls the publisher, maps
/// errors to HTTP status codes.
async fn handle_hook(
    Path(adapter_name): Path<String>,
    State(state): State<Arc<ListenerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Re-load config every request — `bus-hooks.yaml` is operator-
    // edited and the file is small (~few KB). Avoids a separate
    // inotify watcher just for this one file; mirrors the
    // BUS-1.7 design rationale.
    let config = match HooksConfig::load(&state.config_path) {
        Ok(cfg) => cfg,
        Err(ConfigError::Missing(_)) => HooksConfig::default(),
        Err(e) => {
            tracing::warn!(
                target: "mde_bus::hooks",
                error = %e,
                "rejecting hook — config invalid"
            );
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
    };

    let body_json: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error_response(StatusCode::BAD_REQUEST, format!("body not JSON: {e}"));
        }
    };

    let lower_headers = lowercase_headers(&headers);

    let adapter: &dyn Adapter = match state.adapters.get(&adapter_name) {
        Some(boxed) => boxed.as_ref(),
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("unknown adapter: {adapter_name}"),
            );
        }
    };

    let rendered = match match_request(&adapter_name, &lower_headers, &body_json, &config, adapter)
    {
        Ok(Some(r)) => r,
        Ok(None) => return StatusCode::NO_CONTENT.into_response(),
        Err(MatchError::UnknownAdapter(a)) => {
            return error_response(StatusCode::NOT_FOUND, format!("unknown adapter: {a}"));
        }
        Err(MatchError::AdapterRejected) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "adapter rejected request shape".to_string(),
            );
        }
        Err(MatchError::Render(e)) => {
            return error_response(StatusCode::UNPROCESSABLE_ENTITY, format!("render: {e}"));
        }
    };

    // BUS-1.4 — persist BEFORE the outbound POST so a transient
    // ntfy failure doesn't lose the matched-then-rendered
    // message. Persist failure is logged but not fatal — the
    // operator can re-run the audit (detect_divergence) to find
    // un-persisted publishes, and the next request retries the
    // index open if it was a one-off SQLite hiccup.
    let persisted_ulid: Option<String> = if let Some(mtx) = state.persist.as_ref() {
        match mtx.lock() {
            Ok(p) => match p.write(
                &rendered.topic,
                rendered.priority,
                Some(&rendered.title),
                Some(&rendered.body),
            ) {
                Ok(stored) => {
                    // BUS-2.1 / BUS-2.8 — fire the priority →
                    // surface dispatcher through the DND gate.
                    // Loads the mesh-wide DND state from the
                    // GFS-replicated dnd.yaml at publish time
                    // (cheap — typically a < 1 KB file). When
                    // DND is off + the message has no quiet-
                    // hour topic config, dispatch_with_dnd
                    // delegates to the standard `dispatch()`.
                    // Until BUS-2.2..2.8 land the real Iced
                    // surfaces, LogOnlySurfaces just tracing-
                    // logs so the dispatch table is observable
                    // in `journalctl -u mde-bus`.
                    let dnd_state = match state.bus_root.as_ref() {
                        Some(root) => crate::dnd::load_default(root),
                        None => crate::dnd::DndState::default(),
                    };
                    // BUS-2.8.topic-hours — the matched rule's
                    // quiet_after / quiet_until fields are resolved
                    // into a TopicQuietHours during render_rule and
                    // carried on the RenderedPublish here.
                    let topic_hours = rendered.quiet_hours;
                    let now_local = current_local_seconds_of_day();
                    dispatch_with_dnd(
                        &stored,
                        &dnd_state,
                        topic_hours,
                        &[],
                        now_local,
                        &LogOnlySurfaces,
                    );
                    Some(stored.ulid)
                }
                Err(e) => {
                    tracing::warn!(
                        target: "mde_bus::hooks",
                        error = %e,
                        topic = %rendered.topic,
                        "persist failed — publishing without index entry"
                    );
                    None
                }
            },
            Err(_) => {
                tracing::warn!(
                    target: "mde_bus::hooks",
                    "persist mutex poisoned — publishing without index entry"
                );
                None
            }
        }
    } else {
        None
    };

    // BROKER-RESILIENCE-1 — when the broker is known absent (persist-only
    // mode, set by the supervisor on an `evaluate_prereqs` Skip), the message
    // is already durably persisted above; do NOT attempt the outbound POST. A
    // doomed POST would burn a connect-timeout window per request (blocking)
    // and the failed-publish path is exactly what drove the unbounded spool
    // growth on a down broker. Return 202 ACCEPTED — the publish IS accepted
    // (persisted + audited); the spool/audit carry it to peers once the broker
    // is back, identical to the `mde-bus publish --no-broker` contract.
    if state.persist_only {
        tracing::info!(
            target: "mde_bus::hooks",
            adapter = %adapter_name,
            rule = %rendered.rule_name,
            topic = %rendered.topic,
            ulid = ?persisted_ulid,
            "published (persist-only — broker known absent, outbound skipped)"
        );
        return StatusCode::ACCEPTED.into_response();
    }

    match publish_to_ntfy(&state.http_client, &state.broker_base, &rendered).await {
        Ok(()) => {
            tracing::info!(
                target: "mde_bus::hooks",
                adapter = %adapter_name,
                rule = %rendered.rule_name,
                topic = %rendered.topic,
                ulid = ?persisted_ulid,
                "published"
            );
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => {
            tracing::warn!(
                target: "mde_bus::hooks",
                error = %e,
                topic = %rendered.topic,
                ulid = ?persisted_ulid,
                "publish failed (message persisted; retry pending)"
            );
            error_response(StatusCode::BAD_GATEWAY, publisher_error_msg(&e))
        }
    }
}

fn lowercase_headers(h: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for (name, value) in h {
        let n = name.as_str().to_lowercase();
        let v = value.to_str().unwrap_or("").to_string();
        out.insert(n, v);
    }
    out
}

fn publisher_error_msg(e: &PublisherError) -> String {
    match e {
        PublisherError::Transport(s) => format!("ntfy unreachable: {s}"),
        PublisherError::BadStatus { status, .. } => format!("ntfy returned {status}"),
    }
}

fn error_response(status: StatusCode, msg: String) -> Response {
    let body = Json(json!({ "error": msg }));
    (status, body).into_response()
}

/// Errors during listener startup (excluding skip reasons, which
/// are normal supervisor states).
#[derive(Debug, Error)]
pub enum ListenerError {
    /// `tokio::net::TcpListener::bind` failed.
    #[error("bind: {0}")]
    Bind(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn write_yaml(path: &std::path::Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    /// Stand up the listener + a stub ntfy server. Returns the
    /// listener's local addr + the stub's captured-bodies handle.
    async fn fixture(
        hooks_yaml: &str,
    ) -> (
        SocketAddr,
        std::sync::Arc<Mutex<Vec<String>>>,
        tokio::sync::watch::Sender<bool>,
    ) {
        // Stub ntfy: 200 OK on every connection, capture request.
        let captured: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let ntfy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ntfy_addr = ntfy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            loop {
                if let Ok((mut s, _)) = ntfy_listener.accept().await {
                    let cap = captured_clone.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 8192];
                        if let Ok(n) = s.read(&mut buf).await {
                            cap.lock()
                                .unwrap()
                                .push(String::from_utf8_lossy(&buf[..n]).to_string());
                        }
                        let _ = s
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    });
                }
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("bus-hooks.yaml");
        write_yaml(&cfg_path, hooks_yaml);
        // Leak the tempdir for the lifetime of the test — the
        // listener reads the file on every request.
        Box::leak(Box::new(tmp));

        let cfg = ListenerConfig {
            config_path: cfg_path,
            listen_port: 0, // ephemeral
            broker_base: format!("http://{ntfy_addr}"),
            bus_root: None,      // tests don't need persistence
            persist_only: false, // exercise the real outbound POST path
        };
        let outcome = run_listener(IpAddr::from([127, 0, 0, 1]), cfg)
            .await
            .unwrap();
        match outcome {
            ListenerOutcome::Running {
                task: _,
                shutdown_tx,
                local_addr,
            } => (local_addr, captured, shutdown_tx),
            ListenerOutcome::Skipped(r) => panic!("listener skipped: {r:?}"),
        }
    }

    fn sample_github_push_yaml() -> &'static str {
        r#"
adapters:
  github:
    rules:
      - name: github-push
        match:
          event: push
        publish:
          topic: gh/push
          priority: default
          title: "{{ repo }} push to {{ branch }}"
          body: "{{ pusher }} pushed {{ commit_count }} commits"
"#
    }

    fn github_push_payload() -> Value {
        json!({
            "ref": "refs/heads/main",
            "repository": { "full_name": "matthewmackes/MDE-X" },
            "pusher": { "name": "matt" },
            "commits": [{"id":"a","message":"x"}, {"id":"b","message":"y"}],
            "head_commit": {"message": "second commit"},
        })
    }

    #[tokio::test]
    async fn end_to_end_github_push_publishes_to_gh_push_topic() {
        let (addr, captured, _shutdown) = fixture(sample_github_push_yaml()).await;
        let body = github_push_payload();
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/github"))
            .header("X-GitHub-Event", "push")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        // Drain stub-server capture (give it a tick to record).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let raw = captured.lock().unwrap().clone();
        assert_eq!(raw.len(), 1, "expected exactly one publish: {raw:?}");
        let req = &raw[0];
        // The ntfy topic is a single path segment, so `gh/push` flattens to
        // `gh_push` in the URL (the real topic rides the `x-topic` header).
        assert!(
            req.contains("POST /gh_push") && req.contains("x-topic: gh/push"),
            "expected flattened topic in path + real topic header: {req}"
        );
        let title_ok = req.contains("matthewmackes/MDE-X push to main");
        assert!(title_ok, "title rendering wrong:\n{req}");
        let body_ok = req.contains("matt pushed 2 commits");
        assert!(body_ok, "body rendering wrong:\n{req}");
    }

    #[tokio::test]
    async fn unknown_adapter_returns_404() {
        let (addr, _captured, _shutdown) = fixture(sample_github_push_yaml()).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/never-heard-of-it"))
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
    }

    #[tokio::test]
    async fn invalid_body_json_returns_400() {
        let (addr, _captured, _shutdown) = fixture(sample_github_push_yaml()).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/github"))
            .header("X-GitHub-Event", "push")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);
    }

    #[tokio::test]
    async fn no_matching_rule_returns_204() {
        let yaml = r#"
adapters:
  github:
    rules:
      - name: only-pull-request
        match:
          event: pull_request
        publish:
          topic: gh/pr
          title: t
          body: b
"#;
        let (addr, _captured, _shutdown) = fixture(yaml).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/github"))
            .header("X-GitHub-Event", "push")
            .json(&github_push_payload())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
    }

    #[tokio::test]
    async fn missing_required_header_returns_400() {
        let (addr, _captured, _shutdown) = fixture(sample_github_push_yaml()).await;
        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/github"))
            .json(&github_push_payload())
            .send()
            .await
            .unwrap();
        // GitHub adapter requires X-GitHub-Event; without it the
        // extractor returns None → 400.
        assert_eq!(resp.status().as_u16(), 400);
    }

    #[tokio::test]
    async fn persist_only_listener_persists_without_outbound_post() {
        // BROKER-RESILIENCE-1 — when the broker is known absent, the listener
        // runs persist-only: a matched webhook MUST be persisted (so the spool +
        // audit carry it to peers) but MUST NOT attempt the outbound ntfy POST
        // (which would burn a connect-timeout per request + grow the spool on
        // retry — the live wedge). We stand up a stub ntfy that COUNTS
        // connections and assert it receives ZERO, while the message lands in
        // the persist index.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let conns = std::sync::Arc::new(AtomicUsize::new(0));
        let conns_clone = conns.clone();
        let ntfy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ntfy_addr = ntfy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                if let Ok((mut s, _)) = ntfy_listener.accept().await {
                    conns_clone.fetch_add(1, Ordering::SeqCst);
                    use tokio::io::AsyncWriteExt;
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("bus-hooks.yaml");
        write_yaml(&cfg_path, sample_github_push_yaml());
        let bus_root = tmp.path().join("bus");

        let cfg = ListenerConfig {
            config_path: cfg_path,
            listen_port: 0,
            broker_base: format!("http://{ntfy_addr}"), // points at the (counting) stub
            bus_root: Some(bus_root.clone()),
            persist_only: true, // broker known absent → skip the outbound POST
        };
        let outcome = run_listener(IpAddr::from([127, 0, 0, 1]), cfg)
            .await
            .unwrap();
        let addr = match outcome {
            ListenerOutcome::Running { local_addr, .. } => local_addr,
            ListenerOutcome::Skipped(r) => panic!("listener skipped: {r:?}"),
        };

        let resp = reqwest::Client::new()
            .post(format!("http://{addr}/hooks/github"))
            .header("X-GitHub-Event", "push")
            .json(&github_push_payload())
            .send()
            .await
            .unwrap();
        // The publish is ACCEPTED — it was persisted (just not forwarded).
        assert_eq!(resp.status().as_u16(), 202);

        // Give any (erroneous) outbound POST a chance to land before asserting 0.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            conns.load(Ordering::SeqCst),
            0,
            "persist-only listener must NOT attempt an outbound POST to the broker"
        );

        // The message IS in the persist index (spool/audit carry it to peers).
        let p = crate::persist::Persist::open(bus_root).unwrap();
        let rows = p.list_since("gh/push", None).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "persist-only still records the matched publish"
        );
    }
}
