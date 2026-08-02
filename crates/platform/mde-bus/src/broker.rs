//! BUS-1.2 — per-peer ntfy broker supervision.
//!
//! Owns the ntfy subprocess that backs the mesh-wide notification +
//! clipboard bus. Three responsibilities:
//!
//! 1. **Render the broker config** from `data/ntfy/server.yml.tmpl`
//!    against the live Nebula overlay IP (read from the publish
//!    file `nebula_supervisor` writes; same source NF-21.1's
//!    sshd_overlay_bind worker uses).
//! 2. **Spawn ntfy** as a child process bound only to the overlay
//!    IP, so the broker is reachable from inside the mesh and
//!    silently unreachable from the underlay.
//! 3. **Graceful degradation** on missing prereqs — pre-enrollment
//!    peers (no overlay-IP file), dev boxes without ntfy on PATH,
//!    or hosts where the cache dir is unwritable all just log and
//!    skip the spawn. The supervisor re-evaluates on the next tick
//!    so a peer that enrolls or installs ntfy mid-session picks up
//!    automatically.
//!
//! Per design doc `docs/design/v6.x-mackes-bus.md` line 210, the
//! broker uses plain HTTP — Nebula provides transport encryption
//! and the mesh is flat-trust. The bind-to-overlay restriction is
//! the security boundary; no TLS cert is needed.
//!
//! Integration test (2-peer container fixture) lives under the
//! HW carve-out per the BUS-1.2 task body. Local tests cover the
//! pure helpers (render, decide-spawn) deterministically.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Default location of the overlay-IP publish file written by
/// `mackesd_core::workers::nebula_supervisor::publish_overlay_ip`
/// (GF-1.3.a).
pub const DEFAULT_OVERLAY_IP_PATH: &str = "/var/lib/mackesd/nebula/overlay-ip";

/// Default location of the ntfy config template shipped with the
/// RPM under `/usr/share/mde/ntfy/server.yml.tmpl`.
pub const DEFAULT_TEMPLATE_PATH: &str = "/usr/share/mde/ntfy/server.yml.tmpl";

/// Default port the broker listens on inside the mesh. Matches the
/// BUS-1.2 task body exit criterion (`curl http://$peer:8443/v1/health`).
pub const DEFAULT_LISTEN_PORT: u16 = 8443;

/// Default name of the ntfy executable on PATH.
pub const DEFAULT_NTFY_BIN: &str = "ntfy";

/// Configuration handed to the broker supervisor at construction
/// time. Every field has a sensible default — tests override the
/// ones they need to.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// Path to the overlay-IP publish file.
    pub overlay_ip_path: PathBuf,
    /// Path to the ntfy config template (Tera).
    pub template_path: PathBuf,
    /// Directory where the rendered config + ntfy cache live.
    /// Defaults to `~/.cache/mde/bus/ntfy/` under the user.
    pub cache_dir: PathBuf,
    /// Port the broker listens on.
    pub listen_port: u16,
    /// `ntfy` binary name (resolved via `$PATH`) or absolute path.
    pub ntfy_bin: String,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            overlay_ip_path: PathBuf::from(DEFAULT_OVERLAY_IP_PATH),
            template_path: PathBuf::from(DEFAULT_TEMPLATE_PATH),
            cache_dir: default_cache_dir(),
            listen_port: DEFAULT_LISTEN_PORT,
            ntfy_bin: DEFAULT_NTFY_BIN.to_string(),
        }
    }
}

fn default_cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("mde/bus/ntfy")
}

/// Why the broker isn't (currently) running. Reported by
/// [`evaluate_prereqs`] so the supervisor's `start_if_ready` can
/// either spawn or log a single line and try again later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerSkipReason {
    /// Publish file at `<overlay_ip_path>` doesn't exist.
    NoOverlayIp,
    /// Publish file exists but is empty or whitespace-only.
    EmptyOverlayIp,
    /// `ntfy` binary not on PATH.
    NtfyMissing,
    /// Template file at `<template_path>` doesn't exist.
    TemplateMissing,
}

impl std::fmt::Display for BrokerSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOverlayIp => {
                write!(f, "overlay-IP publish file missing (peer not enrolled yet)")
            }
            Self::EmptyOverlayIp => {
                write!(f, "overlay-IP publish file empty (enrolment in progress)")
            }
            Self::NtfyMissing => write!(f, "ntfy binary not on PATH (install the `ntfy` RPM)"),
            Self::TemplateMissing => {
                write!(f, "ntfy config template not present")
            }
        }
    }
}

/// Result of [`evaluate_prereqs`] — either a ready-to-render
/// snapshot of the inputs, or a single skip reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prereqs {
    /// All checks passed; the supervisor may render + spawn.
    Ready {
        /// Overlay IP read from the publish file.
        overlay_ip: String,
    },
    /// Some prerequisite is missing; supervisor should log + skip.
    Skip(BrokerSkipReason),
}

/// Inspect the broker config against the live filesystem + PATH.
/// Pure-fn-style: no spawn, no write — only reads.
pub fn evaluate_prereqs(cfg: &BrokerConfig) -> Prereqs {
    if !cfg.overlay_ip_path.exists() {
        return Prereqs::Skip(BrokerSkipReason::NoOverlayIp);
    }
    let overlay_ip = match std::fs::read_to_string(&cfg.overlay_ip_path) {
        Ok(s) => s.trim().to_string(),
        Err(_) => return Prereqs::Skip(BrokerSkipReason::NoOverlayIp),
    };
    if overlay_ip.is_empty() {
        return Prereqs::Skip(BrokerSkipReason::EmptyOverlayIp);
    }
    if !cfg.template_path.exists() {
        return Prereqs::Skip(BrokerSkipReason::TemplateMissing);
    }
    if which(&cfg.ntfy_bin).is_none() {
        return Prereqs::Skip(BrokerSkipReason::NtfyMissing);
    }
    Prereqs::Ready { overlay_ip }
}

/// Render the ntfy config template against the given vars. Pure
/// helper exposed for tests.
///
/// # Errors
/// Returns an `anyhow::Error` when the template fails to parse,
/// reference a missing variable, or otherwise fail rendering.
pub fn render_config(
    template_body: &str,
    overlay_ip: &str,
    cache_dir: &Path,
) -> anyhow::Result<String> {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("server.yml", template_body)
        .map_err(|e| anyhow::anyhow!("ntfy template parse: {e}"))?;
    let mut ctx = tera::Context::new();
    ctx.insert("overlay_ip", overlay_ip);
    ctx.insert("cache_dir", &cache_dir.display().to_string());
    tera.render("server.yml", &ctx)
        .map_err(|e| anyhow::anyhow!("ntfy template render: {e}"))
}

/// Materialize the rendered config to disk under `cfg.cache_dir`,
/// creating the directory tree if needed. Returns the path written.
///
/// # Errors
/// Returns `std::io::Error` on mkdir/write/rename failure.
pub fn materialize_config(cfg: &BrokerConfig, overlay_ip: &str) -> anyhow::Result<PathBuf> {
    let template_body = std::fs::read_to_string(&cfg.template_path)
        .map_err(|e| anyhow::anyhow!("read ntfy template {}: {e}", cfg.template_path.display()))?;
    let rendered = render_config(&template_body, overlay_ip, &cfg.cache_dir)?;
    std::fs::create_dir_all(&cfg.cache_dir)
        .map_err(|e| anyhow::anyhow!("mkdir {}: {e}", cfg.cache_dir.display()))?;
    let out_path = cfg.cache_dir.join("server.yml");
    let tmp = out_path.with_extension("yml.tmp");
    std::fs::write(&tmp, rendered.as_bytes())
        .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &out_path)
        .map_err(|e| anyhow::anyhow!("rename {} → {}: {e}", tmp.display(), out_path.display()))?;
    Ok(out_path)
}

/// Spawn `ntfy serve --config <rendered>` as a child process.
/// Caller is responsible for awaiting on the returned handle (for
/// supervision restart semantics).
///
/// # Errors
/// Returns `std::io::Error` if the binary can't be spawned (e.g.
/// PATH lookup failed mid-flight, EPERM, etc.).
pub fn spawn_ntfy(cfg: &BrokerConfig, config_path: &Path) -> std::io::Result<Child> {
    Command::new(&cfg.ntfy_bin)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Top-level "render + spawn if ready" entry point. Returns either
/// a running [`Child`] handle or the skip reason. Logs intermediate
/// steps via tracing.
///
/// # Errors
/// Returns `anyhow::Error` only when render-or-spawn fails AFTER
/// prereqs were green (template parse error, ntfy spawn EPERM,
/// etc.). Missing prereqs return `Ok(BrokerOutcome::Skipped(_))`
/// — they're expected non-fatal states.
pub async fn start_if_ready(cfg: &BrokerConfig) -> anyhow::Result<BrokerOutcome> {
    match evaluate_prereqs(cfg) {
        Prereqs::Skip(reason) => {
            tracing::info!(
                target: "mde_bus::broker",
                reason = %reason,
                "skipping ntfy broker spawn"
            );
            Ok(BrokerOutcome::Skipped(reason))
        }
        Prereqs::Ready { overlay_ip } => {
            let config_path = materialize_config(cfg, &overlay_ip)?;
            let mut child = spawn_ntfy(cfg, &config_path)?;
            // Pipe ntfy's stdout/stderr into our tracing layer so
            // operators see broker logs in `journalctl -u mde-bus`.
            if let Some(stdout) = child.stdout.take() {
                tokio::spawn(forward_lines("ntfy.stdout", stdout));
            }
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(forward_lines("ntfy.stderr", stderr));
            }
            tracing::info!(
                target: "mde_bus::broker",
                overlay_ip = %overlay_ip,
                listen_port = cfg.listen_port,
                config = %config_path.display(),
                "ntfy broker spawned"
            );
            Ok(BrokerOutcome::Running { child, overlay_ip })
        }
    }
}

/// Result of [`start_if_ready`].
#[derive(Debug)]
pub enum BrokerOutcome {
    /// Broker spawned successfully.
    Running {
        /// Child handle. Caller awaits on `.wait()` to detect exit
        /// and re-spawn under supervision policy.
        child: Child,
        /// Overlay IP the broker bound to (for logging).
        overlay_ip: String,
    },
    /// Broker did not spawn; reason carried for the caller's log.
    Skipped(BrokerSkipReason),
}

async fn forward_lines<R>(target: &'static str, reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::info!(target: "mde_bus::broker", source = target, "{}", line);
    }
}

/// Minimal `which`-style lookup over `$PATH`. Mirrors the helper
/// in `mackesd_core::workers::firewall_preset::which` so this crate
/// stays free of an extra dep just for one call.
fn which(cmd: &str) -> Option<PathBuf> {
    if cmd.is_empty() {
        return None;
    }
    if Path::new(cmd).is_absolute() {
        return Path::new(cmd).is_file().then(|| PathBuf::from(cmd));
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_body() -> &'static str {
        include_str!("../../../../data/ntfy/server.yml.tmpl")
    }

    #[test]
    fn render_resolves_overlay_ip_and_cache_dir() {
        let rendered =
            render_config(template_body(), "10.42.0.5", Path::new("/tmp/c")).expect("render ok");
        assert!(rendered.contains("listen-http: \"10.42.0.5:8443\""));
        assert!(rendered.contains("cache-file: \"/tmp/c/ntfy.db\""));
        // Per design lock — no TLS config rendered.
        assert!(!rendered.contains("listen-https"));
        assert!(!rendered.contains("key-file"));
    }

    #[test]
    fn render_fails_for_undeclared_variable() {
        let bad = "listen-http: {{ undeclared_var }}";
        let r = render_config(bad, "1.1.1.1", Path::new("/tmp/c"));
        assert!(r.is_err());
    }

    #[test]
    fn prereqs_skip_when_overlay_publish_file_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = BrokerConfig {
            overlay_ip_path: tmp.path().join("overlay-ip"),
            template_path: tmp.path().join("template.tmpl"),
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "ntfy".to_string(),
        };
        assert_eq!(
            evaluate_prereqs(&cfg),
            Prereqs::Skip(BrokerSkipReason::NoOverlayIp)
        );
    }

    #[test]
    fn prereqs_skip_when_overlay_publish_file_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overlay = tmp.path().join("overlay-ip");
        let template = tmp.path().join("template.tmpl");
        std::fs::write(&overlay, "   \n").expect("seed empty overlay");
        std::fs::write(&template, template_body()).expect("seed template");
        let cfg = BrokerConfig {
            overlay_ip_path: overlay,
            template_path: template,
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "ntfy".to_string(),
        };
        assert_eq!(
            evaluate_prereqs(&cfg),
            Prereqs::Skip(BrokerSkipReason::EmptyOverlayIp)
        );
    }

    #[test]
    fn prereqs_skip_when_template_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overlay = tmp.path().join("overlay-ip");
        std::fs::write(&overlay, "10.42.0.5\n").expect("seed overlay");
        let cfg = BrokerConfig {
            overlay_ip_path: overlay,
            template_path: tmp.path().join("missing.tmpl"),
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "ntfy".to_string(),
        };
        assert_eq!(
            evaluate_prereqs(&cfg),
            Prereqs::Skip(BrokerSkipReason::TemplateMissing)
        );
    }

    #[test]
    fn prereqs_skip_when_ntfy_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overlay = tmp.path().join("overlay-ip");
        let template = tmp.path().join("template.tmpl");
        std::fs::write(&overlay, "10.42.0.5\n").expect("seed overlay");
        std::fs::write(&template, template_body()).expect("seed template");
        let cfg = BrokerConfig {
            overlay_ip_path: overlay,
            template_path: template,
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "definitely-not-a-real-binary-xyz".to_string(),
        };
        assert_eq!(
            evaluate_prereqs(&cfg),
            Prereqs::Skip(BrokerSkipReason::NtfyMissing)
        );
    }

    #[test]
    fn prereqs_ready_when_all_inputs_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let overlay = tmp.path().join("overlay-ip");
        let template = tmp.path().join("template.tmpl");
        std::fs::write(&overlay, "10.42.0.5\n").expect("seed overlay");
        std::fs::write(&template, template_body()).expect("seed template");
        // Use `sh` as the stand-in binary — universally available
        // on any Linux test box. We're only verifying the prereq
        // check, not exec'ing.
        let cfg = BrokerConfig {
            overlay_ip_path: overlay,
            template_path: template,
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "sh".to_string(),
        };
        assert_eq!(
            evaluate_prereqs(&cfg),
            Prereqs::Ready {
                overlay_ip: "10.42.0.5".to_string()
            }
        );
    }

    #[test]
    fn materialize_writes_atomically_and_creates_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let template = tmp.path().join("template.tmpl");
        std::fs::write(&template, template_body()).expect("seed template");
        let cfg = BrokerConfig {
            overlay_ip_path: tmp.path().join("overlay-ip"),
            template_path: template,
            cache_dir: tmp.path().join("nested/cache/ntfy"),
            listen_port: 8443,
            ntfy_bin: "ntfy".to_string(),
        };
        let written = materialize_config(&cfg, "10.42.0.7").expect("write ok");
        assert!(written.exists());
        let body = std::fs::read_to_string(&written).expect("read back");
        assert!(body.contains("listen-http: \"10.42.0.7:8443\""));
        // .tmp leftover should be gone after the rename.
        let leftover = cfg.cache_dir.join("server.yml.tmp");
        assert!(!leftover.exists(), ".tmp file should be renamed away");
    }

    #[test]
    fn which_handles_empty_and_missing_binaries() {
        assert!(which("").is_none());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[tokio::test]
    async fn start_if_ready_skips_when_prereqs_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = BrokerConfig {
            overlay_ip_path: tmp.path().join("overlay-ip"),
            template_path: tmp.path().join("template.tmpl"),
            cache_dir: tmp.path().join("cache"),
            listen_port: 8443,
            ntfy_bin: "ntfy".to_string(),
        };
        let outcome = start_if_ready(&cfg).await.expect("ok");
        match outcome {
            BrokerOutcome::Skipped(BrokerSkipReason::NoOverlayIp) => {}
            other => panic!("expected NoOverlayIp skip, got {other:?}"),
        }
    }
}
