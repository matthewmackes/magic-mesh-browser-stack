//! `mde-web-cef` — the Chromium/CEF engine lane for the first-class Browser.
//!
//! This crate intentionally starts as a protocol-compatible, honest-gated helper:
//! it shares the BOOKMARKS-6 wire contract with Servo, exposes the same operator
//! modes, and reports a typed missing-runtime reason until the farm vendors a CEF
//! bundle. That lets the shell and packaging wire the Chromium engine without a
//! placeholder renderer or a fake "CEF works" state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable pointing at a pinned CEF bundle root.
pub const CEF_ROOT_ENV: &str = "MDE_CEF_ROOT";
/// Conventional farm/vendor path for the pinned CEF bundle.
pub const DEFAULT_CEF_ROOT: &str = "/opt/mde/cef";
/// The runtime library expected under the CEF bundle.
pub const CEF_LIB_NAME: &str = "libcef.so";
/// CEF binary distributions place the runtime library under `Release/`.
pub const CEF_RELEASE_DIR: &str = "Release";
/// CEF binary distributions place pak/ICU resources under `Resources/`.
pub const CEF_RESOURCES_DIR: &str = "Resources";
/// The ICU data file CEF requires at runtime.
pub const CEF_ICU_DATA: &str = "icudtl.dat";
/// The primary CEF resource pak.
pub const CEF_RESOURCES_PAK: &str = "resources.pak";
/// Optional env override for the native renderer bridge binary.
pub const CEF_BRIDGE_BIN_ENV: &str = "MDE_CEF_BRIDGE_BIN";
/// The future native bridge binary path packaged beside the helper.
pub const DEFAULT_CEF_BRIDGE_BIN: &str = "/usr/libexec/mackesd/mde-web-cef-renderer";
/// Bridge env carrying the validated CEF bundle root.
pub const CEF_BRIDGE_ROOT_ENV: &str = "MDE_CEF_BRIDGE_ROOT";
/// Bridge env carrying the validated `libcef.so` path.
pub const CEF_BRIDGE_LIBCEF_ENV: &str = "MDE_CEF_BRIDGE_LIBCEF";
/// Bridge env carrying the validated CEF `Release/` directory.
pub const CEF_BRIDGE_RELEASE_ENV: &str = "MDE_CEF_BRIDGE_RELEASE_DIR";
/// Bridge env carrying the validated CEF resources directory.
pub const CEF_BRIDGE_RESOURCES_ENV: &str = "MDE_CEF_BRIDGE_RESOURCES_DIR";
/// Environment variable pointing at an installed Widevine CDM root.
pub const WIDEVINE_ROOT_ENV: &str = "MDE_WIDEVINE_ROOT";
/// Conventional first-run install path for the Widevine CDM.
pub const DEFAULT_WIDEVINE_ROOT: &str = "/opt/mde/widevine";
/// The Widevine CDM shared library name expected by Chromium/CEF.
pub const WIDEVINE_LIB_NAME: &str = "libwidevinecdm.so";
/// Optional Widevine CDM metadata file shipped beside the library by upstream bundles.
pub const WIDEVINE_MANIFEST_NAME: &str = "manifest.json";
/// Bridge env carrying the optional Widevine CDM root.
pub const CEF_BRIDGE_WIDEVINE_ROOT_ENV: &str = "MDE_CEF_BRIDGE_WIDEVINE_ROOT";
/// Bridge env carrying the optional Widevine CDM library path.
pub const CEF_BRIDGE_WIDEVINE_LIB_ENV: &str = "MDE_CEF_BRIDGE_WIDEVINE_LIB";

/// The socket wire contract is the same one used by `mde-web-preview` and the
/// shell client. Include the one source file so golden tests catch drift.
#[path = "../../mde-web-preview-client/src/wire.rs"]
pub mod wire;

/// Helper-side shared-memory frame writer for Chromium/CEF offscreen pixels.
pub mod shm;

/// Helper-side Unix-socket event transport for the frame fd and paint-ready events.
pub mod sock;

/// CEF paint-callback target that publishes offscreen pixels into BOOKMARKS-6 shm.
pub mod offscreen;

/// Dynamic `libcef.so` ABI probe and required lifecycle symbol loader.
pub mod cef_abi;

/// Pinned Linux CEF initialization structs/settings layout for the native loop.
pub mod cef_init;

/// Header-pinned CEF windowless browser creation and callback objects.
pub mod cef_browser;

/// Runtime discovery outcome for the Chromium engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefRuntime {
    /// A bundle root with the expected CEF shared library exists.
    Available {
        /// Bundle root.
        root: PathBuf,
        /// Shared library path.
        libcef: PathBuf,
        /// Resource directory path.
        resources: PathBuf,
    },
    /// No usable CEF bundle is installed.
    Missing {
        /// Bundle root that was checked.
        root: PathBuf,
        /// Human-readable operator reason.
        reason: String,
    },
}

/// Runtime discovery outcome for the optional Widevine CDM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidevineCdm {
    /// A CDM bundle root with the expected shared library exists.
    Available {
        /// Bundle root.
        root: PathBuf,
        /// CDM library path.
        libwidevine: PathBuf,
        /// Optional upstream CDM manifest.
        manifest: Option<PathBuf>,
    },
    /// No usable CDM bundle is installed.
    Missing {
        /// Bundle root that was checked.
        root: PathBuf,
        /// Human-readable operator reason.
        reason: String,
    },
}

impl WidevineCdm {
    /// Whether the optional CDM is available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// Human-readable status for CLI probes and shell gates.
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::Available {
                root,
                libwidevine,
                manifest,
            } => {
                let manifest = manifest
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |path| path.display().to_string());
                format!(
                    "WIDEVINE_OK root={} lib={} manifest={manifest}",
                    root.display(),
                    libwidevine.display()
                )
            }
            Self::Missing { root, reason } => {
                format!("WIDEVINE_MISSING root={} reason={reason}", root.display())
            }
        }
    }
}

impl CefRuntime {
    /// Whether CEF is available.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// Human-readable status for CLI probes and shell gates.
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::Available {
                root,
                libcef,
                resources,
            } => {
                format!(
                    "CEF_OK root={} lib={} resources={}",
                    root.display(),
                    libcef.display(),
                    resources.display()
                )
            }
            Self::Missing { root, reason } => {
                format!("CEF_MISSING root={} reason={reason}", root.display())
            }
        }
    }
}

/// Resolve the configured CEF bundle root.
#[must_use]
pub fn configured_cef_root() -> PathBuf {
    std::env::var_os(CEF_ROOT_ENV).map_or_else(|| PathBuf::from(DEFAULT_CEF_ROOT), PathBuf::from)
}

/// Resolve the configured Widevine CDM root.
#[must_use]
pub fn configured_widevine_root() -> PathBuf {
    std::env::var_os(WIDEVINE_ROOT_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_WIDEVINE_ROOT), PathBuf::from)
}

/// Check whether a CEF runtime bundle is present.
#[must_use]
pub fn detect_runtime(root: impl AsRef<Path>) -> CefRuntime {
    let root = root.as_ref().to_path_buf();
    if let Some(libcef) = find_libcef(&root) {
        if let Some(resources) = find_resources(&root) {
            CefRuntime::Available {
                root,
                libcef,
                resources,
            }
        } else {
            CefRuntime::Missing {
                root,
                reason: format!(
                    "{CEF_RESOURCES_DIR}/{CEF_ICU_DATA} and {CEF_RESOURCES_DIR}/{CEF_RESOURCES_PAK} are not vendored yet"
                ),
            }
        }
    } else {
        CefRuntime::Missing {
            root,
            reason: format!("{CEF_RELEASE_DIR}/{CEF_LIB_NAME} is not vendored yet"),
        }
    }
}

/// Locate `libcef.so` in the installed bundle.
#[must_use]
pub fn find_libcef(root: impl AsRef<Path>) -> Option<PathBuf> {
    let root = root.as_ref();
    [
        root.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME),
        root.join(CEF_LIB_NAME),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// Locate the CEF resources directory.
#[must_use]
pub fn find_resources(root: impl AsRef<Path>) -> Option<PathBuf> {
    let root = root.as_ref();
    [root.join(CEF_RESOURCES_DIR), root.to_path_buf()]
        .into_iter()
        .find(|path| path.join(CEF_ICU_DATA).is_file() && path.join(CEF_RESOURCES_PAK).is_file())
}

/// Check whether an optional Widevine CDM bundle is present.
#[must_use]
pub fn detect_widevine(root: impl AsRef<Path>) -> WidevineCdm {
    let root = root.as_ref().to_path_buf();
    if let Some(libwidevine) = find_widevine_lib(&root) {
        let manifest = find_widevine_manifest(&root);
        WidevineCdm::Available {
            root,
            libwidevine,
            manifest,
        }
    } else {
        WidevineCdm::Missing {
            root,
            reason: format!("{WIDEVINE_LIB_NAME} is not installed; DRM streaming remains gated"),
        }
    }
}

/// Locate `libwidevinecdm.so` in an installed CDM bundle.
#[must_use]
pub fn find_widevine_lib(root: impl AsRef<Path>) -> Option<PathBuf> {
    let root = root.as_ref();
    [
        root.join(WIDEVINE_LIB_NAME),
        root.join("_platform_specific/linux_x64")
            .join(WIDEVINE_LIB_NAME),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// Locate a Widevine CDM manifest when one is present.
#[must_use]
pub fn find_widevine_manifest(root: impl AsRef<Path>) -> Option<PathBuf> {
    let root = root.as_ref();
    [root.join(WIDEVINE_MANIFEST_NAME)]
        .into_iter()
        .find(|path| path.is_file())
}

/// Resolve the native renderer bridge binary path.
#[must_use]
pub fn configured_bridge_bin() -> PathBuf {
    std::env::var_os(CEF_BRIDGE_BIN_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_CEF_BRIDGE_BIN), PathBuf::from)
}

/// The validated paths and environment needed to launch the native CEF renderer bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CefLaunchPlan {
    /// Bundle root.
    pub root: PathBuf,
    /// Runtime library path.
    pub libcef: PathBuf,
    /// Directory containing CEF shared libraries.
    pub release_dir: PathBuf,
    /// Directory containing CEF resources.
    pub resources_dir: PathBuf,
    /// Optional Widevine CDM root + shared library, when installed.
    pub widevine: Option<(PathBuf, PathBuf)>,
    /// Native renderer bridge binary.
    pub bridge_bin: PathBuf,
    /// `LD_LIBRARY_PATH` value for the bridge process.
    pub ld_library_path: OsString,
}

impl CefLaunchPlan {
    /// Create a process launch plan for the runtime and bridge.
    #[must_use]
    pub fn new(runtime: &CefRuntime, bridge_bin: impl AsRef<Path>) -> Option<Self> {
        Self::new_with_widevine(
            runtime,
            &detect_widevine(configured_widevine_root()),
            bridge_bin,
        )
    }

    /// Create a process launch plan for the runtime, optional CDM, and bridge.
    #[must_use]
    pub fn new_with_widevine(
        runtime: &CefRuntime,
        widevine: &WidevineCdm,
        bridge_bin: impl AsRef<Path>,
    ) -> Option<Self> {
        let CefRuntime::Available {
            root,
            libcef,
            resources,
        } = runtime
        else {
            return None;
        };
        let release_dir = libcef.parent()?.to_path_buf();
        Some(Self {
            root: root.clone(),
            libcef: libcef.clone(),
            release_dir: release_dir.clone(),
            resources_dir: resources.clone(),
            widevine: match widevine {
                WidevineCdm::Available {
                    root, libwidevine, ..
                } => Some((root.clone(), libwidevine.clone())),
                WidevineCdm::Missing { .. } => None,
            },
            bridge_bin: bridge_bin.as_ref().to_path_buf(),
            ld_library_path: release_dir.into_os_string(),
        })
    }

    /// Whether the native bridge binary is installed.
    #[must_use]
    pub fn bridge_available(&self) -> bool {
        self.bridge_bin.is_file()
    }

    /// Human-readable launch contract for probes and tests.
    #[must_use]
    pub fn status_line(&self) -> String {
        let widevine = self.widevine.as_ref().map_or_else(
            || "missing".to_owned(),
            |(_, lib)| lib.display().to_string(),
        );
        format!(
            "CEF_LAUNCH root={} bridge={} lib={} resources={} widevine={} ld_library_path={}",
            self.root.display(),
            self.bridge_bin.display(),
            self.libcef.display(),
            self.resources_dir.display(),
            widevine,
            self.release_dir.display()
        )
    }

    /// Renderer gate reason when the native bridge is not packaged yet.
    #[must_use]
    pub fn missing_bridge_line(&self) -> String {
        format!("CEF_RENDERER_MISSING bridge={}", self.bridge_bin.display())
    }

    /// Environment contract passed to the native bridge.
    #[must_use]
    pub fn bridge_env(&self) -> Vec<(&'static str, OsString)> {
        let mut env = vec![
            (CEF_BRIDGE_ROOT_ENV, self.root.clone().into_os_string()),
            (CEF_BRIDGE_LIBCEF_ENV, self.libcef.clone().into_os_string()),
            (
                CEF_BRIDGE_RELEASE_ENV,
                self.release_dir.clone().into_os_string(),
            ),
            (
                CEF_BRIDGE_RESOURCES_ENV,
                self.resources_dir.clone().into_os_string(),
            ),
        ];
        if let Some((root, lib)) = &self.widevine {
            env.push((CEF_BRIDGE_WIDEVINE_ROOT_ENV, root.clone().into_os_string()));
            env.push((CEF_BRIDGE_WIDEVINE_LIB_ENV, lib.clone().into_os_string()));
        }
        env
    }

    /// `LD_LIBRARY_PATH` with the CEF release dir first, preserving any caller value.
    #[must_use]
    pub fn merged_ld_library_path(&self, current: Option<OsString>) -> OsString {
        let mut paths = vec![self.release_dir.clone()];
        if let Some(current) = current {
            paths.extend(std::env::split_paths(&current));
        }
        std::env::join_paths(paths).unwrap_or_else(|_| self.ld_library_path.clone())
    }
}

/// The CLI mode requested by an operator or the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Print runtime status only.
    Probe,
    /// Production tab process, protocol-compatible with Servo once CEF is present.
    Tab,
    /// Headless single-shot render test.
    RenderOnce,
    /// Warm helper process.
    Warm,
    /// Print usage.
    Help,
}

/// Parse the first CLI argument into a mode.
#[must_use]
pub fn parse_mode(arg: Option<&str>) -> Option<Mode> {
    Some(match arg.unwrap_or("probe") {
        "probe" | "status" => Mode::Probe,
        "tab" => Mode::Tab,
        "render-once" => Mode::RenderOnce,
        "warm" => Mode::Warm,
        "help" | "--help" | "-h" => Mode::Help,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{frame, take_frame, ControlMsg};
    use std::path::Path;

    #[test]
    fn missing_runtime_names_the_checked_root() {
        let rt = detect_runtime("/definitely/not/mde-cef");
        assert!(!rt.is_available());
        let line = rt.status_line();
        assert!(line.contains("CEF_MISSING"));
        assert!(line.contains("/definitely/not/mde-cef"));
        assert!(line.contains(CEF_LIB_NAME));
    }

    #[test]
    fn available_runtime_requires_libcef() {
        let dir = std::env::temp_dir().join(format!("mde-web-cef-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("mkdir resources");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        let rt = detect_runtime(&dir);
        assert!(rt.is_available());
        assert_eq!(
            find_libcef(&dir).expect("cef lib"),
            dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME)
        );
        assert_eq!(
            find_resources(&dir).expect("resources"),
            dir.join(CEF_RESOURCES_DIR)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flattened_runtime_layout_still_works_for_test_overrides() {
        let dir =
            std::env::temp_dir().join(format!("mde-web-cef-flat-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(CEF_LIB_NAME), b"test").expect("libcef marker");
        std::fs::write(dir.join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_PAK), b"pak").expect("pak marker");
        assert!(detect_runtime(&dir).is_available());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn widevine_detection_is_optional_and_names_the_checked_root() {
        let rt = detect_widevine("/definitely/not/mde-widevine");
        assert!(!rt.is_available());
        let line = rt.status_line();
        assert!(line.contains("WIDEVINE_MISSING"));
        assert!(line.contains("/definitely/not/mde-widevine"));
        assert!(line.contains(WIDEVINE_LIB_NAME));
    }

    #[test]
    fn widevine_detection_accepts_firefox_style_bundle_layout() {
        let dir =
            std::env::temp_dir().join(format!("mde-web-widevine-test-{}", std::process::id()));
        let platform = dir.join("_platform_specific/linux_x64");
        std::fs::create_dir_all(&platform).expect("mkdir platform");
        std::fs::write(platform.join(WIDEVINE_LIB_NAME), b"test").expect("widevine marker");
        std::fs::write(dir.join(WIDEVINE_MANIFEST_NAME), b"{}").expect("widevine manifest");
        let cdm = detect_widevine(&dir);
        assert!(cdm.is_available());
        assert_eq!(
            find_widevine_lib(&dir).expect("widevine lib"),
            platform.join(WIDEVINE_LIB_NAME)
        );
        assert_eq!(
            find_widevine_manifest(&dir).expect("widevine manifest"),
            dir.join(WIDEVINE_MANIFEST_NAME)
        );
        assert!(cdm.status_line().contains("WIDEVINE_OK"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn runtime_detection_requires_resources_as_well_as_libcef() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-no-resources-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        let rt = detect_runtime(&dir);
        assert!(!rt.is_available());
        assert!(rt.status_line().contains(CEF_RESOURCES_DIR));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mode_parser_covers_servo_compatible_modes() {
        assert_eq!(parse_mode(None), Some(Mode::Probe));
        assert_eq!(parse_mode(Some("tab")), Some(Mode::Tab));
        assert_eq!(parse_mode(Some("render-once")), Some(Mode::RenderOnce));
        assert_eq!(parse_mode(Some("warm")), Some(Mode::Warm));
        assert_eq!(parse_mode(Some("nonsense")), None);
    }

    #[test]
    fn shared_wire_framing_accepts_control_messages() {
        let mut buf = frame(&ControlMsg::Load("https://example.com/".to_owned()).encode());
        let payload = take_frame(&mut buf)
            .expect("framing ok")
            .expect("complete frame");
        assert_eq!(
            ControlMsg::decode(&payload).expect("decode"),
            ControlMsg::Load("https://example.com/".to_owned())
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn launch_plan_names_the_bridge_and_runtime_environment() {
        let dir =
            std::env::temp_dir().join(format!("mde-web-cef-plan-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir release");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("mkdir resources");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        let rt = detect_runtime(&dir);
        let bridge = dir.join("mde-web-cef-renderer");
        let widevine_dir = dir.join("widevine");
        std::fs::create_dir_all(&widevine_dir).expect("mkdir widevine");
        std::fs::write(widevine_dir.join(WIDEVINE_LIB_NAME), b"widevine").expect("widevine marker");
        let widevine = detect_widevine(&widevine_dir);
        let plan = CefLaunchPlan::new_with_widevine(&rt, &widevine, &bridge).expect("launch plan");
        assert_eq!(plan.release_dir, dir.join(CEF_RELEASE_DIR));
        assert_eq!(plan.resources_dir, dir.join(CEF_RESOURCES_DIR));
        assert_eq!(
            plan.ld_library_path,
            dir.join(CEF_RELEASE_DIR).into_os_string()
        );
        assert!(!plan.bridge_available());
        assert!(plan.status_line().contains("CEF_LAUNCH"));
        assert!(plan.status_line().contains("widevine="));
        assert!(plan.missing_bridge_line().contains("CEF_RENDERER_MISSING"));
        let env = plan.bridge_env();
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_BRIDGE_LIBCEF_ENV
                && value
                    == &dir
                        .join(CEF_RELEASE_DIR)
                        .join(CEF_LIB_NAME)
                        .into_os_string()
        }));
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_BRIDGE_RESOURCES_ENV
                && value == &dir.join(CEF_RESOURCES_DIR).into_os_string()
        }));
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_BRIDGE_WIDEVINE_LIB_ENV
                && value == &widevine_dir.join(WIDEVINE_LIB_NAME).into_os_string()
        }));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn launch_plan_prepends_cef_release_to_ld_library_path() {
        let dir = std::env::temp_dir().join(format!("mde-web-cef-ld-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir release");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("mkdir resources");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        let rt = detect_runtime(&dir);
        let plan = CefLaunchPlan::new(&rt, dir.join("bridge")).expect("launch plan");
        let merged = plan.merged_ld_library_path(Some(OsString::from("/usr/lib64:/opt/extra")));
        let paths = std::env::split_paths(&merged).collect::<Vec<_>>();
        assert_eq!(paths.first(), Some(&dir.join(CEF_RELEASE_DIR)));
        assert!(paths.contains(&PathBuf::from("/usr/lib64")));
        assert!(paths.contains(&PathBuf::from("/opt/extra")));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .expect("repo root")
    }

    #[test]
    fn pinned_linux_cef_manifest_names_the_runtime_payload() {
        let manifest =
            std::fs::read_to_string(repo_root().join("packaging/browser/cef-linux64-minimal.env"))
                .expect("cef manifest");
        for needle in [
            "CEF_CHANNEL=\"stable\"",
            "CEF_PLATFORM=\"linux64\"",
            "CEF_TYPE=\"minimal\"",
            "CEF_CHROMIUM_VERSION=\"149.0.7827.201\"",
            "CEF_SIZE_BYTES=\"310966119\"",
            "CEF_SHA256=\"f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc\"",
            "CEF_ACTIVE_LINK=\"/opt/mde/cef\"",
            "CEF_SPACES_REMOTE=\"mcnf-spaces:mcnf-mesh-media/browser/cef/${CEF_ASSET}\"",
        ] {
            assert!(manifest.contains(needle), "missing pin: {needle}");
        }
        assert!(
            manifest.contains(
                "cef_binary_149.0.6+g0d0eeb6+chromium-149.0.7827.201_linux64_minimal.tar.bz2"
            ),
            "the manifest must pin the exact upstream CEF tarball"
        );
        assert!(
            manifest.contains("cef-builds.spotifycdn.com"),
            "the manifest should use the official CEF binary distribution host"
        );
    }

    #[test]
    fn cef_installer_verifies_and_activates_the_real_bundle_layout() {
        let script =
            std::fs::read_to_string(repo_root().join("install-helpers/install-cef-runtime.sh"))
                .expect("cef installer");
        for needle in [
            "sha256sum -c -",
            "curl -fsSL --retry 3",
            "Release/libcef.so",
            "normalize_release_resources",
            "Release/$asset",
            "../Resources/$asset",
            "ln -sfn \"$INSTALL_ROOT\" \"$ACTIVE_LINK\"",
            "mde-cef-runtime.manifest",
            "packaging/browser/cef-linux64-minimal.env",
        ] {
            assert!(script.contains(needle), "installer missing: {needle}");
        }
    }

    #[test]
    fn cef_spaces_mirror_helper_uses_the_pinned_payload_and_verifies_remote_hash() {
        let script = std::fs::read_to_string(
            repo_root().join("install-helpers/mirror-cef-runtime-to-spaces.sh"),
        )
        .expect("cef mirror helper");
        for needle in [
            "packaging/browser/cef-linux64-minimal.env",
            "rclone copyto \"$ARCHIVE\" \"$REMOTE\"",
            "rclone cat \"$REMOTE\" | sha256sum",
            "CEF_SHA256",
            "--push",
            "--dry-run",
        ] {
            assert!(script.contains(needle), "mirror helper missing: {needle}");
        }
    }

    #[test]
    fn widevine_installer_is_operator_pinned_and_does_not_ship_the_cdm() {
        let manifest =
            std::fs::read_to_string(repo_root().join("packaging/browser/widevine-linux64.env"))
                .expect("widevine manifest");
        for needle in [
            "WIDEVINE_PLATFORM=\"linux64\"",
            "WIDEVINE_INSTALL_PARENT=\"/opt/mde/widevine-cdms\"",
            "WIDEVINE_ACTIVE_LINK=\"/opt/mde/widevine\"",
            "WIDEVINE_URL=\"\"",
            "WIDEVINE_SHA256=\"\"",
            "WIDEVINE_LICENSE_NOTE",
        ] {
            assert!(
                manifest.contains(needle),
                "missing manifest field: {needle}"
            );
        }

        let script =
            std::fs::read_to_string(repo_root().join("install-helpers/install-widevine-cdm.sh"))
                .expect("widevine installer");
        for needle in [
            "operator must provide WIDEVINE_URL and WIDEVINE_SHA256",
            "sha256sum -c -",
            "curl -fsSL --retry 3",
            "libwidevinecdm.so",
            "ln -sfn \"$INSTALL_ROOT\" \"$ACTIVE_LINK\"",
            "mde-widevine-cdm.manifest",
            "packaging/browser/widevine-linux64.env",
        ] {
            assert!(script.contains(needle), "installer missing: {needle}");
        }
    }
}
