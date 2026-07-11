//! BROWSER-DD-12 — Browser private offline/mesh translation owner.
//!
//! The Browser shell owns page text extraction and publishes bounded
//! `action/browser/translate` requests. This worker owns the daemon side of that
//! stream: it validates the request, invokes a locally configured offline/mesh
//! translation command when present, publishes a bounded translation result
//! event, and keeps an honest retained status. Missing local translation assets
//! are surfaced as `Unavailable`, never as fabricated translated text.

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

/// Browser-owned translate request topic.
pub const ACTION_TOPIC: &str = "action/browser/translate";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-translate/";

/// Translation result event topic prefix for this node.
pub const RESULT_PREFIX: &str = "event/browser-translate/";

/// Default poll cadence. Translation is an explicit page action.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

const MAX_TEXT_CHARS: usize = 20_000;
const MAX_TRANSLATION_CHARS: usize = 40_000;
const DEFAULT_TRANSLATE_TIMEOUT: Duration = Duration::from_secs(45);
const DEFAULT_TRANSLATE_COMMAND: &str = "/usr/libexec/mackesd/browser-translate";
const EX_UNAVAILABLE: i32 = 69;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;
type TranslateBackend = Arc<dyn Fn(&TranslateRequest) -> TranslateOutcome + Send + Sync>;

/// Parsed Browser translation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateRequest {
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
    /// Detected/requested source language. The Browser currently sends `auto`.
    pub source_lang: String,
    /// Requested target language.
    pub target_lang: String,
    /// Bounded page text to translate.
    pub text: String,
    /// True when the Browser had to clamp extracted text before publishing.
    pub truncated: bool,
}

/// Status published for the local translation owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranslateStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent request id, if any request was accepted.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Page URL from the most recent accepted request.
    pub last_url: Option<String>,
    /// Source language from the most recent accepted request.
    pub last_source_lang: Option<String>,
    /// Target language from the most recent accepted request.
    pub last_target_lang: Option<String>,
    /// Outcome state: `idle`, `translating`, `translated`, `unavailable`, or `error`.
    pub state: String,
    /// Last human-readable error/unavailable reason.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests that produced translated text.
    pub translated: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Character count of the most recent translated text.
    pub last_translation_chars: Option<u64>,
    /// Timestamp of the most recent accepted request.
    pub last_request_ms: Option<u64>,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser private translation requests.
pub struct BrowserTranslateWorker {
    node: String,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    backend: TranslateBackend,
    bus_root_override: Option<std::path::PathBuf>,
    status: TranslateStatus,
}

impl BrowserTranslateWorker {
    /// Create a Browser translate worker for one node.
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
                let backend = ConfiguredCommandTranslate::from_env();
                Arc::new(move |request| backend.translate(request))
            },
            bus_root_override: None,
            status: TranslateStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_url: None,
                last_source_lang: None,
                last_target_lang: None,
                state: "idle".to_owned(),
                last_error: None,
                accepted: 0,
                translated: 0,
                rejected: 0,
                last_translation_chars: None,
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

    /// Override the translation backend used by tests or embedders.
    #[must_use]
    pub fn with_backend(mut self, backend: TranslateBackend) -> Self {
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

    fn drain_requests(&mut self, persist: &Persist) -> Vec<TranslateRequest> {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_translate", error = %e, "list_since failed");
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

    fn accept_request(&mut self, request: &TranslateRequest) {
        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(request.id.clone());
        self.status.last_host = Some(request.host.clone());
        self.status.last_url = Some(request.url.clone());
        self.status.last_source_lang = Some(request.source_lang.clone());
        self.status.last_target_lang = Some(request.target_lang.clone());
        self.status.last_error = None;
        self.status.last_translation_chars = None;
        self.status.last_request_ms = Some(self.now_ms());
        self.status.state = "translating".to_owned();
        self.status.updated_ms = self.now_ms();
    }

    async fn translate_request(&self, request: TranslateRequest) -> TranslateOutcome {
        let backend = Arc::clone(&self.backend);
        let request_for_backend = request;
        #[allow(clippy::redundant_closure)]
        let handle = tokio::task::spawn_blocking(move || backend(&request_for_backend));
        handle.await.unwrap_or_else(|e| {
            TranslateOutcome::Error(format!("translation backend panicked: {e}"))
        })
    }

    fn finish_request(
        &mut self,
        persist: &Persist,
        request: &TranslateRequest,
        outcome: TranslateOutcome,
    ) {
        match outcome {
            TranslateOutcome::Translated(text) => {
                self.status.translated = self.status.translated.saturating_add(1);
                self.status.state = "translated".to_owned();
                self.status.last_error = None;
                self.status.last_translation_chars =
                    Some(u64::try_from(text.chars().count()).unwrap_or(u64::MAX));
                self.publish_result(persist, request, &text);
            }
            TranslateOutcome::Unavailable(reason) => {
                self.status.state = "unavailable".to_owned();
                self.status.last_error = Some(reason);
            }
            TranslateOutcome::Error(err) => {
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

    fn publish_result(&self, persist: &Persist, request: &TranslateRequest, translation: &str) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_translation",
            "source": "browser_translate",
            "node": self.node,
            "request_id": request.id,
            "host": request.host,
            "tab_index": request.tab_index,
            "engine": request.engine,
            "url": request.url,
            "title": request.title,
            "source_lang": request.source_lang,
            "target_lang": request.target_lang,
            "translation": translation,
            "translation_chars": translation.chars().count(),
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }
}

#[async_trait::async_trait]
impl Worker for BrowserTranslateWorker {
    fn name(&self) -> &'static str {
        "browser_translate"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_translate", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_translate", error = %e, "persist open failed; worker idle");
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
                        let outcome = self.translate_request(request.clone()).await;
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

/// Result of handing browser page text to the local offline/mesh translator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslateOutcome {
    /// The backend produced bounded translated text.
    Translated(String),
    /// No usable offline/mesh translation engine/model is configured on this node.
    Unavailable(String),
    /// A configured backend failed.
    Error(String),
}

#[derive(Debug, Clone)]
struct ConfiguredCommandTranslate {
    command: Option<String>,
    timeout: Duration,
}

impl ConfiguredCommandTranslate {
    fn from_env() -> Self {
        Self {
            command: configured_command_from_env(
                |key| std::env::var(key),
                Path::new(DEFAULT_TRANSLATE_COMMAND),
            ),
            timeout: DEFAULT_TRANSLATE_TIMEOUT,
        }
    }

    fn translate(&self, request: &TranslateRequest) -> TranslateOutcome {
        let Some(command) = &self.command else {
            return TranslateOutcome::Unavailable(
                "offline/mesh translation command is not configured; set MDE_BROWSER_TRANSLATE_COMMAND to a local translation pipeline".to_owned(),
            );
        };
        run_translate_command(command, request, self.timeout)
    }
}

fn run_translate_command(
    command: &str,
    request: &TranslateRequest,
    timeout: Duration,
) -> TranslateOutcome {
    let input = serde_json::json!({
        "op": "browser_translate",
        "request_id": request.id,
        "host": request.host,
        "tab_index": request.tab_index,
        "engine": request.engine,
        "url": request.url,
        "title": request.title,
        "source_lang": request.source_lang,
        "target_lang": request.target_lang,
        "text": request.text,
        "truncated": request.truncated,
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
        Err(e) => {
            return TranslateOutcome::Unavailable(format!(
                "could not start translation command: {e}"
            ));
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(input.as_bytes()) {
            let _ = child.kill();
            let _ = child.wait();
            return TranslateOutcome::Error(format!("could not feed translation request: {e}"));
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
                    let translation = clamp_translation(&stdout);
                    if translation.is_empty() {
                        return TranslateOutcome::Unavailable(
                            "translation produced no output".to_owned(),
                        );
                    }
                    return TranslateOutcome::Translated(translation);
                }
                if status.code() == Some(EX_UNAVAILABLE) {
                    return TranslateOutcome::Unavailable(if stderr.is_empty() {
                        "offline/mesh translation runtime is unavailable".to_owned()
                    } else {
                        stderr.to_owned()
                    });
                }
                return TranslateOutcome::Error(if stderr.is_empty() {
                    format!("translation command exited with {status}")
                } else {
                    format!("translation command exited with {status}: {stderr}")
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return TranslateOutcome::Error("translation command timed out".to_owned());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return TranslateOutcome::Error(format!("translation command wait failed: {e}"));
            }
        }
    }
}

fn configured_command_from_env<E>(getenv: E, default_path: &Path) -> Option<String>
where
    E: Fn(&str) -> Result<String, std::env::VarError>,
{
    getenv("MDE_BROWSER_TRANSLATE_COMMAND")
        .ok()
        .or_else(|| getenv("MDE_TRANSLATE_COMMAND").ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            default_path
                .exists()
                .then(|| default_path.display().to_string())
        })
}

/// Parse and validate one Browser translate action payload from the bus.
pub fn parse_request(body: &str, id: &str) -> Result<TranslateRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("translate JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_translate") {
        return Err("translate has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("translate source is not browser".to_owned());
    }
    if v.get("privacy").and_then(serde_json::Value::as_str) != Some("offline_or_mesh_only") {
        return Err("translate privacy must be offline_or_mesh_only".to_owned());
    }
    let host = required_str(&v, "host")?;
    let engine = required_str(&v, "engine")?;
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("translate has an unsupported engine".to_owned());
    }
    let url = required_str(&v, "url")?;
    let source_lang = required_lang(&v, "source_lang")?;
    let target_lang = required_lang(&v, "target_lang")?;
    let text = clamp_text(&required_str(&v, "text")?);
    let title = optional_str(&v, "title");
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let truncated = v
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(TranslateRequest {
        id: id.to_owned(),
        host,
        tab_index,
        engine,
        url,
        title,
        source_lang,
        target_lang,
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
        .ok_or_else(|| format!("translate is missing {key}"))
}

fn required_lang(v: &serde_json::Value, key: &str) -> Result<String, String> {
    let lang = required_str(v, key)?;
    if lang.len() > 32
        || !lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!("translate has an unsupported {key}"));
    }
    Ok(lang)
}

fn optional_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

fn clamp_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

fn clamp_translation(text: &str) -> String {
    text.trim().chars().take(MAX_TRANSLATION_CHARS).collect()
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
            "op": "browser_translate",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "source_lang": "auto",
            "target_lang": "es",
            "text": text,
            "text_chars": text.chars().count(),
            "truncated": false,
        })
        .to_string()
    }

    #[test]
    fn parse_request_accepts_private_browser_translation_shape() {
        let parsed = parse_request(&request("Translate this page."), "01REQ").unwrap();
        assert_eq!(parsed.id, "01REQ");
        assert_eq!(parsed.host, "node-a");
        assert_eq!(parsed.tab_index, 2);
        assert_eq!(parsed.engine, "cef");
        assert_eq!(parsed.url, "https://example.test/");
        assert_eq!(parsed.title, "Example");
        assert_eq!(parsed.source_lang, "auto");
        assert_eq!(parsed.target_lang, "es");
        assert_eq!(parsed.text, "Translate this page.");
    }

    #[test]
    fn parse_request_rejects_malformed_or_non_private_requests() {
        assert!(parse_request("{}", "01").is_err());
        assert!(
            parse_request(
                r#"{"op":"browser_translate","source":"browser","host":"n","privacy":"cloud_ok","engine":"cef","url":"https://example.test/","source_lang":"auto","target_lang":"en","text":"hi"}"#,
                "01"
            )
            .is_err()
        );
        assert!(
            parse_request(
                r#"{"op":"browser_translate","source":"browser","host":"n","privacy":"offline_or_mesh_only","engine":"webkit","url":"https://example.test/","source_lang":"auto","target_lang":"en","text":"hi"}"#,
                "01"
            )
            .is_err()
        );
        assert!(
            parse_request(
                r#"{"op":"browser_translate","source":"browser","host":"n","privacy":"offline_or_mesh_only","engine":"cef","url":"https://example.test/","source_lang":"auto","target_lang":"en","text":"   "}"#,
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
    fn configured_translate_command_prefers_env_then_packaged_default() {
        let tmp = tempfile::tempdir().unwrap();
        let default = tmp.path().join("browser-translate");
        std::fs::write(&default, "#!/bin/sh\n").unwrap();
        let env_command = configured_command_from_env(
            |key| match key {
                "MDE_BROWSER_TRANSLATE_COMMAND" => Ok("  /custom/browser-translate  ".to_owned()),
                _ => Err(std::env::VarError::NotPresent),
            },
            &default,
        );
        assert_eq!(env_command.as_deref(), Some("/custom/browser-translate"));

        let fallback = configured_command_from_env(
            |key| match key {
                "MDE_TRANSLATE_COMMAND" => Ok("/custom/generic-translate".to_owned()),
                _ => Err(std::env::VarError::NotPresent),
            },
            &default,
        );
        assert_eq!(fallback.as_deref(), Some("/custom/generic-translate"));

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
    fn translate_command_exit_69_maps_to_unavailable_status() {
        let parsed = parse_request(&request("hello"), "01REQ").unwrap();
        let outcome = run_translate_command(
            "printf 'no translation model configured' >&2; exit 69",
            &parsed,
            Duration::from_secs(2),
        );
        assert_eq!(
            outcome,
            TranslateOutcome::Unavailable("no translation model configured".to_owned())
        );
    }

    #[test]
    fn translate_command_stdout_is_bounded_translation() {
        let parsed = parse_request(&request("hello"), "01REQ").unwrap();
        let outcome = run_translate_command(
            &format!("printf '{}tail'", "x".repeat(MAX_TRANSLATION_CHARS)),
            &parsed,
            Duration::from_secs(2),
        );
        let TranslateOutcome::Translated(text) = outcome else {
            panic!("expected translation");
        };
        assert_eq!(text.chars().count(), MAX_TRANSLATION_CHARS);
        assert!(!text.ends_with("tail"));
    }

    #[tokio::test]
    async fn apply_request_publishes_translation_result_and_status() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: TranslateBackend = Arc::new(|_: &TranslateRequest| {
            TranslateOutcome::Translated("Hola desde la pagina.".to_owned())
        });
        let mut worker = BrowserTranslateWorker::new("node-a".to_owned())
            .with_backend(backend)
            .with_now_fn(Arc::new(|| 42));

        let request = parse_request(&request("Hello from the page."), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.translate_request(request.clone()).await;
        worker.finish_request(&persist, &request, outcome);
        worker.publish_status(&persist);

        let status: TranslateStatus = serde_json::from_str(
            persist
                .list_since("state/browser-translate/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "translated");
        assert_eq!(status.accepted, 1);
        assert_eq!(status.translated, 1);
        assert_eq!(status.last_translation_chars, Some(21));
        assert_eq!(status.updated_ms, 42);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-translate/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["op"], "browser_translation");
        assert_eq!(result["source"], "browser_translate");
        assert_eq!(result["translation"], "Hola desde la pagina.");
        assert_eq!(result["source_lang"], "auto");
        assert_eq!(result["target_lang"], "es");
        assert_eq!(result["url"], "https://example.test/");
    }

    #[tokio::test]
    async fn apply_request_surfaces_unconfigured_translate_as_unavailable() {
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let backend: TranslateBackend = Arc::new(|_: &TranslateRequest| {
            TranslateOutcome::Unavailable("no translation model configured".to_owned())
        });
        let mut worker = BrowserTranslateWorker::new("node-a".to_owned()).with_backend(backend);

        let request = parse_request(&request("hello"), "01REQ").unwrap();
        worker.accept_request(&request);
        worker.publish_status(&persist);
        let outcome = worker.translate_request(request.clone()).await;
        worker.finish_request(&persist, &request, outcome);
        worker.publish_status(&persist);

        let status: TranslateStatus = serde_json::from_str(
            persist
                .list_since("state/browser-translate/node-a", None)
                .unwrap()
                .last()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status.state, "unavailable");
        assert_eq!(status.translated, 0);
        assert_eq!(
            status.last_error.as_deref(),
            Some("no translation model configured")
        );
        assert!(persist
            .list_since("event/browser-translate/node-a", None)
            .unwrap()
            .is_empty());
    }
}
