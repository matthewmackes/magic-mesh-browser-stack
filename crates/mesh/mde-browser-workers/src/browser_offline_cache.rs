//! BROWSER-DD-12 — Browser offline/mesh cache owner.
//!
//! The Browser helper keeps its HTTP disk cache disabled. When the seated shell
//! chooses "Save Offline Copy", it extracts bounded visible page text and
//! publishes `action/browser/offline-cache`. This worker owns the durable side:
//! it validates the private Browser payload, writes a local snapshot, and mirrors
//! the same cache records into the Syncthing-backed workgroup root so peers can
//! reuse the cache without any external account or browser telemetry.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use sha2::{Digest, Sha256};

use mde_worker_core::{ShutdownToken, Worker};

/// Browser-owned offline cache action topic.
pub const ACTION_TOPIC: &str = "action/browser/offline-cache";

/// Retained-latest status topic for this node.
pub const STATE_PREFIX: &str = "state/browser-offline-cache/";

/// Accepted cache-record event topic prefix for Browser consumers.
pub const RESULT_PREFIX: &str = "event/browser-offline-cache/";

/// Share/local subdirectory holding browser offline cache records.
pub const CACHE_SUBDIR: &str = "browser-offline-cache";

/// Cache record directory under [`CACHE_SUBDIR`].
const PAGES_DIR: &str = "pages";

/// Default poll cadence. The shell only publishes on explicit user action, so a
/// short cadence is cheap and keeps the shared mirror fresh.
pub const DEFAULT_TICK: Duration = Duration::from_secs(2);

/// Hard ceiling for retained page text. The shell already clamps, but the daemon
/// enforces the durable-store bound too.
const MAX_TEXT_CHARS: usize = 64_000;
const MAX_VIEWPORT_PNG_BYTES: usize = 2 * 1024 * 1024;
const MAX_MHTML_BYTES: usize = 4 * 1024 * 1024;
const MAX_PDF_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESOURCE_MANIFEST_ENTRIES: usize = 128;
const MAX_RESOURCE_URL_CHARS: usize = 2_048;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Published status for this node's browser offline-cache owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OfflineCacheStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// True when the shared root is writable and all known local records were
    /// mirrored on the last pass.
    pub syncing: bool,
    /// True when a valid local cache record still needs a shared-root mirror.
    pub pending_local: bool,
    /// Most recent cache id accepted by this node.
    pub last_cache_id: Option<String>,
    /// Browser host name from the most recent accepted snapshot.
    pub last_host: Option<String>,
    /// URL from the most recent accepted snapshot.
    pub last_url: Option<String>,
    /// Local persist timestamp for the most recent accepted snapshot.
    pub last_snapshot_ms: Option<u64>,
    /// Shared-root mirror timestamp for the most recent successful mirror pass.
    pub last_mirror_ms: Option<u64>,
}

/// Worker that persists Browser offline cache snapshots.
pub struct BrowserOfflineCacheWorker {
    node: String,
    local_root: PathBuf,
    share_root: PathBuf,
    cursor: Option<String>,
    last_cache_id: Option<String>,
    last_host: Option<String>,
    last_url: Option<String>,
    last_snapshot_ms: Option<u64>,
    last_mirror_ms: Option<u64>,
    pending_local: bool,
    tick: Duration,
    now_fn: NowFn,
    share_gate: Option<Arc<AtomicBool>>,
    bus_root_override: Option<PathBuf>,
}

impl BrowserOfflineCacheWorker {
    /// Create a Browser offline-cache worker for one node and workgroup share.
    #[must_use]
    pub fn new(node: String, local_root: PathBuf, share_root: PathBuf) -> Self {
        Self {
            node,
            local_root,
            share_root,
            cursor: None,
            last_cache_id: None,
            last_host: None,
            last_url: None,
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
                tracing::debug!(target: "mackesd::browser_offline_cache", error = %e, "list_since failed");
                return;
            }
        };
        for msg in msgs {
            self.cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_snapshot(&body, self.now_ms()) {
                Ok(snapshot) => self.apply_snapshot(snapshot, persist),
                Err(e) => {
                    tracing::warn!(
                        target: "mackesd::browser_offline_cache",
                        ulid = %msg.ulid,
                        error = %e,
                        "discarding malformed browser offline-cache snapshot"
                    );
                }
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: BrowserCacheSnapshot, persist: &Persist) {
        let path = cache_path(&self.local_root, &snapshot.cache_id);
        if let Err(e) = write_atomic(&path, &snapshot.body) {
            tracing::warn!(
                target: "mackesd::browser_offline_cache",
                path = %path.display(),
                error = %e,
                "failed to persist local browser offline-cache snapshot"
            );
            return;
        }
        self.publish_record(persist, &snapshot);
        self.last_cache_id = Some(snapshot.cache_id.clone());
        self.last_host = Some(snapshot.host.clone());
        self.last_url = Some(snapshot.url.clone());
        self.last_snapshot_ms = Some(self.now_ms());
        self.pending_local = true;
        self.mirror_pending();
        self.publish_status(persist);
    }

    fn mirror_pending(&mut self) {
        if !self.share_writable() {
            return;
        }
        let mut mirrored_any = false;
        let mut failed = false;
        for (cache_id, body) in local_cache_entries(&self.local_root) {
            let dst = cache_path(&self.share_root, &cache_id);
            if let Err(e) = write_atomic(&dst, &body) {
                tracing::debug!(
                    target: "mackesd::browser_offline_cache",
                    path = %dst.display(),
                    error = %e,
                    "browser offline-cache mirror skipped"
                );
                failed = true;
            } else {
                mirrored_any = true;
            }
        }
        if mirrored_any {
            self.last_mirror_ms = Some(self.now_ms());
        }
        self.pending_local = failed;
        if !failed {
            self.pending_local = false;
        }
    }

    fn publish_status(&self, persist: &Persist) {
        let status = OfflineCacheStatus {
            node: self.node.clone(),
            syncing: self.share_writable() && !self.pending_local,
            pending_local: self.pending_local,
            last_cache_id: self.last_cache_id.clone(),
            last_host: self.last_host.clone(),
            last_url: self.last_url.clone(),
            last_snapshot_ms: self.last_snapshot_ms,
            last_mirror_ms: self.last_mirror_ms,
        };
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&status) {
            let _ = persist.write(&topic, Priority::Min, None, Some(&body));
        }
    }

    fn publish_record(&self, persist: &Persist, snapshot: &BrowserCacheSnapshot) {
        let topic = format!("{RESULT_PREFIX}{}", self.node);
        let mut body = serde_json::json!({
            "op": "browser_offline_cache_record",
            "source": "browser_offline_cache",
            "node": self.node,
            "cache_id": &snapshot.cache_id,
            "host": &snapshot.host,
            "privacy": "offline_or_mesh_only",
            "tab_index": snapshot.tab_index,
            "engine": &snapshot.engine,
            "url": &snapshot.url,
            "title": &snapshot.title,
            "text": &snapshot.text,
            "text_chars": snapshot.text.chars().count(),
            "cached_ms": snapshot.cached_ms,
            "updated_ms": self.now_ms(),
        });
        if let Some(viewport) = &snapshot.viewport_image {
            body["viewport_image"] = serde_json::json!({
                "mime": &viewport.mime,
                "width": viewport.width,
                "height": viewport.height,
                "data": &viewport.data,
            });
        }
        if !snapshot.resource_manifest.is_empty() {
            body["resource_manifest"] = serde_json::Value::Array(
                snapshot
                    .resource_manifest
                    .iter()
                    .map(|resource| {
                        serde_json::json!({
                            "url": &resource.url,
                            "resource": &resource.resource,
                            "allowed": resource.allowed,
                        })
                    })
                    .collect(),
            );
        }
        if let Some(archive) = &snapshot.archive_mhtml {
            body["archive_mhtml"] = serde_json::json!({
                "mime": &archive.mime,
                "filename": &archive.filename,
                "bytes": archive.bytes,
                "data": &archive.data,
            });
        }
        if let Some(pdf) = &snapshot.pdf_snapshot {
            body["pdf_snapshot"] = serde_json::json!({
                "mime": &pdf.mime,
                "filename": &pdf.filename,
                "bytes": pdf.bytes,
                "data": &pdf.data,
            });
        }
        let body = body.to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }
}

#[async_trait::async_trait]
impl Worker for BrowserOfflineCacheWorker {
    fn name(&self) -> &'static str {
        "browser_offline_cache"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_offline_cache", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_offline_cache", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.mirror_pending();
        self.publish_status(&persist);
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.drain_snapshots(&persist);
                    self.mirror_pending();
                    self.publish_status(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        self.mirror_pending();
        self.publish_status(&persist);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserCacheSnapshot {
    cache_id: String,
    host: String,
    tab_index: usize,
    engine: String,
    url: String,
    title: String,
    text: String,
    viewport_image: Option<ViewportImage>,
    resource_manifest: Vec<ResourceManifestItem>,
    archive_mhtml: Option<MhtmlArchive>,
    pdf_snapshot: Option<PdfSnapshot>,
    cached_ms: u64,
    body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewportImage {
    mime: String,
    width: usize,
    height: usize,
    data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResourceManifestItem {
    url: String,
    resource: String,
    allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MhtmlArchive {
    mime: String,
    filename: String,
    bytes: usize,
    data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PdfSnapshot {
    mime: String,
    filename: String,
    bytes: usize,
    data: String,
}

fn parse_snapshot(body: &str, cached_ms: u64) -> Result<BrowserCacheSnapshot, String> {
    let mut v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("offline-cache JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_offline_cache") {
        return Err("wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("wrong source".to_owned());
    }
    if v.get("privacy").and_then(serde_json::Value::as_str) != Some("offline_or_mesh_only") {
        return Err("offline cache must be private/offline-or-mesh only".to_owned());
    }
    let host = required_str(&v, "host").map(|h| sanitize_path_token(&h))?;
    if host.is_empty() {
        return Err("host has no safe path characters".to_owned());
    }
    let engine = required_str(&v, "engine")?;
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("unsupported engine".to_owned());
    }
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0);
    let url = required_str(&v, "url")?;
    let title = v
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let text = required_str(&v, "text")?;
    if text.trim().is_empty() {
        return Err("empty text".to_owned());
    }
    let text = clamp_chars(text.trim(), MAX_TEXT_CHARS);
    let text_chars = text.chars().count();
    let viewport_image = v
        .get("viewport_image")
        .map(parse_viewport_image)
        .transpose()?;
    let resource_manifest = v
        .get("resource_manifest")
        .map(parse_resource_manifest)
        .transpose()?
        .unwrap_or_default();
    let archive_mhtml = v
        .get("archive_mhtml")
        .map(parse_mhtml_archive)
        .transpose()?;
    let pdf_snapshot = v.get("pdf_snapshot").map(parse_pdf_snapshot).transpose()?;
    let cache_id = cache_id(&url, &text);
    let Some(obj) = v.as_object_mut() else {
        return Err("offline-cache body is not an object".to_owned());
    };
    obj.insert("cache_id".to_owned(), serde_json::json!(cache_id.clone()));
    obj.insert("cached_ms".to_owned(), serde_json::json!(cached_ms));
    obj.insert("text".to_owned(), serde_json::json!(text));
    obj.insert("text_chars".to_owned(), serde_json::json!(text_chars));
    if let Some(viewport) = &viewport_image {
        obj.insert(
            "viewport_image".to_owned(),
            serde_json::json!({
                "mime": &viewport.mime,
                "width": viewport.width,
                "height": viewport.height,
                "data": &viewport.data,
            }),
        );
    }
    if !resource_manifest.is_empty() {
        obj.insert(
            "resource_manifest".to_owned(),
            serde_json::Value::Array(
                resource_manifest
                    .iter()
                    .map(|resource| {
                        serde_json::json!({
                            "url": &resource.url,
                            "resource": &resource.resource,
                            "allowed": resource.allowed,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(archive) = &archive_mhtml {
        obj.insert(
            "archive_mhtml".to_owned(),
            serde_json::json!({
                "mime": &archive.mime,
                "filename": &archive.filename,
                "bytes": archive.bytes,
                "data": &archive.data,
            }),
        );
    }
    if let Some(pdf) = &pdf_snapshot {
        obj.insert(
            "pdf_snapshot".to_owned(),
            serde_json::json!({
                "mime": &pdf.mime,
                "filename": &pdf.filename,
                "bytes": pdf.bytes,
                "data": &pdf.data,
            }),
        );
    }
    obj.insert("mesh_cache".to_owned(), serde_json::json!(true));
    let body =
        serde_json::to_string_pretty(&v).map_err(|e| format!("offline-cache encode: {e}"))?;
    Ok(BrowserCacheSnapshot {
        cache_id,
        host,
        tab_index,
        engine,
        url,
        title,
        text,
        viewport_image,
        resource_manifest,
        archive_mhtml,
        pdf_snapshot,
        cached_ms,
        body,
    })
}

fn parse_viewport_image(v: &serde_json::Value) -> Result<ViewportImage, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "image/png")
        .ok_or_else(|| "viewport image must be image/png".to_owned())?;
    let width = v
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "viewport image is missing width".to_owned())?;
    let height = v
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "viewport image is missing height".to_owned())?;
    let data = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "viewport image is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|err| format!("viewport image base64: {err}"))?;
    if bytes.len() > MAX_VIEWPORT_PNG_BYTES {
        return Err("viewport image is too large".to_owned());
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("viewport image is not a PNG".to_owned());
    }
    Ok(ViewportImage {
        mime: mime.to_owned(),
        width,
        height,
        data: data.to_owned(),
    })
}

fn parse_mhtml_archive(v: &serde_json::Value) -> Result<MhtmlArchive, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "multipart/related")
        .ok_or_else(|| "archive must be multipart/related".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_mhtml_filename(name))
        .ok_or_else(|| "archive filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= MAX_MHTML_BYTES)
        .ok_or_else(|| "archive has invalid byte count".to_owned())?;
    let data = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "archive is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|err| format!("archive base64: {err}"))?;
    validate_mhtml_bytes(&bytes, declared_bytes)?;
    Ok(MhtmlArchive {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data: data.to_owned(),
    })
}

fn parse_resource_manifest(v: &serde_json::Value) -> Result<Vec<ResourceManifestItem>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| "resource manifest must be an array".to_owned())?;
    if items.len() > MAX_RESOURCE_MANIFEST_ENTRIES {
        return Err("resource manifest has too many entries".to_owned());
    }
    items
        .iter()
        .map(parse_resource_manifest_item)
        .collect::<Result<Vec<_>, _>>()
}

fn parse_resource_manifest_item(v: &serde_json::Value) -> Result<ResourceManifestItem, String> {
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty() && url.chars().count() <= MAX_RESOURCE_URL_CHARS)
        .ok_or_else(|| "resource manifest URL is invalid".to_owned())?;
    let resource = v
        .get("resource")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|resource| valid_resource_type(resource))
        .ok_or_else(|| "resource manifest type is invalid".to_owned())?;
    let allowed = v
        .get("allowed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "resource manifest allowed flag is missing".to_owned())?;
    Ok(ResourceManifestItem {
        url: url.to_owned(),
        resource: resource.to_owned(),
        allowed,
    })
}

fn validate_mhtml_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("archive byte count mismatch".to_owned());
    }
    if bytes.len() > MAX_MHTML_BYTES {
        return Err("archive is too large".to_owned());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "archive is not UTF-8 MHTML".to_owned())?;
    if !text.starts_with("MIME-Version: 1.0\r\n") || !text.contains("multipart/related") {
        return Err("archive is not MHTML".to_owned());
    }
    Ok(())
}

fn parse_pdf_snapshot(v: &serde_json::Value) -> Result<PdfSnapshot, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "application/pdf")
        .ok_or_else(|| "PDF snapshot must be application/pdf".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_pdf_filename(name))
        .ok_or_else(|| "PDF snapshot filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= MAX_PDF_BYTES)
        .ok_or_else(|| "PDF snapshot has invalid byte count".to_owned())?;
    let data = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "PDF snapshot is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|err| format!("PDF snapshot base64: {err}"))?;
    validate_pdf_bytes(&bytes, declared_bytes)?;
    Ok(PdfSnapshot {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data: data.to_owned(),
    })
}

fn validate_pdf_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("PDF snapshot byte count mismatch".to_owned());
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err("PDF snapshot is too large".to_owned());
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err("PDF snapshot is not a PDF".to_owned());
    }
    Ok(())
}

fn valid_mhtml_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".mhtml")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_pdf_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".pdf")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_resource_type(resource: &str) -> bool {
    matches!(
        resource,
        "document"
            | "subdocument"
            | "stylesheet"
            | "script"
            | "image"
            | "font"
            | "media"
            | "object"
            | "xhr"
            | "ping"
            | "websocket"
            | "other"
    )
}

fn required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing {key}"))
}

fn cache_id(url: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.trim().as_bytes());
    h.update([0]);
    h.update(text.trim().as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn sanitize_path_token(value: &str) -> String {
    value
        .chars()
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

fn clamp_chars(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_owned()
    } else {
        value.chars().take(max).collect()
    }
}

/// Return the durable cache record path for a stable cache id.
#[must_use]
pub fn cache_path(root: &Path, cache_id: &str) -> PathBuf {
    root.join(CACHE_SUBDIR)
        .join(PAGES_DIR)
        .join(format!("{}.json", sanitize_path_token(cache_id)))
}

fn local_cache_entries(root: &Path) -> Vec<(String, String)> {
    let base = root.join(CACHE_SUBDIR).join(PAGES_DIR);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        out.push((stem.to_owned(), body));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
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

/// Resolve the local durable offline-cache root for this host.
#[must_use]
pub fn resolve_local_root() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("/var/lib/mde/browser-offline-cache"),
        |d| d.join("mde").join("browser-offline-cache"),
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

    fn snapshot(host: &str, url: &str, text: &str) -> String {
        serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": host,
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": url,
            "title": "Example",
            "text": text,
            "text_chars": text.chars().count(),
            "truncated": false
        })
        .to_string()
    }

    fn png_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")
    }

    fn mhtml_bytes() -> Vec<u8> {
        b"MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary=\"x\"\r\n\r\n--x--\r\n"
            .to_vec()
    }

    fn mhtml_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(mhtml_bytes())
    }

    fn pdf_bytes() -> Vec<u8> {
        b"%PDF-1.7\n% offline cache fixture\n".to_vec()
    }

    fn pdf_b64() -> String {
        base64::engine::general_purpose::STANDARD.encode(pdf_bytes())
    }

    #[test]
    fn parse_snapshot_accepts_private_browser_cache_shape() {
        let parsed = parse_snapshot(
            &snapshot("work station/1", "https://example.test/", "Cached body"),
            42,
        )
        .unwrap();

        assert_eq!(parsed.host, "work-station1");
        assert_eq!(parsed.url, "https://example.test/");
        assert_eq!(parsed.title, "Example");
        assert_eq!(parsed.tab_index, 0);
        assert_eq!(parsed.engine, "cef");
        assert_eq!(parsed.text, "Cached body");
        assert_eq!(parsed.cache_id.len(), 64);
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["op"], "browser_offline_cache");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["mesh_cache"], true);
        assert_eq!(v["cached_ms"], 42);
        assert_eq!(v["cache_id"], parsed.cache_id);
    }

    #[test]
    fn parse_snapshot_accepts_private_viewport_png_metadata() {
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached body",
            "viewport_image": {
                "mime": "image/png",
                "width": 12,
                "height": 7,
                "data": png_b64(),
            }
        })
        .to_string();
        let parsed = parse_snapshot(&body, 42).unwrap();
        let viewport = parsed.viewport_image.as_ref().expect("viewport image");
        assert_eq!(viewport.mime, "image/png");
        assert_eq!((viewport.width, viewport.height), (12, 7));
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["viewport_image"]["mime"], "image/png");
        assert_eq!(v["viewport_image"]["width"], 12);
        assert_eq!(v["viewport_image"]["height"], 7);
    }

    #[test]
    fn parse_snapshot_accepts_private_mhtml_archive() {
        let bytes = mhtml_bytes();
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached body",
            "archive_mhtml": {
                "mime": "multipart/related",
                "filename": "mde-browser-123-example.test.mhtml",
                "bytes": bytes.len(),
                "data": mhtml_b64(),
            }
        })
        .to_string();
        let parsed = parse_snapshot(&body, 42).unwrap();
        let archive = parsed.archive_mhtml.as_ref().expect("archive");
        assert_eq!(archive.mime, "multipart/related");
        assert_eq!(archive.filename, "mde-browser-123-example.test.mhtml");
        assert_eq!(archive.bytes, bytes.len());
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["archive_mhtml"]["mime"], "multipart/related");
        assert_eq!(v["archive_mhtml"]["bytes"], bytes.len());
    }

    #[test]
    fn parse_snapshot_accepts_private_resource_manifest() {
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached body",
            "resource_manifest": [
                {
                    "url": "https://example.test/app.js",
                    "resource": "script",
                    "allowed": true,
                },
                {
                    "url": "https://tracker.example/pixel.gif",
                    "resource": "image",
                    "allowed": false,
                }
            ]
        })
        .to_string();
        let parsed = parse_snapshot(&body, 42).unwrap();
        assert_eq!(parsed.resource_manifest.len(), 2);
        assert_eq!(parsed.resource_manifest[0].resource, "script");
        assert!(parsed.resource_manifest[0].allowed);
        assert_eq!(parsed.resource_manifest[1].resource, "image");
        assert!(!parsed.resource_manifest[1].allowed);
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(
            v["resource_manifest"][0]["url"],
            "https://example.test/app.js"
        );
        assert_eq!(v["resource_manifest"][1]["allowed"], false);
    }

    #[test]
    fn parse_snapshot_accepts_private_pdf_snapshot() {
        let bytes = pdf_bytes();
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached body",
            "pdf_snapshot": {
                "mime": "application/pdf",
                "filename": "mde-browser-123-example.test.pdf",
                "bytes": bytes.len(),
                "data": pdf_b64(),
            }
        })
        .to_string();
        let parsed = parse_snapshot(&body, 42).unwrap();
        let pdf = parsed.pdf_snapshot.as_ref().expect("PDF snapshot");
        assert_eq!(pdf.mime, "application/pdf");
        assert_eq!(pdf.filename, "mde-browser-123-example.test.pdf");
        assert_eq!(pdf.bytes, bytes.len());
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["pdf_snapshot"]["mime"], "application/pdf");
        assert_eq!(v["pdf_snapshot"]["bytes"], bytes.len());
    }

    #[test]
    fn parse_snapshot_rejects_non_private_or_malformed_cache_payloads() {
        assert!(parse_snapshot("{}", 0).is_err());
        assert!(parse_snapshot(
            r#"{"op":"browser_offline_cache","source":"cloud","privacy":"offline_or_mesh_only"}"#,
            0
        )
        .is_err());
        assert!(
            parse_snapshot(
                r#"{"op":"browser_offline_cache","source":"browser","privacy":"public","host":"h","engine":"cef","url":"https://example.test/","text":"x"}"#,
                0
            )
            .is_err()
        );
        assert!(
            parse_snapshot(
                r#"{"op":"browser_offline_cache","source":"browser","privacy":"offline_or_mesh_only","host":"h","engine":"webkit","url":"https://example.test/","text":"x"}"#,
                0
            )
            .is_err()
        );
        assert!(parse_snapshot(
            &serde_json::json!({
                "op": "browser_offline_cache",
                "source": "browser",
                "host": "h",
                "privacy": "offline_or_mesh_only",
                "engine": "cef",
                "url": "https://example.test/",
                "text": "x",
                "viewport_image": {
                    "mime": "image/png",
                    "width": 1,
                    "height": 1,
                    "data": base64::engine::general_purpose::STANDARD.encode(b"not png"),
                }
            })
            .to_string(),
            0
        )
        .is_err());
        assert!(parse_snapshot(
            &serde_json::json!({
                "op": "browser_offline_cache",
                "source": "browser",
                "host": "h",
                "privacy": "offline_or_mesh_only",
                "engine": "cef",
                "url": "https://example.test/",
                "text": "x",
                "archive_mhtml": {
                    "mime": "multipart/related",
                    "filename": "../bad.mhtml",
                    "bytes": mhtml_bytes().len(),
                    "data": mhtml_b64(),
                }
            })
            .to_string(),
            0
        )
        .is_err());
        assert!(parse_snapshot(
            &serde_json::json!({
                "op": "browser_offline_cache",
                "source": "browser",
                "host": "h",
                "privacy": "offline_or_mesh_only",
                "engine": "cef",
                "url": "https://example.test/",
                "text": "x",
                "pdf_snapshot": {
                    "mime": "application/pdf",
                    "filename": "../bad.pdf",
                    "bytes": pdf_bytes().len(),
                    "data": pdf_b64(),
                }
            })
            .to_string(),
            0
        )
        .is_err());
        assert!(parse_snapshot(
            &serde_json::json!({
                "op": "browser_offline_cache",
                "source": "browser",
                "host": "h",
                "privacy": "offline_or_mesh_only",
                "engine": "cef",
                "url": "https://example.test/",
                "text": "x",
                "resource_manifest": [
                    {
                        "url": "https://example.test/app.js",
                        "resource": "cookie",
                        "allowed": true,
                    }
                ]
            })
            .to_string(),
            0
        )
        .is_err());
    }

    #[test]
    fn parse_snapshot_clamps_abusive_page_text() {
        let parsed = parse_snapshot(
            &snapshot(
                "node-a",
                "https://long.example/",
                &format!("{}tail", "x".repeat(MAX_TEXT_CHARS)),
            ),
            0,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&parsed.body).unwrap();
        assert_eq!(v["text"].as_str().unwrap().chars().count(), MAX_TEXT_CHARS);
        assert_eq!(
            v["text_chars"],
            u64::try_from(MAX_TEXT_CHARS).expect("fits")
        );
    }

    #[test]
    fn apply_snapshot_writes_local_and_mirrors_when_share_is_up() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(true));
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate)
        .with_now_fn(Arc::new(|| 99));
        let snap =
            parse_snapshot(&snapshot("node-a", "https://mesh.test/", "Mesh body"), 42).unwrap();
        let cache_id = snap.cache_id.clone();

        worker.apply_snapshot(snap, &persist);

        let local_body = std::fs::read_to_string(cache_path(local.path(), &cache_id)).unwrap();
        let share_body = std::fs::read_to_string(cache_path(share.path(), &cache_id)).unwrap();
        assert_eq!(local_body, share_body);
        assert!(!worker.pending_local);
        assert_eq!(worker.last_mirror_ms, Some(99));
        let status = persist
            .list_since("state/browser-offline-cache/node-a", None)
            .unwrap()
            .pop()
            .unwrap();
        let status: OfflineCacheStatus =
            serde_json::from_str(status.body.as_deref().unwrap()).unwrap();
        assert!(status.syncing);
        assert_eq!(status.last_cache_id.as_deref(), Some(cache_id.as_str()));
        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-offline-cache/node-a", None)
                .unwrap()
                .pop()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["op"], "browser_offline_cache_record");
        assert_eq!(result["source"], "browser_offline_cache");
        assert_eq!(result["cache_id"], cache_id);
        assert_eq!(result["privacy"], "offline_or_mesh_only");
        assert_eq!(result["url"], "https://mesh.test/");
        assert_eq!(result["text"], "Mesh body");
    }

    #[test]
    fn apply_snapshot_publishes_viewport_png_metadata() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(Arc::new(AtomicBool::new(false)))
        .with_now_fn(Arc::new(|| 99));
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://mesh.test/",
            "title": "Mesh",
            "text": "Mesh body",
            "viewport_image": {
                "mime": "image/png",
                "width": 2,
                "height": 3,
                "data": png_b64(),
            }
        })
        .to_string();
        let snap = parse_snapshot(&body, 42).unwrap();

        worker.apply_snapshot(snap, &persist);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-offline-cache/node-a", None)
                .unwrap()
                .pop()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["viewport_image"]["mime"], "image/png");
        assert_eq!(result["viewport_image"]["width"], 2);
        assert_eq!(result["viewport_image"]["height"], 3);
        assert_eq!(result["viewport_image"]["data"], png_b64());
    }

    #[test]
    fn apply_snapshot_publishes_mhtml_archive_metadata() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(Arc::new(AtomicBool::new(false)))
        .with_now_fn(Arc::new(|| 99));
        let bytes = mhtml_bytes();
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://mesh.test/",
            "title": "Mesh",
            "text": "Mesh body",
            "archive_mhtml": {
                "mime": "multipart/related",
                "filename": "mde-browser-99-mesh.test.mhtml",
                "bytes": bytes.len(),
                "data": mhtml_b64(),
            }
        })
        .to_string();
        let snap = parse_snapshot(&body, 42).unwrap();

        worker.apply_snapshot(snap, &persist);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-offline-cache/node-a", None)
                .unwrap()
                .pop()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["archive_mhtml"]["mime"], "multipart/related");
        assert_eq!(
            result["archive_mhtml"]["filename"],
            "mde-browser-99-mesh.test.mhtml"
        );
        assert_eq!(result["archive_mhtml"]["bytes"], bytes.len());
        assert_eq!(result["archive_mhtml"]["data"], mhtml_b64());
    }

    #[test]
    fn apply_snapshot_publishes_resource_manifest_metadata() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(Arc::new(AtomicBool::new(false)))
        .with_now_fn(Arc::new(|| 99));
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://mesh.test/",
            "title": "Mesh",
            "text": "Mesh body",
            "resource_manifest": [
                {
                    "url": "https://mesh.test/app.js",
                    "resource": "script",
                    "allowed": true,
                },
                {
                    "url": "https://ads.example/pixel.gif",
                    "resource": "image",
                    "allowed": false,
                }
            ]
        })
        .to_string();
        let snap = parse_snapshot(&body, 42).unwrap();

        worker.apply_snapshot(snap, &persist);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-offline-cache/node-a", None)
                .unwrap()
                .pop()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            result["resource_manifest"][0]["url"],
            "https://mesh.test/app.js"
        );
        assert_eq!(result["resource_manifest"][0]["resource"], "script");
        assert_eq!(result["resource_manifest"][0]["allowed"], true);
        assert_eq!(
            result["resource_manifest"][1]["url"],
            "https://ads.example/pixel.gif"
        );
        assert_eq!(result["resource_manifest"][1]["allowed"], false);
    }

    #[test]
    fn apply_snapshot_publishes_pdf_snapshot_metadata() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(Arc::new(AtomicBool::new(false)))
        .with_now_fn(Arc::new(|| 99));
        let bytes = pdf_bytes();
        let body = serde_json::json!({
            "op": "browser_offline_cache",
            "source": "browser",
            "host": "node-a",
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "cef",
            "url": "https://mesh.test/",
            "title": "Mesh",
            "text": "Mesh body",
            "pdf_snapshot": {
                "mime": "application/pdf",
                "filename": "mde-browser-99-mesh.test.pdf",
                "bytes": bytes.len(),
                "data": pdf_b64(),
            }
        })
        .to_string();
        let snap = parse_snapshot(&body, 42).unwrap();

        worker.apply_snapshot(snap, &persist);

        let result: serde_json::Value = serde_json::from_str(
            persist
                .list_since("event/browser-offline-cache/node-a", None)
                .unwrap()
                .pop()
                .unwrap()
                .body
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(result["pdf_snapshot"]["mime"], "application/pdf");
        assert_eq!(
            result["pdf_snapshot"]["filename"],
            "mde-browser-99-mesh.test.pdf"
        );
        assert_eq!(result["pdf_snapshot"]["bytes"], bytes.len());
        assert_eq!(result["pdf_snapshot"]["data"], pdf_b64());
    }

    #[test]
    fn apply_snapshot_keeps_local_pending_when_share_is_down() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let gate = Arc::new(AtomicBool::new(false));
        let mut worker = BrowserOfflineCacheWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_share_gate(gate.clone());
        let snap =
            parse_snapshot(&snapshot("node-a", "https://mesh.test/", "Mesh body"), 42).unwrap();
        let cache_id = snap.cache_id.clone();

        worker.apply_snapshot(snap, &persist);

        assert!(cache_path(local.path(), &cache_id).is_file());
        assert!(!cache_path(share.path(), &cache_id).exists());
        assert!(worker.pending_local);
        gate.store(true, Ordering::SeqCst);
        worker.mirror_pending();
        assert!(cache_path(share.path(), &cache_id).is_file());
        assert!(!worker.pending_local);
    }
}
