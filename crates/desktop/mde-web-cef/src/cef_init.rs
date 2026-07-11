//! Pinned Linux CEF initialization layout for the renderer bridge.
//!
//! The full browser slice must call `cef_initialize` with CEF's C structs. This
//! module carries the exact Linux layout for the pinned CEF 149 runtime verified
//! from the farm headers, plus a conservative settings builder for windowless
//! offscreen rendering.

use std::ffi::CString;
use std::fmt;
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

/// `sizeof(cef_main_args_t)` for pinned Linux CEF 149.
pub const CEF_MAIN_ARGS_SIZE: usize = 16;
/// `offsetof(cef_main_args_t, argc)`.
pub const CEF_MAIN_ARGS_ARGC_OFFSET: usize = 0;
/// `offsetof(cef_main_args_t, argv)`.
pub const CEF_MAIN_ARGS_ARGV_OFFSET: usize = 8;

/// `sizeof(cef_string_t)` for pinned Linux CEF 149.
pub const CEF_STRING_SIZE: usize = 24;
/// `offsetof(cef_string_t, str)`.
pub const CEF_STRING_STR_OFFSET: usize = 0;
/// `offsetof(cef_string_t, length)`.
pub const CEF_STRING_LENGTH_OFFSET: usize = 8;
/// `offsetof(cef_string_t, dtor)`.
pub const CEF_STRING_DTOR_OFFSET: usize = 16;

/// `sizeof(cef_settings_t)` for pinned Linux CEF 149.
pub const CEF_SETTINGS_SIZE: usize = 448;
/// `offsetof(cef_settings_t, size)`.
pub const CEF_SETTINGS_SIZE_OFFSET: usize = 0;
/// `offsetof(cef_settings_t, no_sandbox)`.
pub const CEF_SETTINGS_NO_SANDBOX_OFFSET: usize = 8;
/// `offsetof(cef_settings_t, browser_subprocess_path)`.
pub const CEF_SETTINGS_BROWSER_SUBPROCESS_PATH_OFFSET: usize = 16;
/// `offsetof(cef_settings_t, multi_threaded_message_loop)`.
pub const CEF_SETTINGS_MULTI_THREADED_MESSAGE_LOOP_OFFSET: usize = 88;
/// `offsetof(cef_settings_t, external_message_pump)`.
pub const CEF_SETTINGS_EXTERNAL_MESSAGE_PUMP_OFFSET: usize = 92;
/// `offsetof(cef_settings_t, windowless_rendering_enabled)`.
pub const CEF_SETTINGS_WINDOWLESS_RENDERING_ENABLED_OFFSET: usize = 96;
/// `offsetof(cef_settings_t, command_line_args_disabled)`.
pub const CEF_SETTINGS_COMMAND_LINE_ARGS_DISABLED_OFFSET: usize = 100;
/// `offsetof(cef_settings_t, cache_path)`.
pub const CEF_SETTINGS_CACHE_PATH_OFFSET: usize = 104;
/// `offsetof(cef_settings_t, root_cache_path)`.
pub const CEF_SETTINGS_ROOT_CACHE_PATH_OFFSET: usize = 128;
/// `offsetof(cef_settings_t, user_agent)`.
pub const CEF_SETTINGS_USER_AGENT_OFFSET: usize = 160;
/// `offsetof(cef_settings_t, locale)`.
pub const CEF_SETTINGS_LOCALE_OFFSET: usize = 208;
/// `offsetof(cef_settings_t, log_file)`.
pub const CEF_SETTINGS_LOG_FILE_OFFSET: usize = 232;
/// `offsetof(cef_settings_t, resources_dir_path)`.
pub const CEF_SETTINGS_RESOURCES_DIR_PATH_OFFSET: usize = 288;
/// `offsetof(cef_settings_t, locales_dir_path)`.
pub const CEF_SETTINGS_LOCALES_DIR_PATH_OFFSET: usize = 312;
/// `offsetof(cef_settings_t, remote_debugging_port)`.
pub const CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET: usize = 336;
/// `offsetof(cef_settings_t, background_color)`.
pub const CEF_SETTINGS_BACKGROUND_COLOR_OFFSET: usize = 344;
/// `offsetof(cef_settings_t, accept_language_list)`.
pub const CEF_SETTINGS_ACCEPT_LANGUAGE_LIST_OFFSET: usize = 352;

/// A fixed, common, non-identifying desktop User-Agent for the CEF engine.
///
/// Keep this aligned with the Servo helper's privacy posture: reveal a generic
/// Linux browser family, not the mesh, node, host OS revision, or engine bridge.
pub const CEF_GENERIC_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Stable locale exposed to web content by the Chromium helper.
pub const CEF_GENERIC_LOCALE: &str = "en-US";

/// Stable Accept-Language list exposed to web content by the Chromium helper.
pub const CEF_GENERIC_ACCEPT_LANGUAGE: &str = "en-US,en";

/// Default loopback Chromium DevTools discovery port for the CEF helper, used
/// **only** when the operator has explicitly opted the debug endpoint in (see
/// [`remote_debugging_port`]).
pub const CEF_REMOTE_DEBUGGING_PORT: i32 = 9222;

/// Environment variable that opts the Chromium DevTools Protocol (CDP)
/// remote-debugging endpoint in at launch. Absent/off by default.
pub const CEF_REMOTE_DEBUG_ENV: &str = "MDE_CEF_REMOTE_DEBUG";

/// Resolved opt-in state for the CDP remote-debugging endpoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RemoteDebug {
    /// Explicitly disabled.
    Off,
    /// Enabled on the given loopback port.
    Port(i32),
}

/// Parse the `MDE_CEF_REMOTE_DEBUG` env value into an explicit opt-in decision.
///
/// `None` means "no explicit env decision" (fall back to the build feature).
/// A falsey value (`0`/`false`/`no`/`off`) is an explicit disable. A port in
/// the usable `1024..=65535` range enables on that port; any other truthy value
/// enables on the default [`CEF_REMOTE_DEBUGGING_PORT`].
fn parse_remote_debug_flag(raw: Option<&str>) -> Option<RemoteDebug> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    ) {
        return Some(RemoteDebug::Off);
    }
    if let Ok(port) = trimmed.parse::<u16>() {
        if (1024..=65535).contains(&port) {
            return Some(RemoteDebug::Port(i32::from(port)));
        }
    }
    Some(RemoteDebug::Port(CEF_REMOTE_DEBUGGING_PORT))
}

/// Combine the compile-time `cef-devtools` feature default with an optional
/// env override into a concrete `remote_debugging_port` value (`0` = disabled).
const fn resolve_remote_debug(feature_default_on: bool, env: Option<RemoteDebug>) -> i32 {
    match env {
        Some(RemoteDebug::Off) => 0,
        Some(RemoteDebug::Port(port)) => port,
        None => {
            if feature_default_on {
                CEF_REMOTE_DEBUGGING_PORT
            } else {
                0
            }
        }
    }
}

/// The `remote_debugging_port` value to hand CEF (`0` = disabled).
///
/// SECURITY (security-4): the Chromium DevTools Protocol (CDP) endpoint this
/// port exposes is an unauthenticated control channel — a peer that reaches it
/// can drive the browser, read cookies/DOM, and execute arbitrary JS in any
/// tab, on a node that also holds the Nebula CA + SSH keys. It is therefore
/// **disabled by default** and never emitted on the shipped/default launch
/// path. It only turns on behind an explicit opt-in: the `cef-devtools` build
/// feature, or the `MDE_CEF_REMOTE_DEBUG` env var at launch (which also lets an
/// operator pin a specific loopback port). The env decision, when present,
/// overrides the feature default in both directions.
#[must_use]
pub fn remote_debugging_port() -> i32 {
    resolve_remote_debug(
        cfg!(feature = "cef-devtools"),
        parse_remote_debug_flag(std::env::var(CEF_REMOTE_DEBUG_ENV).ok().as_deref()),
    )
}

/// `cef_main_args_t` on Linux.
#[repr(C)]
pub struct CefMainArgs {
    argc: c_int,
    argv: *mut *mut c_char,
}

impl CefMainArgs {
    /// Build `cef_main_args_t` from process arguments.
    ///
    /// # Errors
    /// Returns an error when any argument contains an interior NUL byte.
    pub fn from_args(args: &[String]) -> Result<CefMainArgsOwned, CefInitError> {
        CefMainArgsOwned::new(args)
    }
}

/// Owned storage backing [`CefMainArgs`].
pub struct CefMainArgsOwned {
    _args: Vec<CString>,
    argv: Vec<*mut c_char>,
    raw: CefMainArgs,
}

impl CefMainArgsOwned {
    fn new(args: &[String]) -> Result<Self, CefInitError> {
        let c_args = args
            .iter()
            .map(|arg| {
                CString::new(arg.as_str()).map_err(|_| CefInitError::InteriorNul(arg.clone()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut argv = c_args
            .iter()
            .map(|arg| arg.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let raw = CefMainArgs {
            argc: c_int::try_from(argv.len()).map_err(|_| CefInitError::TooManyArgs(argv.len()))?,
            argv: if argv.is_empty() {
                ptr::null_mut()
            } else {
                argv.as_mut_ptr()
            },
        };
        Ok(Self {
            _args: c_args,
            argv,
            raw,
        })
    }

    /// Pointer suitable for `cef_initialize`.
    #[must_use]
    pub const fn as_ptr(&self) -> *const CefMainArgs {
        &self.raw
    }

    /// Number of arguments.
    #[must_use]
    pub fn argc(&self) -> usize {
        self.argv.len()
    }
}

/// Opaque, correctly aligned `cef_settings_t` storage for the pinned CEF layout.
#[repr(C, align(8))]
pub struct CefSettings {
    bytes: [u8; CEF_SETTINGS_SIZE],
}

impl CefSettings {
    /// Build conservative settings for an offscreen browser process.
    #[must_use]
    pub fn windowless_no_sandbox() -> Self {
        let mut settings = Self {
            bytes: [0; CEF_SETTINGS_SIZE],
        };
        settings.put_usize(CEF_SETTINGS_SIZE_OFFSET, CEF_SETTINGS_SIZE);
        settings.put_i32(CEF_SETTINGS_NO_SANDBOX_OFFSET, 1);
        settings.put_i32(CEF_SETTINGS_MULTI_THREADED_MESSAGE_LOOP_OFFSET, 0);
        settings.put_i32(CEF_SETTINGS_EXTERNAL_MESSAGE_PUMP_OFFSET, 1);
        settings.put_i32(CEF_SETTINGS_WINDOWLESS_RENDERING_ENABLED_OFFSET, 1);
        settings.put_i32(CEF_SETTINGS_COMMAND_LINE_ARGS_DISABLED_OFFSET, 0);
        // SECURITY (security-4): `0` disables CDP remote debugging in CEF. Only
        // an explicit opt-in (`cef-devtools` feature / `MDE_CEF_REMOTE_DEBUG`)
        // ever sets a live port here.
        settings.put_i32(
            CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET,
            remote_debugging_port(),
        );
        settings
    }

    fn put_cef_string(&mut self, offset: usize, data: &[u16]) {
        self.put_usize(offset + CEF_STRING_STR_OFFSET, data.as_ptr() as usize);
        self.put_usize(offset + CEF_STRING_LENGTH_OFFSET, data.len());
        self.put_usize(offset + CEF_STRING_DTOR_OFFSET, 0);
    }

    /// Pointer suitable for `cef_initialize`.
    #[must_use]
    pub const fn as_ptr(&self) -> *const c_void {
        self.bytes.as_ptr().cast::<c_void>()
    }

    /// Operator-facing settings summary.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "CEF_INIT_PLAN settings_size={} no_sandbox={} windowless={} external_pump={} multi_threaded_loop={} remote_debugging_port={}",
            self.get_usize(CEF_SETTINGS_SIZE_OFFSET),
            self.get_i32(CEF_SETTINGS_NO_SANDBOX_OFFSET),
            self.get_i32(CEF_SETTINGS_WINDOWLESS_RENDERING_ENABLED_OFFSET),
            self.get_i32(CEF_SETTINGS_EXTERNAL_MESSAGE_PUMP_OFFSET),
            self.get_i32(CEF_SETTINGS_MULTI_THREADED_MESSAGE_LOOP_OFFSET),
            self.get_i32(CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET)
        )
    }

    fn put_usize(&mut self, offset: usize, value: usize) {
        self.bytes[offset..offset + std::mem::size_of::<usize>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn put_i32(&mut self, offset: usize, value: i32) {
        self.bytes[offset..offset + std::mem::size_of::<i32>()]
            .copy_from_slice(&value.to_ne_bytes());
    }

    fn get_usize(&self, offset: usize) -> usize {
        let mut bytes = [0u8; std::mem::size_of::<usize>()];
        bytes.copy_from_slice(&self.bytes[offset..offset + std::mem::size_of::<usize>()]);
        usize::from_ne_bytes(bytes)
    }

    fn get_i32(&self, offset: usize) -> i32 {
        let mut bytes = [0u8; std::mem::size_of::<i32>()];
        bytes.copy_from_slice(&self.bytes[offset..offset + std::mem::size_of::<i32>()]);
        i32::from_ne_bytes(bytes)
    }
}

/// Runtime paths that CEF initialization needs for the pinned bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefInitPaths {
    /// The bridge/subprocess executable path.
    pub browser_subprocess_path: PathBuf,
    /// CEF resources directory.
    pub resources_dir_path: PathBuf,
    /// CEF locales directory.
    pub locales_dir_path: PathBuf,
    /// CEF log file path.
    pub log_file: PathBuf,
    /// Vetted unpacked extension directories to load, when extension support is enabled.
    pub extension_dirs: Vec<PathBuf>,
}

impl CefInitPaths {
    /// Build paths from the bridge executable and CEF resources dir.
    #[must_use]
    pub fn new(bridge_exe: impl Into<PathBuf>, resources_dir: impl AsRef<Path>) -> Self {
        let resources_dir = resources_dir.as_ref().to_path_buf();
        Self {
            browser_subprocess_path: bridge_exe.into(),
            locales_dir_path: resources_dir.join("locales"),
            resources_dir_path: resources_dir,
            log_file: std::env::temp_dir().join("mde-web-cef-renderer.log"),
            extension_dirs: Vec::new(),
        }
    }

    /// Return a copy that loads exactly these vetted unpacked extension dirs.
    #[must_use]
    pub fn with_extension_dirs(mut self, extension_dirs: Vec<PathBuf>) -> Self {
        self.extension_dirs = extension_dirs;
        self
    }

    /// CEF/Chromium switches that mirror the pinned settings paths.
    ///
    /// SECURITY (security-1): `--no-sandbox` disables **Chromium's own** internal
    /// (unprivileged-userns / setuid) sandbox. It stays because the browser
    /// process is instead confined by MCNF's **OS sandbox** (`mde-web-sandbox`,
    /// applied in `renderer.rs` before `cef_initialize`): a user namespace +
    /// seccomp-bpf escape denylist + fully-dropped capabilities + no-new-privs +
    /// a `pivot_root`'d read-only rootfs with NO `$HOME`/SSH/Nebula/mesh data,
    /// which wraps the whole multi-process tree (every Chromium subprocess
    /// inherits it across `exec`). Chromium's own nested sandbox CANNOT be
    /// re-enabled here: its zygote must `unshare`/`mount`/`pivot_root` to build a
    /// renderer view, and those syscalls are exactly what our seccomp denylist
    /// `EPERM`s (nor is a setuid `chrome-sandbox` helper installed, nor would it
    /// work under our throwaway uid map). Re-enabling it would mean gutting the
    /// OS sandbox's seccomp layer — a strictly worse trade. This is documented in
    /// full in `docs/THREAT_MODEL.md` §10; do NOT drop `--no-sandbox` on the
    /// assumption Chromium then self-sandboxes — it would fail to start under the
    /// OS sandbox, or (without it) run entirely unconfined.
    #[must_use]
    pub fn command_line_switches(&self) -> Vec<String> {
        let mut switches = vec![
            // See the function doc: Chromium's internal sandbox is off ON PURPOSE
            // — the OS sandbox (mde-web-sandbox) is the operative confinement.
            "--no-sandbox".to_owned(),
            "--disable-gpu".to_owned(),
            "--disable-gpu-compositing".to_owned(),
            "--ozone-platform=headless".to_owned(),
            format!("--lang={CEF_GENERIC_LOCALE}"),
            format!("--user-agent={CEF_GENERIC_USER_AGENT}"),
            format!("--accept-lang={CEF_GENERIC_ACCEPT_LANGUAGE}"),
            format!(
                "--browser-subprocess-path={}",
                self.browser_subprocess_path.display()
            ),
            format!("--resources-dir-path={}", self.resources_dir_path.display()),
            format!("--locales-dir-path={}", self.locales_dir_path.display()),
            format!(
                "--icu-data-file-path={}",
                self.resources_dir_path.join("icudtl.dat").display()
            ),
        ];
        // SECURITY (security-4): only expose the unauthenticated CDP
        // remote-debugging endpoint when explicitly opted in — never on the
        // default/shipped launch path. See [`remote_debugging_port`].
        let debug_port = remote_debugging_port();
        if debug_port != 0 {
            switches.push(format!("--remote-debugging-port={debug_port}"));
        }
        if self.extension_dirs.is_empty() {
            switches.push("--disable-extensions".to_owned());
        } else {
            let dirs = self
                .extension_dirs
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(",");
            switches.push("--enable-extensions".to_owned());
            switches.push(format!("--load-extension={dirs}"));
            switches.push("--disable-component-extensions-with-background-pages".to_owned());
        }
        switches.extend(chromium_privacy_switches().map(str::to_owned));
        switches
    }
}

/// Chromium command-line switches used for privacy/telemetry hardening.
///
/// `--disable-webrtc` was removed here 2026-07-10 after verifying it is not a
/// real Chromium switch: it is absent from the live upstream switch
/// registries (`content/public/common/content_switches.cc`,
/// `chrome/common/chrome_switches.cc`, fetched directly from
/// `chromium.googlesource.com` against this pinned CEF's Chromium
/// `149.0.7827.201` base — no `kDisableWebRtc`/`"disable-webrtc"` constant
/// anywhere in either file). Chromium's `base::CommandLine` never validates
/// switches against a registry — an unrecognized `--` switch is silently
/// never read by any consuming code, not errored or warned — so this line
/// shipped as inert "hardening" that did nothing. Corroborated by a live
/// Google Chrome Enterprise support-forum administrator report of this exact
/// flag being ignored in production
/// (<https://support.google.com/chrome/a/thread/5939360>), and by a
/// chromium-dev mailing-list thread confirming the only real way to disable
/// WebRTC is the build-time GN flag `enable_webrtc=false` (used by e.g.
/// Chromecast-audio builds) — not available here since this crate links a
/// prebuilt vendored CEF binary, not a from-source build.
///
/// `--force-webrtc-ip-handling-policy=disable_non_proxied_udp` stays: it *is*
/// real (`kForceWebRtcIPHandlingPolicy` = `"force-webrtc-ip-handling-policy"`
/// in `content_switches.cc`, backing the genuine Chrome enterprise policy
/// `WebRtcIPHandling`) and constrains ICE candidate gathering to
/// proxied/relayed transport, which is the correct mechanism for the
/// local-IP-leak concern this bundle is defending against.
///
/// The actual JS-reachable WebRTC surface (`RTCPeerConnection`,
/// `getUserMedia`) has no real command-line or `cef_settings_t` kill switch
/// on a prebuilt CEF binary, so it is now removed at the renderer level
/// instead — see `cef_browser::webrtc_block_script`/`inject_context_shims`,
/// injected per navigation on every `run_windowless_tab` session the same way
/// the passkey bridge shim is.
fn chromium_privacy_switches() -> impl Iterator<Item = &'static str> {
    [
        "--disable-background-networking",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-update",
        "--disable-default-apps",
        "--disable-device-discovery-notifications",
        "--disable-domain-reliability",
        "--disable-metrics",
        "--disable-metrics-reporting",
        "--disable-notifications",
        "--disable-speech-api",
        "--disable-sync",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--disable-features=AutofillServerCommunication,DevicePosture,InterestCohort,MediaRouter,PaymentRequest,PrivacySandboxAdsAPIs,Translate,WebBluetooth,WebGPU,WebUSB",
    ]
    .into_iter()
}

/// Owned settings storage with backing UTF-16 strings kept alive for CEF.
pub struct CefSettingsOwned {
    settings: CefSettings,
    _strings: Vec<Vec<u16>>,
}

impl CefSettingsOwned {
    /// Build windowless settings with explicit runtime paths.
    #[must_use]
    pub fn windowless_no_sandbox(paths: &CefInitPaths) -> Self {
        let mut settings = CefSettings::windowless_no_sandbox();
        let mut strings = Vec::new();
        set_utf16_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_USER_AGENT_OFFSET,
            CEF_GENERIC_USER_AGENT,
        );
        set_utf16_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_LOCALE_OFFSET,
            CEF_GENERIC_LOCALE,
        );
        set_utf16_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_ACCEPT_LANGUAGE_LIST_OFFSET,
            CEF_GENERIC_ACCEPT_LANGUAGE,
        );
        set_path_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_BROWSER_SUBPROCESS_PATH_OFFSET,
            &paths.browser_subprocess_path,
        );
        set_path_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_RESOURCES_DIR_PATH_OFFSET,
            &paths.resources_dir_path,
        );
        set_path_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_LOCALES_DIR_PATH_OFFSET,
            &paths.locales_dir_path,
        );
        set_path_string(
            &mut settings,
            &mut strings,
            CEF_SETTINGS_LOG_FILE_OFFSET,
            &paths.log_file,
        );
        Self {
            settings,
            _strings: strings,
        }
    }

    /// Pointer suitable for `cef_initialize`.
    #[must_use]
    pub const fn as_ptr(&self) -> *const c_void {
        self.settings.as_ptr()
    }

    /// Operator-facing settings summary.
    #[must_use]
    pub fn status_line(&self) -> String {
        self.settings.status_line()
    }

    /// Read a raw pointer-sized field from the opaque settings block.
    #[must_use]
    pub fn ptr_field(&self, offset: usize) -> usize {
        self.settings.get_usize(offset)
    }

    /// Read an integer field from the opaque settings block.
    #[must_use]
    pub fn int_field(&self, offset: usize) -> i32 {
        self.settings.get_i32(offset)
    }
}

fn set_path_string(
    settings: &mut CefSettings,
    strings: &mut Vec<Vec<u16>>,
    offset: usize,
    path: &Path,
) {
    let text = path.to_string_lossy();
    set_utf16_string(settings, strings, offset, &text);
}

fn set_utf16_string(
    settings: &mut CefSettings,
    strings: &mut Vec<Vec<u16>>,
    offset: usize,
    text: &str,
) {
    strings.push(text.encode_utf16().collect::<Vec<_>>());
    let data = strings.last().expect("just pushed");
    settings.put_cef_string(offset, data);
}

/// CEF initialization layout/argument construction error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefInitError {
    /// An argv entry contained an interior NUL byte.
    InteriorNul(String),
    /// Argument count did not fit C `int`.
    TooManyArgs(usize),
}

impl fmt::Display for CefInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InteriorNul(arg) => write!(f, "argument contains an interior NUL: {arg:?}"),
            Self::TooManyArgs(count) => write!(f, "{count} arguments do not fit C int"),
        }
    }
}

impl std::error::Error for CefInitError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn linux_main_args_layout_matches_pinned_cef_headers() {
        assert_eq!(size_of::<CefMainArgs>(), CEF_MAIN_ARGS_SIZE);
        assert_eq!(offset_of!(CefMainArgs, argc), CEF_MAIN_ARGS_ARGC_OFFSET);
        assert_eq!(offset_of!(CefMainArgs, argv), CEF_MAIN_ARGS_ARGV_OFFSET);
    }

    #[test]
    fn opaque_settings_storage_matches_pinned_cef_size_and_alignment() {
        assert_eq!(size_of::<CefSettings>(), CEF_SETTINGS_SIZE);
        assert_eq!(align_of::<CefSettings>(), 8);
    }

    #[test]
    fn pinned_string_layout_matches_farm_probe() {
        assert_eq!(CEF_STRING_SIZE, 24);
        assert_eq!(CEF_STRING_STR_OFFSET, 0);
        assert_eq!(CEF_STRING_LENGTH_OFFSET, 8);
        assert_eq!(CEF_STRING_DTOR_OFFSET, 16);
    }

    #[test]
    fn windowless_settings_set_the_fields_cef_initialize_needs() {
        let settings = CefSettings::windowless_no_sandbox();
        assert_eq!(
            settings.get_usize(CEF_SETTINGS_SIZE_OFFSET),
            CEF_SETTINGS_SIZE
        );
        assert_eq!(settings.get_i32(CEF_SETTINGS_NO_SANDBOX_OFFSET), 1);
        assert_eq!(
            settings.get_i32(CEF_SETTINGS_WINDOWLESS_RENDERING_ENABLED_OFFSET),
            1
        );
        assert_eq!(
            settings.get_i32(CEF_SETTINGS_EXTERNAL_MESSAGE_PUMP_OFFSET),
            1
        );
        assert_eq!(
            settings.get_i32(CEF_SETTINGS_MULTI_THREADED_MESSAGE_LOOP_OFFSET),
            0
        );
        // security-4: the CDP port reflects the opt-in gate — `0` (disabled) on
        // the default/shipped path, not a hardcoded live 9222.
        assert_eq!(
            settings.get_i32(CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET),
            remote_debugging_port()
        );
        let line = settings.status_line();
        assert!(line.contains("CEF_INIT_PLAN"));
        assert!(line.contains("windowless=1"));
        assert!(line.contains(&format!(
            "remote_debugging_port={}",
            remote_debugging_port()
        )));
    }

    #[test]
    fn pinned_settings_path_offsets_match_farm_probe() {
        assert_eq!(CEF_SETTINGS_BROWSER_SUBPROCESS_PATH_OFFSET, 16);
        assert_eq!(CEF_SETTINGS_CACHE_PATH_OFFSET, 104);
        assert_eq!(CEF_SETTINGS_ROOT_CACHE_PATH_OFFSET, 128);
        assert_eq!(CEF_SETTINGS_USER_AGENT_OFFSET, 160);
        assert_eq!(CEF_SETTINGS_LOCALE_OFFSET, 208);
        assert_eq!(CEF_SETTINGS_LOG_FILE_OFFSET, 232);
        assert_eq!(CEF_SETTINGS_RESOURCES_DIR_PATH_OFFSET, 288);
        assert_eq!(CEF_SETTINGS_LOCALES_DIR_PATH_OFFSET, 312);
        assert_eq!(CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET, 336);
    }

    #[test]
    fn owned_settings_keep_utf16_runtime_paths_alive() {
        let paths = CefInitPaths {
            browser_subprocess_path: PathBuf::from("/usr/libexec/mackesd/mde-web-cef-renderer"),
            resources_dir_path: PathBuf::from("/opt/mde/cef/Resources"),
            locales_dir_path: PathBuf::from("/opt/mde/cef/Resources/locales"),
            log_file: PathBuf::from("/tmp/mde-web-cef-renderer.log"),
            extension_dirs: Vec::new(),
        };
        let settings = CefSettingsOwned::windowless_no_sandbox(&paths);
        assert_ne!(
            settings.ptr_field(CEF_SETTINGS_BROWSER_SUBPROCESS_PATH_OFFSET),
            0
        );
        assert_ne!(
            settings.ptr_field(CEF_SETTINGS_RESOURCES_DIR_PATH_OFFSET),
            0
        );
        assert_ne!(settings.ptr_field(CEF_SETTINGS_LOCALES_DIR_PATH_OFFSET), 0);
        assert_ne!(settings.ptr_field(CEF_SETTINGS_LOG_FILE_OFFSET), 0);
        assert_eq!(
            settings.int_field(CEF_SETTINGS_WINDOWLESS_RENDERING_ENABLED_OFFSET),
            1
        );
        assert!(settings.status_line().contains("CEF_INIT_PLAN"));
    }

    #[test]
    fn init_paths_emit_early_chromium_resource_switches() {
        let paths = CefInitPaths::new(
            "/usr/libexec/mackesd/mde-web-cef-renderer",
            "/opt/mde/cef/Resources",
        );
        let switches = paths.command_line_switches();
        assert!(switches.contains(&"--no-sandbox".to_owned()));
        assert!(switches.contains(&"--disable-gpu".to_owned()));
        assert!(switches.contains(&"--ozone-platform=headless".to_owned()));
        assert!(switches
            .iter()
            .any(|s| s == "--resources-dir-path=/opt/mde/cef/Resources"));
        assert!(switches
            .iter()
            .any(|s| s == "--locales-dir-path=/opt/mde/cef/Resources/locales"));
        assert!(switches
            .iter()
            .any(|s| s == "--icu-data-file-path=/opt/mde/cef/Resources/icudtl.dat"));
        assert!(switches
            .iter()
            .any(|s| s.starts_with("--browser-subprocess-path=")));
    }

    #[test]
    fn init_paths_emit_cef_privacy_switches() {
        let paths = CefInitPaths::new(
            "/usr/libexec/mackesd/mde-web-cef-renderer",
            "/opt/mde/cef/Resources",
        );
        let switches = paths.command_line_switches();
        assert!(switches.contains(&format!("--user-agent={CEF_GENERIC_USER_AGENT}")));
        assert!(switches.contains(&format!("--accept-lang={CEF_GENERIC_ACCEPT_LANGUAGE}")));
        assert!(switches.contains(&format!("--lang={CEF_GENERIC_LOCALE}")));
        assert!(switches.contains(&"--disable-background-networking".to_owned()));
        assert!(switches.contains(&"--disable-sync".to_owned()));
        assert!(switches.contains(&"--disable-extensions".to_owned()));
        assert!(switches.contains(&"--disable-metrics-reporting".to_owned()));
        assert!(switches
            .contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_owned()));
        assert!(switches.iter().any(|s| {
            s.starts_with("--disable-features=")
                && s.contains("PrivacySandboxAdsAPIs")
                && s.contains("WebGPU")
                && s.contains("WebUSB")
        }));
    }

    #[test]
    fn init_paths_never_emit_the_inert_disable_webrtc_switch() {
        // `--disable-webrtc` is not a real Chromium switch: verified absent
        // from the live `content_switches.cc`/`chrome_switches.cc` upstream
        // registries, and Chromium silently no-ops unrecognized `--` switches
        // rather than erroring — so shipping it here was a false sense of
        // privacy hardening (WebRTC stayed fully reachable). Regression guard
        // against reintroducing it; the real mitigations are
        // `--force-webrtc-ip-handling-policy` (kept above) plus renderer-level
        // API removal (`cef_browser::webrtc_block_script`).
        let paths = CefInitPaths::new(
            "/usr/libexec/mackesd/mde-web-cef-renderer",
            "/opt/mde/cef/Resources",
        );
        let switches = paths.command_line_switches();
        assert!(!switches.iter().any(|s| s == "--disable-webrtc"));
    }

    #[test]
    fn default_launch_never_exposes_the_cdp_debug_port() {
        // security-4: the Chromium DevTools Protocol endpoint is an
        // unauthenticated control channel (drive the browser, read cookies/DOM,
        // run arbitrary JS). On the default/shipped path — no `cef-devtools`
        // feature, no `MDE_CEF_REMOTE_DEBUG` — neither the command line nor the
        // `cef_settings_t` block may carry a live remote-debugging port.
        let paths = CefInitPaths::new(
            "/usr/libexec/mackesd/mde-web-cef-renderer",
            "/opt/mde/cef/Resources",
        );
        let switches = paths.command_line_switches();
        assert!(
            !switches
                .iter()
                .any(|s| s.starts_with("--remote-debugging-port")),
            "shipped launch must not open the CDP debug port"
        );
        let settings = CefSettings::windowless_no_sandbox();
        let expected = if cfg!(feature = "cef-devtools") {
            CEF_REMOTE_DEBUGGING_PORT
        } else {
            0
        };
        assert_eq!(remote_debugging_port(), expected);
        assert_eq!(
            settings.get_i32(CEF_SETTINGS_REMOTE_DEBUGGING_PORT_OFFSET),
            expected
        );
    }

    #[test]
    fn remote_debug_env_flag_parses_the_opt_in_shapes() {
        // Absent / blank => no env decision (fall back to the build feature).
        assert_eq!(parse_remote_debug_flag(None), None);
        assert_eq!(parse_remote_debug_flag(Some("   ")), None);
        // Explicit disable, case-insensitive.
        assert_eq!(parse_remote_debug_flag(Some("0")), Some(RemoteDebug::Off));
        assert_eq!(parse_remote_debug_flag(Some("off")), Some(RemoteDebug::Off));
        assert_eq!(
            parse_remote_debug_flag(Some("False")),
            Some(RemoteDebug::Off)
        );
        // A usable explicit port wins.
        assert_eq!(
            parse_remote_debug_flag(Some("9333")),
            Some(RemoteDebug::Port(9333))
        );
        // Truthy flag / out-of-range port => default loopback port.
        assert_eq!(
            parse_remote_debug_flag(Some("1")),
            Some(RemoteDebug::Port(CEF_REMOTE_DEBUGGING_PORT))
        );
        assert_eq!(
            parse_remote_debug_flag(Some("on")),
            Some(RemoteDebug::Port(CEF_REMOTE_DEBUGGING_PORT))
        );
        assert_eq!(
            parse_remote_debug_flag(Some("80")),
            Some(RemoteDebug::Port(CEF_REMOTE_DEBUGGING_PORT))
        );
    }

    #[test]
    fn remote_debug_resolution_defaults_off_and_honors_overrides() {
        // Feature off + no env => disabled (the shipped default).
        assert_eq!(resolve_remote_debug(false, None), 0);
        // Feature on + no env => default port (dev/debug build).
        assert_eq!(resolve_remote_debug(true, None), CEF_REMOTE_DEBUGGING_PORT);
        // Env overrides the feature default in both directions.
        assert_eq!(resolve_remote_debug(true, Some(RemoteDebug::Off)), 0);
        assert_eq!(
            resolve_remote_debug(false, Some(RemoteDebug::Port(9345))),
            9345
        );
    }

    #[test]
    fn opting_in_reopens_the_cdp_debug_port() {
        // security-4: prove the opt-in path still works — when resolved on, both
        // the switch and the settings field carry the port. Exercised via the
        // pure resolver so the test does not mutate process-global env.
        assert_eq!(
            resolve_remote_debug(false, Some(RemoteDebug::Port(CEF_REMOTE_DEBUGGING_PORT))),
            CEF_REMOTE_DEBUGGING_PORT
        );
        assert_eq!(resolve_remote_debug(true, None), CEF_REMOTE_DEBUGGING_PORT);
    }

    #[test]
    fn init_paths_load_only_vetted_extensions_when_present() {
        let paths = CefInitPaths::new(
            "/usr/libexec/mackesd/mde-web-cef-renderer",
            "/opt/mde/cef/Resources",
        )
        .with_extension_dirs(vec![
            PathBuf::from("/mnt/mesh-storage/browser/extensions/lastpass"),
            PathBuf::from("/mnt/mesh-storage/browser/extensions/ublock-origin"),
        ]);
        let switches = paths.command_line_switches();
        assert!(!switches.contains(&"--disable-extensions".to_owned()));
        assert!(switches.contains(&"--enable-extensions".to_owned()));
        assert!(
            switches.contains(&"--disable-component-extensions-with-background-pages".to_owned())
        );
        assert!(switches.iter().any(|switch| switch
            == "--load-extension=/mnt/mesh-storage/browser/extensions/lastpass,/mnt/mesh-storage/browser/extensions/ublock-origin"));
    }

    #[test]
    fn owned_settings_pin_generic_browser_identity() {
        let paths = CefInitPaths {
            browser_subprocess_path: PathBuf::from("/usr/libexec/mackesd/mde-web-cef-renderer"),
            resources_dir_path: PathBuf::from("/opt/mde/cef/Resources"),
            locales_dir_path: PathBuf::from("/opt/mde/cef/Resources/locales"),
            log_file: PathBuf::from("/tmp/mde-web-cef-renderer.log"),
            extension_dirs: Vec::new(),
        };
        let settings = CefSettingsOwned::windowless_no_sandbox(&paths);
        let encoded = |text: &str| text.encode_utf16().collect::<Vec<_>>();

        assert_ne!(settings.ptr_field(CEF_SETTINGS_USER_AGENT_OFFSET), 0);
        assert_ne!(settings.ptr_field(CEF_SETTINGS_LOCALE_OFFSET), 0);
        assert_ne!(
            settings.ptr_field(CEF_SETTINGS_ACCEPT_LANGUAGE_LIST_OFFSET),
            0
        );
        assert!(settings._strings.contains(&encoded(CEF_GENERIC_USER_AGENT)));
        assert!(settings._strings.contains(&encoded(CEF_GENERIC_LOCALE)));
        assert!(settings
            ._strings
            .contains(&encoded(CEF_GENERIC_ACCEPT_LANGUAGE)));
    }

    #[test]
    fn main_args_owns_c_strings_for_cef_initialize() {
        let args = CefMainArgs::from_args(&[
            "mde-web-cef-renderer".to_owned(),
            "--type=renderer".to_owned(),
        ])
        .expect("args");
        assert_eq!(args.argc(), 2);
        assert!(!args.as_ptr().is_null());
    }

    #[test]
    fn main_args_rejects_interior_nul() {
        let Err(err) = CefMainArgs::from_args(&["bad\0arg".to_owned()]) else {
            panic!("must fail");
        };
        assert!(matches!(err, CefInitError::InteriorNul(_)));
    }
}
