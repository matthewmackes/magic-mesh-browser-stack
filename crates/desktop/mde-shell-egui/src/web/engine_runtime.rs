//! Browser-engine runtime path resolution — where the Servo/CEF helper binaries
//! and the CEF runtime bundle (lib + ICU/pak resources) live, and which engine
//! is the preferred default on this seat (CEF when its runtime is fully present,
//! else Servo). Pure filesystem/env probing, extracted from the `web` god-module.
//!
//! `use super::*` pulls in the parent module's private `BrowserEngine` + the
//! `*_HELPER_BIN_ENV` / `DEFAULT_CEF_*` / `CEF_*` path constants (a child module
//! may read its parent's private items), so this is a pure relocation.

use super::*;

/// Resolve the selected sandboxed-helper binary path.
#[cfg(feature = "live-helper")]
pub(super) fn helper_bin_path(engine: BrowserEngine) -> std::path::PathBuf {
    let (env, default) = match engine {
        BrowserEngine::Servo => (SERVO_HELPER_BIN_ENV, DEFAULT_SERVO_HELPER_BIN),
        BrowserEngine::Cef => (CEF_HELPER_BIN_ENV, DEFAULT_CEF_HELPER_BIN),
    };
    std::env::var_os(env).map_or_else(
        || std::path::PathBuf::from(default),
        std::path::PathBuf::from,
    )
}

#[cfg(feature = "live-helper")]
fn cef_runtime_root() -> std::path::PathBuf {
    std::env::var_os(CEF_ROOT_ENV).map_or_else(
        || std::path::PathBuf::from(DEFAULT_CEF_ROOT),
        std::path::PathBuf::from,
    )
}

#[cfg(feature = "live-helper")]
pub(super) fn cef_runtime_lib() -> std::path::PathBuf {
    let root = cef_runtime_root();
    let bundled = root.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME);
    if bundled.is_file() {
        bundled
    } else {
        root.join(CEF_LIB_NAME)
    }
}

#[cfg(feature = "live-helper")]
fn cef_runtime_resources() -> std::path::PathBuf {
    let root = cef_runtime_root();
    let bundled = root.join(CEF_RESOURCES_DIR);
    if bundled.join(CEF_ICU_DATA).is_file() && bundled.join(CEF_RESOURCES_PAK).is_file() {
        bundled
    } else {
        root
    }
}

#[cfg(feature = "live-helper")]
pub(super) fn cef_runtime_missing_path() -> Option<std::path::PathBuf> {
    let lib = cef_runtime_lib();
    if !lib.is_file() {
        return Some(lib);
    }
    let resources = cef_runtime_resources();
    for name in [CEF_ICU_DATA, CEF_RESOURCES_PAK] {
        let path = resources.join(name);
        if !path.is_file() {
            return Some(path);
        }
    }
    None
}

#[cfg(feature = "live-helper")]
pub(super) fn preferred_default_engine() -> BrowserEngine {
    if helper_bin_path(BrowserEngine::Cef).is_file() && cef_runtime_missing_path().is_none() {
        BrowserEngine::Cef
    } else {
        BrowserEngine::Servo
    }
}

#[cfg(not(feature = "live-helper"))]
pub(super) const fn preferred_default_engine() -> BrowserEngine {
    BrowserEngine::Servo
}
