//! `mde-web-cef` — the Chromium/CEF engine lane for the first-class Browser.
//!
//! This crate intentionally starts as a protocol-compatible, honest-gated helper:
//! it shares the BOOKMARKS-6 wire contract with Servo, exposes the same operator
//! modes, and reports a typed missing-runtime reason until the farm vendors a CEF
//! bundle. That lets the shell and packaging wire the Chromium engine without a
//! placeholder renderer or a fake "CEF works" state.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

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
/// Bridge env carrying comma-separated vetted unpacked extension directories.
pub const CEF_BRIDGE_EXTENSIONS_ENV: &str = "MDE_CEF_BRIDGE_EXTENSIONS";
/// Bridge env carrying the registry used to vet unpacked extension directories.
pub const CEF_BRIDGE_EXTENSION_REGISTRY_ENV: &str = "MDE_CEF_BRIDGE_EXTENSION_REGISTRY";
/// Optional env override for the curated `WebExtensions` registry.
pub const CEF_EXTENSION_REGISTRY_ENV: &str = "MDE_CEF_EXTENSION_REGISTRY";
/// Env toggle enabling Power Mode sideload entries from the curated registry.
pub const CEF_EXTENSION_POWER_MODE_ENV: &str = "MDE_CEF_EXTENSION_POWER_MODE";
/// Mesh-hosted curated `WebExtensions` registry path.
pub const DEFAULT_CEF_EXTENSION_REGISTRY: &str =
    "/mnt/mesh-storage/browser/extensions/allowlist.env";

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

/// A single vetted unpacked `WebExtension` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedExtension {
    /// Chrome/WebExtension id.
    pub id: String,
    /// Human-readable extension name.
    pub name: String,
    /// Vetted extension version.
    pub version: String,
    /// Unpacked extension directory.
    pub path: PathBuf,
    /// Declared permissions recorded by the registry.
    pub permissions: Vec<String>,
    /// Whether Power Mode may sideload this extension.
    pub power_sideload: bool,
}

/// Curated `WebExtensions` registry discovery state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CefExtensionRegistry {
    /// Registry exists and every entry points at a real unpacked extension dir.
    Available {
        /// Registry path.
        registry: PathBuf,
        /// Vetted extensions.
        extensions: Vec<AllowedExtension>,
    },
    /// Registry was not configured/present.
    Missing {
        /// Registry path that was checked.
        registry: PathBuf,
        /// Human-readable operator reason.
        reason: String,
    },
    /// Registry exists but failed validation.
    Invalid {
        /// Registry path that was checked.
        registry: PathBuf,
        /// Human-readable operator reason.
        reason: String,
    },
}

impl CefExtensionRegistry {
    /// Whether a usable curated registry is present.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// Human-readable status for CLI probes and shell gates.
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::Available {
                registry,
                extensions,
            } => {
                let ids = extensions
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "CEF_EXTENSIONS_READY registry={} allowed={} ids={ids}",
                    registry.display(),
                    extensions.len()
                )
            }
            Self::Missing { registry, reason } => {
                format!(
                    "CEF_EXTENSIONS_MISSING registry={} reason={reason}",
                    registry.display()
                )
            }
            Self::Invalid { registry, reason } => {
                format!(
                    "CEF_EXTENSIONS_INVALID registry={} reason={reason}",
                    registry.display()
                )
            }
        }
    }

    /// Honest runtime gate while CEF extension host support is not wired.
    #[must_use]
    pub fn runtime_gate_line(&self) -> Option<String> {
        let Self::Available {
            registry,
            extensions,
        } = self
        else {
            return None;
        };
        Some(format!(
            "CEF_EXTENSIONS_UNPROVEN registry={} allowed={} reason=live_extension_runtime_smoke_pending",
            registry.display(),
            extensions.len()
        ))
    }

    /// Honest status when registry entries are present but require Power Mode sideload.
    #[must_use]
    pub fn power_mode_gate_line(&self, power_mode: bool) -> Option<String> {
        if power_mode {
            return None;
        }
        let Self::Available {
            registry,
            extensions,
        } = self
        else {
            return None;
        };
        let sideload_count = extensions
            .iter()
            .filter(|extension| extension.power_sideload)
            .count();
        (sideload_count > 0).then(|| {
            format!(
                "CEF_EXTENSIONS_POWER_GATED registry={} sideload={} reason=power_mode_required env={CEF_EXTENSION_POWER_MODE_ENV}",
                registry.display(),
                sideload_count
            )
        })
    }
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

/// Resolve the configured curated `WebExtensions` registry path.
#[must_use]
pub fn configured_extension_registry() -> PathBuf {
    std::env::var_os(CEF_EXTENSION_REGISTRY_ENV).map_or_else(
        || PathBuf::from(DEFAULT_CEF_EXTENSION_REGISTRY),
        PathBuf::from,
    )
}

/// Whether Power Mode extension sideload entries should be included.
#[must_use]
pub fn extension_power_mode_enabled() -> bool {
    std::env::var(CEF_EXTENSION_POWER_MODE_ENV)
        .ok()
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false)
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

/// Check whether a curated `WebExtensions` registry is present and valid.
#[must_use]
pub fn detect_extension_registry(registry: impl AsRef<Path>) -> CefExtensionRegistry {
    let registry = registry.as_ref().to_path_buf();
    if !registry.is_file() {
        return CefExtensionRegistry::Missing {
            registry,
            reason: "registry is not installed; WebExtensions remain gated".to_owned(),
        };
    }
    let base_dir = registry
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    match std::fs::read_to_string(&registry) {
        Ok(text) => match parse_extension_registry(&text, &base_dir) {
            Ok(extensions) => CefExtensionRegistry::Available {
                registry,
                extensions,
            },
            Err(reason) => CefExtensionRegistry::Invalid { registry, reason },
        },
        Err(err) => CefExtensionRegistry::Invalid {
            registry,
            reason: format!("could not read registry: {err}"),
        },
    }
}

/// Parse the curated `WebExtensions` allowlist format.
///
/// The format is intentionally line-oriented and reviewable in Syncthing:
/// `[extension.<32-char chrome id>]` sections with `name`, `version`, `path`,
/// optional comma-separated `permissions`, and optional `power_sideload=true`.
///
/// # Errors
/// Returns a validation error when required fields are absent, ids/paths are
/// unsafe, or a listed unpacked extension directory is missing.
pub fn parse_extension_registry(
    text: &str,
    base_dir: impl AsRef<Path>,
) -> Result<Vec<AllowedExtension>, String> {
    let base_dir = base_dir.as_ref();
    let mut entries = Vec::new();
    let mut current: Option<PendingExtension> = None;
    for (idx, raw_line) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if let Some(pending) = current.take() {
                entries.push(pending.finish(base_dir)?);
            }
            let section = &line[1..line.len() - 1];
            let Some(id) = section.strip_prefix("extension.") else {
                return Err(format!("line {line_no}: expected [extension.<id>] section"));
            };
            validate_extension_id(id)
                .map_err(|reason| format!("line {line_no}: invalid extension id: {reason}"))?;
            current = Some(PendingExtension {
                id: id.to_owned(),
                ..PendingExtension::default()
            });
            continue;
        }
        let Some(pending) = current.as_mut() else {
            return Err(format!(
                "line {line_no}: key appears before an extension section"
            ));
        };
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {line_no}: expected key=value"));
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => pending.name = Some(validate_text_field("name", value, line_no)?),
            "version" => pending.version = Some(validate_text_field("version", value, line_no)?),
            "path" => pending.path = Some(value.to_owned()),
            "permissions" => pending.permissions = parse_permissions(value, line_no)?,
            "power_sideload" => {
                pending.power_sideload = parse_bool(value)
                    .ok_or_else(|| format!("line {line_no}: power_sideload must be true/false"))?;
            }
            other => return Err(format!("line {line_no}: unknown key {other:?}")),
        }
    }
    if let Some(pending) = current.take() {
        entries.push(pending.finish(base_dir)?);
    }
    if entries.is_empty() {
        return Err("registry contains no extension entries".to_owned());
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    for pair in entries.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(format!("duplicate extension id {}", pair[0].id));
        }
    }
    Ok(entries)
}

#[derive(Default)]
struct PendingExtension {
    id: String,
    name: Option<String>,
    version: Option<String>,
    path: Option<String>,
    permissions: Vec<String>,
    power_sideload: bool,
}

impl PendingExtension {
    fn finish(self, base_dir: &Path) -> Result<AllowedExtension, String> {
        let name = self
            .name
            .ok_or_else(|| format!("extension {} is missing name", self.id))?;
        let version = self
            .version
            .ok_or_else(|| format!("extension {} is missing version", self.id))?;
        let raw_path = self
            .path
            .ok_or_else(|| format!("extension {} is missing path", self.id))?;
        let path = resolve_extension_path(base_dir, &raw_path)
            .map_err(|reason| format!("extension {} has invalid path: {reason}", self.id))?;
        if !path.is_dir() {
            return Err(format!(
                "extension {} path is not an unpacked directory: {}",
                self.id,
                path.display()
            ));
        }
        validate_extension_manifest(&path, &version, &self.permissions)
            .map_err(|reason| format!("extension {} manifest is invalid: {reason}", self.id))?;
        Ok(AllowedExtension {
            id: self.id,
            name,
            version,
            path,
            permissions: self.permissions,
            power_sideload: self.power_sideload,
        })
    }
}

fn validate_extension_manifest(
    path: &Path,
    registry_version: &str,
    registry_permissions: &[String],
) -> Result<(), String> {
    let manifest_path = path.join("manifest.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .map_err(|err| format!("{} could not be read: {err}", manifest_path.display()))?;
    if manifest.len() > 256 * 1024 {
        return Err("manifest.json is too large".to_owned());
    }
    let manifest_version = json_number_field(&manifest, "manifest_version")
        .ok_or_else(|| "manifest_version is missing".to_owned())?;
    if !matches!(manifest_version, 2 | 3) {
        return Err(format!(
            "manifest_version {manifest_version} is not supported"
        ));
    }
    let name = json_string_field(&manifest, "name").ok_or_else(|| "name is missing".to_owned())?;
    if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err("name is not a safe manifest string".to_owned());
    }
    let manifest_version_text =
        json_string_field(&manifest, "version").ok_or_else(|| "version is missing".to_owned())?;
    if manifest_version_text.is_empty()
        || manifest_version_text.len() > 64
        || manifest_version_text.chars().any(char::is_control)
    {
        return Err("version is not a safe manifest string".to_owned());
    }
    if registry_version != "operator-pinned" && manifest_version_text != registry_version {
        return Err(format!(
            "registry version {registry_version} does not match manifest version {manifest_version_text}"
        ));
    }
    let manifest_permissions = manifest_permission_tokens(&manifest);
    for permission in registry_permissions {
        if !manifest_permissions.iter().any(|token| token == permission) {
            return Err(format!(
                "registry permission {permission:?} is not declared by manifest"
            ));
        }
    }
    Ok(())
}

fn resolve_extension_path(base_dir: &Path, value: &str) -> Result<PathBuf, String> {
    if value.is_empty() {
        return Err("path is empty".to_owned());
    }
    if value.contains(',') {
        return Err("path must not contain commas".to_owned());
    }
    let path = PathBuf::from(value);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::Prefix(_) | Component::RootDir
        )
    }) && path.is_relative()
    {
        return Err("relative path must stay under the registry directory".to_owned());
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("path must not contain parent-directory segments".to_owned());
    }
    let resolved = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };
    if resolved.to_string_lossy().contains(',') {
        return Err("resolved path must not contain commas".to_owned());
    }
    Ok(resolved)
}

fn manifest_permission_tokens(manifest: &str) -> Vec<String> {
    ["permissions", "host_permissions", "optional_permissions"]
        .into_iter()
        .flat_map(|field| json_string_array_field(manifest, field))
        .collect()
}

fn json_number_field(text: &str, key: &str) -> Option<u32> {
    let start = field_value_start(text, key)?;
    let rest = text[start..].trim_start();
    let digits = rest
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn json_string_field(text: &str, key: &str) -> Option<String> {
    let start = field_value_start(text, key)?;
    parse_json_string(text[start..].trim_start()).map(|(value, _)| value)
}

fn json_string_array_field(text: &str, key: &str) -> Vec<String> {
    let Some(start) = field_value_start(text, key) else {
        return Vec::new();
    };
    let mut rest = text[start..].trim_start();
    let Some(after_open) = rest.strip_prefix('[') else {
        return Vec::new();
    };
    rest = after_open;
    let mut values = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.starts_with(']') {
            return values;
        }
        let Some((value, consumed)) = parse_json_string(rest) else {
            return Vec::new();
        };
        values.push(value);
        rest = &rest[consumed..];
        rest = rest.trim_start();
        if let Some(after_comma) = rest.strip_prefix(',') {
            rest = after_comma;
        } else if rest.starts_with(']') {
            return values;
        } else {
            return Vec::new();
        }
    }
}

fn field_value_start(text: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let key_pos = text.find(&needle)?;
    let after_key = key_pos + needle.len();
    let colon = text[after_key..].find(':')?;
    Some(after_key + colon + 1)
}

fn parse_json_string(text: &str) -> Option<(String, usize)> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if first != '"' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for (idx, ch) in chars {
        if escaped {
            match ch {
                '"' | '\\' | '/' => out.push(ch),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000c}'),
                'u' => return None,
                other => out.push(other),
            }
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some((out, idx + ch.len_utf8())),
            other => out.push(other),
        }
    }
    None
}

fn validate_extension_id(id: &str) -> Result<(), &'static str> {
    if id.len() != 32 {
        return Err("Chrome extension ids are 32 characters");
    }
    if !id.bytes().all(|b| (b'a'..=b'p').contains(&b)) {
        return Err("Chrome extension ids may only contain a-p");
    }
    Ok(())
}

fn validate_text_field(field: &str, value: &str, line_no: usize) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("line {line_no}: {field} is empty"));
    }
    if value.len() > 96 || value.chars().any(char::is_control) {
        return Err(format!(
            "line {line_no}: {field} is not a safe display string"
        ));
    }
    Ok(value.to_owned())
}

fn parse_permissions(value: &str, line_no: usize) -> Result<Vec<String>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|permission| !permission.is_empty())
        .map(|permission| validate_permission_token(permission, line_no))
        .collect()
}

fn validate_permission_token(permission: &str, line_no: usize) -> Result<String, String> {
    if permission.len() > 128
        || permission.chars().any(char::is_control)
        || permission.chars().any(char::is_whitespace)
    {
        return Err(format!("line {line_no}: permission is not a safe token"));
    }
    if is_chrome_api_permission(permission) || is_chrome_match_pattern(permission) {
        Ok(permission.to_owned())
    } else {
        Err(format!(
            "line {line_no}: permission {permission:?} is not a supported Chrome permission or host match pattern"
        ))
    }
}

fn is_chrome_api_permission(permission: &str) -> bool {
    const API_PERMISSIONS: &[&str] = &[
        "activeTab",
        "alarms",
        "background",
        "bookmarks",
        "browsingData",
        "clipboardRead",
        "clipboardWrite",
        "contentSettings",
        "contextMenus",
        "cookies",
        "declarativeContent",
        "declarativeNetRequest",
        "declarativeNetRequestFeedback",
        "declarativeNetRequestWithHostAccess",
        "desktopCapture",
        "downloads",
        "fontSettings",
        "gcm",
        "history",
        "identity",
        "idle",
        "management",
        "nativeMessaging",
        "notifications",
        "offscreen",
        "pageCapture",
        "privacy",
        "proxy",
        "scripting",
        "sessions",
        "sidePanel",
        "storage",
        "system.cpu",
        "system.display",
        "system.memory",
        "system.storage",
        "tabCapture",
        "tabs",
        "topSites",
        "unlimitedStorage",
        "webNavigation",
        "webRequest",
        "webRequestBlocking",
    ];
    API_PERMISSIONS.contains(&permission)
}

fn is_chrome_match_pattern(permission: &str) -> bool {
    if permission == "<all_urls>" {
        return true;
    }
    let Some((scheme, rest)) = permission.split_once("://") else {
        return false;
    };
    if !matches!(
        scheme,
        "*" | "http" | "https" | "file" | "ftp" | "ws" | "wss"
    ) {
        return false;
    }
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    if path.is_empty()
        || path.chars().any(char::is_control)
        || path.chars().any(char::is_whitespace)
    {
        return false;
    }
    if scheme == "file" {
        host.is_empty() || host == "*"
    } else {
        is_chrome_match_host(host)
    }
}

fn is_chrome_match_host(host: &str) -> bool {
    if host == "*" {
        return true;
    }
    let host = host.strip_prefix("*.").unwrap_or(host);
    !host.is_empty()
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
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
    /// Vetted unpacked extension directories from the curated registry.
    pub extensions: Vec<PathBuf>,
    /// Registry used to vet the extension directories.
    pub extension_registry: Option<PathBuf>,
    /// Whether Power Mode sideload entries were included.
    pub extension_power_mode: bool,
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
        Self::new_with_widevine_and_extensions(
            runtime,
            widevine,
            &detect_extension_registry(configured_extension_registry()),
            bridge_bin,
        )
    }

    /// Create a process launch plan for the runtime, optional CDM, extensions, and bridge.
    #[must_use]
    pub fn new_with_widevine_and_extensions(
        runtime: &CefRuntime,
        widevine: &WidevineCdm,
        extensions: &CefExtensionRegistry,
        bridge_bin: impl AsRef<Path>,
    ) -> Option<Self> {
        Self::new_with_widevine_extensions_and_power_mode(
            runtime,
            widevine,
            extensions,
            extension_power_mode_enabled(),
            bridge_bin,
        )
    }

    /// Create a process launch plan with an explicit extension Power Mode gate.
    #[must_use]
    pub fn new_with_widevine_extensions_and_power_mode(
        runtime: &CefRuntime,
        widevine: &WidevineCdm,
        extensions: &CefExtensionRegistry,
        extension_power_mode: bool,
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
            extensions: extension_dirs(extensions, extension_power_mode),
            extension_registry: extension_registry_path(extensions),
            extension_power_mode,
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
            "CEF_LAUNCH root={} bridge={} lib={} resources={} widevine={} extensions={} ld_library_path={}",
            self.root.display(),
            self.bridge_bin.display(),
            self.libcef.display(),
            self.resources_dir.display(),
            widevine,
            self.extensions.len(),
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
            (
                CEF_EXTENSION_POWER_MODE_ENV,
                OsString::from(if self.extension_power_mode {
                    "true"
                } else {
                    "false"
                }),
            ),
        ];
        if let Some((root, lib)) = &self.widevine {
            env.push((CEF_BRIDGE_WIDEVINE_ROOT_ENV, root.clone().into_os_string()));
            env.push((CEF_BRIDGE_WIDEVINE_LIB_ENV, lib.clone().into_os_string()));
        }
        if !self.extensions.is_empty() {
            let joined = self
                .extensions
                .iter()
                .map(|path| path.to_string_lossy())
                .collect::<Vec<_>>()
                .join(",");
            env.push((CEF_BRIDGE_EXTENSIONS_ENV, OsString::from(joined)));
            if let Some(registry) = &self.extension_registry {
                env.push((
                    CEF_BRIDGE_EXTENSION_REGISTRY_ENV,
                    registry.clone().into_os_string(),
                ));
            }
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

fn extension_dirs(registry: &CefExtensionRegistry, power_mode: bool) -> Vec<PathBuf> {
    match registry {
        CefExtensionRegistry::Available { extensions, .. } => extensions
            .iter()
            .filter(|entry| power_mode || !entry.power_sideload)
            .map(|entry| entry.path.clone())
            .collect(),
        CefExtensionRegistry::Missing { .. } | CefExtensionRegistry::Invalid { .. } => Vec::new(),
    }
}

fn extension_registry_path(registry: &CefExtensionRegistry) -> Option<PathBuf> {
    match registry {
        CefExtensionRegistry::Available { registry, .. } => Some(registry.clone()),
        CefExtensionRegistry::Missing { .. } | CefExtensionRegistry::Invalid { .. } => None,
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
        let ext_dir = dir.join("lastpass");
        std::fs::create_dir_all(&ext_dir).expect("mkdir extension");
        write_extension_manifest(&ext_dir, "LastPass", "4.130.0", &["storage"]);
        let registry = CefExtensionRegistry::Available {
            registry: dir.join("allowlist.env"),
            extensions: vec![AllowedExtension {
                id: "hdokiejnpimakedhajhdlcegeplioahd".to_owned(),
                name: "LastPass".to_owned(),
                version: "4.130.0".to_owned(),
                path: ext_dir.clone(),
                permissions: vec!["storage".to_owned()],
                power_sideload: true,
            }],
        };
        let plan = CefLaunchPlan::new_with_widevine_extensions_and_power_mode(
            &rt, &widevine, &registry, true, &bridge,
        )
        .expect("launch plan");
        assert_eq!(plan.release_dir, dir.join(CEF_RELEASE_DIR));
        assert_eq!(plan.resources_dir, dir.join(CEF_RESOURCES_DIR));
        assert_eq!(plan.extensions, vec![ext_dir.clone()]);
        assert_eq!(plan.extension_registry, Some(dir.join("allowlist.env")));
        assert!(plan.extension_power_mode);
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
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_BRIDGE_EXTENSIONS_ENV && value == &ext_dir.clone().into_os_string()
        }));
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_BRIDGE_EXTENSION_REGISTRY_ENV
                && value == &dir.join("allowlist.env").into_os_string()
        }));
        assert!(env.iter().any(|(key, value)| {
            *key == CEF_EXTENSION_POWER_MODE_ENV && value == &OsString::from("true")
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

    #[test]
    fn packaged_webextension_smoke_fixture_is_vetted_and_marker_backed() {
        let root = repo_root().join("packaging/browser");
        let registry =
            std::fs::read_to_string(root.join("webextensions-smoke.env")).expect("smoke registry");
        let entries = parse_extension_registry(&registry, &root).expect("smoke registry parses");
        assert_eq!(entries.len(), 1);
        let smoke = &entries[0];
        assert_eq!(smoke.id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(smoke.name, "MCNF CEF Extension Smoke");
        assert!(smoke.power_sideload);
        assert_eq!(smoke.path, root.join("smoke-extension"));
        assert!(smoke.permissions.contains(&"<all_urls>".to_owned()));

        let manifest = std::fs::read_to_string(root.join("smoke-extension/manifest.json"))
            .expect("smoke manifest");
        assert!(manifest.contains(r#""manifest_version": 2"#));
        assert!(manifest.contains(r#""<all_urls>""#));
        assert!(manifest.contains(r#""smoke.js""#));

        let script =
            std::fs::read_to_string(root.join("smoke-extension/smoke.js")).expect("smoke script");
        assert!(script.contains("mde-cef-extension-smoke-marker"));
        assert!(script.contains("mde-cef-extension-smoke-ok"));
        assert!(script.contains("mde-cef-extension-autofill-ok"));
        assert!(script.contains("mde-cef-extension-smoke-user"));
        assert!(script.contains("mde-cef-extension-smoke-pass"));
        assert!(script.contains("data-mde-cef-extension-autofilled"));
        assert!(script.contains("/mde-cef-extension-smoke?marker=ok&autofill="));
        assert!(!script.contains("<script"));

        let runner = std::fs::read_to_string(
            repo_root().join("install-helpers/browser-cef-webextension-smoke.sh"),
        )
        .expect("smoke runner");
        assert!(runner.contains("mde-cef-extension-autofill-ok"));
        assert!(runner.contains("ReuseTcpServer"));
        assert!(runner.contains("CEF_EXTENSION_AUTOFILL_SMOKE_READY"));
        assert!(runner.contains("CEF_EXTENSIONS_WINDOWLESS_ALLOY_GATED"));
        assert!(runner.contains("MDE_CEF_ALLOW_ALLOY_EXTENSION_SMOKE"));
    }

    #[test]
    fn webextension_registry_accepts_vetted_unpacked_entries_only() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-registry-{}",
            std::process::id()
        ));
        let lastpass = dir.join("lastpass");
        let ublock = dir.join("ublock-origin");
        std::fs::create_dir_all(&lastpass).expect("mkdir lastpass");
        std::fs::create_dir_all(&ublock).expect("mkdir ublock");
        write_extension_manifest(
            &lastpass,
            "LastPass",
            "4.130.0",
            &["storage", "tabs", "webRequest", "<all_urls>"],
        );
        write_extension_manifest(
            &ublock,
            "uBlock Origin",
            "1.60.0",
            &["webRequest", "webRequestBlocking", "storage"],
        );
        let text = r#"
            # mesh-hosted vetted registry
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,tabs,webRequest,<all_urls>
            power_sideload = true

            [extension.cjpalhdlnbpafiamejdnhcphjbkeiagm]
            name = "uBlock Origin"
            version = "1.60.0"
            path = "ublock-origin"
            permissions = webRequest,webRequestBlocking,storage
        "#;
        let entries = parse_extension_registry(text, &dir).expect("registry parses");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "cjpalhdlnbpafiamejdnhcphjbkeiagm");
        assert_eq!(entries[0].path, ublock);
        assert_eq!(entries[1].id, "hdokiejnpimakedhajhdlcegeplioahd");
        assert!(entries[1].power_sideload);
        assert!(entries[1].permissions.contains(&"<all_urls>".to_owned()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn webextension_registry_rejects_unsafe_ids_paths_and_missing_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-registry-bad-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let bad_id = r#"
            [extension.not-a-chrome-id]
            name = "Bad"
            version = "1"
            path = "bad"
        "#;
        assert!(parse_extension_registry(bad_id, &dir).is_err());

        let bad_path = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "Bad"
            version = "1"
            path = "../bad"
        "#;
        assert!(parse_extension_registry(bad_path, &dir).is_err());

        let comma_path = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "Bad"
            version = "1"
            path = "bad,path"
        "#;
        assert!(parse_extension_registry(comma_path, &dir).is_err());

        let missing_dir = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "Missing"
            version = "1"
            path = "missing"
        "#;
        assert!(parse_extension_registry(missing_dir, &dir).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detected_webextension_registry_reports_ready_but_runtime_smoke_pending() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-detect-{}",
            std::process::id()
        ));
        let ext = dir.join("lastpass");
        std::fs::create_dir_all(&ext).expect("mkdir ext");
        write_extension_manifest(&ext, "LastPass", "4.130.0", &["storage", "tabs"]);
        let registry = dir.join("allowlist.env");
        std::fs::write(
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
        .expect("write registry");
        let detected = detect_extension_registry(&registry);
        assert!(detected.is_available());
        assert!(detected.status_line().contains("CEF_EXTENSIONS_READY"));
        let gate = detected
            .runtime_gate_line()
            .expect("available registry has a runtime gate");
        assert!(gate.contains("CEF_EXTENSIONS_UNPROVEN"));
        assert!(gate.contains("live_extension_runtime_smoke_pending"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn extension_launch_plan_requires_power_mode_for_sideload_entries() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-power-mode-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir release");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("mkdir resources");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        let normal = dir.join("normal-extension");
        let sideload = dir.join("lastpass");
        std::fs::create_dir_all(&normal).expect("mkdir normal");
        std::fs::create_dir_all(&sideload).expect("mkdir sideload");
        write_extension_manifest(&normal, "Normal", "1.0.0", &["storage"]);
        write_extension_manifest(&sideload, "LastPass", "4.130.0", &["storage"]);
        let registry = CefExtensionRegistry::Available {
            registry: dir.join("allowlist.env"),
            extensions: vec![
                AllowedExtension {
                    id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    name: "Normal".to_owned(),
                    version: "1.0.0".to_owned(),
                    path: normal.clone(),
                    permissions: vec!["storage".to_owned()],
                    power_sideload: false,
                },
                AllowedExtension {
                    id: "hdokiejnpimakedhajhdlcegeplioahd".to_owned(),
                    name: "LastPass".to_owned(),
                    version: "4.130.0".to_owned(),
                    path: sideload.clone(),
                    permissions: vec!["storage".to_owned()],
                    power_sideload: true,
                },
            ],
        };
        let runtime = detect_runtime(&dir);
        let widevine = detect_widevine(dir.join("widevine"));

        let gated = CefLaunchPlan::new_with_widevine_extensions_and_power_mode(
            &runtime,
            &widevine,
            &registry,
            false,
            dir.join("bridge"),
        )
        .expect("launch plan");
        assert_eq!(gated.extensions, vec![normal.clone()]);
        assert!(!gated.extension_power_mode);
        assert!(registry
            .power_mode_gate_line(false)
            .expect("power gate")
            .contains("CEF_EXTENSIONS_POWER_GATED"));

        let power = CefLaunchPlan::new_with_widevine_extensions_and_power_mode(
            &runtime,
            &widevine,
            &registry,
            true,
            dir.join("bridge"),
        )
        .expect("launch plan");
        assert_eq!(power.extensions, vec![normal, sideload]);
        assert!(registry.power_mode_gate_line(true).is_none());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_webextension_registry_is_an_honest_gate() {
        let detected = detect_extension_registry("/definitely/not/mde-webextensions.env");
        assert!(!detected.is_available());
        assert!(detected.status_line().contains("CEF_EXTENSIONS_MISSING"));
        assert!(detected.runtime_gate_line().is_none());
    }

    #[test]
    fn webextension_registry_rejects_manifest_mismatch_and_undeclared_permissions() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-manifest-bad-{}",
            std::process::id()
        ));
        let ext = dir.join("lastpass");
        std::fs::create_dir_all(&ext).expect("mkdir ext");
        write_extension_manifest(&ext, "LastPass", "4.130.0", &["storage"]);

        let version_mismatch = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.129.0"
            path = "lastpass"
            permissions = storage
        "#;
        assert!(parse_extension_registry(version_mismatch, &dir).is_err());

        let undeclared_permission = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,tabs
        "#;
        assert!(parse_extension_registry(undeclared_permission, &dir).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn webextension_registry_enforces_chrome_permission_shapes() {
        let dir = std::env::temp_dir().join(format!(
            "mde-web-cef-extension-permission-policy-{}",
            std::process::id()
        ));
        let ext = dir.join("lastpass");
        std::fs::create_dir_all(&ext).expect("mkdir ext");
        std::fs::write(
            ext.join("manifest.json"),
            r#"{"manifest_version":3,"name":"LastPass","version":"4.130.0","permissions":["storage","tabs"],"host_permissions":["https://*.example.com/*","<all_urls>"],"optional_permissions":["webRequest"]}"#,
        )
        .expect("manifest");

        let accepted = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,tabs,webRequest,https://*.example.com/*,<all_urls>
        "#;
        parse_extension_registry(accepted, &dir).expect("chrome permission tokens accepted");

        let arbitrary_permission = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,totallyFakePermission
        "#;
        let err = parse_extension_registry(arbitrary_permission, &dir)
            .expect_err("arbitrary permission rejected");
        assert!(err.contains("supported Chrome permission"), "{err}");

        let malformed_host_pattern = r#"
            [extension.hdokiejnpimakedhajhdlcegeplioahd]
            name = "LastPass"
            version = "4.130.0"
            path = "lastpass"
            permissions = storage,https://example.com
        "#;
        let err = parse_extension_registry(malformed_host_pattern, &dir)
            .expect_err("malformed host pattern rejected");
        assert!(err.contains("supported Chrome permission"), "{err}");

        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_extension_manifest(path: &Path, name: &str, version: &str, permissions: &[&str]) {
        let permissions = permissions
            .iter()
            .map(|permission| format!("\"{permission}\""))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(
            path.join("manifest.json"),
            format!(
                "{{\"manifest_version\":3,\"name\":\"{name}\",\"version\":\"{version}\",\"permissions\":[{permissions}]}}"
            ),
        )
        .expect("write extension manifest");
    }
}
