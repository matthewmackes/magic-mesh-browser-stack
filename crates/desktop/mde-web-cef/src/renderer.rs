//! Native Chromium/CEF renderer bridge entrypoint.
//!
//! This binary is packaged beside `mde-web-cef` and receives a fully validated
//! runtime contract from the helper. The next BROWSER-DD-1 slice replaces the
//! final pending gate with the CEF offscreen callback loop; this target exists now
//! so farm/RPM/lighthouse validation can exercise the real Chrome-engine process
//! boundary instead of stopping at a missing bridge binary.

use std::ffi::OsString;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use mde_web_sandbox::SandboxPolicy;

use mde_web_cef::{
    cef_abi::{CefAbi, CefInitializeProbe},
    cef_browser::{
        run_windowless_browser_probe_with_stream, run_windowless_tab, run_windowless_text_probe,
    },
    cef_init::{CefInitPaths, CefMainArgs, CefSettingsOwned},
    detect_extension_registry, extension_power_mode_enabled, CefExtensionRegistry,
    CEF_BRIDGE_EXTENSIONS_ENV, CEF_BRIDGE_EXTENSION_REGISTRY_ENV, CEF_BRIDGE_LIBCEF_ENV,
    CEF_BRIDGE_RELEASE_ENV, CEF_BRIDGE_RESOURCES_ENV, CEF_BRIDGE_ROOT_ENV, CEF_ICU_DATA,
    CEF_RESOURCES_PAK,
};

const CEF_INITIALIZE_PROBE_ENV: &str = "MDE_CEF_INITIALIZE_PROBE";
const CEF_BROWSER_PROBE_ENV: &str = "MDE_CEF_BROWSER_PROBE";
const CEF_TEXT_PROBE_EXPECT_ENV: &str = "MDE_CEF_TEXT_PROBE_EXPECT";
const CEF_ATTACH_STDIN_ENV: &str = "MDE_CEF_ATTACH_STDIN";
const CEF_ALLOW_ALLOY_EXTENSION_SMOKE_ENV: &str = "MDE_CEF_ALLOW_ALLOY_EXTENSION_SMOKE";
const SESSION_SOCKET_FD: i32 = 0;

fn main() -> ExitCode {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map_or("render-once", String::as_str);

    let Some(contract) = BridgeContract::from_env() else {
        eprintln!("CEF_BRIDGE_CONTRACT_MISSING");
        return ExitCode::from(78);
    };

    if let Err(reason) = contract.validate() {
        eprintln!("CEF_BRIDGE_CONTRACT_INVALID reason={reason}");
        return ExitCode::from(78);
    }

    println!(
        "CEF_BRIDGE_READY mode={mode} root={} lib={} release={} resources={} extensions={}",
        contract.root.display(),
        contract.libcef.display(),
        contract.release_dir.display(),
        contract.resources_dir.display(),
        contract.extensions.len()
    );

    // security-1: confine THIS (top-level CEF browser) process BEFORE it dlopens
    // libcef.so or touches a single line of web content. The OS sandbox
    // (mde-web-sandbox) is the same confinement class the Servo helper already
    // gets — a user namespace + seccomp-bpf escape denylist + a fully-dropped
    // capability set + no-new-privs + a pivot_root'd read-only rootfs with NO
    // $HOME / SSH keys / Nebula CA / mesh data — and it wraps the WHOLE
    // multi-process Chromium tree: CEF forks + re-execs its renderer/GPU/utility
    // subprocesses, which inherit this confinement across `exec` (no-new-privs
    // preserves the seccomp filter; the namespaces + rootfs are inherited). A CEF
    // subprocess (`--type=…`) is ALREADY inside the parent's sandbox and must NOT
    // re-apply it (re-`unshare`/`pivot_root` would EPERM under our own denylist),
    // so gate on `!is_cef_subprocess`. Chromium's own internal sandbox stays off
    // (`--no-sandbox`) — see `cef_init.rs` + `docs/THREAT_MODEL.md` §10 for why
    // it cannot be re-enabled nested inside this one, and why the OS sandbox is
    // the honest confinement instead.
    let is_cef_subprocess = args.iter().any(|arg| arg.starts_with("--type="));
    let will_run_cef = mode == "tab"
        || std::env::var_os(CEF_INITIALIZE_PROBE_ENV).is_some()
        || std::env::var_os(CEF_BROWSER_PROBE_ENV).is_some();
    let bridge_exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mde-web-cef-renderer"));
    if will_run_cef && !is_cef_subprocess {
        if let Err(reason) = apply_os_sandbox(&contract, &bridge_exe) {
            eprintln!("CEF_OS_SANDBOX_FAILED reason={reason}");
            return ExitCode::from(78);
        }
    }

    let abi = match CefAbi::load(&contract.libcef).and_then(|abi| {
        let metadata = abi.metadata()?;
        Ok((abi, metadata))
    }) {
        Ok((abi, metadata)) => {
            println!("{}", metadata.status_line());
            abi
        }
        Err(err) => {
            eprintln!("CEF_ABI_MISSING reason={err}");
            return ExitCode::from(78);
        }
    };
    let paths = CefInitPaths::new(bridge_exe, &contract.resources_dir)
        .with_extension_dirs(contract.extensions.clone());
    let settings = CefSettingsOwned::windowless_no_sandbox(&paths);
    println!("{}", settings.status_line());

    let is_tab = mode == "tab";
    // `is_cef_subprocess` + the CEF-init env gates were resolved above (where the
    // OS sandbox is applied for the top-level browser process).
    if is_tab
        || is_cef_subprocess
        || std::env::var_os(CEF_INITIALIZE_PROBE_ENV).is_some()
        || std::env::var_os(CEF_BROWSER_PROBE_ENV).is_some()
    {
        let mut cef_args = args.clone();
        cef_args.extend(paths.command_line_switches());
        let main_args = match CefMainArgs::from_args(&cef_args) {
            Ok(main_args) => main_args,
            Err(err) => {
                eprintln!("CEF_INITIALIZE_ARGS_INVALID reason={err}");
                return ExitCode::from(78);
            }
        };
        println!("CEF_INIT_ARGV argc={}", main_args.argc());
        match abi.initialize_browser_process(&main_args, &settings) {
            Ok(CefInitializeProbe::Initialized) => {
                println!("CEF_INITIALIZE_OK");
                if is_tab {
                    let url = url_arg(&args).unwrap_or("about:blank");
                    let (width, height) = dimensions(&args);
                    let session_socket = session_socket_from_stdin();
                    match run_windowless_tab(&abi, url, width, height, &session_socket) {
                        Ok(probe) => {
                            println!("{}", probe.status_line());
                            return ExitCode::SUCCESS;
                        }
                        Err(err) => {
                            eprintln!("CEF_TAB_FAILED reason={err}");
                            return ExitCode::from(78);
                        }
                    }
                } else if std::env::var_os(CEF_BROWSER_PROBE_ENV).is_some() {
                    let url = url_arg(&args).unwrap_or("https://example.com/");
                    if let Some(expected) = std::env::var_os(CEF_TEXT_PROBE_EXPECT_ENV) {
                        if let Some(line) = windowless_extension_runtime_gate(
                            contract.extensions.len(),
                            allow_alloy_extension_smoke_enabled(),
                        ) {
                            eprintln!("{line}");
                            return ExitCode::from(78);
                        }
                        let expected = expected.to_string_lossy().into_owned();
                        match run_windowless_text_probe(
                            &abi,
                            url,
                            1024,
                            768,
                            Duration::from_secs(15),
                            &expected,
                        ) {
                            Ok(probe) => {
                                println!("{}", probe.status_line());
                                if !contract.extensions.is_empty() {
                                    println!(
                                        "CEF_EXTENSION_SMOKE_READY extensions={} marker_bytes={} text_bytes={}",
                                        contract.extensions.len(),
                                        expected.len(),
                                        probe.text_bytes
                                    );
                                }
                                return ExitCode::SUCCESS;
                            }
                            Err(err) => {
                                eprintln!("CEF_TEXT_PROBE_FAILED reason={err}");
                                return ExitCode::from(78);
                            }
                        }
                    }
                    let session_socket = if std::env::var_os(CEF_ATTACH_STDIN_ENV).is_some() {
                        Some(session_socket_from_stdin())
                    } else {
                        None
                    };
                    let session_socket_ref = session_socket.as_ref();
                    match run_windowless_browser_probe_with_stream(
                        &abi,
                        url,
                        1024,
                        768,
                        Duration::from_secs(15),
                        session_socket_ref,
                    ) {
                        Ok(probe) => {
                            println!("{}", probe.status_line());
                            return ExitCode::SUCCESS;
                        }
                        Err(err) => {
                            eprintln!("CEF_BROWSER_PROBE_FAILED reason={err}");
                            return ExitCode::from(78);
                        }
                    }
                }
                abi.shutdown();
            }
            Ok(outcome @ CefInitializeProbe::SubprocessExit { code }) => {
                println!("{}", outcome.status_line());
                return code_to_exit(code);
            }
            Err(err) => {
                eprintln!("CEF_INITIALIZE_FAILED reason={err}");
                return ExitCode::from(78);
            }
        }
    }

    eprintln!("CEF_OFFSCREEN_PENDING mode={mode}");
    ExitCode::from(78)
}

/// Install the shared OS sandbox on THIS (top-level CEF browser) process.
///
/// Exposes the vendored CEF runtime bundle + any vetted extension dirs read-only
/// inside the confined rootfs (so the browser can `dlopen` `libcef.so` and
/// re-`exec` its subprocess bridge) and NOTHING of the operator's. On success
/// the call forks: the confined child returns and proceeds; the pre-fork process
/// becomes a signal-forwarding supervisor that never returns. A failure is fatal
/// — the caller must never run web content unconfined.
fn apply_os_sandbox(contract: &BridgeContract, bridge_exe: &Path) -> Result<(), String> {
    let binds = cef_extra_readonly_binds(&contract.root, &contract.extensions, bridge_exe);
    let policy = SandboxPolicy::web_cef();
    mde_web_sandbox::apply_with_binds(policy, &binds).map_err(|err| format!("{err:#}"))?;
    // Observable on the seat (stdout/journal) for live confinement verification.
    // `chromium_internal_sandbox=off` is honest: `--no-sandbox` stays because the
    // OS sandbox's seccomp denylist blocks the mount/pivot_root/unshare syscalls
    // Chromium's own nested sandbox would need (see `cef_init.rs` + THREAT_MODEL
    // §10). The OS sandbox — inherited by every Chromium subprocess — is the
    // operative confinement instead.
    println!(
        "CEF_OS_SANDBOX applied=1 host={} mem_max={} extra_binds={} home_visible=0 seccomp=1 caps_dropped=1 chromium_internal_sandbox=off",
        policy.hostname,
        policy.cgroup_memory_max(),
        binds.len(),
    );
    Ok(())
}

/// The extra read-only paths the CEF browser tree needs visible after
/// `pivot_root`: the vendored CEF runtime root (`/opt/mde/cef` — its `Release/`
/// libcef.so + `Resources/`) plus any vetted unpacked extension dirs. Production's
/// subprocess bridge binary lives under `/usr/libexec`, already covered by the
/// sandbox's `/usr` bind; non-`/usr` developer/farm bridge overrides are exposed
/// as exactly that one read-only executable file so Chromium subprocess `execvp`
/// still works after the rootfs pivot.
///
/// SECURITY INVARIANT: this list is ENGINE RUNTIME + vetted extensions only —
/// never a `$HOME`/SSH/Nebula/mesh path. Enforced by the unit tests below; the
/// shared sandbox binds each entry read-only.
fn cef_extra_readonly_binds(
    root: &Path,
    extensions: &[PathBuf],
    bridge_exe: &Path,
) -> Vec<PathBuf> {
    let mut binds = Vec::with_capacity(2 + extensions.len());
    binds.push(root.to_path_buf());
    binds.extend(extensions.iter().cloned());
    if bridge_exe.is_absolute() && !bridge_exe.starts_with("/usr") {
        binds.push(bridge_exe.to_path_buf());
    }
    binds
}

struct BridgeContract {
    root: PathBuf,
    libcef: PathBuf,
    release_dir: PathBuf,
    resources_dir: PathBuf,
    extensions: Vec<PathBuf>,
    extension_registry: Option<PathBuf>,
    extension_power_mode: bool,
}

impl BridgeContract {
    fn from_env() -> Option<Self> {
        Some(Self {
            root: env_path(CEF_BRIDGE_ROOT_ENV)?,
            libcef: env_path(CEF_BRIDGE_LIBCEF_ENV)?,
            release_dir: env_path(CEF_BRIDGE_RELEASE_ENV)?,
            resources_dir: env_path(CEF_BRIDGE_RESOURCES_ENV)?,
            extensions: env_paths(CEF_BRIDGE_EXTENSIONS_ENV),
            extension_registry: env_path(CEF_BRIDGE_EXTENSION_REGISTRY_ENV),
            extension_power_mode: extension_power_mode_enabled(),
        })
    }

    fn validate(&self) -> Result<(), String> {
        if !self.root.is_dir() {
            return Err(format!("{} is not a directory", self.root.display()));
        }
        if !self.libcef.is_file() {
            return Err(format!("{} is not a file", self.libcef.display()));
        }
        if !self.release_dir.is_dir() {
            return Err(format!("{} is not a directory", self.release_dir.display()));
        }
        if !self.resources_dir.join(CEF_ICU_DATA).is_file() {
            return Err(format!(
                "{} missing",
                self.resources_dir.join(CEF_ICU_DATA).display()
            ));
        }
        if !self.resources_dir.join(CEF_RESOURCES_PAK).is_file() {
            return Err(format!(
                "{} missing",
                self.resources_dir.join(CEF_RESOURCES_PAK).display()
            ));
        }
        for path in &self.extensions {
            if !path.is_dir() {
                return Err(format!("extension directory {} missing", path.display()));
            }
        }
        self.validate_extension_registry()
    }

    fn validate_extension_registry(&self) -> Result<(), String> {
        if self.extensions.is_empty() {
            return Ok(());
        }
        let registry = self.extension_registry.as_ref().ok_or_else(|| {
            format!("{CEF_BRIDGE_EXTENSION_REGISTRY_ENV} missing for extension handoff")
        })?;
        let detected = detect_extension_registry(registry);
        let expected = match detected {
            CefExtensionRegistry::Available { extensions, .. } => extensions
                .into_iter()
                .filter(|entry| self.extension_power_mode || !entry.power_sideload)
                .map(|entry| entry.path)
                .collect::<Vec<_>>(),
            CefExtensionRegistry::Missing { .. } | CefExtensionRegistry::Invalid { .. } => {
                return Err(format!(
                    "extension registry did not validate: {}",
                    detected.status_line()
                ));
            }
        };
        if expected != self.extensions {
            return Err(format!(
                "extension handoff does not match registry {} expected={} got={}",
                registry.display(),
                join_paths_for_status(&expected),
                join_paths_for_status(&self.extensions)
            ));
        }
        Ok(())
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(non_empty).map(PathBuf::from)
}

fn env_paths(key: &str) -> Vec<PathBuf> {
    std::env::var_os(key).map_or_else(Vec::new, |value| {
        value
            .to_string_lossy()
            .split(',')
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect()
    })
}

fn join_paths_for_status(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn non_empty(value: &OsString) -> bool {
    !value.is_empty()
}

fn code_to_exit(code: i32) -> ExitCode {
    u8::try_from(code).map_or_else(|_| ExitCode::from(1), ExitCode::from)
}

fn url_arg(args: &[String]) -> Option<&str> {
    args.windows(2)
        .find_map(|pair| (pair[0] == "--url").then_some(pair[1].as_str()))
}

fn dimensions(args: &[String]) -> (u32, u32) {
    (
        u32_flag(args, "--width").unwrap_or(1024),
        u32_flag(args, "--height").unwrap_or(768),
    )
}

fn u32_flag(args: &[String], flag: &str) -> Option<u32> {
    args.windows(2)
        .find_map(|pair| (pair[0] == flag).then(|| pair[1].parse().ok()).flatten())
}

fn allow_alloy_extension_smoke_enabled() -> bool {
    std::env::var(CEF_ALLOW_ALLOY_EXTENSION_SMOKE_ENV)
        .ok()
        .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
}

fn windowless_extension_runtime_gate(
    extension_count: usize,
    allow_alloy_extension_smoke: bool,
) -> Option<String> {
    if extension_count == 0 || allow_alloy_extension_smoke {
        return None;
    }
    Some(format!(
        "CEF_EXTENSIONS_WINDOWLESS_ALLOY_GATED extensions={extension_count} reason=cef_149_windowless_forces_alloy_runtime chrome_runtime_required_for_webextensions override_env={CEF_ALLOW_ALLOY_EXTENSION_SMOKE_ENV}"
    ))
}

fn session_socket_from_stdin() -> UnixStream {
    // SAFETY: this opt-in probe mode follows `mde-web-preview`'s live-helper
    // convention: fd 0 is a connected AF_UNIX session socket owned by the child.
    unsafe { UnixStream::from_raw_fd(SESSION_SOCKET_FD) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn bridge_contract_revalidates_extension_dirs_against_registry() {
        let root = temp_root("mde-cef-bridge-registry");
        let release = root.join("Release");
        let resources = root.join("Resources");
        let lastpass = root.join("lastpass");
        let forged = root.join("forged");
        let registry = root.join("allowlist.env");
        fs::create_dir_all(&release).expect("release dir");
        fs::create_dir_all(&resources).expect("resources dir");
        fs::create_dir_all(&lastpass).expect("lastpass dir");
        fs::create_dir_all(&forged).expect("forged dir");
        fs::write(release.join("libcef.so"), b"fake cef").expect("libcef");
        fs::write(resources.join(CEF_ICU_DATA), b"icu").expect("icu");
        fs::write(resources.join(CEF_RESOURCES_PAK), b"pak").expect("pak");
        write_manifest(&lastpass, "LastPass", "4.130.0", &["storage", "tabs"]);
        write_manifest(&forged, "Forged", "1.0.0", &["storage"]);
        fs::write(
            &registry,
            r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,tabs
            power_sideload = true
            "#,
        )
        .expect("registry");

        let valid = contract(
            &root,
            &release,
            &resources,
            Some(registry.clone()),
            true,
            vec![lastpass.clone()],
        );
        valid.validate().expect("vetted extension handoff");

        let power_gated = contract(
            &root,
            &release,
            &resources,
            Some(registry.clone()),
            false,
            vec![lastpass],
        );
        let err = power_gated
            .validate()
            .expect_err("sideload handoff requires power mode");
        assert!(err.contains("does not match registry"), "{err}");

        let missing_registry = contract(
            &root,
            &release,
            &resources,
            None,
            true,
            vec![forged.clone()],
        );
        let err = missing_registry
            .validate()
            .expect_err("registry pointer required");
        assert!(err.contains(CEF_BRIDGE_EXTENSION_REGISTRY_ENV), "{err}");

        let forged = contract(
            &root,
            &release,
            &resources,
            Some(registry),
            true,
            vec![forged],
        );
        let err = forged
            .validate()
            .expect_err("forged extension dir rejected");
        assert!(err.contains("does not match registry"), "{err}");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cef_extra_binds_expose_the_runtime_and_extensions_never_keys() {
        // security-1: the extra RO binds the CEF browser gets inside its confined
        // rootfs are the vendored runtime, vetted extensions, and the exact
        // renderer bridge executable when a farm/dev override is outside /usr.
        // The sandbox must expose no broad home/keys/mesh path even here.
        let root = PathBuf::from("/opt/mde/cef");
        let exts = vec![
            PathBuf::from("/mnt/mesh-storage/browser/extensions/ublock-origin"),
            PathBuf::from("/mnt/mesh-storage/browser/extensions/lastpass"),
        ];
        let bridge = PathBuf::from("/home/mm/magic-mesh/target/debug/mde-web-cef-renderer");
        let binds = cef_extra_readonly_binds(&root, &exts, &bridge);
        assert!(
            binds.contains(&PathBuf::from("/opt/mde/cef")),
            "runtime root"
        );
        assert!(binds.contains(&exts[0]));
        assert!(binds.contains(&exts[1]));
        assert!(binds.contains(&bridge));
        for bind in &binds {
            let s = bind.to_string_lossy();
            if s.starts_with("/home") {
                assert_eq!(bind, &bridge, "unexpected home bind leaked: {s}");
                assert_eq!(
                    bind.file_name().and_then(|name| name.to_str()),
                    Some("mde-web-cef-renderer"),
                    "home bind must be exactly the renderer bridge"
                );
            }
            assert!(!s.starts_with("/root"), "root home leaked: {s}");
            assert!(!s.contains("ssh"), "ssh keys leaked: {s}");
            assert!(!s.contains("nebula"), "nebula keys leaked: {s}");
            assert!(!s.contains("syncthing"), "syncthing data leaked: {s}");
            assert!(!s.starts_with("/var"), "var (mesh data) leaked: {s}");
        }
    }

    #[test]
    fn cef_extra_binds_default_to_runtime_root_for_packaged_bridge() {
        let binds = cef_extra_readonly_binds(
            &PathBuf::from("/opt/mde/cef"),
            &[],
            &PathBuf::from("/usr/libexec/mackesd/mde-web-cef-renderer"),
        );
        assert_eq!(binds, vec![PathBuf::from("/opt/mde/cef")]);
    }

    #[test]
    fn windowless_extension_runtime_gate_names_chrome_runtime_requirement() {
        let line = windowless_extension_runtime_gate(1, false).expect("extension smoke gated");
        assert!(line.contains("CEF_EXTENSIONS_WINDOWLESS_ALLOY_GATED"));
        assert!(line.contains("cef_149_windowless_forces_alloy_runtime"));
        assert!(line.contains(CEF_ALLOW_ALLOY_EXTENSION_SMOKE_ENV));
        assert!(windowless_extension_runtime_gate(0, false).is_none());
        assert!(windowless_extension_runtime_gate(1, true).is_none());
    }

    fn contract(
        root: &PathBuf,
        release: &PathBuf,
        resources: &PathBuf,
        extension_registry: Option<PathBuf>,
        extension_power_mode: bool,
        extensions: Vec<PathBuf>,
    ) -> BridgeContract {
        BridgeContract {
            root: root.clone(),
            libcef: release.join("libcef.so"),
            release_dir: release.clone(),
            resources_dir: resources.clone(),
            extensions,
            extension_registry,
            extension_power_mode,
        }
    }

    fn write_manifest(path: &PathBuf, name: &str, version: &str, permissions: &[&str]) {
        let permissions = permissions
            .iter()
            .map(|permission| format!("\"{permission}\""))
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            path.join("manifest.json"),
            format!(
                "{{\"manifest_version\":3,\"name\":\"{name}\",\"version\":\"{version}\",\"permissions\":[{permissions}]}}"
            ),
        )
        .expect("manifest");
    }

    fn temp_root(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
