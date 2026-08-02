//! BUS-1.7 — per-peer subscription manifest + live-reload watcher.
//!
//! The operator's subscription preferences live in
//! `~/.local/share/mde/bus/subs.yaml`. This module:
//!
//! 1. **Loads** the YAML into a [`SubsManifest`], seeding the file
//!    from the template at `/usr/share/mde/bus/subs.yaml.tmpl` when
//!    the per-peer file doesn't exist yet.
//! 2. **Watches** the file via 100 ms mtime polling, parsing on
//!    change and broadcasting the new manifest through a
//!    `tokio::sync::watch` channel.
//! 3. **Applies** subscription decisions through three pure
//!    predicates:
//!    - [`SubsManifest::is_subscribed`] — does this topic match a
//!      `topics:` entry?
//!    - [`SubsManifest::is_muted`] — does it match a `mute:` entry?
//!    - [`SubsManifest::is_within_quiet_hours`] — is the given
//!      local clock time inside the optional `quiet_hours` window?
//!
//! Wildcard matching reuses the same MQTT-style rules
//! [`crate::wildcard`] implements for the topic registry, so the
//! subs schema speaks the same dialect as the rest of the bus.
//!
//! Exit criterion (per BUS-1.7 worklist): "editing the manifest
//! changes live delivery within 200 ms." 100 ms polling cadence
//! satisfies that ceiling; tests assert the watcher emits the new
//! state within one tick of an mtime advance.
//!
//! ## Why mtime-polling instead of inotify?
//!
//! Adding `notify` / `inotify` for a 100 ms-deep watch loop adds a
//! moderate dep + a non-trivial async-event-stream lifecycle. The
//! manifest is operator-edited (rare, deliberate writes), not
//! machine-written, so 100 ms polling is cheap enough that the
//! simpler implementation wins. If a future task needs sub-ms
//! detection, swapping in `notify` is mechanical — the public API
//! here doesn't change.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::wildcard;

/// Default per-peer manifest path under XDG data home.
pub const DEFAULT_SUBS_PATH: &str = "~/.local/share/mde/bus/subs.yaml";

/// Default RPM-shipped template path. The `mde-bus daemon` seeds
/// the per-peer file from this template on first launch when the
/// per-peer file doesn't exist.
pub const DEFAULT_TEMPLATE_PATH: &str = "/usr/share/mde/bus/subs.yaml.tmpl";

/// Default watcher tick cadence — 100 ms gives the 200 ms exit
/// criterion plenty of headroom (100 ms detection + a few ms to
/// parse + propagate).
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_millis(100);

/// Keep operator-controlled subscription manifests and the shipped seed
/// bounded before YAML parsing materializes their strings and lists.
const MAX_SUBS_FILE_BYTES: usize = 64 * 1024;

/// Optional `start..end` window during which delivery defaults to
/// no-op. Times are local-clock 24h strings — `"HH:MM"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    /// Window start, `"HH:MM"`.
    pub start: String,
    /// Window end, `"HH:MM"`.
    pub end: String,
}

/// The subscription manifest as serialized on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubsManifest {
    /// Topic patterns to deliver. MQTT-style wildcards. Empty list
    /// means deliver nothing; the template seeds `["#"]` (everything).
    #[serde(default)]
    pub topics: Vec<String>,
    /// Topic patterns to silence even when they match `topics`.
    #[serde(default)]
    pub mute: Vec<String>,
    /// Optional quiet window. Omit to deliver around the clock.
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
}

impl Default for SubsManifest {
    fn default() -> Self {
        Self {
            topics: vec!["#".to_string()],
            mute: Vec::new(),
            quiet_hours: None,
        }
    }
}

impl SubsManifest {
    /// Parse a YAML body. Empty / whitespace-only input yields the
    /// default manifest (deliver everything) so a freshly-created
    /// empty file doesn't crash the watcher.
    ///
    /// # Errors
    /// Returns `serde_yaml::Error` when the YAML is syntactically
    /// invalid or doesn't match the schema.
    pub fn parse_yaml(body: &str) -> Result<Self, serde_yaml::Error> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str(trimmed)
    }

    /// Render the manifest as YAML body. Used by future CLI verbs
    /// (`mde-bus subs add` / `mute` / `remove`) for round-trip.
    ///
    /// # Errors
    /// Returns `serde_yaml::Error` when serialization fails (very
    /// rare — only schema-invariant violations).
    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    /// Does `topic` match a pattern in the `topics:` list? Empty
    /// `topics` means subscribe to nothing.
    #[must_use]
    pub fn is_subscribed(&self, topic: &str) -> bool {
        self.topics
            .iter()
            .any(|pattern| wildcard::matches(pattern, topic))
    }

    /// Does `topic` match a pattern in the `mute:` list?
    #[must_use]
    pub fn is_muted(&self, topic: &str) -> bool {
        self.mute
            .iter()
            .any(|pattern| wildcard::matches(pattern, topic))
    }

    /// Combined filter: should `topic` be delivered RIGHT NOW given
    /// the manifest + the current clock time? `now_hhmm` is the
    /// local time as a `"HH:MM"` string. Tests pass a deterministic
    /// value; live callers pass `chrono::Local::now()` formatted.
    #[must_use]
    pub fn should_deliver(&self, topic: &str, now_hhmm: &str) -> bool {
        if !self.is_subscribed(topic) || self.is_muted(topic) {
            return false;
        }
        !self.is_within_quiet_hours(now_hhmm)
    }

    /// Is `now_hhmm` (`"HH:MM"`) inside the manifest's quiet window?
    /// Returns `false` when no quiet block is defined or the time
    /// can't be parsed (malformed entries fail open — deliver).
    #[must_use]
    pub fn is_within_quiet_hours(&self, now_hhmm: &str) -> bool {
        let Some(qh) = &self.quiet_hours else {
            return false;
        };
        match (
            parse_hhmm(now_hhmm),
            parse_hhmm(&qh.start),
            parse_hhmm(&qh.end),
        ) {
            (Some(now), Some(start), Some(end)) => {
                if start <= end {
                    // Non-wrapping window: 09:00..17:00
                    now >= start && now < end
                } else {
                    // Wrapping window: 22:00..07:00
                    now >= start || now < end
                }
            }
            _ => false,
        }
    }
}

/// Parse `"HH:MM"` into minutes-since-midnight. Returns `None` on
/// malformed input (out-of-range hour/minute, non-numeric, missing
/// colon).
fn parse_hhmm(s: &str) -> Option<u16> {
    let (h_str, m_str) = s.split_once(':')?;
    let h: u16 = h_str.parse().ok()?;
    let m: u16 = m_str.parse().ok()?;
    if h >= 24 || m >= 60 {
        return None;
    }
    Some(h * 60 + m)
}

/// Reason the subs module skipped seeding / loading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubsSkipReason {
    /// XDG data dir couldn't be resolved (no `$XDG_DATA_HOME` /
    /// `$HOME`); the watcher won't know where to look. Caller
    /// should fall back to defaults.
    NoDataDir,
    /// Template file at `<template_path>` wasn't found and the
    /// per-peer file also doesn't exist. Watcher continues with
    /// the in-memory default manifest.
    NoTemplate,
}

impl std::fmt::Display for SubsSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDataDir => write!(f, "no XDG data home — subs manifest path can't be resolved"),
            Self::NoTemplate => {
                write!(f, "subs.yaml.tmpl missing — using in-memory defaults")
            }
        }
    }
}

/// Resolve the per-peer subs.yaml path under XDG data home.
/// Returns `None` when neither `$XDG_DATA_HOME` nor `$HOME` is set.
#[must_use]
pub fn default_per_peer_path() -> Option<PathBuf> {
    crate::default_data_dir().map(|d| d.join("subs.yaml"))
}

/// Read a stable regular file through an `O_NOFOLLOW` descriptor.
///
/// The descriptor is checked rather than the path after opening, so a final
/// symlink, special file, oversized file, file growth, or invalid UTF-8 is
/// rejected before the manifest reaches YAML parsing. Reading one byte beyond
/// the cap catches growth after the initial metadata snapshot without an
/// unbounded allocation.
fn read_bounded_subs_file(path: &Path) -> io::Result<String> {
    let max_bytes_u64 = u64::try_from(MAX_SUBS_FILE_BYTES).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "subscription size cap overflows u64",
        )
    })?;
    let entry = std::fs::symlink_metadata(path)?;
    if !entry.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if entry.len() > max_bytes_u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {MAX_SUBS_FILE_BYTES}-byte subscription limit",
                path.display()
            ),
        ));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0o400000); // O_NOFOLLOW
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        options.custom_flags(0x100); // O_NOFOLLOW

        // Keep unsupported Unix targets fail-closed for a symlink leaf even
        // when their standard library does not expose a known O_NOFOLLOW
        // value in this crate's dependency surface.
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        if !std::fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", path.display()),
            ));
        }
    }

    #[cfg(not(unix))]
    if !std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a regular file", path.display()),
        ));
    }

    let mut file = options.open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} opened as a non-regular file", path.display()),
        ));
    }
    if opened.len() > max_bytes_u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {MAX_SUBS_FILE_BYTES}-byte subscription limit",
                path.display()
            ),
        ));
    }

    let capacity = usize::try_from(opened.len())
        .unwrap_or(MAX_SUBS_FILE_BYTES)
        .min(MAX_SUBS_FILE_BYTES)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SUBS_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {MAX_SUBS_FILE_BYTES}-byte subscription limit",
                path.display()
            ),
        ));
    }

    let after = file.metadata()?;
    if !after.file_type().is_file()
        || after.len() != opened.len()
        || after.len() != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} changed while being read", path.display()),
        ));
    }

    String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid UTF-8: {error}", path.display()),
        )
    })
}

/// Seed the per-peer file from the template if the per-peer file
/// doesn't exist yet. Returns the body that's now on disk (either
/// the freshly-seeded template OR the existing file content).
///
/// # Errors
/// Returns `std::io::Error` on mkdir / read / write / rename
/// failure.
pub fn load_or_seed(per_peer_path: &Path, template_path: &Path) -> Result<String, SubsLoadError> {
    match read_bounded_subs_file(per_peer_path) {
        Ok(body) => return Ok(body),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(SubsLoadError::ReadPerPeer(error)),
    }
    let template_body = match read_bounded_subs_file(template_path) {
        Ok(body) => body,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SubsLoadError::TemplateMissing);
        }
        Err(error) => return Err(SubsLoadError::ReadTemplate(error)),
    };
    if let Some(parent) = per_peer_path.parent() {
        std::fs::create_dir_all(parent).map_err(SubsLoadError::Mkdir)?;
    }
    let tmp = per_peer_path.with_extension("yaml.tmp");
    std::fs::write(&tmp, template_body.as_bytes()).map_err(SubsLoadError::WriteTmp)?;
    std::fs::rename(&tmp, per_peer_path).map_err(SubsLoadError::Rename)?;
    Ok(template_body)
}

/// Errors from [`load_or_seed`].
#[derive(Debug)]
pub enum SubsLoadError {
    /// Could not read the existing per-peer file.
    ReadPerPeer(std::io::Error),
    /// Template file doesn't exist; can't seed.
    TemplateMissing,
    /// Could not read the template body.
    ReadTemplate(std::io::Error),
    /// Could not create the per-peer file's parent dir.
    Mkdir(std::io::Error),
    /// Could not write the temp file during atomic seed.
    WriteTmp(std::io::Error),
    /// Could not rename temp → final during atomic seed.
    Rename(std::io::Error),
}

impl std::fmt::Display for SubsLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadPerPeer(e) => write!(f, "read per-peer subs.yaml: {e}"),
            Self::TemplateMissing => {
                write!(f, "subs.yaml.tmpl missing — can't seed per-peer file")
            }
            Self::ReadTemplate(e) => write!(f, "read subs.yaml.tmpl: {e}"),
            Self::Mkdir(e) => write!(f, "mkdir subs.yaml parent: {e}"),
            Self::WriteTmp(e) => write!(f, "write subs.yaml.tmp: {e}"),
            Self::Rename(e) => write!(f, "rename subs.yaml: {e}"),
        }
    }
}

impl std::error::Error for SubsLoadError {}

/// Live watcher. Polls the per-peer file's mtime every
/// [`DEFAULT_TICK_INTERVAL`]; on advance re-reads + re-parses and
/// publishes the new manifest through an `Arc<tokio::sync::watch>`.
///
/// Cloning is cheap (just clones the `Arc<Receiver>`).
pub struct SubsWatcher {
    per_peer_path: PathBuf,
    tick_interval: Duration,
    tx: Arc<tokio::sync::watch::Sender<SubsManifest>>,
    rx: tokio::sync::watch::Receiver<SubsManifest>,
    last_mtime: Option<SystemTime>,
}

impl SubsWatcher {
    /// Construct a watcher pinned to the given per-peer path. The
    /// initial manifest is seeded from the supplied body (typically
    /// the result of [`load_or_seed`]); empty / unparseable body
    /// falls back to [`SubsManifest::default`].
    #[must_use]
    pub fn new(per_peer_path: PathBuf, initial_body: &str) -> Self {
        let initial = SubsManifest::parse_yaml(initial_body).unwrap_or_default();
        let (tx, rx) = tokio::sync::watch::channel(initial);
        Self {
            per_peer_path,
            tick_interval: DEFAULT_TICK_INTERVAL,
            tx: Arc::new(tx),
            rx,
            last_mtime: None,
        }
    }

    /// Override the tick interval — used by tests that need a
    /// faster pulse.
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Subscribe to manifest updates. Each call returns a fresh
    /// `Receiver` cloned off the watcher's `Sender`; the latest
    /// value is always immediately readable via `borrow()`.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<SubsManifest> {
        self.rx.clone()
    }

    /// Snapshot the current manifest. Cheaper than `subscribe()`
    /// when the caller only needs one read.
    #[must_use]
    pub fn current(&self) -> SubsManifest {
        self.rx.borrow().clone()
    }

    /// Drive one tick of the watch loop. Public so tests can run
    /// it deterministically.
    pub fn tick_once(&mut self) -> TickOutcome {
        let entry = match std::fs::symlink_metadata(&self.per_peer_path) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return TickOutcome::FileMissing;
            }
            Err(_) => return TickOutcome::Idle,
        };
        if !entry.file_type().is_file() {
            return TickOutcome::Idle;
        }
        let now = match entry.modified() {
            Ok(t) => t,
            Err(_) => return TickOutcome::Idle,
        };
        let advanced = self.last_mtime.is_none_or(|last| now > last);
        self.last_mtime = Some(now);
        if !advanced {
            return TickOutcome::Idle;
        }
        let body = match read_bounded_subs_file(&self.per_peer_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "mde_bus::subs",
                    error = %e,
                    path = %self.per_peer_path.display(),
                    "subs.yaml re-read failed"
                );
                return TickOutcome::Idle;
            }
        };
        let parsed = match SubsManifest::parse_yaml(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    target: "mde_bus::subs",
                    error = %e,
                    "subs.yaml parse failed — keeping previous manifest"
                );
                return TickOutcome::ParseFailed;
            }
        };
        let changed = *self.tx.borrow() != parsed;
        if changed {
            // send_replace returns the previous value; we drop it.
            let _ = self.tx.send_replace(parsed);
            tracing::info!(
                target: "mde_bus::subs",
                path = %self.per_peer_path.display(),
                "subs manifest reloaded"
            );
            TickOutcome::Reloaded
        } else {
            // mtime advanced but content was identical — e.g.,
            // `touch subs.yaml`. Treat as no-op.
            TickOutcome::Idle
        }
    }

    /// Long-running async loop. Calls [`Self::tick_once`] every
    /// `tick_interval` until `shutdown.changed()` resolves.
    pub async fn run(&mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            let _ = self.tick_once();
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = tokio::time::sleep(self.tick_interval) => {},
            }
            if *shutdown.borrow() {
                break;
            }
        }
    }
}

/// Per-tick result. Exposed for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// The per-peer file is missing.
    FileMissing,
    /// mtime didn't advance or content unchanged; nothing to do.
    Idle,
    /// File parsed cleanly + manifest differed from previous;
    /// new manifest broadcast.
    Reloaded,
    /// File mtime advanced but parsing failed; previous manifest
    /// retained.
    ParseFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_body() -> &'static str {
        include_str!("../../../../data/bus/subs.yaml.tmpl")
    }

    #[test]
    fn default_manifest_subscribes_to_everything() {
        let m = SubsManifest::default();
        assert!(m.is_subscribed("anything/at/all"));
        assert!(m.is_subscribed("peer/alice/alerts"));
        assert!(!m.is_muted("anything/at/all"));
        assert!(m.quiet_hours.is_none());
    }

    #[test]
    fn template_round_trips_through_yaml() {
        let parsed = SubsManifest::parse_yaml(template_body()).expect("template parses");
        assert_eq!(parsed.topics, vec!["#".to_string()]);
        assert!(parsed.mute.is_empty());
        assert!(parsed.quiet_hours.is_none());
    }

    #[test]
    fn empty_body_yields_default_manifest() {
        assert_eq!(
            SubsManifest::parse_yaml("").expect("empty parses"),
            SubsManifest::default()
        );
        assert_eq!(
            SubsManifest::parse_yaml("   \n\t  \n").expect("whitespace parses"),
            SubsManifest::default()
        );
    }

    #[test]
    fn malformed_yaml_returns_err() {
        let r = SubsManifest::parse_yaml("topics: [unterminated");
        assert!(r.is_err());
    }

    #[test]
    fn subscribe_honors_wildcards() {
        let m = SubsManifest {
            topics: vec!["fleet/+".to_string(), "peer/+/alerts".to_string()],
            mute: Vec::new(),
            quiet_hours: None,
        };
        assert!(m.is_subscribed("fleet/sec"));
        assert!(m.is_subscribed("peer/alice/alerts"));
        assert!(!m.is_subscribed("clipboard/sync"));
    }

    #[test]
    fn mute_silences_matching_topics() {
        let m = SubsManifest {
            topics: vec!["#".to_string()],
            mute: vec!["fdo/system".to_string()],
            quiet_hours: None,
        };
        assert!(m.is_subscribed("fdo/system"));
        assert!(m.is_muted("fdo/system"));
        assert!(!m.should_deliver("fdo/system", "12:00"));
        assert!(m.should_deliver("peer/alice/alerts", "12:00"));
    }

    #[test]
    fn quiet_hours_non_wrapping_window() {
        let m = SubsManifest {
            topics: vec!["#".to_string()],
            mute: Vec::new(),
            quiet_hours: Some(QuietHours {
                start: "09:00".to_string(),
                end: "17:00".to_string(),
            }),
        };
        assert!(m.is_within_quiet_hours("12:00"));
        assert!(m.is_within_quiet_hours("09:00")); // boundary inclusive
        assert!(!m.is_within_quiet_hours("17:00")); // end exclusive
        assert!(!m.is_within_quiet_hours("08:59"));
        assert!(!m.is_within_quiet_hours("18:00"));
    }

    #[test]
    fn quiet_hours_wrapping_window() {
        // 22:00..07:00 — most-natural night-time
        let m = SubsManifest {
            topics: vec!["#".to_string()],
            mute: Vec::new(),
            quiet_hours: Some(QuietHours {
                start: "22:00".to_string(),
                end: "07:00".to_string(),
            }),
        };
        assert!(m.is_within_quiet_hours("22:30"));
        assert!(m.is_within_quiet_hours("02:00"));
        assert!(m.is_within_quiet_hours("06:59"));
        assert!(!m.is_within_quiet_hours("07:00"));
        assert!(!m.is_within_quiet_hours("12:00"));
        assert!(!m.is_within_quiet_hours("21:59"));
    }

    #[test]
    fn malformed_clock_strings_fail_open() {
        let m = SubsManifest {
            topics: vec!["#".to_string()],
            mute: Vec::new(),
            quiet_hours: Some(QuietHours {
                start: "not-a-time".to_string(),
                end: "17:00".to_string(),
            }),
        };
        // Malformed start → fail open (deliver).
        assert!(!m.is_within_quiet_hours("12:00"));
    }

    #[test]
    fn yaml_round_trip_via_serde() {
        let m = SubsManifest {
            topics: vec!["fleet/+".to_string(), "peer/+/alerts".to_string()],
            mute: vec!["fdo/system".to_string()],
            quiet_hours: Some(QuietHours {
                start: "22:00".to_string(),
                end: "07:00".to_string(),
            }),
        };
        let body = m.to_yaml().expect("serialize");
        let parsed = SubsManifest::parse_yaml(&body).expect("parse");
        assert_eq!(m, parsed);
    }

    #[test]
    fn load_or_seed_creates_per_peer_from_template() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let template = tmp.path().join("subs.yaml.tmpl");
        std::fs::write(&template, template_body()).expect("seed template");
        assert!(!per_peer.exists());
        let body = load_or_seed(&per_peer, &template).expect("seed ok");
        assert!(per_peer.exists());
        assert!(body.contains("topics:"));
        // Second call returns the existing body, not the template
        // — operator edits survive.
        std::fs::write(&per_peer, "topics: [special]\n").expect("operator edit");
        let body2 = load_or_seed(&per_peer, &template).expect("re-read ok");
        assert!(body2.contains("special"));
    }

    #[test]
    fn load_or_seed_errors_when_template_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let template = tmp.path().join("nonexistent.tmpl");
        let r = load_or_seed(&per_peer, &template);
        assert!(matches!(r, Err(SubsLoadError::TemplateMissing)));
    }

    #[cfg(unix)]
    #[test]
    fn load_or_seed_rejects_final_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let template = tmp.path().join("subs.yaml.tmpl");
        let target = tmp.path().join("outside.yaml");
        std::fs::write(&target, "topics: [outside]\n").expect("outside body");
        std::fs::write(&template, template_body()).expect("seed template");

        symlink(&target, &per_peer).expect("per-peer symlink");
        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadPerPeer(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));
        assert_eq!(
            std::fs::read_to_string(&target).expect("outside remains readable"),
            "topics: [outside]\n"
        );

        std::fs::remove_file(&per_peer).expect("remove per-peer symlink");
        std::fs::remove_file(&template).expect("remove regular template");
        symlink(&target, &template).expect("template symlink");
        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadTemplate(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn load_or_seed_rejects_oversized_inputs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let template = tmp.path().join("subs.yaml.tmpl");
        let oversized = vec![b'x'; MAX_SUBS_FILE_BYTES + 1];
        std::fs::write(&per_peer, &oversized).expect("oversized per-peer body");
        std::fs::write(&template, template_body()).expect("seed template");

        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadPerPeer(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));

        std::fs::remove_file(&per_peer).expect("remove oversized per-peer body");
        std::fs::write(&template, &oversized).expect("oversized template body");
        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadTemplate(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[test]
    fn load_or_seed_rejects_invalid_utf8_inputs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let template = tmp.path().join("subs.yaml.tmpl");
        let invalid_utf8 = b"topics: [\xff]\n";
        std::fs::write(&per_peer, invalid_utf8).expect("invalid per-peer body");
        std::fs::write(&template, template_body()).expect("seed template");

        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadPerPeer(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));

        std::fs::remove_file(&per_peer).expect("remove invalid per-peer body");
        std::fs::write(&template, invalid_utf8).expect("invalid template body");
        assert!(matches!(
            load_or_seed(&per_peer, &template),
            Err(SubsLoadError::ReadTemplate(error))
                if error.kind() == io::ErrorKind::InvalidData
        ));
    }

    #[tokio::test]
    async fn watcher_reload_on_mtime_advance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        std::fs::write(&per_peer, "topics: [\"a\"]\n").expect("seed");
        let mut w = SubsWatcher::new(per_peer.clone(), "topics: [\"a\"]\n");
        // First tick records mtime; same body — no reload.
        assert_eq!(w.tick_once(), TickOutcome::Idle);
        // Wait > mtime resolution then rewrite with new content.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&per_peer, "topics: [\"b\"]\n").expect("rewrite");
        assert_eq!(w.tick_once(), TickOutcome::Reloaded);
        let current = w.current();
        assert_eq!(current.topics, vec!["b".to_string()]);
    }

    #[tokio::test]
    async fn watcher_emits_to_subscriber_on_reload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        std::fs::write(&per_peer, "topics: [\"a\"]\n").expect("seed");
        let mut w = SubsWatcher::new(per_peer.clone(), "topics: [\"a\"]\n");
        let mut sub = w.subscribe();
        assert_eq!(sub.borrow_and_update().topics, vec!["a".to_string()]);
        w.tick_once();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&per_peer, "topics: [\"b\", \"c\"]\n").expect("rewrite");
        assert_eq!(w.tick_once(), TickOutcome::Reloaded);
        // Subscriber sees the change.
        assert!(sub.has_changed().expect("recv alive"));
        let snap = sub.borrow_and_update();
        assert_eq!(snap.topics, vec!["b".to_string(), "c".to_string()]);
    }

    #[tokio::test]
    async fn watcher_keeps_previous_manifest_on_parse_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        std::fs::write(&per_peer, "topics: [\"a\"]\n").expect("seed");
        let mut w = SubsWatcher::new(per_peer.clone(), "topics: [\"a\"]\n");
        w.tick_once();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&per_peer, "topics: [unterminated\n").expect("corrupt");
        assert_eq!(w.tick_once(), TickOutcome::ParseFailed);
        // Previous manifest still wins.
        assert_eq!(w.current().topics, vec!["a".to_string()]);
    }

    #[tokio::test]
    async fn watcher_keeps_previous_manifest_on_bounded_read_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        std::fs::write(&per_peer, "topics: [\"a\"]\n").expect("seed");
        let mut w = SubsWatcher::new(per_peer.clone(), "topics: [\"a\"]\n");
        w.tick_once();

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&per_peer, vec![b'x'; MAX_SUBS_FILE_BYTES + 1]).expect("oversized rewrite");
        assert_eq!(w.tick_once(), TickOutcome::Idle);
        assert_eq!(w.current().topics, vec!["a".to_string()]);

        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&per_peer, b"topics: [\xff]\n").expect("invalid UTF-8 rewrite");
        assert_eq!(w.tick_once(), TickOutcome::Idle);
        assert_eq!(w.current().topics, vec!["a".to_string()]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_keeps_previous_manifest_on_final_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let per_peer = tmp.path().join("subs.yaml");
        let target = tmp.path().join("outside.yaml");
        std::fs::write(&per_peer, "topics: [\"a\"]\n").expect("seed");
        std::fs::write(&target, "topics: [\"b\"]\n").expect("outside body");
        let mut w = SubsWatcher::new(per_peer.clone(), "topics: [\"a\"]\n");
        w.tick_once();

        std::thread::sleep(Duration::from_millis(20));
        std::fs::remove_file(&per_peer).expect("remove regular manifest");
        symlink(&target, &per_peer).expect("manifest symlink");
        assert_eq!(w.tick_once(), TickOutcome::Idle);
        assert_eq!(w.current().topics, vec!["a".to_string()]);
    }
}
