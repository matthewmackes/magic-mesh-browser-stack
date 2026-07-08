//! Dynamic `libcef.so` ABI loader for the Chromium bridge.
//!
//! The CEF binary bundle is installed at runtime, so the bridge cannot link
//! `libcef.so` at build time. This module verifies that the runtime exports the
//! lifecycle and metadata functions the offscreen renderer loop needs, then calls
//! only the safe metadata functions until the full callback tree is wired.

use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::{c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use crate::cef_init::{CefMainArgsOwned, CefSettingsOwned};

/// CEF API version selected for the bridge metadata hash.
///
/// Matches `CEF_API_VERSION_EXPERIMENTAL` in `cef_api_hash.h` for the pinned CEF
/// bundle. The later callback slice can pin an explicit stable API version once
/// we generate Rust C structs from the headers.
pub const CEF_API_VERSION_EXPERIMENTAL: c_int = 999_999;

const REQUIRED_SYMBOLS: &[&str] = &[
    "cef_api_hash",
    "cef_api_version",
    "cef_version_info",
    "cef_execute_process",
    "cef_initialize",
    "cef_get_exit_code",
    "cef_do_message_loop_work",
    "cef_run_message_loop",
    "cef_quit_message_loop",
    "cef_browser_host_create_browser_sync",
    "cef_string_userfree_utf16_free",
    "cef_shutdown",
];

type CefApiHash = unsafe extern "C" fn(c_int, c_int) -> *const c_char;
type CefApiVersion = unsafe extern "C" fn() -> c_int;
type CefVersionInfo = unsafe extern "C" fn(c_int) -> c_int;
type CefExecuteProcess = unsafe extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> c_int;
type CefInitialize =
    unsafe extern "C" fn(*const c_void, *const c_void, *mut c_void, *mut c_void) -> c_int;
type CefGetExitCode = unsafe extern "C" fn() -> c_int;
type CefDoMessageLoopWork = unsafe extern "C" fn();
type CefBrowserHostCreateBrowserSync = unsafe extern "C" fn(
    *const c_void,
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *mut c_void,
) -> *mut c_void;
pub(crate) type CefStringUserfreeUtf16Free = unsafe extern "C" fn(*mut c_void);
type CefShutdown = unsafe extern "C" fn();

/// Loaded metadata from `libcef.so`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefAbiMetadata {
    /// Configured API version reported by `cef_api_version`.
    pub api_version: i32,
    /// Platform API hash.
    pub platform_hash: String,
    /// CEF commit hash reported by the runtime.
    pub commit_hash: String,
    /// CEF major version.
    pub cef_major: i32,
    /// CEF minor version.
    pub cef_minor: i32,
    /// CEF patch version.
    pub cef_patch: i32,
    /// Chromium major version.
    pub chrome_major: i32,
    /// Chromium build version.
    pub chrome_build: i32,
    /// Chromium patch version.
    pub chrome_patch: i32,
}

impl CefAbiMetadata {
    /// Operator-facing status line for probes and farm logs.
    #[must_use]
    pub fn status_line(&self) -> String {
        format!(
            "CEF_ABI_OK api={} cef={}.{}.{} chrome={}.{}.{} platform_hash={} commit={}",
            self.api_version,
            self.cef_major,
            self.cef_minor,
            self.cef_patch,
            self.chrome_major,
            self.chrome_build,
            self.chrome_patch,
            self.platform_hash,
            self.commit_hash
        )
    }
}

/// Error returned while loading/probing `libcef.so`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefAbiError {
    /// Library path contained an interior NUL byte.
    BadPath(PathBuf),
    /// `dlopen` failed.
    Open {
        /// Library path passed to `dlopen`.
        path: PathBuf,
        /// Dynamic loader error text.
        reason: String,
    },
    /// A required symbol was not exported.
    MissingSymbol {
        /// Library path passed to `dlsym`.
        path: PathBuf,
        /// Required CEF symbol name.
        symbol: String,
    },
    /// CEF returned a null metadata string.
    NullMetadata {
        /// Metadata function that returned null.
        symbol: &'static str,
        /// Metadata entry requested.
        entry: i32,
    },
    /// `cef_initialize` returned false.
    InitializeFailed {
        /// CEF exit code after failed initialization.
        exit_code: i32,
    },
}

impl fmt::Display for CefAbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPath(path) => write!(f, "{} cannot be passed to dlopen", path.display()),
            Self::Open { path, reason } => write!(f, "dlopen {} failed: {reason}", path.display()),
            Self::MissingSymbol { path, symbol } => {
                write!(f, "{} does not export {symbol}", path.display())
            }
            Self::NullMetadata { symbol, entry } => {
                write!(f, "{symbol} returned null for entry {entry}")
            }
            Self::InitializeFailed { exit_code } => {
                write!(f, "cef_initialize failed with exit code {exit_code}")
            }
        }
    }
}

impl std::error::Error for CefAbiError {}

/// Dynamic handle for the loaded CEF runtime.
pub struct CefAbi {
    path: PathBuf,
    handle: NonNull<c_void>,
    cef_api_hash: CefApiHash,
    cef_api_version: CefApiVersion,
    cef_version_info: CefVersionInfo,
    cef_execute_process: CefExecuteProcess,
    cef_initialize: CefInitialize,
    cef_get_exit_code: CefGetExitCode,
    cef_do_message_loop_work: CefDoMessageLoopWork,
    cef_browser_host_create_browser_sync: CefBrowserHostCreateBrowserSync,
    cef_string_userfree_utf16_free: CefStringUserfreeUtf16Free,
    cef_shutdown: CefShutdown,
}

impl CefAbi {
    /// Load `libcef.so` and resolve the required bridge symbols.
    ///
    /// # Errors
    /// Returns [`CefAbiError`] when the library cannot be opened or a required
    /// CEF API symbol is missing.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CefAbiError> {
        let path = path.as_ref().to_path_buf();
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| CefAbiError::BadPath(path.clone()))?;
        // SAFETY: `c_path` is a valid NUL-terminated filesystem path. The handle
        // is owned by `Self` and closed in Drop.
        let raw = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        let Some(handle) = NonNull::new(raw) else {
            return Err(CefAbiError::Open {
                path,
                reason: dlerror_string(),
            });
        };

        let loaded = Self {
            cef_api_hash: load_symbol(handle, &path, "cef_api_hash")?,
            cef_api_version: load_symbol(handle, &path, "cef_api_version")?,
            cef_version_info: load_symbol(handle, &path, "cef_version_info")?,
            cef_execute_process: load_symbol(handle, &path, "cef_execute_process")?,
            cef_initialize: load_symbol(handle, &path, "cef_initialize")?,
            cef_get_exit_code: load_symbol(handle, &path, "cef_get_exit_code")?,
            cef_do_message_loop_work: load_symbol(handle, &path, "cef_do_message_loop_work")?,
            cef_browser_host_create_browser_sync: load_symbol(
                handle,
                &path,
                "cef_browser_host_create_browser_sync",
            )?,
            cef_string_userfree_utf16_free: load_symbol(
                handle,
                &path,
                "cef_string_userfree_utf16_free",
            )?,
            cef_shutdown: load_symbol(handle, &path, "cef_shutdown")?,
            path,
            handle,
        };
        for symbol in ["cef_run_message_loop", "cef_quit_message_loop"] {
            let _: *mut c_void = load_raw_symbol(loaded.handle, &loaded.path, symbol)?;
        }
        Ok(loaded)
    }

    /// Probe metadata from the loaded CEF runtime.
    ///
    /// # Errors
    /// Returns [`CefAbiError`] if CEF returns a null metadata pointer.
    pub fn metadata(&self) -> Result<CefAbiMetadata, CefAbiError> {
        let platform_hash = self.hash(0)?;
        let commit_hash = self.hash(2)?;
        // SAFETY: the loaded symbol has CEF's documented signature and requires no
        // prior initialization for version metadata.
        let api_version = unsafe { (self.cef_api_version)() };
        Ok(CefAbiMetadata {
            api_version,
            platform_hash,
            commit_hash,
            cef_major: self.version_info(0),
            cef_minor: self.version_info(1),
            cef_patch: self.version_info(2),
            chrome_major: self.version_info(4),
            chrome_build: self.version_info(6),
            chrome_patch: self.version_info(7),
        })
    }

    fn hash(&self, entry: c_int) -> Result<String, CefAbiError> {
        // SAFETY: the loaded symbol has CEF's documented signature and returns a
        // library-owned NUL-terminated string or null.
        let ptr = unsafe { (self.cef_api_hash)(CEF_API_VERSION_EXPERIMENTAL, entry) };
        if ptr.is_null() {
            return Err(CefAbiError::NullMetadata {
                symbol: "cef_api_hash",
                entry,
            });
        }
        // SAFETY: CEF returned a non-null pointer to a NUL-terminated string owned
        // by the library for the process lifetime.
        Ok(unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned())
    }

    fn version_info(&self, entry: c_int) -> i32 {
        // SAFETY: the loaded symbol has CEF's documented signature and reads a
        // version table entry.
        unsafe { (self.cef_version_info)(entry) }
    }

    /// Run the CEF process-dispatch + browser-process initialization handshake.
    ///
    /// This does not create a browser yet. It proves the runtime accepts our
    /// Linux main args and `cef_settings_t` layout, then immediately shuts CEF
    /// down if initialization succeeds.
    ///
    /// # Errors
    /// Returns [`CefAbiError`] when CEF reports initialization failure.
    pub fn initialize_probe(
        &self,
        main_args: &CefMainArgsOwned,
        settings: &CefSettingsOwned,
    ) -> Result<CefInitializeProbe, CefAbiError> {
        let outcome = self.initialize_browser_process(main_args, settings)?;
        if outcome == CefInitializeProbe::Initialized {
            self.shutdown();
        }
        Ok(outcome)
    }

    /// Run CEF process dispatch and initialize the browser process without
    /// shutting CEF down. Call [`Self::shutdown`] after all browser callbacks are
    /// finished.
    ///
    /// # Errors
    /// Returns [`CefAbiError`] when CEF reports initialization failure.
    pub fn initialize_browser_process(
        &self,
        main_args: &CefMainArgsOwned,
        settings: &CefSettingsOwned,
    ) -> Result<CefInitializeProbe, CefAbiError> {
        // SAFETY: `main_args` points to an owned Linux `cef_main_args_t`; null app
        // and sandbox pointers match CEF's documented Linux contract.
        let process_code = unsafe {
            (self.cef_execute_process)(
                main_args.as_ptr().cast::<c_void>(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if process_code >= 0 {
            return Ok(CefInitializeProbe::SubprocessExit { code: process_code });
        }

        // SAFETY: `settings` owns an aligned, pinned-layout `cef_settings_t` block
        // and backing strings for the duration of the call; app/sandbox nulls are
        // accepted by CEF on Linux.
        let ok = unsafe {
            (self.cef_initialize)(
                main_args.as_ptr().cast::<c_void>(),
                settings.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: CEF documents this as valid after failed initialization.
            let exit_code = unsafe { (self.cef_get_exit_code)() };
            return Err(CefAbiError::InitializeFailed { exit_code });
        }

        Ok(CefInitializeProbe::Initialized)
    }

    /// Run one iteration of the CEF message loop for external-pump probes.
    pub fn do_message_loop_work(&self) {
        // SAFETY: caller invokes this only after successful `cef_initialize` and
        // before `cef_shutdown`.
        unsafe {
            (self.cef_do_message_loop_work)();
        }
    }

    /// Create a synchronous browser using the pinned C API structs.
    #[must_use]
    pub fn create_browser_sync(
        &self,
        window_info: *const c_void,
        client: *mut c_void,
        url: *const c_void,
        settings: *const c_void,
    ) -> *mut c_void {
        // SAFETY: pointers are built from header-pinned structs in `cef_browser`
        // and kept alive by the caller for the browser probe lifetime.
        unsafe {
            (self.cef_browser_host_create_browser_sync)(
                window_info,
                client,
                url,
                settings,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    }

    /// Free a CEF userfree UTF-16 string returned by CEF getter APIs.
    #[must_use]
    pub(crate) const fn string_userfree_utf16_free(&self) -> CefStringUserfreeUtf16Free {
        self.cef_string_userfree_utf16_free
    }

    /// Shut down CEF after a successful browser-process initialization.
    pub fn shutdown(&self) {
        // SAFETY: caller uses this after successful `cef_initialize`; CEF owns the
        // teardown ordering internally.
        unsafe {
            (self.cef_shutdown)();
        }
    }
}

/// Outcome of the CEF initialization probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefInitializeProbe {
    /// CEF identified this process as a subprocess and returned its exit code.
    SubprocessExit {
        /// CEF subprocess exit code.
        code: i32,
    },
    /// Browser-process initialization succeeded and was shut down.
    Initialized,
}

impl CefInitializeProbe {
    /// Operator-facing status line.
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::SubprocessExit { code } => format!("CEF_INITIALIZE_SUBPROCESS_EXIT code={code}"),
            Self::Initialized => "CEF_INITIALIZE_OK".to_owned(),
        }
    }
}

impl Drop for CefAbi {
    fn drop(&mut self) {
        // SAFETY: `handle` was returned by `dlopen` and is owned by this struct.
        unsafe {
            libc::dlclose(self.handle.as_ptr());
        }
    }
}

fn load_symbol<T: Copy>(
    handle: NonNull<c_void>,
    path: &Path,
    symbol: &str,
) -> Result<T, CefAbiError> {
    let raw = load_raw_symbol(handle, path, symbol)?;
    // SAFETY: caller selects `T` to match the CEF symbol's documented function
    // pointer type. The raw pointer is non-null by construction.
    Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&raw) })
}

fn load_raw_symbol(
    handle: NonNull<c_void>,
    path: &Path,
    symbol: &str,
) -> Result<*mut c_void, CefAbiError> {
    let c_symbol = CString::new(symbol).expect("symbol literals do not contain NUL");
    // SAFETY: `handle` is a live dlopen handle and `c_symbol` is NUL-terminated.
    let ptr = unsafe { libc::dlsym(handle.as_ptr(), c_symbol.as_ptr()) };
    if ptr.is_null() {
        return Err(CefAbiError::MissingSymbol {
            path: path.to_path_buf(),
            symbol: symbol.to_owned(),
        });
    }
    Ok(ptr)
}

fn dlerror_string() -> String {
    // SAFETY: `dlerror` returns either null or a process-owned C string.
    let ptr = unsafe { libc::dlerror() };
    if ptr.is_null() {
        "unknown error".to_owned()
    } else {
        // SAFETY: non-null `dlerror` result is NUL-terminated.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Required CEF symbols for the bridge. Exposed for contract tests.
#[must_use]
pub const fn required_symbols() -> &'static [&'static str] {
    REQUIRED_SYMBOLS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_symbols_cover_metadata_and_lifecycle() {
        for symbol in [
            "cef_api_hash",
            "cef_api_version",
            "cef_version_info",
            "cef_execute_process",
            "cef_initialize",
            "cef_get_exit_code",
            "cef_do_message_loop_work",
            "cef_run_message_loop",
            "cef_quit_message_loop",
            "cef_browser_host_create_browser_sync",
            "cef_string_userfree_utf16_free",
            "cef_shutdown",
        ] {
            assert!(required_symbols().contains(&symbol));
        }
    }

    #[test]
    fn missing_library_is_a_typed_error() {
        let missing = std::env::temp_dir().join("mde-web-cef-definitely-missing-libcef.so");
        let Err(err) = CefAbi::load(&missing) else {
            panic!("missing lib must fail");
        };
        assert!(matches!(err, CefAbiError::Open { .. }));
        assert!(err.to_string().contains("dlopen"));
    }

    #[test]
    fn abi_status_line_is_operator_readable() {
        let meta = CefAbiMetadata {
            api_version: 999_999,
            platform_hash: "hash".to_owned(),
            commit_hash: "commit".to_owned(),
            cef_major: 149,
            cef_minor: 0,
            cef_patch: 6,
            chrome_major: 149,
            chrome_build: 7827,
            chrome_patch: 201,
        };
        let line = meta.status_line();
        assert!(line.contains("CEF_ABI_OK"));
        assert!(line.contains("cef=149.0.6"));
        assert!(line.contains("chrome=149.7827.201"));
    }

    #[test]
    fn initialize_probe_status_lines_are_operator_readable() {
        assert_eq!(
            CefInitializeProbe::Initialized.status_line(),
            "CEF_INITIALIZE_OK"
        );
        assert_eq!(
            CefInitializeProbe::SubprocessExit { code: 3 }.status_line(),
            "CEF_INITIALIZE_SUBPROCESS_EXIT code=3"
        );
    }

    #[test]
    fn initialize_failure_is_typed() {
        let err = CefAbiError::InitializeFailed { exit_code: 42 };
        assert!(err.to_string().contains("cef_initialize failed"));
        assert!(err.to_string().contains("42"));
    }
}
