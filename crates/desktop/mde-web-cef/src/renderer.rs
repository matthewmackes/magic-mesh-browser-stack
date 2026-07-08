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
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use mde_web_cef::{
    cef_abi::{CefAbi, CefInitializeProbe},
    cef_browser::{run_windowless_browser_probe_with_stream, run_windowless_tab},
    cef_init::{CefInitPaths, CefMainArgs, CefSettingsOwned},
    CEF_BRIDGE_LIBCEF_ENV, CEF_BRIDGE_RELEASE_ENV, CEF_BRIDGE_RESOURCES_ENV, CEF_BRIDGE_ROOT_ENV,
    CEF_ICU_DATA, CEF_RESOURCES_PAK,
};

const CEF_INITIALIZE_PROBE_ENV: &str = "MDE_CEF_INITIALIZE_PROBE";
const CEF_BROWSER_PROBE_ENV: &str = "MDE_CEF_BROWSER_PROBE";
const CEF_ATTACH_STDIN_ENV: &str = "MDE_CEF_ATTACH_STDIN";
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
        "CEF_BRIDGE_READY mode={mode} root={} lib={} release={} resources={}",
        contract.root.display(),
        contract.libcef.display(),
        contract.release_dir.display(),
        contract.resources_dir.display()
    );

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
    let bridge_exe =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mde-web-cef-renderer"));
    let paths = CefInitPaths::new(bridge_exe, &contract.resources_dir);
    let settings = CefSettingsOwned::windowless_no_sandbox(&paths);
    println!("{}", settings.status_line());

    let is_tab = mode == "tab";
    let is_cef_subprocess = args.iter().any(|arg| arg.starts_with("--type="));
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

struct BridgeContract {
    root: PathBuf,
    libcef: PathBuf,
    release_dir: PathBuf,
    resources_dir: PathBuf,
}

impl BridgeContract {
    fn from_env() -> Option<Self> {
        Some(Self {
            root: env_path(CEF_BRIDGE_ROOT_ENV)?,
            libcef: env_path(CEF_BRIDGE_LIBCEF_ENV)?,
            release_dir: env_path(CEF_BRIDGE_RELEASE_ENV)?,
            resources_dir: env_path(CEF_BRIDGE_RESOURCES_ENV)?,
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
        Ok(())
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(non_empty).map(PathBuf::from)
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

fn session_socket_from_stdin() -> UnixStream {
    // SAFETY: this opt-in probe mode follows `mde-web-preview`'s live-helper
    // convention: fd 0 is a connected AF_UNIX session socket owned by the child.
    unsafe { UnixStream::from_raw_fd(SESSION_SOCKET_FD) }
}
