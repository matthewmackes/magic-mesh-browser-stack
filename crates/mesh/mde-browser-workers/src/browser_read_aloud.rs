//! BROWSER-DD-11 — Browser read-aloud/TTS owner.
//!
//! The Browser shell owns page text extraction and publishes bounded
//! `action/browser/read-aloud` requests. This worker owns the daemon side of that
//! stream: it validates the request, invokes a locally configured offline TTS
//! command when present, and publishes an honest `state/browser-read-aloud/<node>`
//! status. Missing voice assets or an unconfigured command are surfaced as
//! `Unavailable`, never as fake playback.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Browser-owned read-aloud request topic.
pub const ACTION_TOPIC: &str = "action/browser/read-aloud";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-read-aloud/";

/// Default poll cadence. Read-aloud is an explicit user action, not a high-rate
/// stream, so a short poll gives responsive UI without busy work.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

const MAX_TEXT_CHARS: usize = 20_000;
const DEFAULT_TTS_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TTS_COMMAND: &str = "/usr/libexec/mackesd/browser-read-aloud-tts";
const EX_UNAVAILABLE: i32 = 69;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;
type TtsBackend = Arc<dyn Fn(&ReadAloudRequest) -> SpeakOutcome + Send + Sync>;

/// Parsed Browser read-aloud request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadAloudRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Browser host that published the request.
    pub host: String,
    /// Tab index in the Browser shell that originated the request.
    pub tab_index: u64,
    /// Browser engine wire label.
    pub engine: String,
    /// Page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Bounded page text to speak.
    pub text: String,
    /// True when the Browser had to clamp extracted text before publishing.
    pub truncated: bool,
}

/// Status published for the local read-aloud owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadAloudStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent request id, if any request was accepted.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Page URL from the most recent accepted request.
    pub last_url: Option<String>,
    /// Page title from the most recent accepted request.
    pub last_title: Option<String>,
    /// Outcome state: `idle`, `speaking`, `spoken`, `unavailable`, or `error`.
    pub state: String,
    /// Last human-readable error/unavailable reason.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests that successfully reached the configured TTS backend.
    pub spoken: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Timestamp of the most recent accepted request.
    pub last_request_ms: Option<u64>,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser read-aloud requests.
pub struct BrowserReadAloudWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    backend: TtsBackend,
    bus_root_override: Option<std::path::PathBuf>,
    status: ReadAloudStatus,
}

impl BrowserReadAloudWorker {
    /// Create a Browser read-aloud worker for one node.
    #[must_use]
    pub fn new(node: String) -> Self {
        let now_fn: NowFn = Arc::new(default_now);
        let updated_ms = now_fn();
        Self {
            node: node.clone(),
            cursor: None,
            tick: DEFAULT_TICK,
            now_fn,
            backend: {
                let backend = ConfiguredCommandTts::from_env();
                Arc::new(move |request| backend.speak(request))
            },
            bus_root_override: None,
            status: ReadAloudStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_url: None,
                last_title: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                spoken: 0,
                rejected: 0,
                last_request_ms: None,
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

    /// Override the TTS backend used by tests or embedders.
    #[must_use]
    pub fn with_backend(mut self, backend: TtsBackend) -> Self {
        self.backend = backend;
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

    fn drain_requests(&mut self, persist: &Persist) -> Vec<ReadAloudRequest> {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_read_aloud", error = %e, "list_since failed");
                return Vec::new();
            }
        };
        let mut requests = Vec::new();
        for msg in msgs {
            self.cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_request(&body, &msg.ulid) {
                Ok(request) => requests.push(request),
                Err(e) => {
                    self.status.rejected = self.status.rejected.saturating_add(1);
                    self.status.state = "error".to_owned();
                    self.status.last_error = Some(e);
                    self.status.updated_ms = self.now_ms();
                    self.publish_status(persist);
                }
            }
        }
        requests
    }

    fn accept_request(&mut self, request: &ReadAloudRequest) {
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_url = Some(request.url.clone());
        self.status.last_title = Some(request.title.clone());
        self.status.last_error = None;
        self.status.last_request_ms = Some(self.now_ms());
        self.status.state = "speaking".to_owned();
        self.status.updated_ms = self.now_ms();
    }

    async fn speak_request(&self, request: ReadAloudRequest) -> SpeakOutcome {
        let backend = Arc::clone(&self.backend);
        let request_for_backend = request;
        #[allow(clippy::redundant_closure)]
        let handle = tokio::task::spawn_blocking(move || backend(&request_for_backend));
        handle
            .await
            .unwrap_or_else(|e| SpeakOutcome::Error(format!("TTS backend panicked: {e}")))
    }

    fn finish_request(&mut self, outcome: SpeakOutcome) {
        match outcome {
            SpeakOutcome::Spoken => {
                self.status.spoken = self.status.spoken.saturating_add(1);
                self.status.state = "spoken".to_owned();
                self.status.last_error = None;
            }
            SpeakOutcome::Unavailable(reason) => {
                self.status.state = "unavailable".to_owned();
                self.status.last_error = Some(reason);
            }
            SpeakOutcome::Error(err) => {
                self.status.state = "error".to_owned();
                self.status.last_error = Some(err);
            }
        }
        self.status.updated_ms = self.now_ms();
    }

    fn publish_status(&self, persist: &Persist) {
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&self.status) {
            let _ = persist.write(&topic, Priority::Default, None, Some(&body));
        }
    }
}

#[async_trait::async_trait]
impl Worker for BrowserReadAloudWorker {
    fn name(&self) -> &'static str {
        "browser_read_aloud"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_read_aloud", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_read_aloud", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.status.updated_ms = self.now_ms();
        self.publish_status(&persist);
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    for request in self.drain_requests(&persist) {
                        self.accept_request(&request);
                        self.publish_status(&persist);
                        let outcome = self.speak_request(request).await;
                        self.finish_request(outcome);
                        self.publish_status(&persist);
                    }
                }
                () = shutdown.wait() => break,
            }
        }
        Ok(())
    }
}

/// Result of handing browser page text to the local offline TTS backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpeakOutcome {
    /// The configured TTS backend completed successfully.
    Spoken,
    /// No usable offline TTS engine/model is configured on this node.
    Unavailable(String),
    /// A configured backend failed.
    Error(String),
}

#[derive(Debug, Clone)]
struct ConfiguredCommandTts {
    command: Option<String>,
    timeout: Duration,
}

impl ConfiguredCommandTts {
    fn from_env() -> Self {
        Self {
            command: configured_command_from_env(
                |key| std::env::var(key),
                Path::new(DEFAULT_TTS_COMMAND),
            ),
            timeout: DEFAULT_TTS_TIMEOUT,
        }
    }

    fn speak(&self, request: &ReadAloudRequest) -> SpeakOutcome {
        let Some(command) = &self.command else {
            return SpeakOutcome::Unavailable(
                "offline TTS command is not configured; set MDE_BROWSER_TTS_COMMAND to a Piper/Kokoro playback pipeline".to_owned(),
            );
        };
        run_tts_command(command, &request.text, self.timeout)
    }
}

fn run_tts_command(command: &str, text: &str, timeout: Duration) -> SpeakOutcome {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return SpeakOutcome::Unavailable(format!("could not start TTS command: {e}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(text.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return SpeakOutcome::Error(format!("could not feed TTS text: {e}"));
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                if status.success() {
                    return SpeakOutcome::Spoken;
                }
                let stderr = stderr.trim();
                if status.code() == Some(EX_UNAVAILABLE) {
                    return SpeakOutcome::Unavailable(if stderr.is_empty() {
                        "offline TTS runtime is unavailable".to_owned()
                    } else {
                        stderr.to_owned()
                    });
                }
                return SpeakOutcome::Error(if stderr.is_empty() {
                    format!("TTS command exited with {status}")
                } else {
                    format!("TTS command exited with {status}: {stderr}")
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return SpeakOutcome::Error("TTS command timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return SpeakOutcome::Error(format!("TTS command wait failed: {e}")),
        }
    }
}

fn configured_command_from_env<E>(getenv: E, default_path: &Path) -> Option<String>
where
    E: Fn(&str) -> Result<String, std::env::VarError>,
{
    getenv("MDE_BROWSER_TTS_COMMAND")
        .ok()
        .or_else(|| getenv("MDE_TTS_COMMAND").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            default_path
                .exists()
                .then(|| default_path.display().to_string())
        })
}

/// Parse and validate one Browser read-aloud action payload from the bus.
pub fn parse_request(body: &str, id: &str) -> Result<ReadAloudRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("read-aloud JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_read_aloud") {
        return Err("read-aloud has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("read-aloud source is not browser".to_owned());
    }
    let host = required_str(&v, "host")?;
    let engine = required_str(&v, "engine")?;
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("read-aloud has an unsupported engine".to_owned());
    }
    let url = required_str(&v, "url")?;
    let text = required_str(&v, "text")?;
    let text = clamp_text(&text);
    let title = v
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let truncated = v
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(ReadAloudRequest {
        id: id.to_owned(),
        host,
        tab_index,
        engine,
        url,
        title,
        text,
        truncated,
    })
}

fn required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("read-aloud is missing {key}"))
}

fn clamp_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(text: &str) -> String {
        serde_json::json!({
            "op": "browser_read_aloud",
            "source": "browser",
            "host": "node-a",
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": text,
            "text_chars": text.chars().count(),
            "truncated": false,
        })
        .to_string()
    }

    #[test]
    fn parse_request_accepts_browser_page_text_shape() {
        let parsed = parse_request(&request("Read this page."), "01REQ").unwrap();
        assert_eq!(parsed.id, "01REQ");
        assert_eq!(parsed.host, "node-a");
        assert_eq!(parsed.tab_index, 2);
        assert_eq!(parsed.engine, "cef");
        assert_eq!(parsed.url, "https://example.test/");
        assert_eq!(parsed.title, "Example");
        assert_eq!(parsed.text, "Read this page.");
    }

    #[test]
    fn parse_request_rejects_malformed_or_empty_requests() {
        assert!(parse_request("{}", "01").is_err());
        assert!(
            parse_request(
                r#"{"op":"browser_read_aloud","source":"browser","host":"n","engine":"webkit","url":"https://example.test/","text":"hi"}"#,
                "01"
            )
            .is_err()
        );
        assert!(
            parse_request(
                r#"{"op":"browser_read_aloud","source":"browser","host":"n","engine":"servo","url":"https://example.test/","text":"   "}"#,
                "01"
            )
            .is_err()
        );
    }

    #[test]
    fn parse_request_clamps_abusive_bus_text() {
        let parsed = parse_request(
            &request(&format!("{}tail", "x".repeat(MAX_TEXT_CHARS))),
            "01",
        )
        .unwrap();
        assert_eq!(parsed.text.chars().count(), MAX_TEXT_CHARS);
        assert!(!parsed.text.ends_with("tail"));
    }

    #[test]
    fn configured_tts_command_prefers_env_then_packaged_default() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("browser-read-aloud-tts");
        std::fs::write(&default, "#!/bin/sh\n").unwrap();
        let env_command = configured_command_from_env(
            |key| match key {
                "MDE_BROWSER_TTS_COMMAND" => Ok("  /custom/browser-tts  ".to_owned()),
                _ => Err(std::env::VarError::NotPresent),
            },
            &default,
        );
        assert_eq!(env_command.as_deref(), Some("/custom/browser-tts"));

        let default_command =
            configured_command_from_env(|_| Err(std::env::VarError::NotPresent), &default);
        assert_eq!(default_command, Some(default.display().to_string()));

        let missing_default = configured_command_from_env(
            |_| Err(std::env::VarError::NotPresent),
            &tmp.path().join("missing"),
        );
        assert_eq!(missing_default, None);
    }

    #[test]
    fn tts_command_exit_69_maps_to_unavailable_status() {
        let outcome = run_tts_command(
            "printf 'no voice model configured' >&2; exit 69",
            "hello",
            Duration::from_secs(2),
        );
        assert_eq!(
            outcome,
            SpeakOutcome::Unavailable("no voice model configured".to_owned())
        );
    }

    #[tokio::test]
    async fn apply_request_publishes_spoken_status() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: TtsBackend = Arc::new(|_: &ReadAloudRequest| SpeakOutcome::Spoken);
        let mut worker = BrowserReadAloudWorker::new("node-a".to_owned())
            .with_backend(backend)
            .with_now_fn(Arc::new(|| 42));

        let request = parse_request(&request("hello"), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.speak_request(request).await;
        worker.finish_request(outcome);
        worker.publish_status(&persist);

        let msgs = persist
            .list_since("state/browser-read-aloud/node-a", None)
            .unwrap();
        assert!(
            msgs.len() >= 2,
            "speaking and final statuses should both publish"
        );
        let status: ReadAloudStatus =
            serde_json::from_str(msgs.last().unwrap().body.as_deref().unwrap()).unwrap();
        assert_eq!(status.state, "spoken");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.spoken, 1);
        assert_eq!(status.last_url.as_deref(), Some("https://example.test/"));
        assert_eq!(status.updated_ms, 42);
    }

    #[tokio::test]
    async fn apply_request_surfaces_unconfigured_tts_as_unavailable() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: TtsBackend = Arc::new(|_: &ReadAloudRequest| {
            SpeakOutcome::Unavailable("no voice model configured".to_owned())
        });
        let mut worker = BrowserReadAloudWorker::new("node-a".to_owned()).with_backend(backend);

        let request = parse_request(&request("hello"), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.speak_request(request).await;
        worker.finish_request(outcome);
        worker.publish_status(&persist);

        let status: ReadAloudStatus = serde_json::from_str(
            persist
                .list_since("state/browser-read-aloud/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "unavailable");
        assert_eq!(status.spoken, 0);
        assert_eq!(
            status.last_error.as_deref(),
            Some("no voice model configured")
        );
    }
}
