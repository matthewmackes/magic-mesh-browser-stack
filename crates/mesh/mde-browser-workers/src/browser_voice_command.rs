//! BROWSER-DD-11 — Browser voice-command/dictation STT owner.
//!
//! The Browser shell owns the visible command affordance and active-tab context,
//! then publishes `action/browser/voice-command`. This worker owns the daemon side:
//! it validates the request, invokes a configured local STT/capture command when
//! present, publishes a bounded transcript result event, and keeps an honest
//! retained status. Missing STT runtime/model/audio capture is `Unavailable`,
//! never a fabricated transcript.

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

/// Browser-owned voice-command/dictation request topic.
pub const ACTION_TOPIC: &str = "action/browser/voice-command";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-voice-command/";

/// Transcript result event topic prefix for this node.
pub const RESULT_PREFIX: &str = "event/browser-voice-command/";

/// Default poll cadence. Voice command is an explicit user action.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

const MAX_TRANSCRIPT_CHARS: usize = 4_096;
const DEFAULT_STT_TIMEOUT: Duration = Duration::from_secs(45);
const DEFAULT_STT_COMMAND: &str = "/usr/libexec/mackesd/browser-voice-command-stt";
const EX_UNAVAILABLE: i32 = 69;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;
type SttBackend = Arc<dyn Fn(&VoiceCommandRequest) -> SttOutcome + Send + Sync>;

/// Browser voice-command mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    /// Interpret the transcript as a Browser command.
    Command,
    /// Insert or route the transcript as dictation.
    Dictation,
}

impl VoiceMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Dictation => "dictation",
        }
    }
}

/// Parsed Browser voice-command request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceCommandRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Browser host that published the request.
    pub host: String,
    /// Command mode requested by the shell.
    pub mode: VoiceMode,
    /// Tab index in the Browser shell that originated the request.
    pub tab_index: u64,
    /// Browser engine wire label.
    pub engine: String,
    /// Page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Address-bar draft at request time.
    pub address: String,
    /// Focus target at request time (`page` or `chrome`).
    pub focus: String,
    /// Transcript character budget requested by the shell.
    pub max_transcript_chars: usize,
}

/// Status published for the local voice-command owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoiceCommandStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent request id, if any request was accepted.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Page URL from the most recent accepted request.
    pub last_url: Option<String>,
    /// Mode from the most recent accepted request.
    pub last_mode: Option<String>,
    /// Outcome state: `idle`, `listening`, `transcribed`, `unavailable`, or `error`.
    pub state: String,
    /// Last human-readable error/unavailable reason.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests that produced a non-empty transcript.
    pub transcribed: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Character count of the most recent transcript.
    pub last_transcript_chars: Option<u64>,
    /// Timestamp of the most recent accepted request.
    pub last_request_ms: Option<u64>,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser voice-command/dictation requests.
pub struct BrowserVoiceCommandWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    backend: SttBackend,
    bus_root_override: Option<std::path::PathBuf>,
    status: VoiceCommandStatus,
}

impl BrowserVoiceCommandWorker {
    /// Create a Browser voice-command worker for one node.
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
                let backend = ConfiguredCommandStt::from_env();
                Arc::new(move |request| backend.transcribe(request))
            },
            bus_root_override: None,
            status: VoiceCommandStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_url: None,
                last_mode: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                transcribed: 0,
                rejected: 0,
                last_transcript_chars: None,
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

    /// Override the STT backend used by tests or embedders.
    #[must_use]
    pub fn with_backend(mut self, backend: SttBackend) -> Self {
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

    fn drain_requests(&mut self, persist: &Persist) -> Vec<VoiceCommandRequest> {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_voice_command", error = %e, "list_since failed");
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

    fn accept_request(&mut self, request: &VoiceCommandRequest) {
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_url = Some(request.url.clone());
        self.status.last_mode = Some(request.mode.wire().to_owned());
        self.status.last_error = None;
        self.status.last_transcript_chars = None;
        self.status.last_request_ms = Some(self.now_ms());
        self.status.state = "listening".to_owned();
        self.status.updated_ms = self.now_ms();
    }

    async fn transcribe_request(&self, request: VoiceCommandRequest) -> SttOutcome {
        let backend = Arc::clone(&self.backend);
        let request_for_backend = request;
        #[allow(clippy::redundant_closure)]
        let handle = tokio::task::spawn_blocking(move || backend(&request_for_backend));
        handle
            .await
            .unwrap_or_else(|e| SttOutcome::Error(format!("STT backend panicked: {e}")))
    }

    fn finish_request(
        &mut self,
        persist: &Persist,
        request: &VoiceCommandRequest,
        outcome: SttOutcome,
    ) {
        match outcome {
            SttOutcome::Transcript(transcript) => {
                self.status.transcribed = self.status.transcribed.saturating_add(1);
                self.status.state = "transcribed".to_owned();
                self.status.last_error = None;
                self.status.last_transcript_chars =
                    Some(u64::try_from(transcript.chars().count()).unwrap_or(u64::MAX));
                self.publish_result(persist, request, &transcript);
            }
            SttOutcome::Unavailable(reason) => {
                self.status.state = "unavailable".to_owned();
                self.status.last_error = Some(reason);
            }
            SttOutcome::Error(err) => {
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

    fn publish_result(&self, persist: &Persist, request: &VoiceCommandRequest, transcript: &str) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_voice_transcript",
            "source": "browser_voice_command",
            "node": self.node,
            "request_id": request.id,
            "host": request.host,
            "mode": request.mode.wire(),
            "tab_index": request.tab_index,
            "engine": request.engine,
            "url": request.url,
            "title": request.title,
            "address": request.address,
            "focus": request.focus,
            "transcript": transcript,
            "transcript_chars": transcript.chars().count(),
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }
}

#[async_trait::async_trait]
impl Worker for BrowserVoiceCommandWorker {
    fn name(&self) -> &'static str {
        "browser_voice_command"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_voice_command", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_voice_command", error = %e, "persist open failed; worker idle");
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
                        let outcome = self.transcribe_request(request.clone()).await;
                        self.finish_request(&persist, &request, outcome);
                        self.publish_status(&persist);
                    }
                }
                () = shutdown.wait() => break,
            }
        }
        Ok(())
    }
}

/// Result of handing the request to the local offline STT backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttOutcome {
    /// The backend produced a bounded transcript.
    Transcript(String),
    /// No usable offline STT engine/model/capture path is configured on this node.
    Unavailable(String),
    /// A configured backend failed.
    Error(String),
}

#[derive(Debug, Clone)]
struct ConfiguredCommandStt {
    command: Option<String>,
    timeout: Duration,
}

impl ConfiguredCommandStt {
    fn from_env() -> Self {
        Self {
            command: configured_command_from_env(
                |key| std::env::var(key),
                Path::new(DEFAULT_STT_COMMAND),
            ),
            timeout: DEFAULT_STT_TIMEOUT,
        }
    }

    fn transcribe(&self, request: &VoiceCommandRequest) -> SttOutcome {
        let Some(command) = &self.command else {
            return SttOutcome::Unavailable(
                "offline STT command is not configured; set MDE_BROWSER_STT_COMMAND to an on-device capture/transcription pipeline".to_owned(),
            );
        };
        run_stt_command(command, request, self.timeout)
    }
}

fn run_stt_command(command: &str, request: &VoiceCommandRequest, timeout: Duration) -> SttOutcome {
    let input = serde_json::json!({
        "op": "browser_voice_command",
        "request_id": request.id,
        "host": request.host,
        "mode": request.mode.wire(),
        "tab_index": request.tab_index,
        "engine": request.engine,
        "url": request.url,
        "title": request.title,
        "address": request.address,
        "focus": request.focus,
        "max_transcript_chars": request.max_transcript_chars,
    })
    .to_string();
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return SttOutcome::Unavailable(format!("could not start STT command: {e}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(input.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return SttOutcome::Error(format!("could not feed STT request: {e}"));
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout);
                }
                let mut stderr = String::new();
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                let stderr = stderr.trim();
                if status.success() {
                    let transcript = clamp_transcript(&stdout);
                    if transcript.is_empty() {
                        return SttOutcome::Unavailable("STT produced no transcript".to_owned());
                    }
                    return SttOutcome::Transcript(transcript);
                }
                if status.code() == Some(EX_UNAVAILABLE) {
                    return SttOutcome::Unavailable(if stderr.is_empty() {
                        "offline STT runtime is unavailable".to_owned()
                    } else {
                        stderr.to_owned()
                    });
                }
                return SttOutcome::Error(if stderr.is_empty() {
                    format!("STT command exited with {status}")
                } else {
                    format!("STT command exited with {status}: {stderr}")
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return SttOutcome::Error("STT command timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return SttOutcome::Error(format!("STT command wait failed: {e}")),
        }
    }
}

fn configured_command_from_env<E>(getenv: E, default_path: &Path) -> Option<String>
where
    E: Fn(&str) -> Result<String, std::env::VarError>,
{
    getenv("MDE_BROWSER_STT_COMMAND")
        .ok()
        .or_else(|| getenv("MDE_STT_COMMAND").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            default_path
                .exists()
                .then(|| default_path.display().to_string())
        })
}

/// Parse and validate one Browser voice-command action payload from the bus.
pub fn parse_request(body: &str, id: &str) -> Result<VoiceCommandRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("voice-command JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_voice_command") {
        return Err("voice-command has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("voice-command source is not browser".to_owned());
    }
    let host = required_str(&v, "host")?;
    let mode = match required_str(&v, "mode")?.as_str() {
        "command" => VoiceMode::Command,
        "dictation" => VoiceMode::Dictation,
        _ => return Err("voice-command has an unsupported mode".to_owned()),
    };
    let engine = required_str(&v, "engine")?;
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("voice-command has an unsupported engine".to_owned());
    }
    let focus = required_str(&v, "focus")?;
    if !matches!(focus.as_str(), "page" | "chrome") {
        return Err("voice-command has an unsupported focus target".to_owned());
    }
    let url = required_str(&v, "url")?;
    let title = optional_str(&v, "title");
    let address = optional_str(&v, "address");
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let max_transcript_chars = v
        .get("max_transcript_chars")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(MAX_TRANSCRIPT_CHARS)
        .clamp(1, MAX_TRANSCRIPT_CHARS);
    Ok(VoiceCommandRequest {
        id: id.to_owned(),
        host,
        mode,
        tab_index,
        engine,
        url,
        title,
        address,
        focus,
        max_transcript_chars,
    })
}

fn required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("voice-command is missing {key}"))
}

fn optional_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

fn clamp_transcript(text: &str) -> String {
    text.trim().chars().take(MAX_TRANSCRIPT_CHARS).collect()
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(mode: &str) -> String {
        serde_json::json!({
            "op": "browser_voice_command",
            "source": "browser",
            "host": "node-a",
            "mode": mode,
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "address": "https://example.test/current",
            "focus": "page",
            "max_transcript_chars": 4096,
        })
        .to_string()
    }

    #[test]
    fn parse_request_accepts_browser_voice_shape() {
        let parsed = parse_request(&request("dictation"), "01REQ").unwrap();
        assert_eq!(parsed.id, "01REQ");
        assert_eq!(parsed.host, "node-a");
        assert_eq!(parsed.mode, VoiceMode::Dictation);
        assert_eq!(parsed.tab_index, 2);
        assert_eq!(parsed.engine, "cef");
        assert_eq!(parsed.url, "https://example.test/");
        assert_eq!(parsed.title, "Example");
        assert_eq!(parsed.address, "https://example.test/current");
        assert_eq!(parsed.focus, "page");
        assert_eq!(parsed.max_transcript_chars, 4096);
    }

    #[test]
    fn parse_request_rejects_malformed_voice_requests() {
        assert!(parse_request("{}", "01").is_err());
        assert!(
            parse_request(
                r#"{"op":"browser_voice_command","source":"browser","host":"n","mode":"sing","engine":"cef","url":"https://example.test/","focus":"page"}"#,
                "01"
            )
            .is_err()
        );
        assert!(
            parse_request(
                r#"{"op":"browser_voice_command","source":"browser","host":"n","mode":"command","engine":"webkit","url":"https://example.test/","focus":"page"}"#,
                "01"
            )
            .is_err()
        );
        assert!(
            parse_request(
                r#"{"op":"browser_voice_command","source":"browser","host":"n","mode":"command","engine":"cef","url":"https://example.test/","focus":"sidebar"}"#,
                "01"
            )
            .is_err()
        );
    }

    #[test]
    fn configured_stt_command_prefers_env_then_packaged_default() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("browser-voice-command-stt");
        std::fs::write(&default, "#!/bin/sh\n").unwrap();
        let env_command = configured_command_from_env(
            |key| match key {
                "MDE_BROWSER_STT_COMMAND" => Ok("  /custom/browser-stt  ".to_owned()),
                _ => Err(std::env::VarError::NotPresent),
            },
            &default,
        );
        assert_eq!(env_command.as_deref(), Some("/custom/browser-stt"));

        let fallback = configured_command_from_env(
            |key| match key {
                "MDE_STT_COMMAND" => Ok("/custom/generic-stt".to_owned()),
                _ => Err(std::env::VarError::NotPresent),
            },
            &default,
        );
        assert_eq!(fallback.as_deref(), Some("/custom/generic-stt"));

        let default_command =
            configured_command_from_env(|_| Err(std::env::VarError::NotPresent), &default);
        assert_eq!(default_command, Some(default.display().to_string()));

        let missing = configured_command_from_env(
            |_| Err(std::env::VarError::NotPresent),
            &tmp.path().join("missing"),
        );
        assert_eq!(missing, None);
    }

    #[test]
    fn stt_command_exit_69_maps_to_unavailable_status() {
        let parsed = parse_request(&request("command"), "01REQ").unwrap();
        let outcome = run_stt_command(
            "printf 'no STT model configured' >&2; exit 69",
            &parsed,
            Duration::from_secs(2),
        );
        assert_eq!(
            outcome,
            SttOutcome::Unavailable("no STT model configured".to_owned())
        );
    }

    #[test]
    fn stt_command_stdout_is_bounded_transcript() {
        let parsed = parse_request(&request("command"), "01REQ").unwrap();
        let outcome = run_stt_command(
            &format!("printf '{}tail'", "x".repeat(MAX_TRANSCRIPT_CHARS)),
            &parsed,
            Duration::from_secs(2),
        );
        let SttOutcome::Transcript(text) = outcome else {
            panic!("expected transcript");
        };
        assert_eq!(text.chars().count(), MAX_TRANSCRIPT_CHARS);
        assert!(!text.ends_with("tail"));
    }

    #[tokio::test]
    async fn apply_request_publishes_transcript_result_and_status() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: SttBackend =
            Arc::new(|_: &VoiceCommandRequest| SttOutcome::Transcript("open new tab".to_owned()));
        let mut worker = BrowserVoiceCommandWorker::new("node-a".to_owned())
            .with_backend(backend)
            .with_now_fn(Arc::new(|| 42));

        let request = parse_request(&request("command"), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.transcribe_request(request.clone()).await;
        worker.finish_request(&persist, &request, outcome);
        worker.publish_status(&persist);

        let status: VoiceCommandStatus = serde_json::from_str(
            persist
                .list_since("state/browser-voice-command/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "transcribed");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.transcribed, 1);
        assert_eq!(status.last_transcript_chars, Some(12));
        assert_eq!(status.updated_ms, 42);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-voice-command/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["op"], "browser_voice_transcript");
        assert_eq!(result["mode"], "command");
        assert_eq!(result["transcript"], "open new tab");
        assert_eq!(result["url"], "https://example.test/");
    }

    #[tokio::test]
    async fn apply_request_surfaces_unconfigured_stt_as_unavailable() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: SttBackend = Arc::new(|_: &VoiceCommandRequest| {
            SttOutcome::Unavailable("no STT model configured".to_owned())
        });
        let mut worker = BrowserVoiceCommandWorker::new("node-a".to_owned()).with_backend(backend);

        let request = parse_request(&request("dictation"), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.transcribe_request(request.clone()).await;
        worker.finish_request(&persist, &request, outcome);
        worker.publish_status(&persist);

        let status: VoiceCommandStatus = serde_json::from_str(
            persist
                .list_since("state/browser-voice-command/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "unavailable");
        assert_eq!(status.transcribed, 0);
        assert_eq!(
            status.last_error.as_deref(),
            Some("no STT model configured")
        );
        assert!(persist
            .list_since("event/browser-voice-command/node-a", None)
            .unwrap()
            .is_empty());
    }
}
