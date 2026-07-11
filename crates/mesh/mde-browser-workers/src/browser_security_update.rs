//! BROWSER-DD-12 — Browser CEF security-update status owner.
//!
//! Browser engine/runtime updates ride an independent fast path, separate from
//! the desktop shell. This worker owns the daemon-side proof point for that path:
//! it compares the packaged CEF runtime manifest against the active installed
//! runtime, invokes the packaged installer when the active runtime is absent or
//! mismatched, then publishes an honest retained status. The installer still owns
//! all archive download, SHA-256 verification, extraction, and symlink promotion;
//! this worker owns the independent timed trigger and fleet-visible result.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Retained-latest topic prefix carrying this node's browser runtime posture.
pub const STATE_PREFIX: &str = "state/browser-security-update/";

/// Default packaged CEF runtime manifest installed by the browser package.
pub const DEFAULT_MANIFEST_PATH: &str = "/usr/share/magic-mesh/browser/cef-linux64-minimal.env";

/// Default active runtime symlink used by `install-cef-runtime`.
pub const DEFAULT_ACTIVE_LINK: &str = "/opt/mde/cef";

/// Installed-runtime manifest written next to the extracted CEF runtime.
pub const INSTALLED_MANIFEST_FILE: &str = "mde-cef-runtime.manifest";

/// Default updater command shipped by the Workstation RPM.
pub const DEFAULT_UPDATER_COMMAND: &str = "/usr/libexec/mackesd/install-cef-runtime";

/// Default status cadence. Engine security status changes only when the fast
/// updater/provisioning path promotes a new runtime.
pub const DEFAULT_TICK: Duration = Duration::from_secs(300);

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Published browser runtime update posture for one node.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowserSecurityUpdateStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// `current`, `missing`, `mismatch`, or `manifest_missing`.
    pub state: String,
    /// Expected CEF build from the packaged updater manifest.
    pub expected_cef_version: Option<String>,
    /// Expected Chromium build from the packaged updater manifest.
    pub expected_chromium_version: Option<String>,
    /// Expected release channel from the packaged updater manifest.
    pub expected_channel: Option<String>,
    /// Expected archive asset name from the packaged updater manifest.
    pub expected_asset: Option<String>,
    /// Expected archive SHA-256 from the packaged updater manifest.
    pub expected_sha256: Option<String>,
    /// Manifest path inspected by this worker.
    pub manifest_path: String,
    /// Active runtime link/path inspected by this worker.
    pub active_link: String,
    /// Resolved active runtime directory, when present.
    pub active_runtime: Option<String>,
    /// Installed CEF version from the active runtime manifest.
    pub installed_version: Option<String>,
    /// Installed Chromium version from the active runtime manifest.
    pub installed_chromium: Option<String>,
    /// Installed archive SHA-256 from the active runtime manifest.
    pub installed_sha256: Option<String>,
    /// Whether `Release/libcef.so` exists under the active runtime.
    pub libcef_present: bool,
    /// Human-readable reason when the state is not `current`.
    pub last_error: Option<String>,
    /// Installer command this worker would invoke for `missing`/`mismatch`.
    pub updater_command: Option<String>,
    /// `idle`, `installing`, `attempted`, `failed`, or `unavailable`.
    pub updater_state: String,
    /// Wall-clock ms of the most recent updater attempt.
    pub last_update_ms: Option<u64>,
    /// Process exit code from the most recent updater attempt, when it spawned.
    pub last_update_exit_code: Option<i32>,
    /// Process spawn/stderr summary from the most recent failed updater attempt.
    pub last_update_error: Option<String>,
    /// Wall-clock ms for this inspection.
    pub updated_ms: u64,
}

/// Worker that publishes this node's browser CEF runtime security posture.
pub struct BrowserSecurityUpdateWorker {
    node: String,
    manifest_path: PathBuf,
    active_link_override: Option<PathBuf>,
    updater_command: PathBuf,
    tick: Duration,
    now_fn: NowFn,
    bus_root_override: Option<PathBuf>,
    last_update_ms: Option<u64>,
    last_update_exit_code: Option<i32>,
    last_update_error: Option<String>,
}

impl BrowserSecurityUpdateWorker {
    /// Create a browser security-update worker for one node.
    #[must_use]
    pub fn new(node: String) -> Self {
        Self {
            node,
            manifest_path: resolve_manifest_path(),
            active_link_override: None,
            updater_command: PathBuf::from(DEFAULT_UPDATER_COMMAND),
            tick: DEFAULT_TICK,
            now_fn: Arc::new(default_now),
            bus_root_override: None,
            last_update_ms: None,
            last_update_exit_code: None,
            last_update_error: None,
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

    /// Override the packaged CEF manifest path.
    #[must_use]
    pub fn with_manifest_path(mut self, path: PathBuf) -> Self {
        self.manifest_path = path;
        self
    }

    /// Override the active CEF runtime link/path.
    #[must_use]
    pub fn with_active_link(mut self, path: PathBuf) -> Self {
        self.active_link_override = Some(path);
        self
    }

    /// Override the updater command used for deterministic tests or packaging
    /// variants.
    #[must_use]
    pub fn with_updater_command(mut self, path: PathBuf) -> Self {
        self.updater_command = path;
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

    fn inspect(&self) -> BrowserSecurityUpdateStatus {
        let mut status = inspect_runtime(
            &self.node,
            &self.manifest_path,
            self.active_link_override.as_deref(),
            self.now_ms(),
        );
        let updater_state = updater_state_for_status(&status.state);
        self.apply_update_fields(&mut status, updater_state);
        status
    }

    fn publish_status(&self, persist: &Persist) -> BrowserSecurityUpdateStatus {
        let status = self.inspect();
        self.publish_status_record(persist, &status);
        status
    }

    fn publish_status_record(&self, persist: &Persist, status: &BrowserSecurityUpdateStatus) {
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&status) {
            let _ = persist.write(&topic, Priority::Min, None, Some(&body));
        }
    }

    fn status_with_updater_state(&self, updater_state: &str) -> BrowserSecurityUpdateStatus {
        let mut status = inspect_runtime(
            &self.node,
            &self.manifest_path,
            self.active_link_override.as_deref(),
            self.now_ms(),
        );
        self.apply_update_fields(&mut status, updater_state);
        status
    }

    fn apply_update_fields(&self, status: &mut BrowserSecurityUpdateStatus, updater_state: &str) {
        status.updater_command = Some(self.updater_command.display().to_string());
        status.updater_state = updater_state.to_owned();
        status.last_update_ms = self.last_update_ms;
        status.last_update_exit_code = self.last_update_exit_code;
        status.last_update_error = self.last_update_error.clone();
    }

    fn publish_update_cycle(&mut self, persist: &Persist) {
        let status = self.status_with_updater_state("idle");
        if !should_attempt_update(&status.state) {
            self.publish_status_record(persist, &status);
            return;
        }

        let installing = self.status_with_updater_state("installing");
        self.publish_status_record(persist, &installing);
        self.last_update_ms = Some(self.now_ms());
        let invocation = UpdaterInvocation {
            command: self.updater_command.clone(),
            manifest_path: self.manifest_path.clone(),
            active_link_override: self.active_link_override.clone(),
        };
        let outcome = run_update_command(invocation);
        self.apply_update_outcome(outcome);
        let final_state = if self.last_update_error.is_some() {
            "failed"
        } else {
            "attempted"
        };
        let status = self.status_with_updater_state(final_state);
        self.publish_status_record(persist, &status);
    }

    fn apply_update_outcome(&mut self, outcome: UpdateOutcome) {
        self.last_update_exit_code = outcome.exit_code;
        self.last_update_error = outcome.error;
    }
}

#[async_trait::async_trait]
impl Worker for BrowserSecurityUpdateWorker {
    fn name(&self) -> &'static str {
        "browser_security_update"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_security_update", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_security_update", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.publish_update_cycle(&persist);
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.publish_update_cycle(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        self.publish_status(&persist);
        Ok(())
    }
}

/// Resolve the expected CEF manifest path. The update/provisioning helper already
/// honors `MDE_CEF_MANIFEST`; this worker follows the same override for status.
#[must_use]
pub fn resolve_manifest_path() -> PathBuf {
    std::env::var_os("MDE_CEF_MANIFEST")
        .map_or_else(|| PathBuf::from(DEFAULT_MANIFEST_PATH), PathBuf::from)
}

fn inspect_runtime(
    node: &str,
    manifest_path: &Path,
    active_link_override: Option<&Path>,
    updated_ms: u64,
) -> BrowserSecurityUpdateStatus {
    let mut status = BrowserSecurityUpdateStatus {
        node: node.to_owned(),
        state: "manifest_missing".to_owned(),
        expected_cef_version: None,
        expected_chromium_version: None,
        expected_channel: None,
        expected_asset: None,
        expected_sha256: None,
        manifest_path: manifest_path.display().to_string(),
        active_link: active_link_override
            .unwrap_or_else(|| Path::new(DEFAULT_ACTIVE_LINK))
            .display()
            .to_string(),
        active_runtime: None,
        installed_version: None,
        installed_chromium: None,
        installed_sha256: None,
        libcef_present: false,
        last_error: None,
        updater_command: None,
        updater_state: "idle".to_owned(),
        last_update_ms: None,
        last_update_exit_code: None,
        last_update_error: None,
        updated_ms,
    };

    let manifest_text = match std::fs::read_to_string(manifest_path) {
        Ok(text) => text,
        Err(e) => {
            status.last_error = Some(format!("expected CEF manifest unreadable: {e}"));
            return status;
        }
    };
    let expected = parse_key_values(&manifest_text);
    status.expected_cef_version = expected.get("CEF_VERSION").cloned();
    status.expected_chromium_version = expected.get("CEF_CHROMIUM_VERSION").cloned();
    status.expected_channel = expected.get("CEF_CHANNEL").cloned();
    status.expected_asset = expected.get("CEF_ASSET").cloned();
    status.expected_sha256 = expected.get("CEF_SHA256").cloned();

    let active_link = active_link_override.map_or_else(
        || {
            expected
                .get("CEF_ACTIVE_LINK")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(DEFAULT_ACTIVE_LINK))
        },
        PathBuf::from,
    );
    status.active_link = active_link.display().to_string();

    let Some(active_runtime) = resolve_active_runtime(&active_link) else {
        status.state = "missing".to_owned();
        status.last_error = Some("active CEF runtime path is absent".to_owned());
        return status;
    };
    status.active_runtime = Some(active_runtime.display().to_string());

    let libcef = active_runtime.join("Release").join("libcef.so");
    status.libcef_present = libcef.is_file();
    if !status.libcef_present {
        status.state = "missing".to_owned();
        status.last_error = Some(format!("active CEF runtime missing {}", libcef.display()));
        return status;
    }

    let installed_manifest = active_runtime.join(INSTALLED_MANIFEST_FILE);
    let installed_text = match std::fs::read_to_string(&installed_manifest) {
        Ok(text) => text,
        Err(e) => {
            status.state = "mismatch".to_owned();
            status.last_error = Some(format!(
                "installed CEF runtime manifest unreadable at {}: {e}",
                installed_manifest.display()
            ));
            return status;
        }
    };
    let installed = parse_key_values(&installed_text);
    status.installed_version = installed.get("version").cloned();
    status.installed_chromium = installed.get("chromium").cloned();
    status.installed_sha256 = installed.get("sha256").cloned();

    let version_ok = status.expected_cef_version == status.installed_version;
    let chromium_ok = status.expected_chromium_version == status.installed_chromium;
    let sha_ok = status.expected_sha256 == status.installed_sha256;
    if version_ok && chromium_ok && sha_ok {
        status.state = "current".to_owned();
        status.last_error = None;
    } else {
        status.state = "mismatch".to_owned();
        status.last_error = Some("active CEF runtime does not match packaged manifest".to_owned());
    }
    status
}

fn should_attempt_update(state: &str) -> bool {
    matches!(state, "missing" | "mismatch")
}

fn updater_state_for_status(state: &str) -> &'static str {
    if should_attempt_update(state) {
        "unavailable"
    } else {
        "idle"
    }
}

#[derive(Debug, Clone)]
struct UpdaterInvocation {
    command: PathBuf,
    manifest_path: PathBuf,
    active_link_override: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UpdateOutcome {
    exit_code: Option<i32>,
    error: Option<String>,
}

fn run_update_command(invocation: UpdaterInvocation) -> UpdateOutcome {
    let mut cmd = Command::new(&invocation.command);
    cmd.env("MDE_CEF_MANIFEST", &invocation.manifest_path);
    if let Some(active_link) = &invocation.active_link_override {
        cmd.env("MDE_CEF_ACTIVE_LINK", active_link);
    }
    match cmd.output() {
        Ok(output) if output.status.success() => UpdateOutcome {
            exit_code: output.status.code(),
            error: None,
        },
        Ok(output) => UpdateOutcome {
            exit_code: output.status.code(),
            error: Some(update_error_summary(&output.stderr, &output.stdout)),
        },
        Err(e) => UpdateOutcome {
            exit_code: None,
            error: Some(format!(
                "failed to spawn updater {}: {e}",
                invocation.command.display()
            )),
        },
    }
}

fn update_error_summary(stderr: &[u8], stdout: &[u8]) -> String {
    let text = if stderr.is_empty() { stdout } else { stderr };
    let summary = String::from_utf8_lossy(text).trim().to_owned();
    if summary.is_empty() {
        "updater exited unsuccessfully without output".to_owned()
    } else {
        summary.chars().take(512).collect()
    }
}

fn resolve_active_runtime(active_link: &Path) -> Option<PathBuf> {
    match std::fs::read_link(active_link) {
        Ok(target) => {
            let resolved = if target.is_absolute() {
                target
            } else {
                active_link
                    .parent()
                    .map_or(target.clone(), |parent| parent.join(&target))
            };
            Some(canonical_or_original(resolved))
        }
        Err(_) if active_link.exists() => Some(canonical_or_original(active_link.to_path_buf())),
        Err(_) => None,
    }
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

fn parse_key_values(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(parse_key_value_line)
        .collect::<BTreeMap<_, _>>()
}

fn parse_key_value_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_owned(), unquote(value.trim()).to_owned()))
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(active_link: &Path) -> String {
        format!(
            r#"CEF_VERSION="149.0.6+g0d0eeb6+chromium-149.0.7827.201"
CEF_CHROMIUM_VERSION="149.0.7827.201"
CEF_CHANNEL="stable"
CEF_PLATFORM="linux64"
CEF_TYPE="minimal"
CEF_ASSET="cef_binary_149.0.6+g0d0eeb6+chromium-149.0.7827.201_linux64_minimal.tar.bz2"
CEF_SHA256="f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc"
CEF_ACTIVE_LINK="{}"
"#,
            active_link.display()
        )
    }

    fn write_current_runtime(root: &Path, version: &str, chromium: &str, sha256: &str) {
        std::fs::create_dir_all(root.join("Release")).unwrap();
        std::fs::write(root.join("Release").join("libcef.so"), b"cef").unwrap();
        std::fs::write(
            root.join(INSTALLED_MANIFEST_FILE),
            format!("version={version}\nchromium={chromium}\nsha256={sha256}\n"),
        )
        .unwrap();
    }

    fn write_fake_updater(path: &Path) {
        std::fs::write(
            path,
            r#"#!/bin/bash
set -euo pipefail
. "$MDE_CEF_MANIFEST"
runtime="$(dirname "$MDE_CEF_ACTIVE_LINK")/runtime-current"
mkdir -p "$runtime/Release"
printf cef > "$runtime/Release/libcef.so"
printf 'version=%s\nchromium=%s\nsha256=%s\n' "$CEF_VERSION" "$CEF_CHROMIUM_VERSION" "$CEF_SHA256" > "$runtime/mde-cef-runtime.manifest"
ln -sfn "$runtime" "$MDE_CEF_ACTIVE_LINK"
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn inspect_runtime_reports_current_for_matching_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        let runtime = dir.path().join("runtime");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();
        write_current_runtime(
            &runtime,
            "149.0.6+g0d0eeb6+chromium-149.0.7827.201",
            "149.0.7827.201",
            "f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc",
        );
        std::os::unix::fs::symlink(&runtime, &active_link).unwrap();

        let status = inspect_runtime("node-a", &manifest_path, None, 42);

        assert_eq!(status.state, "current");
        assert_eq!(
            status.expected_cef_version.as_deref(),
            Some("149.0.6+g0d0eeb6+chromium-149.0.7827.201")
        );
        assert_eq!(status.installed_chromium.as_deref(), Some("149.0.7827.201"));
        assert!(status.libcef_present);
        assert_eq!(status.last_error, None);
        assert_eq!(status.updated_ms, 42);
    }

    #[test]
    fn inspect_runtime_reports_missing_when_active_runtime_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();

        let status = inspect_runtime("node-a", &manifest_path, None, 42);

        assert_eq!(status.state, "missing");
        assert!(!status.libcef_present);
        assert!(status.last_error.unwrap().contains("absent"));
    }

    #[test]
    fn inspect_runtime_reports_mismatch_for_wrong_installed_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        let runtime = dir.path().join("runtime");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();
        write_current_runtime(&runtime, "old", "old", "bad");
        std::os::unix::fs::symlink(&runtime, &active_link).unwrap();

        let status = inspect_runtime("node-a", &manifest_path, None, 42);

        assert_eq!(status.state, "mismatch");
        assert_eq!(status.installed_version.as_deref(), Some("old"));
        assert!(status.libcef_present);
        assert!(status.last_error.unwrap().contains("does not match"));
    }

    #[test]
    fn inspect_runtime_reports_manifest_missing_before_runtime_checks() {
        let dir = tempfile::tempdir().unwrap();
        let status = inspect_runtime("node-a", &dir.path().join("missing.env"), None, 42);

        assert_eq!(status.state, "manifest_missing");
        assert!(status.expected_cef_version.is_none());
        assert!(status.last_error.unwrap().contains("manifest unreadable"));
    }

    #[test]
    fn worker_publishes_retained_security_update_status() {
        let dir = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        let runtime = dir.path().join("runtime");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();
        write_current_runtime(
            &runtime,
            "149.0.6+g0d0eeb6+chromium-149.0.7827.201",
            "149.0.7827.201",
            "f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc",
        );
        std::os::unix::fs::symlink(&runtime, &active_link).unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let worker = BrowserSecurityUpdateWorker::new("node-a".to_owned())
            .with_manifest_path(manifest_path)
            .with_active_link(active_link)
            .with_bus_root(bus.path().to_path_buf())
            .with_now_fn(Arc::new(|| 99));

        let status = worker.publish_status(&persist);

        assert_eq!(status.state, "current");
        let published = persist
            .list_since("state/browser-security-update/node-a", None)
            .unwrap()
            .pop()
            .unwrap();
        let published: BrowserSecurityUpdateStatus =
            serde_json::from_str(published.body.as_deref().unwrap()).unwrap();
        assert_eq!(published.state, "current");
        assert_eq!(published.updated_ms, 99);
        assert_eq!(published.expected_channel.as_deref(), Some("stable"));
        assert_eq!(published.updater_state, "idle");
    }

    #[test]
    fn update_cycle_runs_installer_for_missing_runtime_and_republishes_current() {
        let dir = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        let updater = dir.path().join("install-cef-runtime");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();
        write_fake_updater(&updater);
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserSecurityUpdateWorker::new("node-a".to_owned())
            .with_manifest_path(manifest_path)
            .with_active_link(active_link)
            .with_updater_command(updater)
            .with_now_fn(Arc::new(|| 123));

        worker.publish_update_cycle(&persist);

        let rows = persist
            .list_since("state/browser-security-update/node-a", None)
            .unwrap();
        assert!(
            rows.iter().any(|msg| msg
                .body
                .as_deref()
                .is_some_and(|body| body.contains(r#""updater_state":"installing""#))),
            "missing runtime publishes an installing status before invoking the updater"
        );
        let published: BrowserSecurityUpdateStatus =
            serde_json::from_str(rows.last().unwrap().body.as_deref().unwrap()).unwrap();
        assert_eq!(published.state, "current");
        assert_eq!(published.updater_state, "attempted");
        assert_eq!(published.last_update_ms, Some(123));
        assert_eq!(published.last_update_exit_code, Some(0));
        assert_eq!(published.last_update_error, None);
    }

    #[test]
    fn update_cycle_surfaces_installer_failure_without_faking_current() {
        let dir = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("cef.env");
        let active_link = dir.path().join("cef");
        let updater = dir.path().join("install-cef-runtime");
        std::fs::write(&manifest_path, manifest(&active_link)).unwrap();
        std::fs::write(
            &updater,
            "#!/bin/bash\necho installer unavailable >&2\nexit 69\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&updater).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&updater, perms).unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let mut worker = BrowserSecurityUpdateWorker::new("node-a".to_owned())
            .with_manifest_path(manifest_path)
            .with_active_link(active_link)
            .with_updater_command(updater)
            .with_now_fn(Arc::new(|| 123));

        worker.publish_update_cycle(&persist);

        let published = persist
            .list_since("state/browser-security-update/node-a", None)
            .unwrap()
            .pop()
            .unwrap();
        let published: BrowserSecurityUpdateStatus =
            serde_json::from_str(published.body.as_deref().unwrap()).unwrap();
        assert_eq!(published.state, "missing");
        assert_eq!(published.updater_state, "failed");
        assert_eq!(published.last_update_exit_code, Some(69));
        assert!(published
            .last_update_error
            .as_deref()
            .unwrap()
            .contains("installer unavailable"));
    }
}
