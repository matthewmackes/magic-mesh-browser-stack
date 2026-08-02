//! BUS-1.10 — Tera templating engine.
//!
//! Locked Round 10 of the 104-Q poll: Tera (Rust-native,
//! Jinja2-style) with mesh variables, an `exec("cmd")` function that
//! shells out and captures stdout, and an `include("path")` function
//! that pulls in a GFS file. The `exec` function is a flat-trust
//! amplifier — any peer with the mesh passcode can publish a
//! template that runs commands on every render-target peer.
//! Acceptable under [[project_open_mesh_directive]] and documented
//! both here and in `docs/design/v6.x-mackes-bus.md` § 10.
//!
//! SECURITY (security-3): the `exec` shell-out primitive is a
//! fleet-wide arbitrary-command (RCE) capability. It is gated behind
//! the **non-default** `template-exec` cargo feature and is NOT
//! registered in the default build — no shipped template uses it. The
//! curated mesh variables and the path-scoped `include_file` function
//! remain available unconditionally.
//!
//! Mesh variables (curated set, locked Round 10):
//! - `peer.hostname` — `/proc/sys/kernel/hostname`
//! - `peer.overlay_ip` — `/var/lib/mackesd/nebula/overlay-ip` (empty when not enrolled)
//! - `peer.uptime_s` — first integer in `/proc/uptime`
//! - `mesh.size` — count of files under `/var/lib/mackesd/nebula/peers/` (1 fallback)
//! - `time.iso8601` — `chrono::Utc::now()` ISO-8601 string
//! - `time.unix` — same instant as integer seconds since epoch
//! - `system.load_1` — first value in `/proc/loadavg`
//! - `system.mem_used_mb` — `MemTotal - MemAvailable` from `/proc/meminfo`

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tera::{Tera, Value as TeraValue};

/// Maximum stdout the `{{exec}}` function captures from a child
/// process before truncation. Bounds template-explosion attacks and
/// keeps audit-log entries small.
pub const EXEC_STDOUT_CAP_BYTES: usize = 4 * 1024;

/// Maximum wall-clock time the `{{exec}}` function gives a child
/// process before killing it. Templates that shell out to something
/// slow render a documented placeholder instead of hanging the bus.
pub const EXEC_TIMEOUT: Duration = Duration::from_secs(5);

/// `/proc` and the local overlay-IP mirror contain small scalar values. Keep
/// their reads bounded even when a test fixture or a damaged mirror is not.
const MAX_TEMPLATE_VAR_BYTES: usize = 64 * 1024;

/// Includes are user-visible template material, but must not allow an
/// unbounded allocation in the bus render path.
const MAX_INCLUDE_FILE_BYTES: usize = 1024 * 1024;

#[cfg(target_os = "linux")]
const O_NOFOLLOW_NONBLOCK: i32 = 0o404000; // O_NOFOLLOW | O_NONBLOCK

/// Source files for the mesh variables. Pulled into a struct so tests
/// can point at temp files instead of real `/proc` and `/var`.
#[derive(Debug, Clone)]
pub struct VarSources {
    pub hostname_path: PathBuf,
    pub overlay_ip_path: PathBuf,
    pub uptime_path: PathBuf,
    pub peers_dir: PathBuf,
    pub loadavg_path: PathBuf,
    pub meminfo_path: PathBuf,
}

impl Default for VarSources {
    fn default() -> Self {
        Self {
            hostname_path: PathBuf::from("/proc/sys/kernel/hostname"),
            overlay_ip_path: PathBuf::from("/var/lib/mackesd/nebula/overlay-ip"),
            uptime_path: PathBuf::from("/proc/uptime"),
            peers_dir: PathBuf::from("/var/lib/mackesd/nebula/peers"),
            loadavg_path: PathBuf::from("/proc/loadavg"),
            meminfo_path: PathBuf::from("/proc/meminfo"),
        }
    }
}

/// Renderer holds the configured Tera instance, the variable
/// sources, and the optional GFS base used by `include`. Build one
/// at daemon start; clone freely (everything inside is `Arc`-wrapped
/// or owned-cheap).
#[derive(Clone)]
pub struct Renderer {
    tera: Arc<Tera>,
    sources: VarSources,
    include_root: Arc<PathBuf>,
}

impl Renderer {
    /// Construct the renderer with `/var/lib/mde/bus/include/` as
    /// the include-root and real `/proc`/`/var` paths for mesh
    /// variables.
    pub fn new() -> Self {
        Self::with(
            VarSources::default(),
            PathBuf::from("/var/lib/mde/bus/include"),
        )
    }

    /// Construct with explicit sources + include-root (used in tests
    /// and by callers that want to scope `include` to mesh-home).
    pub fn with(sources: VarSources, include_root: PathBuf) -> Self {
        let mut tera = Tera::default();
        // Register the two custom functions. Tera's `register_function`
        // takes a `Fn(&HashMap<String, Value>) -> tera::Result<Value>`.
        let inc_root = include_root.clone();
        // SECURITY (security-3): the shell-exec template primitive is a
        // flat-trust, fleet-wide arbitrary-command capability. It is
        // registered ONLY when the non-default `template-exec` feature is
        // enabled, so the default build cannot resolve `exec(...)` at all.
        #[cfg(feature = "template-exec")]
        tera.register_function(
            "exec",
            move |args: &HashMap<String, TeraValue>| -> tera::Result<TeraValue> {
                exec_function(args)
            },
        );
        tera.register_function(
            "include_file",
            move |args: &HashMap<String, TeraValue>| -> tera::Result<TeraValue> {
                include_function(args, &inc_root)
            },
        );
        Self {
            tera: Arc::new(tera),
            sources,
            include_root: Arc::new(include_root),
        }
    }

    /// Render a one-shot template string against the live mesh
    /// variable values. Returns the rendered body or a structural
    /// Tera error.
    pub fn render(&self, template: &str) -> anyhow::Result<String> {
        let vars = self.build_context();
        let mut ctx = tera::Context::new();
        for (k, v) in vars {
            ctx.insert(k, &v);
        }
        // Tera's one-shot interface needs to re-register the
        // functions every call (each Renderer carries a configured
        // Tera, but `one_off` builds a fresh ephemeral instance).
        // Clone the configured Tera into a mutable local, render,
        // discard.
        let mut local = (*self.tera).clone();
        local
            .render_str(template, &ctx)
            .context("Tera render failed")
    }

    /// Build the curated mesh-variable map. Public for the
    /// `mde-bus render --dump-vars` debug path the BUS-1.8 CLI will
    /// add.
    pub fn build_context(&self) -> HashMap<&'static str, Value> {
        let mut out = HashMap::new();
        out.insert("peer", self.peer_vars());
        out.insert("mesh", self.mesh_vars());
        out.insert("time", time_vars());
        out.insert("system", self.system_vars());
        out
    }

    fn peer_vars(&self) -> Value {
        json!({
            "hostname": read_trim(&self.sources.hostname_path).unwrap_or_else(|| String::from("unknown")),
            "overlay_ip": read_trim(&self.sources.overlay_ip_path).unwrap_or_default(),
            "uptime_s": read_uptime_seconds(&self.sources.uptime_path).unwrap_or(0),
        })
    }

    fn mesh_vars(&self) -> Value {
        json!({
            "size": count_peers(&self.sources.peers_dir),
        })
    }

    fn system_vars(&self) -> Value {
        json!({
            "load_1": read_load_1(&self.sources.loadavg_path).unwrap_or(0.0),
            "mem_used_mb": read_mem_used_mb(&self.sources.meminfo_path).unwrap_or(0),
        })
    }

    /// Path the `include_file` function searches under. Useful for
    /// tests + audit.
    #[must_use]
    pub fn include_root(&self) -> &std::path::Path {
        self.include_root.as_ref()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

fn time_vars() -> Value {
    let now = chrono::Utc::now();
    json!({
        "iso8601": now.to_rfc3339(),
        "unix": now.timestamp(),
    })
}

fn open_read_no_follow(path: &std::path::Path) -> io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;

        // O_NONBLOCK prevents a hostile FIFO from hanging the render thread
        // before the descriptor-backed regular-file check can reject it.
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW_NONBLOCK)
            .open(path)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // The production platform is Linux. Keep the library portable for
        // non-Linux targets with the strongest std-only pre-open check they
        // provide; Linux takes the descriptor-level O_NOFOLLOW path above.
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "template source is not a regular file",
            ));
        }
        std::fs::File::open(path)
    }
}

fn read_bounded_bytes(path: &std::path::Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut file = open_read_no_follow(path)?;
    let metadata = file.metadata()?;
    let max_bytes_u64 = u64::try_from(max_bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "template read limit does not fit in u64",
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "template source is not a regular file",
        ));
    }
    if metadata.len() > max_bytes_u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "template source exceeds its read limit",
        ));
    }
    let expected_len = metadata.len();
    let capacity = usize::try_from(expected_len)
        .unwrap_or(max_bytes)
        .min(max_bytes)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    (&mut file)
        .take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if !after.file_type().is_file() || after.len() != expected_len || bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "template source changed or exceeds its read limit",
        ));
    }
    Ok(bytes)
}

fn read_bounded_text(path: &std::path::Path, max_bytes: usize) -> Option<String> {
    String::from_utf8(read_bounded_bytes(path, max_bytes).ok()?).ok()
}

fn read_trim(path: &std::path::Path) -> Option<String> {
    read_bounded_text(path, MAX_TEMPLATE_VAR_BYTES)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_uptime_seconds(path: &std::path::Path) -> Option<u64> {
    let raw = read_bounded_text(path, MAX_TEMPLATE_VAR_BYTES)?;
    let first = raw.split_ascii_whitespace().next()?;
    // /proc/uptime is `"<seconds.fractional> <idle>"` — chop the dot.
    let int_part = first.split('.').next()?;
    int_part.parse::<u64>().ok()
}

fn count_peers(dir: &std::path::Path) -> u64 {
    // Fallback to 1 (just self) when the dir doesn't exist or is
    // unreadable — matches the worklist note "pre-mesh-home: just
    // this peer".
    std::fs::read_dir(dir)
        .map(|it| it.filter_map(Result::ok).count() as u64)
        .unwrap_or(1)
}

fn read_load_1(path: &std::path::Path) -> Option<f64> {
    let raw = read_bounded_text(path, MAX_TEMPLATE_VAR_BYTES)?;
    let first = raw.split_ascii_whitespace().next()?;
    first.parse::<f64>().ok()
}

fn read_mem_used_mb(path: &std::path::Path) -> Option<u64> {
    let raw = read_bounded_text(path, MAX_TEMPLATE_VAR_BYTES)?;
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in raw.lines() {
        let mut it = line.split_ascii_whitespace();
        let key = it.next()?;
        let value = it.next()?;
        match key {
            "MemTotal:" => total = value.parse().ok(),
            "MemAvailable:" => avail = value.parse().ok(),
            _ => {}
        }
        if total.is_some() && avail.is_some() {
            break;
        }
    }
    // /proc/meminfo values are in kB.
    let kb_used = total?.saturating_sub(avail?);
    Some(kb_used / 1024)
}

/// The shell-exec template primitive. Gated behind the non-default
/// `template-exec` feature (security-3): shells out via `sh -c <cmd>`
/// and captures stdout. A flat-trust, fleet-wide arbitrary-command
/// capability — never compiled into the default build.
#[cfg(feature = "template-exec")]
fn exec_function(args: &HashMap<String, TeraValue>) -> tera::Result<TeraValue> {
    let cmd = args
        .get("cmd")
        .and_then(TeraValue::as_str)
        .ok_or_else(|| tera::Error::msg("`exec` requires a string `cmd=` argument"))?;
    // Use `sh -c` so callers can write `exec(cmd="uptime -p")` and
    // get the same behavior they would in a shell. Per the design
    // doc this is a flat-trust amplifier.
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Ok(TeraValue::String(format!("[exec spawn failed: {e}]"))),
    };
    // Coarse timeout — busy-poll for `EXEC_TIMEOUT`; kill on expiry.
    let deadline = std::time::Instant::now() + EXEC_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(TeraValue::String(format!(
                        "[exec timed out after {}s]",
                        EXEC_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Ok(TeraValue::String(format!("[exec wait failed: {e}]"))),
        }
    }
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return Ok(TeraValue::String(String::new())),
    };
    use std::io::Read;
    let mut buf = Vec::with_capacity(EXEC_STDOUT_CAP_BYTES);
    let mut chunk = [0u8; 1024];
    while buf.len() < EXEC_STDOUT_CAP_BYTES {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = EXEC_STDOUT_CAP_BYTES - buf.len();
                let take = n.min(remaining);
                buf.extend_from_slice(&chunk[..take]);
                if take < n {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    Ok(TeraValue::String(text))
}

fn include_function(
    args: &HashMap<String, TeraValue>,
    root: &std::path::Path,
) -> tera::Result<TeraValue> {
    let path = args
        .get("path")
        .and_then(TeraValue::as_str)
        .ok_or_else(|| tera::Error::msg("`include_file` requires a string `path=` argument"))?;
    // Reject parent-dir escapes — only files under the configured
    // include-root are readable.
    if path.contains("..") || path.starts_with('/') {
        return Err(tera::Error::msg(format!(
            "`include_file` path `{path}` escapes include-root"
        )));
    }
    let target = root.join(path);
    // Preserve the lexical traversal guard above and also reject a symlinked
    // parent that resolves outside the configured root. The final read still
    // uses O_NOFOLLOW on the descriptor, so a final symlink is rejected too.
    if let (Ok(canonical_root), Ok(canonical_target)) =
        (std::fs::canonicalize(root), std::fs::canonicalize(&target))
    {
        if !canonical_target.starts_with(&canonical_root) {
            return Err(tera::Error::msg(format!(
                "`include_file` path `{path}` escapes include-root"
            )));
        }
    }
    let bytes = read_bounded_bytes(&target, MAX_INCLUDE_FILE_BYTES).map_err(|e| {
        tera::Error::msg(format!(
            "`include_file` failed to read {}: {e}",
            target.display()
        ))
    })?;
    let contents = String::from_utf8(bytes).map_err(|_| {
        tera::Error::msg(format!(
            "`include_file` rejected {}: invalid UTF-8",
            target.display()
        ))
    })?;
    Ok(TeraValue::String(contents))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fake_sources(dir: &std::path::Path) -> VarSources {
        // Write fixture files into a tempdir so tests don't depend on
        // the real /proc.
        let hostname = dir.join("hostname");
        std::fs::write(&hostname, "alpha\n").unwrap();
        let overlay = dir.join("overlay-ip");
        std::fs::write(&overlay, "100.64.0.5\n").unwrap();
        let uptime = dir.join("uptime");
        std::fs::write(&uptime, "12345.67 6789.00\n").unwrap();
        let peers = dir.join("peers");
        std::fs::create_dir(&peers).unwrap();
        for n in ["alpha", "beta", "gamma"] {
            std::fs::File::create(peers.join(n)).unwrap();
        }
        let loadavg = dir.join("loadavg");
        std::fs::write(&loadavg, "0.42 0.50 0.60 1/100 12345\n").unwrap();
        let meminfo = dir.join("meminfo");
        let mut f = std::fs::File::create(&meminfo).unwrap();
        writeln!(f, "MemTotal:       16000000 kB").unwrap();
        writeln!(f, "MemFree:         4000000 kB").unwrap();
        writeln!(f, "MemAvailable:   10000000 kB").unwrap();
        VarSources {
            hostname_path: hostname,
            overlay_ip_path: overlay,
            uptime_path: uptime,
            peers_dir: peers,
            loadavg_path: loadavg,
            meminfo_path: meminfo,
        }
    }

    #[test]
    fn renders_curated_mesh_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let out = r
            .render("h={{peer.hostname}} ip={{peer.overlay_ip}} up={{peer.uptime_s}}")
            .unwrap();
        assert_eq!(out, "h=alpha ip=100.64.0.5 up=12345");
    }

    #[test]
    fn renders_mesh_size_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let out = r
            .render("size={{mesh.size}} load={{system.load_1}}")
            .unwrap();
        assert_eq!(out, "size=3 load=0.42");
    }

    #[test]
    fn mem_used_mb_is_total_minus_available() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let out = r.render("u={{system.mem_used_mb}}").unwrap();
        // (16_000_000 - 10_000_000) kB / 1024 = 5859 MB
        assert_eq!(out, "u=5859");
    }

    #[cfg(unix)]
    #[test]
    fn hostile_variable_symlinks_fail_closed() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let mut sources = fake_sources(tmp.path());
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, "hostile\n").unwrap();

        let hostname_link = tmp.path().join("hostname-link");
        let overlay_link = tmp.path().join("overlay-link");
        let uptime_link = tmp.path().join("uptime-link");
        let loadavg_link = tmp.path().join("loadavg-link");
        let meminfo_link = tmp.path().join("meminfo-link");
        for link in [
            &hostname_link,
            &overlay_link,
            &uptime_link,
            &loadavg_link,
            &meminfo_link,
        ] {
            symlink(&outside, link).unwrap();
        }
        sources.hostname_path = hostname_link;
        sources.overlay_ip_path = overlay_link;
        sources.uptime_path = uptime_link;
        sources.loadavg_path = loadavg_link;
        sources.meminfo_path = meminfo_link;

        let r = Renderer::with(sources, tmp.path().join("inc"));
        let vars = r.build_context();
        assert_eq!(vars["peer"]["hostname"], json!("unknown"));
        assert_eq!(vars["peer"]["overlay_ip"], json!(""));
        assert_eq!(vars["peer"]["uptime_s"], json!(0));
        assert_eq!(vars["system"]["load_1"], json!(0.0));
        assert_eq!(vars["system"]["mem_used_mb"], json!(0));
    }

    #[test]
    fn hostile_variable_bytes_use_honest_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = fake_sources(tmp.path());
        std::fs::write(&sources.hostname_path, [0xff, 0xfe]).unwrap();
        std::fs::write(
            &sources.overlay_ip_path,
            vec![b'9'; MAX_TEMPLATE_VAR_BYTES + 1],
        )
        .unwrap();
        std::fs::write(&sources.uptime_path, [0xff]).unwrap();
        std::fs::write(&sources.loadavg_path, [0xff]).unwrap();
        std::fs::write(&sources.meminfo_path, [0xff]).unwrap();

        let r = Renderer::with(sources, tmp.path().join("inc"));
        let vars = r.build_context();
        assert_eq!(vars["peer"]["hostname"], json!("unknown"));
        assert_eq!(vars["peer"]["overlay_ip"], json!(""));
        assert_eq!(vars["peer"]["uptime_s"], json!(0));
        assert_eq!(vars["system"]["load_1"], json!(0.0));
        assert_eq!(vars["system"]["mem_used_mb"], json!(0));
    }

    // SECURITY (security-3): with the default feature set, the
    // shell-exec primitive must NOT be registered. Tera cannot resolve
    // `exec(...)`, so rendering fails and the command never runs.
    #[test]
    #[cfg(not(feature = "template-exec"))]
    fn exec_function_absent_in_default_build() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let result = r.render(r#"x={{ exec(cmd="echo hello") }}"#);
        assert!(
            result.is_err(),
            "expected render to fail with `exec` unregistered, got Ok: {result:?}"
        );
        let msg = format!("{:#}", result.unwrap_err());
        // The command must NOT have executed — output is never "x=hello".
        assert!(
            !msg.contains("x=hello"),
            "exec appears to have run despite being feature-gated: {msg}"
        );
        // Tera surfaces an unknown-function error naming `exec`.
        assert!(
            msg.contains("exec") || msg.to_lowercase().contains("function"),
            "expected an unknown-function error for `exec`, got: {msg}"
        );
    }

    #[test]
    #[cfg(feature = "template-exec")]
    fn exec_function_captures_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        // `echo` is in /bin on Fedora — same env the bus runs in.
        let out = r.render(r#"x={{ exec(cmd="echo hello") }}"#).unwrap();
        assert_eq!(out, "x=hello");
    }

    #[test]
    #[cfg(feature = "template-exec")]
    fn exec_function_truncates_oversize_output() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        // `head -c $(N+1)` generates more bytes than the cap so we can
        // assert truncation actually happened. `tr -d '\n'` strips the
        // newline `yes` injects after every "A".
        let over = EXEC_STDOUT_CAP_BYTES + 100;
        let cmd = format!("yes A | tr -d '\\n' | head -c {over}");
        let out = r
            .render(&format!(r#"x={{{{ exec(cmd="{cmd}") }}}}"#))
            .unwrap();
        let body = out.strip_prefix("x=").unwrap();
        assert!(
            body.len() <= EXEC_STDOUT_CAP_BYTES,
            "expected output truncated to <= {} bytes, got {}",
            EXEC_STDOUT_CAP_BYTES,
            body.len()
        );
        assert!(
            body.chars().all(|c| c == 'A'),
            "expected pure 'A' output after `tr -d '\\n'`, got: {:?}",
            &body[..body.len().min(64)]
        );
    }

    #[test]
    fn include_function_reads_from_include_root() {
        let tmp = tempfile::tempdir().unwrap();
        let inc = tmp.path().join("inc");
        std::fs::create_dir(&inc).unwrap();
        std::fs::write(inc.join("hello.txt"), "from disk").unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), inc);
        let out = r
            .render(r#"x={{ include_file(path="hello.txt") }}"#)
            .unwrap();
        assert_eq!(out, "x=from disk");
    }

    #[test]
    fn include_function_rejects_invalid_oversized_and_non_regular_files() {
        let tmp = tempfile::tempdir().unwrap();
        let inc = tmp.path().join("inc");
        std::fs::create_dir(&inc).unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), inc.clone());

        std::fs::write(inc.join("invalid.txt"), [0xff, 0xfe]).unwrap();
        let err = r
            .render(r#"x={{ include_file(path="invalid.txt") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid UTF-8"), "unexpected error: {msg}");

        std::fs::write(
            inc.join("oversized.txt"),
            vec![b'x'; MAX_INCLUDE_FILE_BYTES + 1],
        )
        .unwrap();
        let err = r
            .render(r#"x={{ include_file(path="oversized.txt") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exceeds its read limit"),
            "unexpected error: {msg}"
        );

        std::fs::create_dir(inc.join("directory")).unwrap();
        let err = r
            .render(r#"x={{ include_file(path="directory") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a regular file"),
            "unexpected error: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn include_function_rejects_symlinked_file_and_parent_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let inc = tmp.path().join("inc");
        let outside_dir = tmp.path().join("outside");
        std::fs::create_dir(&inc).unwrap();
        std::fs::create_dir(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("secret.txt"), "outside").unwrap();
        symlink(outside_dir.join("secret.txt"), inc.join("file-link.txt")).unwrap();
        symlink(&outside_dir, inc.join("parent-link")).unwrap();

        let r = Renderer::with(fake_sources(tmp.path()), inc);
        let err = r
            .render(r#"x={{ include_file(path="file-link.txt") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to read") || msg.contains("escapes include-root"),
            "unexpected error: {msg}"
        );

        let err = r
            .render(r#"x={{ include_file(path="parent-link/secret.txt") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes include-root"),
            "unexpected parent escape error: {msg}"
        );
    }

    #[test]
    fn include_function_rejects_path_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let err = r
            .render(r#"x={{ include_file(path="../etc/passwd") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("escapes include-root"),
            "expected path-escape rejection, got: {msg}"
        );
        let err = r
            .render(r#"x={{ include_file(path="/etc/passwd") }}"#)
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("escapes include-root"));
    }

    #[test]
    fn time_iso8601_is_iso_formatted() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Renderer::with(fake_sources(tmp.path()), tmp.path().join("inc"));
        let out = r.render("t={{time.iso8601}}").unwrap();
        let body = out.strip_prefix("t=").unwrap();
        // Cheap shape check: chrono RFC3339 always contains `T` and ends with a TZ.
        assert!(body.contains('T'), "expected ISO-8601 separator in {body}");
        let has_tz = body.ends_with('Z') || body.contains('+') || body.contains('-');
        assert!(has_tz, "expected a timezone marker in {body}");
    }
}
