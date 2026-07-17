//! The **Browser** surface — first-party browser chrome rendered egui-native.
//!
//! The Browser bridge brokers an out-of-process page engine *into* the one shell
//! (the same EMBED model as the VDI Desktop surface): the engine renders offscreen
//! into a shared-memory frame; [`mde_web_preview_client`] receives that frame fd
//! over the per-session socket, maps it read-only, and hands the shell an
//! [`egui::ColorImage`]. This panel uploads that image to a `TextureHandle` on a
//! paint-ready (never a per-frame re-upload), paints it as the body, wires the
//! navigation chrome (back / forward / reload / address bar, §4 tokens) to the
//! control socket, maps page pointer positions into frame device pixels, and
//! forwards page-owned keyboard/text/wheel input over the engine control socket.
//!
//! ```text
//!   session.take_frame() ─▶ ColorImage ─▶ TextureHandle ─▶ ui paints the body
//!   chrome + ui.input     ───────────────────────────────▶ session control/input
//! ```
//!
//! Each tab is an independent [`WebSession`], so one page crashing surfaces an
//! honest "page crashed" state for THAT tab only (respawn-on-reload) and never
//! touches the others (per-session isolation). Spawning a live page engine is the
//! client crate's `live-helper` path, honest-gated to a GPU seat; with no live
//! session attached this surface shows an honest gated `EmptyState`, never a fake
//! page (§7).

use base64::Engine as _;
use mackes_mesh_types::peers::default_workgroup_root;
use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use mde_chat::{MessageKind, Severity};
use mde_editor_egui::spell::{self, SpellMiss};
use mde_egui::egui::{self, TextureHandle, TextureOptions};
use mde_egui::{ChipTone, Style};
use mde_files_egui::model::FileSearchTarget;
use mde_files_egui::transfers::{
    FileTransfers, Method as TransferMethod, TransferJob, TransferPolicy, TransferState,
    TransferVerb, TransfersClient,
};

use mde_web_preview_client::{
    host_of, BeforeUnloadDialog, CertError, FilterListSource, FilterListStore, JsDialog,
    LoginCaptureStatus, ManagedUrlPolicy, RequestFilter, SafeBrowsingBlocklist, SessionState,
    WebSession,
};
use qrcode::QrCode;
use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

use mde_egui::search_omnibox::{ranked_hits, SearchDomain, SearchItem};

// ── live-helper: spawning the real sandboxed `mde-web-preview` helper ──────────
//
// Gated behind `mde-shell-egui`'s `live-helper` feature, which turns on the client
// crate's `live-helper` spawn API ([`WebSession::spawn`] + [`SpawnSpec`]). OFF by
// default so the shell stays portable and the Browser surface shows its honest
// gated EmptyState (§7); ON, the surface spawns the sandboxed helper on first open.
#[cfg(feature = "live-helper")]
use mde_web_preview_client::session::SpawnSpec;

/// The Servo page-engine binary the RPM installs; overridable via
/// [`SERVO_HELPER_BIN_ENV`] for the test bed / dev builds.
#[cfg(feature = "live-helper")]
const DEFAULT_SERVO_HELPER_BIN: &str = "/usr/bin/mde-web-preview";

/// The env var overriding [`DEFAULT_SERVO_HELPER_BIN`] (test bed / dev builds).
#[cfg(feature = "live-helper")]
const SERVO_HELPER_BIN_ENV: &str = "MDE_WEB_PREVIEW_BIN";

/// The Chromium/CEF helper binary once BROWSER-DD-1 is packaged.
#[cfg(feature = "live-helper")]
const DEFAULT_CEF_HELPER_BIN: &str = "/usr/bin/mde-web-cef";

/// The env var overriding [`DEFAULT_CEF_HELPER_BIN`] (test bed / dev builds).
#[cfg(feature = "live-helper")]
const CEF_HELPER_BIN_ENV: &str = "MDE_WEB_CEF_BIN";

/// CEF process env toggled from Browser Power Mode. The helper forwards this to
/// the native renderer bridge before Chromium switches are assembled.
#[cfg(feature = "live-helper")]
const CEF_BROWSER_POWER_MODE_ENV: &str = "MDE_CEF_BROWSER_POWER_MODE";

/// Existing CEF extension gate env; Browser Power Mode also unlocks the curated
/// sideload entries for newly spawned CEF helpers.
#[cfg(feature = "live-helper")]
const CEF_EXTENSION_POWER_MODE_ENV: &str = "MDE_CEF_EXTENSION_POWER_MODE";

const CEF_DEVTOOLS_URL: &str = "http://127.0.0.1:9222/";
const CEF_DEVTOOLS_LIST_URL: &str = "http://127.0.0.1:9222/json/list";
const CEF_DEVTOOLS_TIMEOUT: Duration = Duration::from_millis(450);

pub(super) fn browser_product_label() -> String {
    let codename = mde_theme::brand::build::info().codename;
    if codename.is_empty() {
        format!("{} Browser", mde_theme::brand::logo::PRODUCT_NAME)
    } else {
        format!("{codename} Browser")
    }
}

/// Environment variable pointing at a pinned CEF bundle root (mirrors
/// `mde-web-cef`; duplicated here so the shell can gate honestly without
/// depending on the excluded helper crate).
#[cfg(feature = "live-helper")]
const CEF_ROOT_ENV: &str = "MDE_CEF_ROOT";

/// Conventional farm/vendor path for the pinned CEF bundle.
#[cfg(feature = "live-helper")]
const DEFAULT_CEF_ROOT: &str = "/opt/mde/cef";

/// The runtime library expected under the CEF bundle.
#[cfg(feature = "live-helper")]
const CEF_LIB_NAME: &str = "libcef.so";

/// CEF binary distributions install their runtime library under `Release/`.
#[cfg(feature = "live-helper")]
const CEF_RELEASE_DIR: &str = "Release";

/// CEF binary distributions install pak/ICU resources under `Resources/`.
#[cfg(feature = "live-helper")]
const CEF_RESOURCES_DIR: &str = "Resources";

#[cfg(feature = "live-helper")]
const CEF_ICU_DATA: &str = "icudtl.dat";

#[cfg(feature = "live-helper")]
const CEF_RESOURCES_PAK: &str = "resources.pak";

/// The native new-tab URL. A real helper session still loads this, while the shell
/// overlays the Quazar dashboard chrome for it.
const NEW_TAB_URL: &str = "about:blank";

/// The first page a freshly spawned live tab loads.
#[cfg(feature = "live-helper")]
const START_URL: &str = NEW_TAB_URL;

/// Browser-owned internal options page. This URL is never sent to a helper; it
/// lives as tab metadata and renders in `active_body` before page pixels.
const BROWSER_OPTIONS_URL: &str = "mde://browser/options";

/// DD-9's browser media policy is block-all autoplay by default. The helpers own
/// the engine-specific shim; the shell owns the per-tab policy bit and mirrors it
/// into every fresh helper session.
const DEFAULT_AUTOPLAY_BLOCKED: bool = true;

/// Front-door privacy explainer shown on the new-tab dashboard — the browser is
/// private by design (no persistent profile: the sandbox has no writable `$HOME`),
/// so history and cookies never outlive the session.
const PRIVATE_MODE_EXPLAINER: &str =
    "Private by default: history and cookies clear when you close the browser";
#[cfg(feature = "live-helper")]
const NO_GPU_SEAT_NOTICE: &str =
    "The Browser needs a GPU seat to open a live page; none is available here.";

/// The fallback helper view geometry (device px) when no live seat size is known
/// yet (hermetic tests, first frame before the seat is probed). A live spawn
/// pre-sizes to the seat instead — see [`WebState::note_seat_px`].
#[cfg(feature = "live-helper")]
const INIT_W: u32 = 1280;
#[cfg(feature = "live-helper")]
const INIT_H: u32 = 800;

/// A per-axis ceiling (device px) for the pre-sized frame channel and for any live
/// resize target (browser-1). The shm frame region is `w * h * 4` bytes, so this
/// bounds one tab's channel to ~64 MiB even on an oversized seat; 4096 covers 4K
/// UHD (3840×2160) at native 1:1, and a larger seat paints clamped — still
/// click-correct via [`map_pointer_to_frame`], just gently upscaled.
const MAX_CHANNEL_DIM: u32 = 4096;

const CHROME_FONT: f32 = 10.0;
const CHROME_BUTTON: f32 = 20.0;
const CHROME_TAB_H: f32 = 22.0;
const CHROME_TAB_W: f32 = 132.0;
const CHROME_TAB_RAIL_W: f32 = 160.0;
/// The floor a horizontal tab pill shrinks to once the strip is crowded. Below
/// this the strip stops shrinking and scrolls horizontally instead of wrapping
/// onto a second row (the standard desktop-browser overflow behaviour).
const CHROME_TAB_MIN_W: f32 = 54.0;
/// The fixed, compact width of a pinned tab's pill (favicon only, no title, no ×) —
/// Chrome's pinned tabs collapse to an icon. Constant so pinned tabs never shrink
/// under the crowded-strip overflow the way unpinned pills do.
const CHROME_TAB_PINNED_W: f32 = 24.0;
const CHROME_TAB_CLOSE: f32 = 18.0;
const CHROME_NEW_TAB_W: f32 = 58.0;
const CHROME_OMNIBOX_H: f32 = 22.0;
const CHROME_GAP: f32 = 2.0;
const DEFAULT_VERTICAL_TABS: bool = true;
const DEFAULT_DENIED_PERMISSIONS: &str = "location, camera, microphone, notifications, clipboard";
const PAGE_ZOOM_MIN: u16 = 50;
const PAGE_ZOOM_MAX: u16 = 200;
const PAGE_ZOOM_STEP: u16 = 10;
const CUPS_PRINT_TIMEOUT: Duration = Duration::from_secs(5);

mod engine_runtime;
use engine_runtime::*;

/// The browser body is scaled to fill the surface, so sample it linearly.
const BROWSER_TEX: TextureOptions = TextureOptions::LINEAR;

/// One browser tab: its driven session and the GPU texture its frames upload into.
/// A named, colored tab group (Chrome-style): every tab carrying its index renders
/// a colored strip and can be operated on as a set (close-group). Session-only.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TabGroup {
    name: String,
    color: egui::Color32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserInternalPage {
    Options,
}

impl BrowserInternalPage {
    const fn url(self) -> &'static str {
        match self {
            Self::Options => BROWSER_OPTIONS_URL,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Options => "Browser Options",
        }
    }

    fn from_url(url: &str) -> Option<Self> {
        url.trim()
            .eq_ignore_ascii_case(BROWSER_OPTIONS_URL)
            .then_some(Self::Options)
    }
}

/// A distinct Browser-local group color, cycled by group index over the chrome
/// Material palette so successive groups are visually separable.
fn tab_group_color(index: usize) -> egui::Color32 {
    chrome_ui::tab_group_color(index)
}

struct Tab {
    /// Stable shell-local identity for tab-scoped prompt state. Indices move when
    /// tabs close or reorder; this id does not.
    id: u64,
    /// The IPC + shm session driving one sandboxed helper.
    session: WebSession,
    /// Engine that owns this helper session.
    engine: BrowserEngine,
    /// Browser-owned internal page rendered by the shell instead of page pixels.
    internal_page: Option<BrowserInternalPage>,
    /// The peer end of an inert local session socket used only to satisfy the
    /// tab's existing `WebSession` storage while an internal page is active.
    internal_peer: Option<UnixStream>,
    /// Named container identity for the tab. Helpers are already one session per
    /// tab; this records the user-facing isolation bucket in the chrome.
    container: ContainerProfile,
    /// Browser UX intent for where this tab should land once the compositor-side
    /// multi-display handoff is wired. This is per-tab chrome state, not a fake
    /// output move.
    display_target: DisplayTarget,
    /// Tab-group membership — an index into [`WebState::tab_groups`], or `None` when
    /// the tab is ungrouped. Grouped tabs render a colored strip.
    group: Option<usize>,
    /// Whether the tab is pinned. Pinned tabs cluster at the front of the strip
    /// (the [`WebState::sort_pinned_stable`] invariant), render compact (favicon
    /// only, no title, no inline close), and survive a stray click on the ×
    /// (Chrome's pinned-tab affordance). Session-only, like every other tab bit.
    pinned: bool,
    /// Per-tab audio mute state mirrored to the helper.
    muted: bool,
    /// Per-tab autoplay-block state mirrored to the helper.
    autoplay_blocked: bool,
    /// Per-tab forced dark rendering state mirrored to the helper.
    force_dark: bool,
    /// Per-tab reader-mode state mirrored to the helper.
    reader_mode: bool,
    /// Per-tab built-in userscript-library state mirrored to the helper.
    user_scripts: bool,
    /// Per-tab page-visible User-Agent override mirrored to the helper.
    user_agent: UserAgentOverride,
    /// Per-tab page-visible device profile override mirrored to the helper.
    device_profile: DeviceProfile,
    /// Last operator/page activity seen by the shell for idle-suspend accounting.
    last_activity: Instant,
    /// Whether this inactive tab has been shell-suspended after the idle timeout.
    idle_suspended: bool,
    /// Whether the painted page canvas owns keyboard/text input. This is tracked
    /// per tab instead of relying only on egui response focus, which can be lost
    /// when chrome widgets rebuild between frames.
    page_focused: bool,
    /// The body texture — allocated on the first frame, then updated in place with
    /// [`TextureHandle::set`] on each subsequent paint-ready (egui reuses the
    /// allocation, so a live page is not a per-frame upload churn).
    texture: Option<TextureHandle>,
    /// Last helper frame retained on the CPU side for viewport capture. The GPU
    /// texture is not readable, so capture uses this exact pre-upload image.
    /// Held behind an `Arc` so retaining it costs a refcount bump, not a
    /// full-resolution pixel deep copy, on every decoded frame (the same `Arc`
    /// is handed to the texture upload — `egui::ImageData` stores `Arc<ColorImage>`).
    last_frame: Option<std::sync::Arc<egui::ColorImage>>,
    /// Last shell-local resource-request sequence audited from this tab's session.
    /// `recent_resource_requests()` is a bounded snapshot, so this watermark keeps
    /// pre-network resource-block audit events one-shot without changing helper wire.
    last_audited_resource_seq: u64,
    /// Last TLS/certificate block audited for this tab. The engine stores the
    /// current cert error until a fresh load clears it, so this prevents per-frame
    /// duplicate Bus events while still auditing a repeated block after navigation.
    last_audited_cert_error: Option<CertError>,
    /// Debounces panel-size changes into a single settled `session.resize` so a
    /// drag-resize drives the helper's CSS viewport once, not every frame
    /// (browser-1).
    resizer: ViewportResizer,
    /// The decoded favicon texture cache for this tab, keyed to the fingerprint of
    /// the [`WebSession::favicon`] PNG bytes it was built from — a favicon is
    /// PNG-decoded once per distinct set of bytes, not every frame (§Q7 bound).
    /// `None` until the page reports its first favicon; `Some` with an inner
    /// `texture: None` records "these exact bytes failed to decode" so a
    /// malformed favicon isn't retried every frame either.
    favicon_cache: Option<FaviconCache>,
}

/// One tab's decoded-favicon cache slot. See [`Tab::favicon_cache`].
#[derive(Clone)]
struct FaviconCache {
    /// A cheap hash of the PNG bytes the cached texture was decoded from.
    fingerprint: u64,
    /// The uploaded texture, or `None` when those bytes failed to PNG-decode.
    texture: Option<TextureHandle>,
}

/// How long a new panel device size must hold steady before it is committed to the
/// helper as a `session.resize` — long enough that a drag-resize sends ONE settled
/// resize instead of one per frame, short enough to feel immediate.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Active live Browser pages must keep the DRM loop waking even without pointer
/// input; otherwise video/canvas pages only advance when another event arrives.
const LIVE_PAGE_REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// Debounces browser-panel viewport-size changes (browser-1, item 2).
///
/// The helper's page CSS viewport should track the real panel, but re-sending a
/// resize every frame during a window drag would thrash the engine's relayout. So
/// this tracks the last size actually committed to the helper plus a *candidate*
/// that must hold steady for [`RESIZE_DEBOUNCE`] before it is committed. A size
/// that flickers back to the committed value cancels the pending change; a no-op
/// frame (size unchanged) never resizes.
#[derive(Debug, Clone, Default, PartialEq)]
struct ViewportResizer {
    /// The size last committed to the helper (`None` = nothing sent yet).
    sent: Option<(u32, u32)>,
    /// A pending candidate size and the instant it was first observed.
    pending: Option<((u32, u32), Instant)>,
}

impl ViewportResizer {
    /// Fold this frame's `target` device size at time `now`. Returns `Some(size)`
    /// exactly once — on the frame a *changed* size settles (held ≥ `debounce`) —
    /// and `None` otherwise (unchanged, or still settling).
    fn observe(
        &mut self,
        target: (u32, u32),
        now: Instant,
        debounce: Duration,
    ) -> Option<(u32, u32)> {
        if self.sent == Some(target) {
            // Already at this size — cancel any pending change back to it.
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((sz, since)) if sz == target => {
                if now.duration_since(since) >= debounce {
                    self.sent = Some(target);
                    self.pending = None;
                    return Some(target);
                }
            }
            // First sighting of a new candidate (or the candidate just changed):
            // (re)start its debounce clock.
            _ => self.pending = Some((target, now)),
        }
        None
    }

    /// Whether a change is still settling — the panel should keep repainting so the
    /// debounce fires even with no further input.
    const fn is_settling(&self) -> bool {
        self.pending.is_some()
    }
}

mod device_profile;
use device_profile::*;

mod site_data;
use site_data::*;
mod history;
use history::*;

mod printing;
use printing::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SavedPdf {
    path: PathBuf,
    url: String,
    title: String,
}

/// A CEF-intercepted download whose filename tripped [`download_is_dangerous`],
/// parked pending the user's explicit Keep/Discard choice (the downloads
/// drawer's "this type of file can harm your device" banner). Only one is
/// parked at a time — a second dangerous interception before the first is
/// resolved simply replaces it, matching the single-slot `insecure_prompt` /
/// `pending_saved_pdfs`-style gates already used elsewhere in this surface.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingDangerousDownload {
    id: u64,
    url: String,
    filename: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedPolicyBlockTrigger {
    ChromeLoad,
    NewTab,
    HttpContinue,
    HttpsUpgrade,
    HelperDocument,
    #[cfg(feature = "live-helper")]
    LiveSpawn,
    Download,
}

impl ManagedPolicyBlockTrigger {
    const fn wire(self) -> &'static str {
        match self {
            Self::ChromeLoad => "chrome_load",
            Self::NewTab => "new_tab",
            Self::HttpContinue => "http_continue",
            Self::HttpsUpgrade => "https_upgrade",
            Self::HelperDocument => "helper_document",
            #[cfg(feature = "live-helper")]
            Self::LiveSpawn => "live_spawn",
            Self::Download => "download",
        }
    }
}

/// Where a pending plain-HTTP navigation should resume after the user answers
/// the Browser HTTPS prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InsecureNavigationTarget {
    ActiveTab,
    NewTab(BrowserEngine),
}

impl InsecureNavigationTarget {
    const fn wire(self) -> &'static str {
        match self {
            Self::ActiveTab => "active_tab",
            Self::NewTab(_) => "new_tab",
        }
    }

    const fn engine_override(self) -> Option<BrowserEngine> {
        match self {
            Self::ActiveTab => None,
            Self::NewTab(engine) => Some(engine),
        }
    }
}

/// A top-level navigation blocked by operator-managed Browser policy. Stored in
/// shell state so chrome-originated loads (omnibox, bookmarks, send-tab, restore)
/// can paint the same full-page interstitial as helper-originated page clicks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ManagedPolicyBlock {
    url: String,
    rule: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MixedContentBlockAudit {
    engine: BrowserEngine,
    page_url: String,
    title: String,
    url: String,
    resource: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CertificateErrorAudit {
    engine: BrowserEngine,
    title: String,
    error: CertError,
}

#[derive(Default)]
struct BrowserTabPollEvents {
    pdf_events: Vec<(String, bool)>,
    page_text_events: Vec<(u64, String)>,
    page_scrape_events: Vec<(u64, String)>,
    passkey_events: Vec<(u64, BrowserEngine, String)>,
    js_dialog_events: Vec<(usize, JsDialog)>,
    popup_opens: Vec<(BrowserEngine, String)>,
    download_submits: Vec<(u64, String, String)>,
    login_captures: Vec<(u64, LoginCaptureStatus)>,
    mixed_content_blocks: Vec<MixedContentBlockAudit>,
    certificate_errors: Vec<CertificateErrorAudit>,
}

/// One saved login in the SESSION-ONLY credential store. Keyed by normalized host
/// so a revisit offers it for autofill. In-memory only — never persisted to disk
/// (the browser is private-by-default; the sandbox has no writable $HOME). The
/// user adds these through the password menu or by accepting a host-bound
/// auto-capture prompt after a submitted login.
#[derive(Clone, PartialEq, Eq)]
struct StoredLogin {
    /// Host the credential belongs to (e.g. `mail.example.com`).
    host: String,
    /// Username / email.
    username: String,
    /// Password (session RAM only).
    password: String,
}

impl std::fmt::Debug for StoredLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredLogin")
            .field("host", &self.host)
            .field("username", &"<redacted>")
            .field("username_bytes", &self.username.len())
            .field("password", &"<redacted>")
            .field("password_bytes", &self.password.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PendingLoginSave {
    tab_id: u64,
    host: String,
    username: String,
    password: String,
}

impl std::fmt::Debug for PendingLoginSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingLoginSave")
            .field("tab_id", &self.tab_id)
            .field("host", &self.host)
            .field("username", &"<redacted>")
            .field("username_bytes", &self.username.len())
            .field("password", &"<redacted>")
            .field("password_bytes", &self.password.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingPasskeyConsent {
    tab_id: u64,
    engine: BrowserEngine,
    handoff_body: String,
    client_request_id: String,
    ceremony: String,
    origin: String,
    rp_id: String,
    user_name: Option<String>,
}

impl PendingPasskeyConsent {
    fn from_handoff(
        tab_id: u64,
        engine: BrowserEngine,
        handoff_body: String,
        client_request_id: String,
    ) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_str(&handoff_body)
            .map_err(|err| format!("passkey handoff JSON: {err}"))?;
        let field = |key: &str| {
            v.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("passkey handoff missing {key}"))
        };
        let user_name = v
            .get("user_name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let ceremony = field("ceremony")?;
        let origin = field("origin")?;
        let rp_id = field("rp_id")?;
        Ok(Self {
            tab_id,
            engine,
            handoff_body,
            client_request_id,
            ceremony,
            origin,
            rp_id,
            user_name,
        })
    }

    fn verb(&self) -> &'static str {
        if self.ceremony == "create" {
            "create a passkey"
        } else {
            "use a passkey"
        }
    }

    fn display_origin(&self) -> String {
        chrome_ui::origin_label(&self.origin)
    }
}

fn passkey_page_request_notice(err: &str) -> String {
    let lower = err.to_ascii_lowercase();
    let reason = if lower.contains("unsupported ceremony") {
        "this passkey action is not supported"
    } else if lower.contains("json") {
        "the passkey request could not be read"
    } else if lower.contains("missing") {
        "the passkey request was incomplete"
    } else {
        "the passkey request could not be verified"
    };
    format!("Passkey: {reason}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

const fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

const fn plural_u32(count: u32) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

const DOWNLOADS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SEND_TAB_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Startup tab intents spawn live helpers immediately today. Keep session
/// restore and send-tab replay bounded so stale or poisoned state cannot freeze
/// the seat.
#[cfg(any(test, feature = "live-helper"))]
const MAX_EAGER_BROWSER_STARTUP_OPEN_TABS: usize = 8;
const VOICE_COMMAND_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MEDIA_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(200);
const SHARE_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TRANSLATION_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const OFFLINE_CACHE_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SPEECH_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PASSKEY_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(1);
const PASSKEY_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SECURITY_UPDATE_STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SESSION_SNAPSHOT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_TAB_SUSPEND_AFTER: Duration = Duration::from_secs(30 * 60);
const CURATED_USERSCRIPT_COUNT: usize = 100;

// ── BOOKMARKS-BAR: the daemon bookmark-store mirror + single-row chrome layout ──
/// The daemon-retained converged bookmark [`mde_bookmarks::Collection`] topic —
/// the SAME `state/bookmarks/collection` the mackesd bookmarks worker publishes
/// and the Surface::Bookmarks manager hydrates from. The Browser mirrors it into
/// its own bar row (§6 local mirror of a Bus topic, never a mackesd dep).
const STATE_BOOKMARKS_COLLECTION: &str = "state/bookmarks/collection";
/// The bookmark collection is a persisted+synced store, not per-frame chatter, so
/// the bar mirror re-reads on a relaxed cadence (an explicit user act adds one).
const BOOKMARKS_COLLECTION_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// One top-level bookmark projected onto the bar: just the display title and its
/// navigation target. Folded from the daemon [`mde_bookmarks::Collection`]'s
/// render-ordered roots ([`bookmark_bar_links_from`]); the browser never re-derives
/// the CRDT tree — it mirrors the converged leaves it means to click.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BookmarkBarLink {
    /// The display title (falls back to the URL when the stored title is blank so a
    /// bar button always shows something legible).
    title: String,
    /// The navigation target handed to `load_target` / `request_new_tab_with_url`.
    url: String,
}

/// One local file projected into Browser omnibox suggestions.
///
/// The shell supplies these from the Files model; Browser only ranks and commits
/// the already-built `file://` URL, so it does not crawl the filesystem or persist
/// private paths.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserFileSuggestion {
    title: String,
    path: PathBuf,
    url: String,
}

/// Fold a converged [`mde_bookmarks::Collection`] into the bar's top-level bookmark
/// links, in the collection's own render order (`roots()` is order-key sorted).
/// Folders are omitted — the bar is a flat quick-launch strip of the top-level
/// bookmarks, the same subset a browser's bookmarks bar surfaces.
fn bookmark_bar_links_from(collection: &mde_bookmarks::Collection) -> Vec<BookmarkBarLink> {
    collection
        .roots()
        .into_iter()
        .filter_map(|item| match item {
            mde_bookmarks::Item::Bookmark(b) => {
                let title = if b.title.trim().is_empty() {
                    b.url.clone()
                } else {
                    b.title
                };
                Some(BookmarkBarLink { title, url: b.url })
            }
            mde_bookmarks::Item::Folder(_) => None,
        })
        .collect()
}

/// Every bookmark in the collection — top-level AND nested in any folder — as
/// `{title, url}`. Feeds BOTH the toolbar star's bookmarked-state membership (via
/// [`bookmarked_url_set`]) and the omnibox bookmark autocomplete. Walks the tree via
/// [`mde_bookmarks::Collection::children`]; runs once per converged collection
/// update, not per frame. A blank stored title falls back to the URL (as the bar does).
fn all_bookmarks(collection: &mde_bookmarks::Collection) -> Vec<BookmarkBarLink> {
    let mut out = Vec::new();
    let mut folders = vec![None]; // Option<Uuid> stack, seeded with the roots
    while let Some(parent) = folders.pop() {
        for item in collection.children(parent) {
            match item {
                mde_bookmarks::Item::Bookmark(b) => {
                    let title = if b.title.trim().is_empty() {
                        b.url.clone()
                    } else {
                        b.title
                    };
                    out.push(BookmarkBarLink { title, url: b.url });
                }
                mde_bookmarks::Item::Folder(f) => folders.push(Some(f.id)),
            }
        }
    }
    out
}

/// The normalized-URL membership set for the toolbar star, derived from
/// [`all_bookmarks`] (Chrome's star is filled if the page lives in ANY folder).
fn bookmarked_url_set(bookmarks: &[BookmarkBarLink]) -> std::collections::HashSet<String> {
    bookmarks
        .iter()
        .map(|b| bookmark_membership_key(&b.url).to_owned())
        .collect()
}

/// Bookmarks whose title or URL/fuzzy title matches the draft, most-relevant
/// first through the shared omnibox ranker, capped. Powers omnibox bookmark
/// autocomplete — the highest-signal suggestion class, so it renders above history.
fn matching_bookmarks(index: &[BookmarkBarLink], draft: &str, cap: usize) -> Vec<BookmarkBarLink> {
    let items = index.iter().cloned().enumerate().map(|(idx, bookmark)| {
        SearchItem::new(
            SearchDomain::BrowserBookmark,
            bookmark.title.clone(),
            bookmark.url.clone(),
            bookmark,
        )
        .with_source_rank(idx)
    });
    ranked_hits(draft, items, cap)
        .into_iter()
        .map(|hit| hit.item.payload)
        .collect()
}

fn browser_file_suggestion_from_item(
    item: SearchItem<FileSearchTarget>,
) -> Option<BrowserFileSuggestion> {
    let path = item.payload.path?;
    let url = file_url_for_path(&path).ok()?;
    Some(BrowserFileSuggestion {
        title: item.title,
        path,
        url,
    })
}

fn browser_file_suggestions(
    items: impl IntoIterator<Item = SearchItem<FileSearchTarget>>,
) -> Vec<BrowserFileSuggestion> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for item in items {
        let Some(file) = browser_file_suggestion_from_item(item) else {
            continue;
        };
        if seen.insert(file.url.clone()) {
            out.push(file);
        }
    }
    out
}

fn matching_file_suggestions(
    index: &[BrowserFileSuggestion],
    draft: &str,
    cap: usize,
) -> Vec<BrowserFileSuggestion> {
    let items = index.iter().cloned().enumerate().map(|(idx, file)| {
        SearchItem::new(
            SearchDomain::File,
            file.title.clone(),
            file.path.display().to_string(),
            file,
        )
        .with_source_rank(idx)
    });
    ranked_hits(draft, items, cap)
        .into_iter()
        .map(|hit| hit.item.payload)
        .collect()
}

/// Normalize a URL for bookmarked-state membership so a trailing-slash-only
/// difference (`https://x.com` vs `https://x.com/`) still lights the star, matching
/// Chrome's host-root equivalence without a full URL parse.
fn bookmark_membership_key(url: &str) -> &str {
    url.strip_suffix('/').unwrap_or(url)
}

mod userscripts;
use userscripts::*;

const fn download_state_rank(state: TransferState) -> u8 {
    match state {
        TransferState::Running => 0,
        TransferState::Queued => 1,
        TransferState::Paused => 2,
        TransferState::Failed => 3,
        TransferState::Done => 4,
    }
}

/// Extensions Chrome-style download-danger checks flag as potentially harmful:
/// executables, installers, scripts, and the package formats every desktop OS
/// this shell targets would happily run on double-click.
const DANGEROUS_DOWNLOAD_EXTENSIONS: &[&str] = &[
    "exe", "scr", "bat", "cmd", "com", "pif", "msi", "msix", "vbs", "vbe", "js", "jse", "jar",
    "ps1", "wsf", "hta", "cpl", "dll", "lnk", "reg", "sh", "run", "deb", "rpm", "dmg", "pkg",
    "apk", "gadget",
];

/// Pure classifier: does `filename` look like it could harm the device if run?
/// Case-insensitive on the final extension — the one that actually decides how
/// the OS opens the file — plus the second-to-last segment, so a masquerading
/// double extension is caught from either side (`invoice.pdf.exe` *and*
/// `invoice.exe.pdf` both flag, not just the visible-name trick). It also checks
/// percent-decoded, path-leaf, Windows trailing-dot/space, and ADS-style variants
/// so encoded or platform-normalized executable names cannot slip through.
/// Paint-free and side-effect-free so it's directly unit-testable
/// ([`submit_download_to_ledger`] is the only caller that acts on it).
fn download_is_dangerous(filename: &str) -> bool {
    if download_name_variant_is_dangerous(filename) {
        return true;
    }
    let mut current = filename.to_owned();
    for _ in 0..2 {
        let Some(decoded) =
            percent_decode_download_name(&current).filter(|decoded| decoded != &current)
        else {
            return false;
        };
        if download_name_variant_is_dangerous(&decoded) {
            return true;
        }
        current = decoded;
    }
    false
}

fn download_name_variant_is_dangerous(filename: &str) -> bool {
    fn is_dangerous_ext(part: &str) -> bool {
        DANGEROUS_DOWNLOAD_EXTENSIONS.contains(&part.trim().to_ascii_lowercase().as_str())
    }

    let leaf = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim()
        .trim_end_matches(|ch: char| ch == '.' || ch.is_ascii_whitespace());
    if leaf.is_empty() {
        return false;
    }

    if let Some((stream_owner, _)) = leaf.split_once(':') {
        if download_extension_chain_is_dangerous(stream_owner, is_dangerous_ext) {
            return true;
        }
    }
    download_extension_chain_is_dangerous(leaf, is_dangerous_ext)
}

fn download_extension_chain_is_dangerous(
    filename: &str,
    is_dangerous_ext: impl Fn(&str) -> bool,
) -> bool {
    let mut parts: Vec<&str> = filename.split('.').map(str::trim).collect();
    // A leading empty segment is a dotfile's leading dot (`.bashrc`), not an
    // extension boundary.
    while parts.first() == Some(&"") {
        parts.remove(0);
    }
    while parts.last() == Some(&"") {
        parts.pop();
    }
    if parts.len() < 2 {
        return false;
    }
    if is_dangerous_ext(parts[parts.len() - 1]) {
        return true;
    }
    parts.len() >= 3 && is_dangerous_ext(parts[parts.len() - 2])
}

fn percent_decode_download_name(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut decoded_any = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (
                    download_hex_value(bytes[i + 1]),
                    download_hex_value(bytes[i + 2]),
                ) {
                    out.push((hi << 4) | lo);
                    decoded_any = true;
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    decoded_any.then(|| String::from_utf8_lossy(&out).into_owned())
}

const fn download_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn download_url_path_is_dangerous(url: &str) -> bool {
    let path_only = url.split(['?', '#']).next().unwrap_or(url);
    let path = path_only.split_once("://").map_or(path_only, |(_, rest)| {
        rest.find('/').map_or("", |path_start| &rest[path_start..])
    });
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .is_some_and(download_is_dangerous)
}

/// The filename a download should be saved under: the engine's suggested name
/// if it gave one, else derived from the URL's last non-empty path segment
/// (query/fragment stripped FIRST — otherwise a signed link like
/// `…/file.zip?token=x` derives the query as the filename instead of
/// `file.zip`).
fn resolve_download_filename(url: &str, filename: &str) -> String {
    let name = filename.trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let path_only = url.split(['?', '#']).next().unwrap_or(url);
    path_only
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("download")
        .to_owned()
}

trait DownloadOpener {
    fn open_path(&self, path: &Path) -> Result<(), String>;
    fn reveal_path(&self, path: &Path) -> Result<(), String>;
}

#[derive(Debug, Default)]
struct XdgDownloadOpener;

impl DownloadOpener for XdgDownloadOpener {
    fn open_path(&self, path: &Path) -> Result<(), String> {
        spawn_xdg_open(path)
    }

    fn reveal_path(&self, path: &Path) -> Result<(), String> {
        spawn_xdg_open(path)
    }
}

fn spawn_xdg_open(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("download path is empty".to_owned());
    }
    Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("xdg-open {} failed: {err}", path.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserDownloadTarget {
    open: PathBuf,
    reveal: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserDownloadManifest {
    asset_url: String,
    suggested_filename: String,
    kind: Option<String>,
}

fn safe_browser_download_filename(raw: &str) -> String {
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let mut out = String::new();
    let mut last_dash = false;
    for ch in leaf.chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            last_dash = false;
            Some(ch)
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
        if out.len() >= 128 {
            break;
        }
    }
    let out = out.trim_matches(['.', '-', '_']);
    if out.is_empty() {
        "browser-media".to_string()
    } else {
        out.to_string()
    }
}

fn completed_browser_download_target(job: &TransferJob) -> Result<BrowserDownloadTarget, String> {
    if job.method != TransferMethod::BrowserDownload {
        return Err("Download action only supports browser downloads".to_owned());
    }
    if job.state != TransferState::Done {
        return Err("Download is not complete yet".to_owned());
    }
    let source = PathBuf::from(job.source.trim());
    let dest = PathBuf::from(job.dest.trim());
    if source.as_os_str().is_empty() || dest.as_os_str().is_empty() {
        return Err("Download output path unavailable".to_owned());
    }
    if let Some(manifest) = browser_download_manifest(&source)? {
        return Ok(browser_manifest_target(&dest, &manifest));
    }
    let open = if dest.is_dir() {
        let file_name = source
            .file_name()
            .ok_or_else(|| "Download output path unavailable".to_owned())?;
        dest.join(file_name)
    } else {
        dest
    };
    Ok(BrowserDownloadTarget {
        reveal: reveal_target_for(&open),
        open,
    })
}

fn browser_download_manifest(source: &Path) -> Result<Option<BrowserDownloadManifest>, String> {
    if source.extension().and_then(|ext| ext.to_str()) != Some("json")
        || !source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".download.json"))
    {
        return Ok(None);
    }
    let body = std::fs::read(source)
        .map_err(|err| format!("Download request {} is unreadable: {err}", source.display()))?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|err| format!("Download request {} is not JSON: {err}", source.display()))?;
    if value.get("op").and_then(serde_json::Value::as_str) != Some("browser_media_download_request")
    {
        return Ok(None);
    }
    let asset_url = value
        .get("asset_url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        .ok_or_else(|| "Download request is missing a valid asset URL".to_owned())?
        .to_owned();
    let suggested = value
        .get("suggested_filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("browser-media");
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(|kind| kind.to_ascii_lowercase());
    Ok(Some(BrowserDownloadManifest {
        asset_url,
        suggested_filename: safe_browser_download_filename(suggested),
        kind,
    }))
}

fn browser_manifest_target(
    dest: &Path,
    manifest: &BrowserDownloadManifest,
) -> BrowserDownloadTarget {
    if browser_manifest_is_hls(manifest) {
        let (package_dir, manifest_filename) =
            browser_package_destination(dest, &manifest.suggested_filename, "hls");
        return BrowserDownloadTarget {
            open: package_dir.join(manifest_filename),
            reveal: package_dir,
        };
    }
    if browser_manifest_is_dash(manifest) {
        let (package_dir, manifest_filename) =
            browser_package_destination(dest, &manifest.suggested_filename, "dash");
        return BrowserDownloadTarget {
            open: package_dir.join(manifest_filename),
            reveal: package_dir,
        };
    }
    let open = if dest.is_dir() {
        dest.join(&manifest.suggested_filename)
    } else {
        dest.to_path_buf()
    };
    BrowserDownloadTarget {
        reveal: reveal_target_for(&open),
        open,
    }
}

fn browser_package_destination(
    dest: &Path,
    suggested_filename: &str,
    extension: &str,
) -> (PathBuf, String) {
    let manifest_filename = safe_browser_download_filename(suggested_filename);
    let stem = Path::new(&manifest_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(safe_browser_download_filename)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "browser-media".to_string());
    if dest.is_dir() {
        return (dest.join(format!("{stem}.{extension}")), manifest_filename);
    }
    let package_dir = dest.with_extension(extension);
    let filename = dest
        .file_name()
        .and_then(|name| name.to_str())
        .map(safe_browser_download_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or(manifest_filename);
    (package_dir, filename)
}

fn browser_manifest_is_hls(manifest: &BrowserDownloadManifest) -> bool {
    manifest.kind.as_deref() == Some("hls") || url_path_ends_with(&manifest.asset_url, ".m3u8")
}

fn browser_manifest_is_dash(manifest: &BrowserDownloadManifest) -> bool {
    manifest.kind.as_deref() == Some("dash") || url_path_ends_with(&manifest.asset_url, ".mpd")
}

fn url_path_ends_with(url: &str, suffix: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase()
        .ends_with(suffix)
}

fn reveal_target_for(open: &Path) -> PathBuf {
    open.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| open.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabOpenIntent {
    NewForeground(BrowserEngine),
    NewForegroundUrl {
        engine: BrowserEngine,
        url: String,
    },
    ReplaceActiveUrl {
        index: usize,
        engine: BrowserEngine,
        url: String,
    },
}

/// One entry on the session-only reopen stack (Ctrl+Shift+T / History →
/// Reopen Closed Tab).
///
/// Deliberately in-memory only: the stack is never written to disk, never part
/// of the session-sync snapshot, and never published to the Bus — closing a tab
/// must actually retire its trace (the Q74/Q80 privacy locks,
/// `docs/THREAT_MODEL.md`). It lives and dies with this shell process.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClosedTab {
    /// The engine-committed URL the reopen loads.
    url: String,
    /// Last page title, used by the History menu's reopen item label.
    title: String,
    /// Engine that owned the closed session — the reopen keeps it.
    engine: BrowserEngine,
}

/// Maximum retained reopenable closed tabs — a short, bounded stack (Chrome
/// keeps a similarly short recently-closed list).
const CLOSED_TAB_STACK_CAP: usize = 10;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BrowserEngine {
    #[default]
    Servo,
    Cef,
}

impl BrowserEngine {
    const fn wire(self) -> &'static str {
        match self {
            Self::Servo => "servo",
            Self::Cef => "cef",
        }
    }

    fn from_wire(s: &str) -> Option<Self> {
        match s {
            "servo" => Some(Self::Servo),
            "cef" => Some(Self::Cef),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserPolicySourceKind {
    FilterLists,
    SafeBrowsing,
    ManagedUrl,
    CustomFilterRules,
}

impl BrowserPolicySourceKind {
    const fn policy(self) -> &'static str {
        match self {
            Self::FilterLists => "filter_lists",
            Self::SafeBrowsing => "safe_browsing",
            Self::ManagedUrl => "managed_url",
            Self::CustomFilterRules => "custom_filter_rules",
        }
    }

    const fn op(self) -> &'static str {
        match self {
            Self::FilterLists => "browser_filter_list_source_status",
            Self::SafeBrowsing => "browser_safe_browsing_source_status",
            Self::ManagedUrl => "browser_managed_url_policy_source_status",
            Self::CustomFilterRules => "browser_custom_filter_rules_source_status",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::FilterLists => "Filter lists",
            Self::SafeBrowsing => "Safe browsing",
            Self::ManagedUrl => "Managed policy",
            Self::CustomFilterRules => "Custom filters",
        }
    }

    const fn item_label(self) -> &'static str {
        match self {
            Self::FilterLists => "filter source",
            Self::SafeBrowsing => "unsafe site rule",
            Self::ManagedUrl => "URL block rule",
            Self::CustomFilterRules => "custom rule",
        }
    }

    fn topic(self, host: &str) -> String {
        match self {
            Self::FilterLists => browser_filter_list_source_topic(host),
            Self::SafeBrowsing => browser_safe_browsing_source_topic(host),
            Self::ManagedUrl => browser_managed_policy_source_topic(host),
            Self::CustomFilterRules => browser_custom_filter_rules_source_topic(host),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserPolicySourceState {
    Unknown,
    Loaded,
    Empty,
    Missing,
    Error,
}

impl BrowserPolicySourceState {
    const fn wire(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Loaded => "loaded",
            Self::Empty => "empty",
            Self::Missing => "missing",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserPolicySourceStatus {
    kind: BrowserPolicySourceKind,
    state: BrowserPolicySourceState,
    source_path: PathBuf,
    item_count: usize,
    effective_count: usize,
    checked_ms: u64,
    loaded_ms: Option<u64>,
    error: Option<String>,
}

impl BrowserPolicySourceStatus {
    fn unknown(kind: BrowserPolicySourceKind, path: PathBuf) -> Self {
        Self {
            kind,
            state: BrowserPolicySourceState::Unknown,
            source_path: path,
            item_count: 0,
            effective_count: 0,
            checked_ms: 0,
            loaded_ms: None,
            error: None,
        }
    }

    fn loaded(
        kind: BrowserPolicySourceKind,
        path: PathBuf,
        item_count: usize,
        checked_ms: u64,
    ) -> Self {
        Self {
            kind,
            state: if item_count == 0 {
                BrowserPolicySourceState::Empty
            } else {
                BrowserPolicySourceState::Loaded
            },
            source_path: path,
            item_count,
            effective_count: item_count,
            checked_ms,
            loaded_ms: Some(checked_ms),
            error: None,
        }
    }

    fn missing(&self, path: PathBuf, checked_ms: u64, effective_count: usize) -> Self {
        self.failed(
            BrowserPolicySourceState::Missing,
            path,
            checked_ms,
            effective_count,
            None,
        )
    }

    fn error(&self, path: PathBuf, checked_ms: u64, effective_count: usize, error: String) -> Self {
        self.failed(
            BrowserPolicySourceState::Error,
            path,
            checked_ms,
            effective_count,
            Some(error),
        )
    }

    fn failed(
        &self,
        state: BrowserPolicySourceState,
        path: PathBuf,
        checked_ms: u64,
        effective_count: usize,
        error: Option<String>,
    ) -> Self {
        Self {
            kind: self.kind,
            state,
            source_path: path,
            item_count: 0,
            effective_count,
            checked_ms,
            loaded_ms: self.loaded_ms,
            error,
        }
    }

    fn summary(&self) -> String {
        let item = self.kind.item_label();
        let items = plural(self.effective_count);
        match self.state {
            BrowserPolicySourceState::Unknown => {
                format!("{}: source not checked yet", self.kind.label())
            }
            BrowserPolicySourceState::Loaded => format!(
                "{}: {} {}{} loaded",
                self.kind.label(),
                self.effective_count,
                item,
                items
            ),
            BrowserPolicySourceState::Empty => {
                format!(
                    "{}: source loaded, no {}s configured",
                    self.kind.label(),
                    item
                )
            }
            BrowserPolicySourceState::Missing => format!(
                "{}: source missing; {} last-good {}{} active",
                self.kind.label(),
                self.effective_count,
                item,
                items
            ),
            BrowserPolicySourceState::Error => format!(
                "{}: source read error; {} last-good {}{} active",
                self.kind.label(),
                self.effective_count,
                item,
                items
            ),
        }
    }
}

/// The Browser surface's state: the open tabs, the active one, and the address-bar
/// edit buffer.
pub(crate) struct WebState {
    /// Next shell-local tab id. Starts at 1 so id 0 remains available for tests
    /// that exercise credential parsing without constructing a tab.
    next_tab_id: u64,
    /// The open browser tabs (each an isolated session). Empty until a session
    /// attaches — spawning the live helper is the gated `live-helper` path.
    tabs: Vec<Tab>,
    /// Index of the active tab in [`Self::tabs`].
    active: usize,
    /// Engine selected for future live-helper tabs.
    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "read by live-helper spawning and Browser tests")
    )]
    engine: BrowserEngine,
    /// The address-bar edit buffer for the active tab.
    address: String,
    /// Whether the omnibox TextEdit owned keyboard focus on the last painted
    /// frame. Tracked as chrome state (the same idiom as [`Tab::page_focused`])
    /// because the per-frame engine→address sync runs BEFORE the omnibox is
    /// rebuilt each frame — an in-progress operator edit is never clobbered by
    /// an engine redirect.
    omnibox_focused: bool,
    /// Whether ANY Browser chrome text field (omnibox, find bar, dashboard
    /// search) owned keyboard focus on the last painted frame — the guard that
    /// keeps the tab accelerators (Ctrl+T/W/Tab/1-9) from firing mid-edit.
    chrome_edit_focus: bool,
    /// The active tab's engine-committed URL as of the last per-frame sync, so
    /// [`Self::sync_address_on_engine_nav`] rewrites the address bar only on a
    /// real engine transition (redirect / page navigation), not every frame —
    /// a blurred-but-unsubmitted draft survives until the engine really moves.
    last_engine_url: Option<String>,
    /// Set when Reload is pressed on a *crashed* active tab — the shell (or a test)
    /// drains it and swaps in a fresh session (respawn-on-reload).
    respawn_requested: bool,
    /// Set by the visible tab strip's `+` button or the session-restore seam.
    /// Live-helper builds drain this by spawning helper tabs; portable builds
    /// surface the honest gate only.
    open_requested: VecDeque<TabOpenIntent>,
    /// Set when Browser chrome asks to open the rich Bookmarks manager surface.
    open_bookmarks_requested: bool,
    /// Most-recently-closed tabs (newest LAST), bounded by
    /// [`CLOSED_TAB_STACK_CAP`], feeding Ctrl+Shift+T / History → Reopen
    /// Closed Tab. Session-only by design — see [`ClosedTab`] (Q74/Q80).
    closed_tabs: Vec<ClosedTab>,
    /// BROWSER-DD-2 vertical-tabs preference. This is purely chrome layout: it
    /// reuses the same tab/session operations and never creates a second tab model.
    vertical_tabs: bool,
    /// Immersive/fullscreen mode (F11): the browser chrome (tab strip, nav bar,
    /// bookmarks bar, drawers) is hidden and only the page body renders. Session-only.
    fullscreen: bool,
    /// HTTPS-only prompt latch. Explicit `http://` navigations pause here until
    /// the operator upgrades to HTTPS, continues over HTTP, or cancels.
    insecure_prompt: Option<String>,
    /// Destination for the pending HTTPS prompt: active-tab load or a new-tab
    /// open intent that must not bypass the same transport decision.
    insecure_prompt_target: InsecureNavigationTarget,
    /// Quazar new-tab dashboard search draft. This is chrome state, not page
    /// content; submitted searches load the mesh SearXNG URL into the active tab.
    dashboard_query: String,
    /// New-tab speed-dial shortcuts. These start with mesh-local defaults but are
    /// browser state so session sync can carry an operator's current dashboard.
    speed_dial: Vec<SpeedDialEntry>,
    /// Page find draft shown in the compact find bar.
    find_query: String,
    /// The query of the last find submitted — a repeat means "next/prev match"
    /// (native find's `find_next`), a change means a fresh search from the top.
    last_find_query: Option<String>,
    /// Whether the compact find bar is open.
    find_open: bool,
    /// Current page zoom percentage sent to the active helper.
    page_zoom_percent: u16,
    /// BROWSER-DD-2 live SearXNG suggestions for the omnibox. Suggestions are
    /// fetched off-thread from the mesh-local service; the UI only polls this
    /// small state object so typing never blocks a frame.
    suggestions: SuggestionState,
    /// Local file candidates supplied by the shell-owned Files model. Browser
    /// ranks them into omnibox suggestions but does not crawl or persist them.
    file_omnibox_index: Vec<BrowserFileSuggestion>,
    /// BROWSER-DD-11 offline spellcheck worker. Page text is extracted by the
    /// helper, then Hunspell runs off the UI thread and reports an honest result.
    spellcheck: SpellcheckState,
    /// Latest Browser page-text spellcheck result visible in the spelling drawer.
    latest_spellcheck: Option<BrowserSpellcheckResult>,
    /// Next shell-minted page-text request id for spellcheck/TTS seams.
    next_page_text_request_id: u64,
    /// Page-text requests owned by Browser spellcheck, keyed by request id.
    pending_spell_requests: BTreeMap<u64, usize>,
    /// Page-text requests owned by Browser read-aloud, keyed by request id.
    pending_read_aloud_requests: BTreeMap<u64, ReadAloudRequest>,
    /// Page-text requests owned by Power-mode scrape exports, keyed by request id.
    pending_scrape_export_requests: BTreeMap<u64, ScrapeExportRequest>,
    /// Page-text requests owned by Browser translate-page, keyed by request id.
    pending_translate_requests: BTreeMap<u64, TranslateRequest>,
    /// Page-text requests owned by Browser offline-cache snapshots, keyed by
    /// request id.
    pending_offline_cache_requests: BTreeMap<u64, OfflineCacheRequest>,
    /// BROWSER-DD-3 native blocker policy. Starts with the bundled seed lists so
    /// tracker blocking is default-on offline; synced/custom sources and per-site
    /// toggles mutate this store, then every open tab receives a freshly compiled
    /// [`RequestFilter`].
    adfilter_store: FilterListStore,
    /// Current read posture for the mesh-synced compiled filter-list store.
    filter_list_source_status: BrowserPolicySourceStatus,
    /// Current read posture for the operator-managed custom filter rules file.
    custom_filter_rules_source_status: BrowserPolicySourceStatus,
    /// Mesh-hosted safe-browsing host blocklists. The worker/file-sync half can
    /// replace these hosts; the Browser compiles them into every live session.
    safe_browsing_hosts: Vec<String>,
    /// Current read posture for the mesh-hosted safe-browsing source file.
    safe_browsing_source_status: BrowserPolicySourceStatus,
    /// Operator-managed URL policy. Rules are read from the workgroup root and
    /// enforced before chrome-initiated loads and helper resource fetches.
    managed_url_policy: ManagedUrlPolicy,
    /// Current read posture for the operator-managed URL policy source file.
    managed_policy_source_status: BrowserPolicySourceStatus,
    /// BROWSER-DD-3 per-site permission manager. The helpers enforce default-deny
    /// for sensitive prompts; the shell tracks sites the operator explicitly
    /// forgot so the menu has a real state transition without offering fake allow
    /// toggles the engines cannot honor yet.
    forgotten_permission_sites: Vec<String>,
    /// BROWSER-DD-8 prompted-device API trail. Helpers still enforce default-deny;
    /// this records the operator-facing prompt/deny decisions per first-party site
    /// and publishes a typed handoff for the later engine grant hook.
    site_permission_prompts: Vec<SitePermissionPrompt>,
    /// BROWSER-DD-3 per-site data manager. Tracks committed first-party hosts,
    /// live tab counts, last-seen timestamps, and explicit clear actions.
    site_data: SiteDataManager,
    /// Session-only in-memory browsing history (B3, Q74/Q80 — never persisted).
    history: HistoryStore,
    /// Whether the History drawer is open.
    history_open: bool,
    /// Shared Transfers client. Browser downloads are just `browser_download`
    /// rows in the daemon-owned ledger, so Files and Browser show one queue.
    transfers: Box<dyn TransfersClient>,
    /// Platform opener for completed Browser downloads. Production uses
    /// `xdg-open`; tests inject a recorder so farm gates never launch host apps.
    download_opener: Box<dyn DownloadOpener>,
    /// Browser-originated transfers filtered from the shared ledger.
    download_jobs: Vec<TransferJob>,
    /// Browser transfer ids already announced to the mesh notification feed.
    notified_downloads: BTreeSet<String>,
    /// BROWSER-DD-8 power mode. When enabled, the Browser exposes the developer /
    /// media / scrape tool menu; disabled keeps the default clean browser chrome.
    power_mode: bool,
    /// Last `action/browser/session-sync` body published. Keeps unchanged frames
    /// and ledger refreshes from flooding the Bus while still making every state
    /// transition observable.
    last_session_sync_body: Option<String>,
    /// Last retained `state/browser-media/<node>` signature published. Keeps page
    /// metadata polling from flooding the Bus while still clearing to idle when
    /// the helper reports that media disappeared.
    last_media_status_signature: Option<String>,
    /// Last `action/browser/media-control/<node>` ULID applied by this shell.
    media_control_cursor: Option<String>,
    /// Last time the Browser scanned platform media-control actions.
    media_control_last_poll: Option<Instant>,
    /// Shell-owned Browser mini-player/PiP overlay. This is a view over the
    /// selected Browser media tab's retained frame + existing transport controls,
    /// not a claim that the engine has detached a native video element.
    media_pip_open: bool,
    /// One-shot startup restore latch. The Browser reads the daemon-owned latest
    /// session-sync snapshot once, before the live-helper blank-tab fallback.
    #[cfg(any(test, feature = "live-helper"))]
    startup_restore_attempted: bool,
    /// Candidate roots for daemon-persisted startup restore snapshots. Production
    /// probes the local durable root first, then the Syncthing-backed workgroup
    /// root; tests inject temp roots without touching operator state.
    session_restore_roots: Vec<PathBuf>,
    /// Last time the Browser scanned the daemon-owned send-tab outbox for concrete
    /// node-addressed records.
    incoming_send_tab_last_poll: Option<Instant>,
    /// Send-tab records this shell already processed. Backed by durable
    /// tombstones so surviving/unlinkable outbox files cannot replay on restart.
    consumed_send_tab_records: BTreeSet<String>,
    /// Last `event/browser-voice-command/<node>` ULID applied by this shell.
    voice_command_result_cursor: Option<String>,
    /// Last time the Browser scanned voice-command transcript results.
    voice_command_result_last_poll: Option<Instant>,
    /// Latest daemon-owned read-aloud/TTS status for this node.
    latest_read_aloud_status: Option<BrowserReadAloudStatus>,
    /// Latest daemon-owned voice-command/STT status for this node.
    latest_voice_command_status: Option<BrowserVoiceCommandStatus>,
    /// Last time the Browser scanned retained speech-owner status topics.
    speech_status_last_poll: Option<Instant>,
    /// Latest daemon-owned passkey/WebAuthn ceremony status for this node.
    latest_passkey_status: Option<BrowserPasskeyStatus>,
    /// Last time the Browser scanned retained passkey-owner status.
    passkey_status_last_poll: Option<Instant>,
    /// Last `event/browser-passkeys/<node>` ULID applied by this shell.
    passkey_result_cursor: Option<String>,
    /// Last time the Browser scanned passkey completion events.
    passkey_result_last_poll: Option<Instant>,
    /// Helper page request ids waiting for daemon passkey completion, keyed by
    /// the bridge-minted `client_request_id` and routed by stable tab id.
    pending_passkey_requests: BTreeMap<String, u64>,
    /// A page-origin WebAuthn ceremony waiting for shell approval before the
    /// daemon can create/sign anything.
    pending_passkey_consent: Option<PendingPasskeyConsent>,
    /// Last `event/browser-share/<node>` ULID applied by this shell.
    share_result_cursor: Option<String>,
    /// Last time the Browser scanned accepted share route events.
    share_result_last_poll: Option<Instant>,
    /// Latest accepted daemon QR-share route visible in the Browser drawer.
    latest_qr_share: Option<BrowserQrShareResult>,
    /// Last `event/browser-translate/<node>` ULID applied by this shell.
    translation_result_cursor: Option<String>,
    /// Last time the Browser scanned translation results.
    translation_result_last_poll: Option<Instant>,
    /// Latest private translation result visible in the Browser drawer.
    latest_translation: Option<BrowserTranslationResult>,
    /// Last `event/browser-offline-cache/<node>` ULID applied by this shell.
    offline_cache_result_cursor: Option<String>,
    /// Last time the Browser scanned offline-cache record results.
    offline_cache_result_last_poll: Option<Instant>,
    /// Latest private offline-cache record visible in the Browser drawer.
    latest_offline_cache: Option<BrowserOfflineCacheResult>,
    /// Private cache records keyed by exact and conservative canonical URL aliases
    /// for unavailable-page fallback rendering. Records come only from the daemon
    /// cache owner.
    offline_cache_by_url: BTreeMap<String, BrowserOfflineCacheResult>,
    /// Latest daemon-owned CEF runtime update posture for this node.
    latest_security_update: Option<BrowserSecurityUpdateStatus>,
    /// Last time the Browser scanned the retained CEF update status topic.
    security_update_last_poll: Option<Instant>,
    /// Whether the compact download manager drawer is visible.
    downloads_open: bool,
    /// Last time the browser refreshed its ledger view.
    downloads_last_poll: Option<Instant>,
    /// Last time the per-frame catch-all rebuilt + published the session
    /// snapshot. Genuine mutations publish immediately via their own
    /// `publish_session_snapshot()` calls; this gate only throttles the
    /// unconditional per-paint rebuild in `web_panel`.
    session_snapshot_last_poll: Option<Instant>,
    /// Last lifecycle dispatch failure, shown inline instead of being swallowed.
    download_notice: Option<String>,
    /// A CEF-intercepted download flagged by [`download_is_dangerous`], parked
    /// pending the user's Keep/Discard choice instead of being silently
    /// submitted to the ledger. `None` once resolved either way; a safe
    /// download never touches this field.
    pending_dangerous_download: Option<PendingDangerousDownload>,
    /// Ledger job ids the user dismissed from the downloads drawer ("Remove
    /// from list" / "Clear all"). The Transfers ledger job itself is
    /// untouched — this only hides it from the Browser's own view, which
    /// [`WebState::refresh_downloads`] rebuilds from the ledger every poll.
    dismissed_download_ids: BTreeSet<String>,
    /// The source URL behind each ledger-submitted download, keyed by ledger
    /// job id, so the drawer's "Copy link" can put the real download URL on
    /// the clipboard — the ledger job's own `source` field is the local
    /// `.download.json` manifest path, not the URL.
    download_source_urls: BTreeMap<String, String>,
    /// Last viewport-capture result, shown inline instead of being swallowed.
    capture_notice: Option<String>,
    /// A chrome-originated top-level navigation blocked by managed URL policy.
    managed_policy_block: Option<ManagedPolicyBlock>,
    /// Last successfully saved user PDF. CUPS spool PDFs are excluded; this feeds
    /// the CEF-backed built-in PDF viewer action and offline-cache PDF snapshots.
    last_saved_pdf: Option<SavedPdf>,
    /// Helper-produced save-PDF requests waiting for confirmation, keyed by path.
    pending_saved_pdfs: BTreeMap<String, SavedPdf>,
    /// Compact CUPS destination/options drawer for Browser print jobs.
    print_settings_open: bool,
    /// Locally discovered CUPS destinations from `lpstat -e` plus default marker.
    cups_printers: Vec<CupsPrinter>,
    /// Last CUPS destination discovery error, shown inside the print drawer.
    cups_notice: Option<String>,
    /// Print options applied when the helper-produced PDF is submitted to CUPS.
    cups_settings: CupsPrintSettings,
    /// PDFs waiting for helper confirmation before submission to CUPS via `lp`.
    pending_cups_prints: BTreeMap<String, CupsPrintRequest>,
    /// Bus root for Browser-owned platform handoff actions. Defaults to the node
    /// client data dir; tests inject a temp root so persisted actions are asserted
    /// without touching the operator's real bus.
    bus_root: Option<PathBuf>,
    /// BOOKMARKS-BAR — whether the horizontal bookmarks bar is shown below the nav
    /// chrome. A session-only chrome toggle (View → Show Bookmarks Bar), defaulting
    /// hidden like the other browser chrome toggles (find / downloads / vertical).
    bookmarks_bar_visible: bool,
    /// Live query text for the tab-search dropdown (Chrome's "Search tabs" ⌄). A
    /// session-only, in-memory UI field — cleared when a result is chosen.
    tab_search_query: String,
    /// The top-level bookmark links mirrored from `state/bookmarks/collection` — the
    /// buttons the bar renders. Rebuilt each poll from the converged daemon
    /// collection; empty until the first snapshot is seen.
    bookmark_bar_links: Vec<BookmarkBarLink>,
    /// Membership set of every bookmarked URL (all folders, normalized via
    /// [`bookmark_membership_key`]) so the toolbar star reflects bookmarked state.
    bookmarked_urls: std::collections::HashSet<String>,
    /// Every bookmark (all folders) as `{title, url}` for omnibox autocomplete
    /// ([`matching_bookmarks`]). Derived alongside [`Self::bookmarked_urls`].
    bookmark_index: Vec<BookmarkBarLink>,
    /// Configurable search engines with keyword shortcuts ([`keyword_search_target`]).
    search_engines: Vec<SearchEngine>,
    /// Named, colored tab groups; a tab's [`Tab::group`] indexes into this. Session-only.
    tab_groups: Vec<TabGroup>,
    /// User-authored CSS site styles (safe userscript slice — CSS only), folded into
    /// the injected userscript bundle. Session-only.
    user_site_styles: Vec<UserSiteStyle>,
    /// Session HSTS: hosts the user chose to upgrade to HTTPS — future plain-http
    /// navigations to them auto-upgrade silently instead of re-prompting. In-memory
    /// only (no persistence, per the operator's session-HSTS decision).
    hsts_hosts: std::collections::HashSet<String>,
    /// Session-only per-site permission grants — `(origin, kind)` pairs the user
    /// ALLOWED this session (kind: 0 geolocation, 1 notifications, 2 clipboard, 3
    /// camera, 4 microphone, 5 camera + microphone). A future same-origin-same-kind
    /// request auto-allows without re-prompting; a block is never remembered
    /// (Chrome re-prompts after a block). In-memory only, per the operator's
    /// session-only permission decision (browser-gated-features).
    granted_permissions: std::collections::HashSet<(String, u8)>,
    /// The session-only saved-login store (see [`StoredLogin`]). In-memory; cleared
    /// on shell exit; never persisted. Drives the omnibox 🔑 autofill affordance.
    session_logins: Vec<StoredLogin>,
    /// Draft inputs for the 🔑 menu's "save a login for this site" mini-form.
    login_user_draft: String,
    login_pass_draft: String,
    /// An auto-captured login awaiting the user's "Save password?" decision (a form
    /// submit the engine beaconed). `None` when nothing is pending.
    pending_login_save: Option<PendingLoginSave>,
    /// Whether the Site Styles editor drawer is open, and its two input drafts.
    site_styles_open: bool,
    site_style_host_draft: String,
    site_style_css_draft: String,
    /// Bus cursor for the bookmark-collection mirror, so each converged snapshot is
    /// folded once (the exact `list_since` cursor idiom the other pollers use).
    bookmarks_collection_cursor: Option<String>,
    /// Throttle for the relaxed bookmark-collection re-read.
    bookmarks_collection_last_poll: Option<Instant>,
    /// Throttle for the mesh-synced compiled filter-list store re-read.
    filter_lists_last_poll: Option<Instant>,
    /// Throttle for the operator-managed custom filter rules re-read.
    custom_filter_rules_last_poll: Option<Instant>,
    /// Throttle for the operator-curated safe-browsing blocklist re-read.
    safe_browsing_last_poll: Option<Instant>,
    /// Throttle for the operator-managed URL policy re-read.
    managed_policy_last_poll: Option<Instant>,
    /// Region-capture mode is armed; the next drag over the page image writes a
    /// cropped PNG from the retained helper frame.
    capture_region_mode: bool,
    /// Region-capture drag anchor in helper-frame pixel coordinates.
    capture_region_start: Option<egui::Pos2>,
    /// Region-capture current drag point in helper-frame pixel coordinates.
    capture_region_current: Option<egui::Pos2>,
    /// An honest gated notice shown in place of the `EmptyState` when a `live-helper`
    /// open couldn't proceed (no seat · helper binary absent · spawn failed). `None`
    /// = the default gated caption. Only ever set on the live path — a named reason,
    /// never a fake page (§7).
    #[cfg(feature = "live-helper")]
    gate_notice: Option<String>,
    /// One-shot latch so the real `Command::spawn` is attempted at most once per
    /// surface lifetime — a spawn *failure* must not respawn a process every frame.
    #[cfg(feature = "live-helper")]
    spawn_attempted: bool,
    /// The live seat's output size in device pixels, refreshed each frame from the
    /// egui context ([`Self::note_seat_px`]). A freshly spawned helper pre-sizes its
    /// frame channel to this — the ceiling of any panel-driven resize — so an
    /// enlarged paint never overflows the channel (browser-1, item 3). Defaults to
    /// the `(INIT_W, INIT_H)` fallback until a real seat is seen.
    #[cfg(feature = "live-helper")]
    seat_px: (u32, u32),
}

impl Default for WebState {
    fn default() -> Self {
        Self {
            next_tab_id: 1,
            tabs: Vec::new(),
            active: 0,
            engine: preferred_default_engine(),
            address: String::new(),
            omnibox_focused: false,
            chrome_edit_focus: false,
            last_engine_url: None,
            respawn_requested: false,
            open_requested: VecDeque::new(),
            open_bookmarks_requested: false,
            closed_tabs: Vec::new(),
            vertical_tabs: DEFAULT_VERTICAL_TABS,
            fullscreen: false,
            insecure_prompt: None,
            insecure_prompt_target: InsecureNavigationTarget::ActiveTab,
            dashboard_query: String::new(),
            speed_dial: default_speed_dial(),
            find_query: String::new(),
            last_find_query: None,
            find_open: false,
            page_zoom_percent: 100,
            suggestions: SuggestionState::default(),
            file_omnibox_index: Vec::new(),
            spellcheck: SpellcheckState::default(),
            latest_spellcheck: None,
            next_page_text_request_id: 1,
            pending_spell_requests: BTreeMap::new(),
            pending_read_aloud_requests: BTreeMap::new(),
            pending_scrape_export_requests: BTreeMap::new(),
            pending_translate_requests: BTreeMap::new(),
            pending_offline_cache_requests: BTreeMap::new(),
            adfilter_store: FilterListStore::with_bundled(),
            filter_list_source_status: BrowserPolicySourceStatus::unknown(
                BrowserPolicySourceKind::FilterLists,
                default_workgroup_root().join(ADFILTER_COMPILED_STORE_PATH),
            ),
            custom_filter_rules_source_status: BrowserPolicySourceStatus::unknown(
                BrowserPolicySourceKind::CustomFilterRules,
                default_workgroup_root().join(CUSTOM_FILTER_RULES_PATH),
            ),
            safe_browsing_hosts: Vec::new(),
            safe_browsing_source_status: BrowserPolicySourceStatus::unknown(
                BrowserPolicySourceKind::SafeBrowsing,
                default_workgroup_root().join(SAFE_BROWSING_HOSTS_PATH),
            ),
            managed_url_policy: ManagedUrlPolicy::empty(),
            managed_policy_source_status: BrowserPolicySourceStatus::unknown(
                BrowserPolicySourceKind::ManagedUrl,
                default_workgroup_root().join(MANAGED_URL_POLICY_PATH),
            ),
            forgotten_permission_sites: Vec::new(),
            site_permission_prompts: Vec::new(),
            site_data: SiteDataManager::default(),
            history: HistoryStore::default(),
            history_open: false,
            transfers: Box::new(FileTransfers::from_env()),
            download_opener: Box::<XdgDownloadOpener>::default(),
            download_jobs: Vec::new(),
            notified_downloads: BTreeSet::new(),
            power_mode: false,
            last_session_sync_body: None,
            last_media_status_signature: None,
            media_control_cursor: None,
            media_control_last_poll: None,
            media_pip_open: false,
            #[cfg(any(test, feature = "live-helper"))]
            startup_restore_attempted: false,
            session_restore_roots: default_session_restore_roots(),
            incoming_send_tab_last_poll: None,
            consumed_send_tab_records: BTreeSet::new(),
            voice_command_result_cursor: None,
            voice_command_result_last_poll: None,
            latest_read_aloud_status: None,
            latest_voice_command_status: None,
            speech_status_last_poll: None,
            latest_passkey_status: None,
            passkey_status_last_poll: None,
            passkey_result_cursor: None,
            passkey_result_last_poll: None,
            pending_passkey_requests: BTreeMap::new(),
            pending_passkey_consent: None,
            share_result_cursor: None,
            share_result_last_poll: None,
            latest_qr_share: None,
            translation_result_cursor: None,
            translation_result_last_poll: None,
            latest_translation: None,
            offline_cache_result_cursor: None,
            offline_cache_result_last_poll: None,
            latest_offline_cache: None,
            offline_cache_by_url: BTreeMap::new(),
            latest_security_update: None,
            security_update_last_poll: None,
            downloads_open: false,
            downloads_last_poll: None,
            session_snapshot_last_poll: None,
            download_notice: None,
            pending_dangerous_download: None,
            dismissed_download_ids: BTreeSet::new(),
            download_source_urls: BTreeMap::new(),
            capture_notice: None,
            managed_policy_block: None,
            last_saved_pdf: None,
            pending_saved_pdfs: BTreeMap::new(),
            print_settings_open: false,
            cups_printers: Vec::new(),
            cups_notice: None,
            cups_settings: CupsPrintSettings::default(),
            pending_cups_prints: BTreeMap::new(),
            bus_root: mde_bus::client_data_dir(),
            bookmarks_bar_visible: false,
            tab_search_query: String::new(),
            bookmark_bar_links: Vec::new(),
            bookmarked_urls: std::collections::HashSet::new(),
            bookmark_index: Vec::new(),
            search_engines: default_search_engines(),
            tab_groups: Vec::new(),
            user_site_styles: Vec::new(),
            hsts_hosts: std::collections::HashSet::new(),
            granted_permissions: std::collections::HashSet::new(),
            session_logins: Vec::new(),
            login_user_draft: String::new(),
            login_pass_draft: String::new(),
            pending_login_save: None,
            site_styles_open: false,
            site_style_host_draft: String::new(),
            site_style_css_draft: String::new(),
            bookmarks_collection_cursor: None,
            filter_lists_last_poll: None,
            custom_filter_rules_last_poll: None,
            safe_browsing_last_poll: None,
            managed_policy_last_poll: None,
            bookmarks_collection_last_poll: None,
            capture_region_mode: false,
            capture_region_start: None,
            capture_region_current: None,
            #[cfg(feature = "live-helper")]
            gate_notice: None,
            #[cfg(feature = "live-helper")]
            spawn_attempted: false,
            #[cfg(feature = "live-helper")]
            seat_px: (INIT_W, INIT_H),
        }
    }
}

impl WebState {
    /// The active tab, if any.
    fn active_tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    fn active_internal_page(&self) -> Option<BrowserInternalPage> {
        self.tabs.get(self.active).and_then(|tab| tab.internal_page)
    }

    fn open_or_focus_internal_page(&mut self, page: BrowserInternalPage) {
        if let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.internal_page == Some(page))
        {
            self.select_tab(index);
            return;
        }
        if let Err(err) = self.push_internal_page(page) {
            self.capture_notice = Some(format!("Could not open {}: {err}", page.title()));
        }
    }

    fn open_options_tab(&mut self) {
        self.open_or_focus_internal_page(BrowserInternalPage::Options);
    }

    fn clear_active_internal_page_for_load(&mut self, url: &str) -> Option<usize> {
        let index = self.active;
        let tab = self.tabs.get_mut(index)?;
        tab.internal_page?;
        tab.internal_page = None;
        tab.internal_peer = None;
        tab.texture = None;
        tab.last_frame = None;
        tab.favicon_cache = None;
        tab.resizer = ViewportResizer::default();
        tab.last_activity = Instant::now();
        tab.idle_suspended = false;
        self.address = url.to_owned();
        self.last_engine_url = None;
        Some(index)
    }

    fn tab_index_by_id(&self, tab_id: u64) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == tab_id)
    }

    /// WIN7-4 — the open-tab count, the SAME `self.tabs` length the Browser
    /// accessibility summary already folds into its "Active tab X of N" string
    /// (no second read, §7). Backs the Start Menu Browser tile's live fact.
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    /// Whether the shell should refresh local file candidates for the Browser
    /// omnibox this frame. Avoids a per-frame Files home snapshot while the
    /// Browser is merely displaying a page.
    pub(crate) fn wants_file_omnibox_items(&self) -> bool {
        self.omnibox_focused && !self.address.trim().is_empty()
    }

    /// Refresh Browser's local file suggestion source from the shell-owned Files
    /// model. The source is kept in memory only and re-ranked for the current
    /// omnibox draft; Browser does not scan directories itself.
    pub(crate) fn set_file_omnibox_items(&mut self, items: Vec<SearchItem<FileSearchTarget>>) {
        self.file_omnibox_index = browser_file_suggestions(items);
        self.update_file_suggestions_for_address();
    }

    /// Browser candidates for the shell-owned front door: persisted bookmarks,
    /// session-only history, and an explicit web-search action for the typed query.
    /// The history half stays in memory only, matching Browser privacy locks.
    pub(crate) fn search_omnibox_items(&self, query: &str) -> Vec<SearchItem<String>> {
        let query = query.trim();
        let mut items = Vec::new();
        items.extend(
            matching_bookmarks(&self.bookmark_index, query, 5)
                .into_iter()
                .enumerate()
                .map(|(idx, bookmark)| {
                    SearchItem::new(
                        SearchDomain::BrowserBookmark,
                        bookmark.title,
                        bookmark.url.clone(),
                        bookmark.url,
                    )
                    .with_source_rank(idx)
                }),
        );
        let history_offset = items.len();
        items.extend(
            self.history
                .matching(query)
                .take(5)
                .enumerate()
                .map(|(idx, visit)| {
                    let title = if visit.title.trim().is_empty() {
                        visit.url.clone()
                    } else {
                        visit.title.clone()
                    };
                    SearchItem::new(
                        SearchDomain::BrowserHistory,
                        title,
                        visit.url.clone(),
                        visit.url.clone(),
                    )
                    .with_source_rank(history_offset + idx)
                }),
        );
        if !query.is_empty() {
            let search_target =
                keyword_search_target(query, &self.search_engines).unwrap_or_else(|| {
                    format!("{DEFAULT_SEARCH_URL}?q={}", percent_encode_query(query))
                });
            items.push(
                SearchItem::new(
                    SearchDomain::WebSuggestion,
                    format!("Search web for {query}"),
                    search_target,
                    query.to_owned(),
                )
                .with_source_rank(items.len()),
            );
        }
        items
    }

    /// Activate a shell-front-door Browser target through Browser's normal address
    /// submission/new-tab path.
    pub(crate) fn open_search_omnibox_target(&mut self, target: &str) {
        let Some(url) =
            keyword_search_target(target, &self.search_engines).or_else(|| omnibox_target(target))
        else {
            return;
        };
        if self.tabs.is_empty() {
            self.request_new_tab_with_url(self.engine, url);
            return;
        }
        self.address = url.clone();
        self.load_target(url);
    }

    #[cfg(test)]
    fn with_transfers(mut self, transfers: Box<dyn TransfersClient>) -> Self {
        self.transfers = transfers;
        self.refresh_downloads();
        self
    }

    #[cfg(test)]
    fn with_download_opener(mut self, opener: Box<dyn DownloadOpener>) -> Self {
        self.download_opener = opener;
        self
    }

    #[cfg(test)]
    fn with_bus_root(mut self, bus_root: Option<PathBuf>) -> Self {
        self.bus_root = bus_root;
        self
    }

    #[cfg(test)]
    fn with_session_restore_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.session_restore_roots = roots;
        self.startup_restore_attempted = false;
        self
    }

    /// Refresh the Browser's view of daemon-owned transfer jobs, keeping only
    /// browser-originated rows and prioritizing active work over history.
    fn refresh_downloads(&mut self) {
        let mut jobs: Vec<TransferJob> = self
            .transfers
            .jobs()
            .into_iter()
            .filter(|job| {
                job.method == TransferMethod::BrowserDownload
                    && !self.dismissed_download_ids.contains(&job.id)
            })
            .collect();
        jobs.sort_by(|a, b| {
            download_state_rank(a.state)
                .cmp(&download_state_rank(b.state))
                .then_with(|| b.updated_ms.cmp(&a.updated_ms))
                .then_with(|| b.created_ms.cmp(&a.created_ms))
        });
        if self.downloads_last_poll.is_some() {
            for job in &jobs {
                self.publish_download_notification(job);
            }
        }
        self.download_jobs = jobs;
        self.downloads_last_poll = Some(Instant::now());
        self.publish_session_snapshot();
    }

    fn publish_session_snapshot(&mut self) {
        let body = browser_session_sync_body(self);
        if self.last_session_sync_body.as_deref() == Some(body.as_str()) {
            return;
        }
        publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_SESSION_SYNC, &body);
        self.last_session_sync_body = Some(body);
    }

    fn publish_media_status_if_changed(&mut self) {
        let signature = browser_media_status_signature(self);
        if self.last_media_status_signature.as_deref() == Some(signature.as_str()) {
            return;
        }
        let host = local_hostname();
        let topic = browser_media_status_topic(&host);
        let body = browser_media_status_body(self, unix_ms());
        publish_to_bus(self.bus_root.as_deref(), &topic, &body);
        self.last_media_status_signature = Some(signature);
    }

    fn poll_media_control_actions(&mut self) {
        if self
            .media_control_last_poll
            .is_some_and(|last| last.elapsed() < MEDIA_CONTROL_POLL_INTERVAL)
        {
            return;
        }
        self.media_control_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_media_control_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.media_control_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.media_control_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(request) = parse_browser_media_control_request(body) else {
                continue;
            };
            self.apply_browser_media_control(request);
        }
    }

    fn apply_browser_media_control(&mut self, request: BrowserMediaControlRequest) -> bool {
        let Some(index) = request
            .tab_id
            .and_then(|tab_id| self.tab_index_by_id(tab_id))
            .or_else(|| browser_media_status_tab_index(self))
            .or_else(|| (!self.tabs.is_empty()).then_some(self.active.min(self.tabs.len() - 1)))
        else {
            return false;
        };
        let Some(tab) = self.tabs.get_mut(index) else {
            return false;
        };
        if tab.internal_page.is_some() {
            return false;
        }
        tab.session.media_transport(request.action);
        tab.last_activity = Instant::now();
        true
    }

    /// Drive the Browser media target selected by the now-playing fold: an explicit
    /// tab id when provided by Bus/MPRIS, otherwise the active media tab, audible
    /// background tab, or active live tab fallback. Hardware media keys use this so
    /// foreground browsing does not steal Play/Pause from background media.
    pub(crate) fn selected_media_transport(
        &mut self,
        action: mde_web_preview_client::MediaTransportAction,
    ) -> bool {
        self.apply_browser_media_control(BrowserMediaControlRequest {
            action,
            tab_id: None,
        })
    }

    fn media_pip_available(&self) -> bool {
        browser_media_status_tab_index(self).is_some_and(|index| {
            self.tabs.get(index).is_some_and(|tab| {
                tab.internal_page.is_none()
                    && !tab.session.is_crashed()
                    && tab.session.media_metadata().is_some()
            })
        })
    }

    pub(crate) fn toggle_media_pip(&mut self) {
        if self.media_pip_open {
            self.media_pip_open = false;
        } else if self.media_pip_available() {
            self.media_pip_open = true;
        }
    }

    /// Rebuild + publish the session snapshot at a UI-safe cadence. Genuine
    /// mutations (new tab, navigation, download completion, …) still publish
    /// immediately through their own `publish_session_snapshot()` calls; this
    /// per-frame catch-all only needs to pick up async tab-poll changes, so it
    /// runs ~1×/s instead of every vblank to avoid rebuilding the full
    /// serde_json body (and reallocating the open-tab host Vec) on every paint.
    /// The first frame still publishes immediately (last_poll is None), and the
    /// string-compare dedup in `publish_session_snapshot` prevents redundant
    /// bus traffic.
    fn poll_session_snapshot(&mut self) {
        if self
            .session_snapshot_last_poll
            .is_some_and(|last| last.elapsed() < SESSION_SNAPSHOT_POLL_INTERVAL)
        {
            return;
        }
        self.update_site_data_from_tabs();
        self.record_history_from_active_tab();
        self.publish_session_snapshot();
        self.session_snapshot_last_poll = Some(Instant::now());
    }

    fn publish_download_notification(&mut self, job: &TransferJob) {
        let (severity, summary) = match job.state {
            TransferState::Done => (
                Severity::Info,
                format!("Browser download complete: {}", short_transfer_name(job)),
            ),
            TransferState::Failed => (
                Severity::Warning,
                format!("Browser download failed: {}", short_transfer_name(job)),
            ),
            _ => return,
        };
        if !self.notified_downloads.insert(job.id.clone()) {
            return;
        }
        let body = browser_notify_body(
            severity,
            &summary,
            Some(&format!("{} -> {}", job.source, job.dest)),
        );
        publish_to_bus(self.bus_root.as_deref(), EVENT_NOTIFY_BROWSER, &body);
    }

    /// Poll the transfer ledger at a UI-safe cadence. The client reads local files,
    /// but the Browser still avoids scanning it every paint.
    fn poll_downloads(&mut self) {
        if self
            .downloads_last_poll
            .is_some_and(|last| last.elapsed() < DOWNLOADS_POLL_INTERVAL)
        {
            return;
        }
        self.refresh_downloads();
    }

    fn poll_browser_services_before_tabs(&mut self) {
        self.poll_suggestions();
        self.poll_downloads();
        self.poll_incoming_send_tabs();
        self.poll_voice_command_results();
        self.poll_media_control_actions();
        self.poll_speech_statuses();
        self.poll_passkey_status();
        self.poll_passkey_results();
        self.poll_share_results();
        self.poll_translation_results();
        self.poll_offline_cache_results();
        self.poll_security_update_status();
        self.poll_bookmarks_collection();
        self.poll_filter_lists();
        self.poll_custom_filter_rules();
        self.poll_safe_browsing_hosts();
        self.poll_managed_url_policy();
        self.suspend_idle_tabs(Instant::now());
    }

    /// Poll every tab so background tabs keep receiving events and one tab's
    /// crash/interstitial/download signal is observed without disturbing the
    /// others. UI-visible side effects are applied after the mutable tab pass.
    fn poll_tabs_for_panel(&mut self) -> BrowserTabPollEvents {
        let mut events = BrowserTabPollEvents::default();
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            if tab.internal_page.is_some() {
                continue;
            }
            if tab.idle_suspended && idx != self.active {
                continue;
            }
            tab.session.poll();
            if let Some(error) = tab.session.cert_error().cloned() {
                if tab.last_audited_cert_error.as_ref() != Some(&error) {
                    events.certificate_errors.push(CertificateErrorAudit {
                        engine: tab.engine,
                        title: tab.session.title().to_owned(),
                        error: error.clone(),
                    });
                    tab.last_audited_cert_error = Some(error);
                }
            } else {
                tab.last_audited_cert_error = None;
            }

            let resources = tab.session.recent_resource_requests();
            let mut max_resource_seq = tab.last_audited_resource_seq;
            for resource in resources
                .iter()
                .filter(|resource| resource.seq > tab.last_audited_resource_seq)
            {
                max_resource_seq = max_resource_seq.max(resource.seq);
                if resource.blocked_by.as_deref() == Some("mixed-content:http") {
                    events.mixed_content_blocks.push(MixedContentBlockAudit {
                        engine: tab.engine,
                        page_url: tab.session.nav().url.clone(),
                        title: tab.session.title().to_owned(),
                        url: resource.url.clone(),
                        resource: resource.resource,
                    });
                }
            }
            tab.last_audited_resource_seq = max_resource_seq;

            for event in tab.session.drain_pdf_events() {
                events.pdf_events.push((event.path, event.ok));
            }
            for event in tab.session.drain_page_text_events() {
                events.page_text_events.push((event.id, event.text));
            }
            for event in tab.session.drain_page_scrape_events() {
                events.page_scrape_events.push((event.id, event.body));
            }
            for event in tab.session.drain_passkey_events() {
                events.passkey_events.push((tab.id, tab.engine, event.body));
            }
            // window.open / target=_blank the engine cancelled → open as a real
            // tab on the same engine (EventMsg::PopupRequested).
            for request in tab.session.drain_popup_requests() {
                events.popup_opens.push((tab.engine, request.url));
            }
            // Downloads the engine intercepted → submit to the mesh Transfers
            // ledger after this mutable tab pass.
            for event in tab.session.drain_download_events() {
                events
                    .download_submits
                    .push((event.id, event.url, event.filename));
            }
            // A submitted login (auto-capture) → offer to save it (session-only).
            for capture in tab.session.drain_login_captures() {
                events.login_captures.push((tab.id, capture));
            }
            // JavaScript dialogs were auto-resolved by the engine; surface them
            // as passive Browser notices so pages do not silently surprise the
            // operator.
            for dialog in tab.session.drain_js_dialog_events() {
                events.js_dialog_events.push((idx, dialog));
            }
        }
        events
    }

    fn apply_tab_poll_events(&mut self, events: BrowserTabPollEvents) {
        for (engine, url) in events.popup_opens {
            self.request_new_tab_with_url(engine, url);
        }
        for (id, url, filename) in events.download_submits {
            self.submit_download_to_ledger(id, &url, &filename);
        }
        let mut pdf_notice = None;
        for (path, ok) in events.pdf_events {
            pdf_notice = Some(self.handle_pdf_event(path, ok));
        }
        if let Some(notice) = pdf_notice {
            self.capture_notice = Some(notice);
        }
        for (id, text) in events.page_text_events {
            self.handle_page_text_event(id, text);
        }
        for (id, body) in events.page_scrape_events {
            self.handle_page_scrape_event(id, body);
        }
        for (tab_id, engine, body) in events.passkey_events {
            self.handle_passkey_event(tab_id, engine, &body);
        }
        for (tab_index, dialog) in events.js_dialog_events {
            self.handle_js_dialog_event(tab_index, &dialog);
        }
        for (tab_id, capture) in events.login_captures {
            self.handle_login_capture_from_tab(tab_id, &capture.origin, &capture.body);
        }
        for block in events.mixed_content_blocks {
            self.publish_mixed_content_block(&block, unix_ms());
        }
        for audit in events.certificate_errors {
            self.publish_certificate_error(&audit, unix_ms());
        }
    }

    fn finish_browser_panel_poll(&mut self) {
        self.poll_spellcheck();
        // Engine-driven navigations (redirects, page scripts) land in the address
        // bar here; the tab poll has already drained this frame's nav events, and
        // the focus guard keeps operator edits intact.
        self.sync_address_on_engine_nav();
        self.publish_media_status_if_changed();
        self.poll_session_snapshot();
    }

    /// Upload one tab's pending frame only when one is present, so an idle page
    /// never triggers a texture re-upload. If a retained CPU frame exists but no
    /// GPU texture has been created for that tab yet, build it once so shell-owned
    /// consumers such as Browser PiP can paint background media without selecting
    /// the tab.
    fn upload_tab_frame(&mut self, ctx: &egui::Context, index: usize) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.internal_page.is_some() {
            return;
        }
        if let Some(img) = tab.session.take_frame() {
            // Share one Arc<ColorImage> between the retained CPU frame and the
            // GPU upload instead of deep-copying full-resolution pixels on
            // every decoded frame.
            let img = std::sync::Arc::new(img);
            tab.last_frame = Some(img.clone());
            match tab.texture.as_mut() {
                Some(handle) => handle.set(img, BROWSER_TEX),
                None => {
                    tab.texture =
                        Some(ctx.load_texture(format!("web-preview-{}", tab.id), img, BROWSER_TEX));
                }
            }
        } else if tab.texture.is_none() {
            if let Some(img) = tab.last_frame.clone() {
                tab.texture =
                    Some(ctx.load_texture(format!("web-preview-{}", tab.id), img, BROWSER_TEX));
            }
        }
    }

    /// Upload the active tab's pending frame only when one is present, so an idle
    /// page never triggers a texture re-upload.
    fn upload_active_frame(&mut self, ctx: &egui::Context) {
        if self.active_internal_page().is_some() {
            return;
        }
        self.upload_tab_frame(ctx, self.active);
    }

    fn upload_media_pip_frame(&mut self, ctx: &egui::Context) {
        if !self.media_pip_open {
            return;
        }
        if let Some(index) = browser_media_status_tab_index(self) {
            self.upload_tab_frame(ctx, index);
        }
    }

    fn active_live_page_needs_repaint(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            tab.internal_page.is_none()
                && !tab.idle_suspended
                && !tab.session.is_crashed()
                && (tab.texture.is_some()
                    || tab.last_frame.is_some()
                    || tab.session.nav().loading
                    || tab.session.media_metadata().is_some()
                    || tab.session.audible())
        })
    }

    fn media_pip_needs_repaint(&self) -> bool {
        if !self.media_pip_open {
            return false;
        }
        let Some(index) = browser_media_status_tab_index(self) else {
            return false;
        };
        self.tabs.get(index).is_some_and(|tab| {
            tab.internal_page.is_none()
                && !tab.idle_suspended
                && !tab.session.is_crashed()
                && tab_media_is_playing(tab)
        })
    }

    fn request_browser_frame_repaint(&self, ctx: &egui::Context) {
        if self.active_live_page_needs_repaint() || self.media_pip_needs_repaint() {
            ctx.request_repaint_after(LIVE_PAGE_REPAINT_INTERVAL);
        }
    }

    fn mark_tab_active(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
        }
    }

    fn mark_active_tab_activity(&mut self) {
        self.mark_tab_active(self.active);
    }

    fn suspend_idle_tabs(&mut self, now: Instant) {
        let mut suspended = Vec::new();
        for (idx, tab) in self.tabs.iter_mut().enumerate() {
            if idx == self.active
                || tab.internal_page.is_some()
                || tab.idle_suspended
                || tab.session.is_crashed()
            {
                continue;
            }
            if tab.session.audible() && !tab.muted {
                continue;
            }
            if now.duration_since(tab.last_activity) < IDLE_TAB_SUSPEND_AFTER {
                continue;
            }
            tab.session.stop();
            tab.idle_suspended = true;
            suspended.push((
                idx,
                tab.engine,
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
            ));
        }
        for (idx, engine, url, title) in suspended {
            let body = browser_tab_suspend_body(idx, engine, &url, &title, IDLE_TAB_SUSPEND_AFTER);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_TAB_SUSPEND, &body);
        }
        self.publish_session_snapshot();
    }

    fn download_counts(&self) -> (usize, usize) {
        (
            self.download_jobs
                .iter()
                .filter(|job| job.state.is_active())
                .count(),
            self.download_jobs.len(),
        )
    }

    /// A compact Browser-download projection for the shell's shared file-operation
    /// status cell. The daemon transfer ledger remains the source of truth; this
    /// folds the Browser-filtered view the downloads drawer already shows.
    pub(crate) fn operation_progress_summary(
        &self,
    ) -> Option<mde_files_egui::model::OperationProgressSummary> {
        let mut active = 0;
        let mut known_progress = 0;
        let mut progress_total = 0.0;
        let mut first_label: Option<String> = None;

        for job in self
            .download_jobs
            .iter()
            .filter(|job| job.state.is_active())
        {
            active += 1;
            if first_label.is_none() {
                first_label = Some(short_transfer_name(job));
            }
            if let Some(progress) = job.progress {
                known_progress += 1;
                progress_total += (f32::from(progress) / 100.0).clamp(0.0, 1.0);
            }
        }

        if active == 0 {
            return None;
        }

        let label = if active == 1 {
            first_label.unwrap_or_else(|| "Browser download".to_owned())
        } else {
            format!("{active} browser downloads")
        };

        Some(mde_files_egui::model::OperationProgressSummary {
            active,
            known_progress,
            fraction: (known_progress > 0).then_some(progress_total / known_progress as f32),
            label,
        })
    }

    fn dispatch_download_verb(&mut self, verb: TransferVerb) {
        match self.transfers.dispatch(&verb) {
            Ok(()) => {
                self.download_notice = None;
                self.refresh_downloads();
            }
            Err(err) => self.download_notice = Some(err),
        }
    }

    fn open_download(&mut self, id: &str) {
        let target = self
            .download_jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or_else(|| "Download is no longer visible".to_owned())
            .and_then(completed_browser_download_target);
        match target {
            Ok(target) => match self.download_opener.open_path(&target.open) {
                Ok(()) => {
                    self.download_notice = None;
                    self.capture_notice =
                        Some(format!("Opening {}", browser_output_label(&target.open)));
                }
                Err(err) => self.download_notice = Some(format!("Open failed: {err}")),
            },
            Err(err) => self.download_notice = Some(err),
        }
    }

    fn reveal_download(&mut self, id: &str) {
        let target = self
            .download_jobs
            .iter()
            .find(|job| job.id == id)
            .ok_or_else(|| "Download is no longer visible".to_owned())
            .and_then(completed_browser_download_target);
        match target {
            Ok(target) => match self.download_opener.reveal_path(&target.reveal) {
                Ok(()) => {
                    self.download_notice = None;
                    self.capture_notice =
                        Some(format!("Showing {}", browser_output_label(&target.reveal)));
                }
                Err(err) => self.download_notice = Some(format!("Show failed: {err}")),
            },
            Err(err) => self.download_notice = Some(err),
        }
    }

    /// Append a session as a new tab and make it active. The live helper-spawn open
    /// path (gated) and the tests both funnel through here; the default gated build
    /// opens no tabs and shows the honest `EmptyState`, so this is unused there.
    #[cfg(test)]
    pub(crate) fn push_session(&mut self, session: WebSession) {
        self.push_session_with_engine(session, self.engine);
    }

    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "used by live-helper spawning and Browser tests")
    )]
    fn push_session_with_engine(&mut self, session: WebSession, engine: BrowserEngine) {
        let mut session = session;
        let url = session.nav().url.clone();
        session.set_filter(self.compiled_request_filter_for_url(&url));
        session.set_autoplay_blocked(DEFAULT_AUTOPLAY_BLOCKED);
        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.saturating_add(1);
        self.tabs.push(Tab {
            id,
            session,
            engine,
            internal_page: None,
            internal_peer: None,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            group: None,
            pinned: false,
            muted: false,
            autoplay_blocked: DEFAULT_AUTOPLAY_BLOCKED,
            force_dark: false,
            reader_mode: false,
            user_scripts: false,
            user_agent: UserAgentOverride::Default,
            device_profile: DeviceProfile::Default,
            last_activity: Instant::now(),
            idle_suspended: false,
            page_focused: false,
            texture: None,
            last_frame: None,
            last_audited_resource_seq: 0,
            last_audited_cert_error: None,
            resizer: ViewportResizer::default(),
            favicon_cache: None,
        });
        self.active = self.tabs.len() - 1;
        self.publish_session_snapshot();
    }

    fn push_internal_page(&mut self, page: BrowserInternalPage) -> Result<(), String> {
        let (shell, peer) = UnixStream::pair().map_err(|err| err.to_string())?;
        let session = WebSession::from_stream(shell, None).map_err(|err| err.to_string())?;
        let id = self.next_tab_id;
        self.next_tab_id = self.next_tab_id.saturating_add(1);
        self.tabs.push(Tab {
            id,
            session,
            engine: self.engine,
            internal_page: Some(page),
            internal_peer: Some(peer),
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            group: None,
            pinned: false,
            muted: false,
            autoplay_blocked: DEFAULT_AUTOPLAY_BLOCKED,
            force_dark: false,
            reader_mode: false,
            user_scripts: false,
            user_agent: UserAgentOverride::Default,
            device_profile: DeviceProfile::Default,
            last_activity: Instant::now(),
            idle_suspended: false,
            page_focused: false,
            texture: None,
            last_frame: None,
            last_audited_resource_seq: 0,
            last_audited_cert_error: None,
            resizer: ViewportResizer::default(),
            favicon_cache: None,
        });
        self.active = self.tabs.len() - 1;
        self.address = page.url().to_owned();
        self.last_engine_url = None;
        self.publish_session_snapshot();
        Ok(())
    }

    /// Request a foreground tab. The surface owns the visible affordance; the shell
    /// live-helper path owns the process spawn, so tests and portable builds can
    /// assert the intent without fabricating a helper.
    fn request_new_tab(&mut self, engine: BrowserEngine) {
        self.open_requested
            .push_back(TabOpenIntent::NewForeground(engine));
    }

    fn request_new_tab_with_url(&mut self, engine: BrowserEngine, url: String) {
        if let Some(page) = BrowserInternalPage::from_url(&url) {
            self.open_or_focus_internal_page(page);
            return;
        }
        let prompt_plain_http = is_plain_http(&url) && !browser_internal_plain_http_new_tab(&url);
        if prompt_plain_http {
            if host_of(&url).is_some_and(|h| self.hsts_hosts.contains(&h)) {
                let upgraded = https_upgrade(&url);
                self.publish_insecure_navigation(
                    engine,
                    &url,
                    "",
                    "auto_upgrade",
                    "new_tab",
                    "session_hsts",
                    Some(&upgraded),
                    unix_ms(),
                );
                self.request_new_tab_with_url(engine, upgraded);
                return;
            }
        }
        if let Some(block) = self.managed_policy_block_for(&url) {
            self.block_managed_navigation(block, ManagedPolicyBlockTrigger::NewTab, Some(engine));
            return;
        }
        if prompt_plain_http {
            self.prompt_insecure_navigation(url, InsecureNavigationTarget::NewTab(engine));
            return;
        }
        self.managed_policy_block = None;
        self.queue_new_tab_url(engine, url);
    }

    fn queue_new_tab_url(&mut self, engine: BrowserEngine, url: String) {
        self.open_requested
            .push_back(TabOpenIntent::NewForegroundUrl { engine, url });
    }

    /// Retain a closing tab on the bounded, session-only reopen stack. Blank
    /// sessions (no committed URL yet) are skipped — there is nothing to
    /// restore. The stack never leaves this process (Q74/Q80): no persistence,
    /// no snapshot, no Bus publish.
    fn remember_closed_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.internal_page.is_some() {
            return;
        }
        let url = tab.session.nav().url.trim().to_owned();
        if url.is_empty() {
            return;
        }
        self.closed_tabs.push(ClosedTab {
            url,
            title: tab.session.title().trim().to_owned(),
            engine: tab.engine,
        });
        if self.closed_tabs.len() > CLOSED_TAB_STACK_CAP {
            self.closed_tabs.remove(0);
        }
    }

    /// Reopen the most recently closed tab (Ctrl+Shift+T / History → Reopen
    /// Closed Tab): pop the reopen stack and enqueue a foreground open of its
    /// URL on its original engine — the exact open seam the tab strip's `+`
    /// buttons use. A drained stack is a silent no-op.
    fn restore_closed_tab(&mut self) {
        if let Some(closed) = self.closed_tabs.pop() {
            self.request_new_tab_with_url(closed.engine, closed.url);
        }
    }

    /// Cycle to the next tab, wrapping past the end (Ctrl+Tab).
    fn select_next_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.select_tab((self.active + 1) % self.tabs.len());
        }
    }

    /// Cycle to the previous tab, wrapping past the start (Ctrl+Shift+Tab).
    fn select_prev_tab(&mut self) {
        if self.tabs.len() > 1 {
            self.select_tab((self.active + self.tabs.len() - 1) % self.tabs.len());
        }
    }

    fn toggle_power_mode(&mut self) {
        self.power_mode = !self.power_mode;
        self.publish_session_snapshot();
    }

    fn open_active_view_source(&mut self) {
        let Some((engine, url)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((tab.engine, url))
            }
        }) else {
            self.capture_notice = Some("View source unavailable: no live page".to_owned());
            return;
        };
        let source_url = if url.starts_with("view-source:") {
            url
        } else {
            format!("view-source:{url}")
        };
        self.request_new_tab_with_url(engine, source_url);
        self.capture_notice = Some("Power mode: opening page source".to_owned());
    }

    fn open_chromium_devtools(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice =
                Some("Chromium DevTools unavailable: no live Chromium page".to_owned());
            return;
        };
        if tab.engine != BrowserEngine::Cef || tab.session.is_crashed() {
            self.capture_notice = Some("Chromium DevTools requires a live Chromium tab".to_owned());
            return;
        }
        let active_url = tab.session.nav().url.trim().to_owned();
        let (url, notice) = match chromium_devtools_frontend_for_active_url(&active_url) {
            Ok(Some(url)) => (
                url,
                "Power mode: opening Chromium DevTools for active page".to_owned(),
            ),
            Ok(None) => (
                CEF_DEVTOOLS_URL.to_owned(),
                "Power mode: opening Chromium DevTools target list".to_owned(),
            ),
            Err(err) => (
                CEF_DEVTOOLS_URL.to_owned(),
                format!("Power mode: opening Chromium DevTools target list ({err})"),
            ),
        };
        self.request_new_tab_with_url(BrowserEngine::Cef, url);
        self.capture_notice = Some(notice);
    }

    /// Apply a Browser session-sync snapshot from the future mesh sync owner by
    /// restoring shell-owned settings and enqueueing tab opens through the existing
    /// live-helper path. The active tab is queued last because each live open
    /// foregrounds the newly attached helper.
    #[cfg(any(test, feature = "live-helper"))]
    fn restore_session_sync_snapshot(&mut self, body: &str) -> Result<usize, String> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|err| format!("session snapshot JSON: {err}"))?;
        if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_session_sync") {
            return Err("session snapshot has the wrong op".to_owned());
        }
        let settings = v.get("settings").unwrap_or(&serde_json::Value::Null);
        if let Some(engine) = settings
            .get("future_engine")
            .and_then(serde_json::Value::as_str)
            .and_then(BrowserEngine::from_wire)
        {
            self.engine = engine;
        }
        if let Some(vertical) = settings
            .get("vertical_tabs")
            .and_then(serde_json::Value::as_bool)
        {
            self.vertical_tabs = vertical;
        } else {
            self.vertical_tabs = DEFAULT_VERTICAL_TABS;
        }
        if let Some(zoom) = settings
            .get("page_zoom_percent")
            .and_then(serde_json::Value::as_u64)
        {
            self.page_zoom_percent = u16::try_from(zoom)
                .unwrap_or(PAGE_ZOOM_MAX)
                .clamp(PAGE_ZOOM_MIN, PAGE_ZOOM_MAX);
        }
        if let Some(find_open) = settings
            .get("find_open")
            .and_then(serde_json::Value::as_bool)
        {
            self.find_open = find_open;
        }
        if let Some(downloads_open) = settings
            .get("downloads_open")
            .and_then(serde_json::Value::as_bool)
        {
            self.downloads_open = downloads_open;
        }
        if let Some(power_mode) = settings
            .get("power_mode")
            .and_then(serde_json::Value::as_bool)
        {
            self.power_mode = power_mode;
        }
        if let Some(speed_dial) = speed_dial_from_settings(settings) {
            self.speed_dial = speed_dial;
        }

        let active_index = v.get("active_index").and_then(serde_json::Value::as_u64);
        let tabs = v
            .get("tabs")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "session snapshot is missing tabs[]".to_owned())?;
        let mut restore_tabs = Vec::new();
        for (fallback_index, tab) in tabs.iter().enumerate() {
            let url = tab
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if url.is_empty() {
                continue;
            }
            let engine = tab
                .get("engine")
                .and_then(serde_json::Value::as_str)
                .and_then(BrowserEngine::from_wire)
                .unwrap_or(self.engine);
            let index = tab
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(fallback_index as u64);
            restore_tabs.push((index, engine, url.to_owned()));
        }
        restore_tabs.sort_by_key(|(index, _, _)| *index);
        if let Some(active_index) = active_index {
            restore_tabs.sort_by_key(|(index, _, _)| (*index == active_index, *index));
        }
        let total_count = restore_tabs.len();
        if total_count > MAX_EAGER_BROWSER_STARTUP_OPEN_TABS {
            let active = active_index
                .and_then(|active_index| {
                    restore_tabs
                        .iter()
                        .position(|(index, _, _)| *index == active_index)
                })
                .map(|pos| restore_tabs.remove(pos));
            let keep_without_active =
                MAX_EAGER_BROWSER_STARTUP_OPEN_TABS.saturating_sub(usize::from(active.is_some()));
            restore_tabs.truncate(keep_without_active);
            if let Some(active) = active {
                restore_tabs.push(active);
            }
            self.capture_notice = Some(format!(
                "Session restore opened {} tabs and skipped {} older tab{} to keep Browser responsive",
                restore_tabs.len(),
                total_count.saturating_sub(restore_tabs.len()),
                plural(total_count.saturating_sub(restore_tabs.len()))
            ));
        }
        self.open_requested.clear();
        let count = restore_tabs.len();
        for (_, engine, url) in restore_tabs {
            self.request_new_tab_with_url(engine, url);
        }
        Ok(count)
    }

    #[cfg(any(test, feature = "live-helper"))]
    fn cap_eager_startup_open_requests(&mut self) {
        let total_count = self.open_requested.len();
        if total_count <= MAX_EAGER_BROWSER_STARTUP_OPEN_TABS {
            return;
        }
        self.open_requested
            .truncate(MAX_EAGER_BROWSER_STARTUP_OPEN_TABS);
        let skipped = total_count.saturating_sub(self.open_requested.len());
        self.capture_notice = Some(format!(
            "Browser startup queued {} tabs and skipped {} queued tab{} to keep Browser responsive",
            self.open_requested.len(),
            skipped,
            plural(skipped)
        ));
    }

    /// One-shot startup restore from the daemon-owned latest snapshot files. The
    /// helper-spawn path drains the resulting open queue, so restore and ordinary
    /// new-tab creation stay on the same code path.
    #[cfg(any(test, feature = "live-helper"))]
    fn restore_startup_session_once(&mut self) -> Option<usize> {
        if self.startup_restore_attempted {
            return None;
        }
        self.startup_restore_attempted = true;
        let host = local_hostname();
        for root in self.session_restore_roots.clone() {
            let path = session_sync_latest_path(&root, &host);
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            match self.restore_session_sync_snapshot(&body) {
                Ok(0) => continue,
                Ok(count) => return Some(count),
                Err(_) => continue,
            }
        }
        None
    }

    fn drain_incoming_send_tabs(&mut self) -> usize {
        let host = local_hostname();
        let sanitized_host = sanitize_session_host(&host);
        let mut opened = 0;
        let mut seen = BTreeSet::new();
        for root in self.session_restore_roots.clone() {
            let inbox = send_tab_inbox_dir(&root, &host);
            for path in incoming_send_tab_files(&root, &host) {
                let key = path
                    .strip_prefix(&inbox)
                    .map(|rel| rel.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.to_string_lossy().to_string());
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let record_id = send_tab_consumed_record_id(&key, &body);
                if !seen.insert(record_id.clone())
                    || self.consumed_send_tab_records.contains(&record_id)
                    || send_tab_record_is_consumed(&self.session_restore_roots, &host, &record_id)
                {
                    self.remember_consumed_send_tab(&host, &record_id);
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                match browser_send_tab_open_intent(&body, &sanitized_host) {
                    Ok(BrowserSendTabOpenDecision::Open(engine, url)) => {
                        self.request_new_tab_with_url(engine, url);
                        self.remember_consumed_send_tab(&host, &record_id);
                        let _ = std::fs::remove_file(&path);
                        opened += 1;
                    }
                    Ok(BrowserSendTabOpenDecision::Consume) => {
                        self.remember_consumed_send_tab(&host, &record_id);
                        let _ = std::fs::remove_file(&path);
                    }
                    Err(_) => {
                        self.remember_consumed_send_tab(&host, &record_id);
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            cleanup_empty_send_tab_source_dirs(&root, &host);
        }
        opened
    }

    fn remember_consumed_send_tab(&mut self, host: &str, record_id: &str) {
        self.consumed_send_tab_records.insert(record_id.to_owned());
        let _ = write_send_tab_consumed_marker(&self.session_restore_roots, host, record_id);
    }

    fn poll_incoming_send_tabs(&mut self) {
        if self
            .incoming_send_tab_last_poll
            .is_some_and(|last| last.elapsed() < SEND_TAB_POLL_INTERVAL)
        {
            return;
        }
        self.incoming_send_tab_last_poll = Some(Instant::now());
        self.drain_incoming_send_tabs();
    }

    fn poll_voice_command_results(&mut self) {
        if self
            .voice_command_result_last_poll
            .is_some_and(|last| last.elapsed() < VOICE_COMMAND_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.voice_command_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_voice_command_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.voice_command_result_cursor.as_deref())
        else {
            return;
        };
        for msg in msgs {
            self.voice_command_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(result) = parse_voice_transcript_result(body) else {
                continue;
            };
            self.apply_voice_transcript_result(result);
        }
    }

    /// BOOKMARKS-BAR — mirror the daemon's converged bookmark [`mde_bookmarks::Collection`]
    /// from `state/bookmarks/collection` into the bar's top-level link row. The
    /// EXACT cursor-based `list_since` idiom the sibling pollers use: read every new
    /// retained snapshot since the last cursor, keep the LAST one (the topic is
    /// retained-latest), and fold it into flat bar links. A `state/*` mirror only —
    /// the mackesd bookmarks worker stays the single writer of the op-log (§6).
    fn poll_bookmarks_collection(&mut self) {
        if self
            .bookmarks_collection_last_poll
            .is_some_and(|last| last.elapsed() < BOOKMARKS_COLLECTION_POLL_INTERVAL)
        {
            return;
        }
        self.bookmarks_collection_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let Ok(msgs) = persist.list_since(
            STATE_BOOKMARKS_COLLECTION,
            self.bookmarks_collection_cursor.as_deref(),
        ) else {
            return;
        };
        for msg in msgs {
            self.bookmarks_collection_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            if let Ok(collection) = serde_json::from_str::<mde_bookmarks::Collection>(body) {
                self.bookmark_bar_links = bookmark_bar_links_from(&collection);
                let all = all_bookmarks(&collection);
                self.bookmarked_urls = bookmarked_url_set(&all);
                self.bookmark_index = all;
            }
        }
    }

    /// Toggle the bookmarks bar (View → Show/Hide Bookmarks Bar). Session-only, like
    /// the vertical-tabs and downloads chrome toggles.
    fn toggle_bookmarks_bar(&mut self) {
        self.bookmarks_bar_visible = !self.bookmarks_bar_visible;
    }

    fn poll_speech_statuses(&mut self) {
        if self
            .speech_status_last_poll
            .is_some_and(|last| last.elapsed() < SPEECH_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.speech_status_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let host = local_hostname();
        let read_topic = browser_read_aloud_status_topic(&host);
        if let Ok(msgs) = persist.list_since(&read_topic, None) {
            for msg in msgs {
                let Some(body) = msg.body.as_deref() else {
                    continue;
                };
                if let Ok(status) = parse_read_aloud_status(body) {
                    self.latest_read_aloud_status = Some(status);
                }
            }
        }
        let voice_topic = browser_voice_command_status_topic(&host);
        if let Ok(msgs) = persist.list_since(&voice_topic, None) {
            for msg in msgs {
                let Some(body) = msg.body.as_deref() else {
                    continue;
                };
                if let Ok(status) = parse_voice_command_status(body) {
                    self.latest_voice_command_status = Some(status);
                }
            }
        }
    }

    fn poll_passkey_status(&mut self) {
        if self
            .passkey_status_last_poll
            .is_some_and(|last| last.elapsed() < PASSKEY_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.passkey_status_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_passkey_status_topic(&local_hostname());
        let Ok(mut msgs) = persist.list_since(&topic, None) else {
            return;
        };
        let Some(msg) = msgs.pop() else {
            return;
        };
        let Some(body) = msg.body.as_deref() else {
            return;
        };
        let Ok(status) = parse_passkey_status(body) else {
            return;
        };
        self.latest_passkey_status = Some(status);
    }

    fn poll_passkey_results(&mut self) {
        if self
            .passkey_result_last_poll
            .is_some_and(|last| last.elapsed() < PASSKEY_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.passkey_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_passkey_event_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.passkey_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.passkey_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(completion) = parse_passkey_completion(body) else {
                continue;
            };
            let Some(tab_id) = self
                .pending_passkey_requests
                .remove(&completion.client_request_id)
            else {
                continue;
            };
            let Some(tab_index) = self.tab_index_by_id(tab_id) else {
                continue;
            };
            let Some(tab) = self.tabs.get_mut(tab_index) else {
                continue;
            };
            tab.session.complete_passkey(completion.body);
            self.capture_notice = Some("Passkey: returned result to page".to_owned());
        }
    }

    fn poll_share_results(&mut self) {
        if self
            .share_result_last_poll
            .is_some_and(|last| last.elapsed() < SHARE_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.share_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_share_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.share_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.share_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(route) = parse_share_route_result(body) else {
                continue;
            };
            self.apply_share_route_result(route);
        }
    }

    fn poll_translation_results(&mut self) {
        if self
            .translation_result_last_poll
            .is_some_and(|last| last.elapsed() < TRANSLATION_RESULT_POLL_INTERVAL)
        {
            return;
        }
        self.translation_result_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_translation_result_topic(&local_hostname());
        let Ok(msgs) = persist.list_since(&topic, self.translation_result_cursor.as_deref()) else {
            return;
        };
        for msg in msgs {
            self.translation_result_cursor = Some(msg.ulid.clone());
            let Some(body) = msg.body.as_deref() else {
                continue;
            };
            let Ok(result) = parse_translation_result(body) else {
                continue;
            };
            self.apply_translation_result(result);
        }
    }

    fn poll_security_update_status(&mut self) {
        self.refresh_security_update_status(false);
    }

    fn refresh_security_update_status(&mut self, force: bool) {
        if !force
            && self
                .security_update_last_poll
                .is_some_and(|last| last.elapsed() < SECURITY_UPDATE_STATUS_POLL_INTERVAL)
        {
            return;
        }
        self.security_update_last_poll = Some(Instant::now());
        let Some(root) = self.bus_root.as_deref() else {
            return;
        };
        let Ok(persist) = Persist::open(root.to_path_buf()) else {
            return;
        };
        let topic = browser_security_update_status_topic(&local_hostname());
        let Ok(mut msgs) = persist.list_since(&topic, None) else {
            return;
        };
        let Some(msg) = msgs.pop() else {
            return;
        };
        let Some(body) = msg.body.as_deref() else {
            return;
        };
        let Ok(status) = parse_security_update_status(body) else {
            return;
        };
        self.latest_security_update = Some(status);
    }

    #[cfg(feature = "live-helper")]
    fn refresh_security_update_status_for_launch(&mut self) {
        self.refresh_security_update_status(true);
    }

    fn apply_translation_result(&mut self, result: BrowserTranslationResult) {
        if result.host != local_hostname() {
            return;
        }
        let chars = result.translation.chars().count();
        self.capture_notice = Some(format!(
            "Translation ready: {} character{}",
            chars,
            plural(chars)
        ));
        self.latest_translation = Some(result);
    }

    fn apply_share_route_result(&mut self, route: BrowserShareRouteResult) {
        if route.host != local_hostname() || route.target != BrowserShareTarget::Qr {
            return;
        }
        match qr_share_result(route) {
            Ok(result) => {
                self.capture_notice = Some("QR share ready".to_owned());
                self.latest_qr_share = Some(result);
            }
            Err(err) => {
                self.capture_notice = Some(format!("QR share unavailable: {err}"));
            }
        }
    }

    fn apply_voice_transcript_result(&mut self, result: VoiceTranscriptResult) {
        if result.host != local_hostname() {
            return;
        }
        match result.mode {
            VoiceCommandMode::Dictation => self.apply_voice_dictation(result),
            VoiceCommandMode::Command => self.apply_voice_command(&result.transcript),
        }
    }

    fn apply_voice_dictation(&mut self, result: VoiceTranscriptResult) {
        let Some(tab) = self.tabs.get_mut(result.tab_index) else {
            self.capture_notice =
                Some("Dictation result ignored: tab is no longer open".to_owned());
            return;
        };
        if result.focus != "page" || !tab.page_focused {
            self.capture_notice =
                Some("Dictation result ready: focus the page before dictating".to_owned());
            return;
        }
        let event = egui::Event::Text(result.transcript.clone());
        tab.session.send_input(&event, 1.0);
        tab.last_activity = Instant::now();
        self.capture_notice = Some(format!(
            "Dictation inserted {} character{}",
            result.transcript.chars().count(),
            plural(result.transcript.chars().count())
        ));
    }

    fn apply_voice_command(&mut self, transcript: &str) {
        let Some(action) = voice_command_action(transcript) else {
            self.capture_notice = Some(format!(
                "Voice command not recognized: {}",
                ellipsize(transcript.trim(), 48)
            ));
            return;
        };
        match action {
            BrowserVoiceAction::NewTab => {
                self.request_new_tab(self.engine);
                self.capture_notice = Some("Voice command: new tab".to_owned());
            }
            BrowserVoiceAction::CloseTab => {
                self.close_tab(self.active);
                self.capture_notice = Some("Voice command: close tab".to_owned());
            }
            BrowserVoiceAction::Back => {
                if let Some(tab) = self.active_tab() {
                    tab.session.go_back();
                    self.mark_active_tab_activity();
                    self.capture_notice = Some("Voice command: back".to_owned());
                }
            }
            BrowserVoiceAction::Forward => {
                if let Some(tab) = self.active_tab() {
                    tab.session.go_forward();
                    self.mark_active_tab_activity();
                    self.capture_notice = Some("Voice command: forward".to_owned());
                }
            }
            BrowserVoiceAction::Reload => {
                let crashed = self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.session.is_crashed());
                if crashed {
                    self.respawn_requested = true;
                } else if let Some(tab) = self.active_tab() {
                    tab.session.reload();
                    self.mark_active_tab_activity();
                }
                self.capture_notice = Some("Voice command: reload".to_owned());
            }
            BrowserVoiceAction::ReadAloud => {
                self.request_active_read_aloud();
            }
            BrowserVoiceAction::Find(query) => {
                self.find_open = true;
                self.find_query = query;
                self.submit_find(false);
                self.capture_notice = Some("Voice command: find".to_owned());
            }
        }
    }

    fn request_bookmarks_manager(&mut self) {
        self.open_bookmarks_requested = true;
    }

    pub(crate) fn take_bookmarks_manager_request(&mut self) -> bool {
        std::mem::take(&mut self.open_bookmarks_requested)
    }

    /// Drain a pending new-tab request.
    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "drained by the live-helper shell path and tests")
    )]
    fn take_open_request(&mut self) -> Option<TabOpenIntent> {
        self.open_requested.pop_front()
    }

    /// Select an existing tab. Out-of-range indices are ignored so stale UI events
    /// cannot panic after a close.
    fn select_tab(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            self.mark_tab_active(index);
            self.sync_address_from_active();
            self.publish_session_snapshot();
        }
    }

    /// Close a tab and keep a stable active index. The helper child is killed by
    /// `WebSession`'s `Drop`, so this is the real process-teardown path. The
    /// closing tab's URL is retained on the in-memory reopen stack first, so
    /// every close affordance (strip ×, context menu, middle-click, Ctrl+W,
    /// voice) feeds Ctrl+Shift+T through this one seam.
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let closing_tab_id = self.tabs[index].id;
        self.remember_closed_tab(index);
        self.tabs.remove(index);
        if self
            .pending_login_save
            .as_ref()
            .is_some_and(|pending| pending.tab_id == closing_tab_id)
        {
            self.pending_login_save = None;
        }
        if self
            .pending_passkey_consent
            .as_ref()
            .is_some_and(|pending| pending.tab_id == closing_tab_id)
        {
            self.pending_passkey_consent = None;
        }
        self.pending_passkey_requests
            .retain(|_, tab_id| *tab_id != closing_tab_id);
        if self.tabs.is_empty() {
            self.active = 0;
            self.address.clear();
        } else if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
            self.sync_address_from_active();
        } else if index <= self.active {
            self.active = self.active.saturating_sub(1);
            self.sync_address_from_active();
        }
        self.publish_session_snapshot();
    }

    /// Move one tab to a new index while preserving which session is active.
    fn move_tab(&mut self, from: usize, to: usize) {
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let active_tab = self.active;
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = if active_tab == from {
            to
        } else if from < active_tab && to >= active_tab {
            active_tab.saturating_sub(1)
        } else if from > active_tab && to <= active_tab {
            active_tab + 1
        } else {
            active_tab
        };
        // A drag that crossed the pinned/unpinned boundary snaps back to its
        // cluster, keeping pinned tabs at the front (Chrome's invariant).
        self.sort_pinned_stable();
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    /// Pin or unpin the tab at `index`, then re-cluster so pinned tabs sit at the
    /// front of the strip. No-op when the flag already matches.
    fn set_tab_pinned(&mut self, index: usize, pinned: bool) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        if tab.pinned == pinned {
            return;
        }
        tab.pinned = pinned;
        self.sort_pinned_stable();
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    /// Re-establish the pinned-first invariant: pinned tabs cluster at the front in
    /// their existing relative order, unpinned follow in theirs. A *stable*
    /// partition, so a same-cluster reorder is preserved while a cross-boundary
    /// drag snaps back. Tracks which session stays active across the permutation.
    /// Early-returns when the strip is already partitioned (the common case) so a
    /// plain in-cluster drag costs no allocation.
    fn sort_pinned_stable(&mut self) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        let boundary = self.tabs.iter().take_while(|t| t.pinned).count();
        if self.tabs.iter().skip(boundary).all(|t| !t.pinned) {
            return; // already pinned-first
        }
        let active = self.active;
        let mut order: Vec<usize> = (0..n).collect();
        // Stable sort by `!pinned`: pinned (key `false`) before unpinned (`true`);
        // equal keys keep their ascending original order (Rust `sort_by_key` is stable).
        order.sort_by_key(|&i| !self.tabs[i].pinned);
        let new_active = order.iter().position(|&i| i == active).unwrap_or(0);
        let mut slots: Vec<Option<Tab>> = self.tabs.drain(..).map(Some).collect();
        self.tabs = order.iter().map(|&i| slots[i].take().unwrap()).collect();
        self.active = new_active;
    }

    /// Duplicate the tab at `index` into a fresh foreground tab on the same engine
    /// and URL (Chrome's "Duplicate tab"), through the exact open seam the `+`
    /// buttons use. A blank tab duplicates to a new blank tab.
    fn duplicate_tab(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if let Some(page) = tab.internal_page {
            self.open_or_focus_internal_page(page);
            return;
        }
        let engine = tab.engine;
        let url = tab.session.nav().url.trim().to_owned();
        if url.is_empty() {
            self.request_new_tab(engine);
        } else {
            self.request_new_tab_with_url(engine, url);
        }
    }

    /// Close every tab except `keep` — and except pinned tabs, which Chrome's
    /// "Close other tabs" always spares. Closes right-to-left so indices stay valid;
    /// closed non-blank tabs land on the reopen stack; `keep` stays active.
    fn close_other_tabs(&mut self, keep: usize) {
        if keep >= self.tabs.len() {
            return;
        }
        let mut keep = keep;
        for i in (0..self.tabs.len()).rev() {
            if i != keep && !self.tabs[i].pinned {
                self.close_tab(i);
                if i < keep {
                    keep -= 1; // a removal left of `keep` shifts it down one
                }
            }
        }
        self.select_tab(keep);
    }

    /// Close every non-pinned tab to the right of `from` (Chrome's "Close tabs to
    /// the right"). Right-to-left so indices stay valid; pinned tabs are spared.
    fn close_tabs_to_the_right(&mut self, from: usize) {
        if from >= self.tabs.len() {
            return;
        }
        for i in (from + 1..self.tabs.len()).rev() {
            if !self.tabs[i].pinned {
                self.close_tab(i);
            }
        }
    }

    /// Put the tab at `index` into a fresh tab group (Chrome's "Add tab to new
    /// group"), minting the group with a cycled color and a default name.
    fn new_group_from_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let group_index = self.tab_groups.len();
        self.tab_groups.push(TabGroup {
            name: format!("Group {}", group_index + 1),
            color: tab_group_color(group_index),
        });
        self.tabs[index].group = Some(group_index);
    }

    /// Remove the tab at `index` from its group (leaves the group itself; other tabs
    /// keep their membership since group indices must stay stable).
    fn ungroup_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.group = None;
        }
    }

    fn set_vertical_tabs(&mut self, enabled: bool) {
        self.vertical_tabs = enabled;
        self.publish_session_snapshot();
    }

    fn toggle_vertical_tabs(&mut self) {
        self.set_vertical_tabs(!self.vertical_tabs);
    }

    fn select_engine(&mut self, engine: BrowserEngine) {
        if self.engine == engine {
            return;
        }
        self.engine = engine;
        self.publish_session_snapshot();
    }

    fn submit_address(&mut self) {
        let crashed = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.session.is_crashed());
        if self.tabs.is_empty() || crashed {
            return;
        }
        // A keyword shortcut ("img sunset") wins over the default URL/search router.
        let Some(url) = keyword_search_target(&self.address, &self.search_engines)
            .or_else(|| omnibox_target(&self.address))
        else {
            return;
        };
        self.suggestions.clear();
        self.address = url.clone();
        self.load_target(url);
    }

    fn load_target(&mut self, url: String) {
        if let Some(page) = BrowserInternalPage::from_url(&url) {
            self.open_or_focus_internal_page(page);
            return;
        }
        if is_plain_http(&url) {
            // Session HSTS: a host the user already upgraded auto-upgrades silently
            // (the one-shot recursion re-enters with an https URL, so is_plain_http
            // is false and it falls through to the normal load).
            if host_of(&url).is_some_and(|h| self.hsts_hosts.contains(&h)) {
                let upgraded = https_upgrade(&url);
                let engine = self
                    .tabs
                    .get(self.active)
                    .map_or(self.engine, |tab| tab.engine);
                let title = self
                    .tabs
                    .get(self.active)
                    .map_or(String::new(), |tab| tab.session.title().to_owned());
                self.publish_insecure_navigation(
                    engine,
                    &url,
                    &title,
                    "auto_upgrade",
                    "active_tab",
                    "session_hsts",
                    Some(&upgraded),
                    unix_ms(),
                );
                self.load_target(upgraded);
                return;
            }
        }
        if let Some(block) = self.managed_policy_block_for(&url) {
            self.block_managed_navigation(block, ManagedPolicyBlockTrigger::ChromeLoad, None);
            return;
        }
        if is_plain_http(&url) {
            self.prompt_insecure_navigation(url, InsecureNavigationTarget::ActiveTab);
            return;
        }
        if let Some(protocol) = ExternalProtocol::from_url(&url) {
            self.clear_insecure_prompt();
            self.managed_policy_block = None;
            self.publish_external_protocol(protocol, &url);
            return;
        }
        if let Some(index) = self.clear_active_internal_page_for_load(&url) {
            self.clear_insecure_prompt();
            self.managed_policy_block = None;
            self.open_requested
                .push_back(TabOpenIntent::ReplaceActiveUrl {
                    index,
                    engine: self.engine,
                    url,
                });
            self.publish_session_snapshot();
            return;
        }
        self.clear_insecure_prompt();
        self.managed_policy_block = None;
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(url);
        }
    }

    /// BOOKMARKS-BAR — open a bar bookmark. A plain click navigates the active tab
    /// (`load_target`, syncing the omnibox like the toolbar Go button); a
    /// middle-click — or a click with no live tab to reuse — opens it in a new
    /// foreground tab on the preferred engine (`request_new_tab_with_url`), the same
    /// two open seams the tab strip and History reopen already use.
    fn open_bookmark(&mut self, url: String, new_tab: bool) {
        if url.trim().is_empty() {
            return;
        }
        if new_tab || self.tabs.is_empty() {
            self.request_new_tab_with_url(self.engine, url);
            return;
        }
        self.address = url.clone();
        self.load_target(url);
    }

    fn publish_external_protocol(&mut self, protocol: ExternalProtocol, url: &str) {
        match protocol {
            ExternalProtocol::Tel => {
                let body = voice_dial_body(url);
                publish_to_bus(self.bus_root.as_deref(), ACTION_VOICE_DIAL, &body);
            }
            ExternalProtocol::Mailto | ExternalProtocol::Magnet => {
                let body = browser_protocol_handoff_body(protocol, url);
                publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_PROTOCOL, &body);
            }
        }
        self.capture_notice = Some(format!(
            "Handed {} link to {}",
            protocol.scheme(),
            protocol.target_label()
        ));
    }

    fn submit_dashboard_search(&mut self) {
        let q = self.dashboard_query.trim();
        if q.is_empty() {
            return;
        }
        let url = keyword_search_target(q, &self.search_engines)
            .unwrap_or_else(|| format!("{DEFAULT_SEARCH_URL}?q={}", percent_encode_query(q)));
        self.address = url.clone();
        self.load_target(url);
    }

    fn open_mesh_service(&mut self, url: String) {
        self.address = url.clone();
        self.load_target(url);
    }

    fn prompt_insecure_navigation(&mut self, url: String, target: InsecureNavigationTarget) {
        let (engine, title) = self.insecure_navigation_context(target);
        let upgraded = https_upgrade(&url);
        self.insecure_prompt = Some(url.clone());
        self.insecure_prompt_target = target;
        self.publish_insecure_navigation(
            engine,
            &url,
            &title,
            "prompt",
            target.wire(),
            "navigation_prompt",
            Some(&upgraded),
            unix_ms(),
        );
    }

    fn clear_insecure_prompt(&mut self) {
        self.insecure_prompt = None;
        self.insecure_prompt_target = InsecureNavigationTarget::ActiveTab;
    }

    fn take_insecure_prompt(&mut self) -> Option<(String, InsecureNavigationTarget)> {
        let url = self.insecure_prompt.take()?;
        let target = self.insecure_prompt_target;
        self.insecure_prompt_target = InsecureNavigationTarget::ActiveTab;
        Some((url, target))
    }

    fn insecure_navigation_context(
        &self,
        target: InsecureNavigationTarget,
    ) -> (BrowserEngine, String) {
        if let Some(engine) = target.engine_override() {
            return (engine, String::new());
        }
        self.tabs
            .get(self.active)
            .map_or((self.engine, String::new()), |tab| {
                (tab.engine, tab.session.title().to_owned())
            })
    }

    fn resume_insecure_navigation(&mut self, target: InsecureNavigationTarget, url: String) {
        match target {
            InsecureNavigationTarget::ActiveTab => {
                self.address = url.clone();
                self.mark_active_tab_activity();
                if let Some(tab) = self.active_tab() {
                    tab.session.load(url);
                }
            }
            InsecureNavigationTarget::NewTab(engine) => {
                self.queue_new_tab_url(engine, url);
            }
        }
    }

    fn continue_insecure_load(&mut self) {
        let Some((url, target)) = self.take_insecure_prompt() else {
            return;
        };
        if let Some(block) = self.managed_policy_block_for(&url) {
            self.block_managed_navigation(
                block,
                ManagedPolicyBlockTrigger::HttpContinue,
                target.engine_override(),
            );
            return;
        }
        let (engine, title) = self.insecure_navigation_context(target);
        self.publish_insecure_navigation(
            engine,
            &url,
            &title,
            "continue",
            target.wire(),
            "navigation_prompt",
            None,
            unix_ms(),
        );
        self.managed_policy_block = None;
        self.resume_insecure_navigation(target, url);
    }

    fn upgrade_insecure_load(&mut self) {
        let Some((url, target)) = self.take_insecure_prompt() else {
            return;
        };
        // Remember this host for the session so we auto-upgrade it next time (HSTS).
        if let Some(host) = host_of(&url) {
            self.hsts_hosts.insert(host);
        }
        let upgraded = https_upgrade(&url);
        if let Some(block) = self.managed_policy_block_for(&upgraded) {
            self.block_managed_navigation(
                block,
                ManagedPolicyBlockTrigger::HttpsUpgrade,
                target.engine_override(),
            );
            return;
        }
        let (engine, title) = self.insecure_navigation_context(target);
        self.publish_insecure_navigation(
            engine,
            &url,
            &title,
            "upgrade",
            target.wire(),
            "navigation_prompt",
            Some(&upgraded),
            unix_ms(),
        );
        self.managed_policy_block = None;
        self.resume_insecure_navigation(target, upgraded);
    }

    fn cancel_insecure_load(&mut self) {
        let Some((url, target)) = self.take_insecure_prompt() else {
            return;
        };
        let (engine, title) = self.insecure_navigation_context(target);
        self.publish_insecure_navigation(
            engine,
            &url,
            &title,
            "cancel",
            target.wire(),
            "navigation_prompt",
            None,
            unix_ms(),
        );
    }

    /// Clear ALL browsing data in one front-door action (Privacy → Clear all
    /// browsing data): the session-only history, every download from the list, the
    /// reopen-closed-tab stack, and the active tab's session state — the clears that
    /// were previously scattered across three separate drawers/menus. Everything here
    /// is session-only by design (nothing was ever persisted — Q74/Q80), so this
    /// forgets in-memory state rather than wiping a disk profile.
    fn clear_all_browsing_data(&mut self) {
        let counts = BrowserBrowsingDataClearCounts {
            history_entries: self.history.visits().count(),
            downloads: self.download_jobs.len(),
            reopen_entries: self.closed_tabs.len(),
            saved_logins: self.session_logins.len(),
            permission_grants: self.granted_permissions.len(),
        };
        let (engine, active_url, active_title, active_host) =
            self.tabs.get(self.active).map_or_else(
                || (self.engine, String::new(), String::new(), String::new()),
                |tab| {
                    let url = tab.session.nav().url.trim().to_owned();
                    let host = host_of(&url).unwrap_or_default();
                    (tab.engine, url, tab.session.title().to_owned(), host)
                },
            );
        let cleared_ms = unix_ms();
        self.history.clear();
        self.dismiss_all_downloads();
        self.closed_tabs.clear();
        // Site data: saved logins + per-site permission grants are browsing data too,
        // so "Clear All Browsing Data" forgets them (a granted site then re-prompts;
        // a saved login must be re-entered). HSTS is deliberately NOT cleared — it's a
        // security upgrade, and forgetting it would downgrade a site back to plain http.
        self.session_logins.clear();
        self.granted_permissions.clear();
        self.login_user_draft.clear();
        self.login_pass_draft.clear();
        self.pending_login_save = None;
        self.clear_active_session_data();
        self.publish_browsing_data_clear(
            engine,
            &active_url,
            &active_title,
            &active_host,
            counts,
            cleared_ms,
        );
    }

    fn clear_active_session_data(&mut self) {
        let cleared_site = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            let host = host_of(&url)?;
            Some((host, tab.engine, url, tab.session.title().to_owned()))
        });
        self.mark_active_tab_activity();
        self.clear_insecure_prompt();
        self.dashboard_query.clear();
        self.find_query.clear();
        self.find_open = false;
        self.page_zoom_percent = 100;
        self.suggestions.clear();
        self.address = NEW_TAB_URL.to_owned();
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.texture = None;
            tab.last_frame = None;
            tab.muted = false;
            tab.autoplay_blocked = DEFAULT_AUTOPLAY_BLOCKED;
            tab.force_dark = false;
            tab.reader_mode = false;
            tab.user_scripts = false;
            tab.user_agent = UserAgentOverride::Default;
            tab.device_profile = DeviceProfile::Default;
            tab.session.load(NEW_TAB_URL);
            tab.session.set_zoom(self.page_zoom_percent);
            tab.session.clear_find();
            tab.session.set_audio_muted(false);
            tab.session.set_autoplay_blocked(DEFAULT_AUTOPLAY_BLOCKED);
            tab.session.set_force_dark(false);
            tab.session.set_reader_mode(false);
            tab.session.set_user_scripts(false, "");
            tab.session.set_user_agent("");
            tab.session
                .set_device_profile(DeviceProfile::Default.wire(), 0, 0, 100, false);
        }
        if let Some((host, engine, url, title)) = cleared_site {
            let cleared_ms = unix_ms();
            self.site_data.mark_cleared(&host, cleared_ms);
            self.publish_site_data_clear(engine, &url, &title, &host, "current_tab", cleared_ms);
        }
    }

    fn active_tab_has_frame(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            tab.internal_page.is_none() && tab.last_frame.is_some() && !tab.session.is_crashed()
        })
    }

    fn capture_active_viewport(&mut self) {
        match self.capture_active_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_full_page(&mut self) {
        match self.capture_active_full_page_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured full page", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_mhtml(&mut self) {
        match self.capture_active_mhtml_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured web archive", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_annotated_viewport(&mut self) {
        match self.capture_active_annotated_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured annotated", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_callout_viewport(&mut self) {
        match self.capture_active_callout_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured callout", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn capture_active_freehand_viewport(&mut self) {
        match self.capture_active_freehand_viewport_to_dir(browser_capture_dir()) {
            Ok(path) => {
                self.record_capture_success("Captured freehand", &path);
            }
            Err(err) => {
                self.capture_notice = Some(format!("Capture failed: {err}"));
            }
        }
    }

    fn export_active_media_manifest(&mut self) {
        match self
            .export_active_media_manifest_to_dirs(browser_media_spool_dir(), browser_capture_dir())
        {
            Ok(id) => {
                self.capture_notice = Some(Self::media_export_queued_notice(&id));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(Self::media_export_failed_notice(&err));
            }
        }
    }

    fn export_active_media_manifest_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<String, String> {
        let Some((url, title, engine, resources)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((
                    url,
                    tab.session.title().to_owned(),
                    tab.engine,
                    tab.session.recent_resource_requests(),
                ))
            }
        }) else {
            return Err("no live page to sniff".to_owned());
        };
        let now = unix_ms();
        std::fs::create_dir_all(&spool_dir)
            .map_err(|err| format!("create media spool dir: {err}"))?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|err| format!("create media destination dir: {err}"))?;
        let body = active_page_media_manifest(&url, &title, engine, now, &resources)?;
        let path = spool_dir.join(media_manifest_filename_for(&url, &title, now));
        std::fs::write(&path, body)
            .map_err(|err| format!("write media manifest {}: {err}", path.display()))?;
        enqueue_browser_output(
            self.transfers.as_ref(),
            &path.to_string_lossy(),
            dest_dir.to_string_lossy().as_ref(),
        )
    }

    fn media_export_queued_notice(id: &str) -> String {
        format!("Power mode: queued media list ({id})")
    }

    fn media_export_failed_notice(detail: &str) -> String {
        let label = Self::media_export_error_label(detail);
        if label.is_empty() {
            "Media export failed".to_owned()
        } else {
            format!("Media export failed: {label}")
        }
    }

    fn media_export_error_label(detail: &str) -> String {
        let trimmed = detail.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("no live page") || lower.contains("no active tab") {
            return "no live page".to_owned();
        }
        if lower.contains("write media manifest") {
            return "could not save the media list".to_owned();
        }
        if lower.contains("create media spool dir") {
            return "could not prepare the media export".to_owned();
        }
        if lower.contains("create media destination dir") {
            return "could not open the media export folder".to_owned();
        }
        if lower.contains('/') || lower.contains('\\') || lower.contains("manifest") {
            return "could not complete the media export".to_owned();
        }
        sentence_case_ascii(trimmed)
    }

    fn download_observed_media_assets(&mut self) {
        match self.download_observed_media_assets_to_dirs(
            browser_media_spool_dir(),
            browser_capture_dir(),
        ) {
            Ok(ids) => {
                self.capture_notice = Some(format!(
                    "Power mode: queued observed media downloads ({} assets)",
                    ids.len()
                ));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(Self::media_download_queue_failed_notice("Media", &err));
            }
        }
    }

    fn download_observed_image_assets(&mut self) {
        match self.download_observed_image_assets_to_dirs(
            browser_media_spool_dir(),
            browser_capture_dir(),
        ) {
            Ok(ids) => {
                self.capture_notice = Some(format!(
                    "Power mode: queued observed image downloads ({} assets)",
                    ids.len()
                ));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(Self::media_download_queue_failed_notice("Image", &err));
            }
        }
    }

    fn media_download_queue_failed_notice(kind: &str, detail: &str) -> String {
        let label = Self::media_download_queue_error_label(detail);
        if label.is_empty() {
            format!("{kind} download queue failed")
        } else {
            format!("{kind} download queue failed: {label}")
        }
    }

    fn media_download_queue_error_label(detail: &str) -> String {
        let trimmed = detail.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower.contains("no live page") || lower.contains("no active tab") {
            return "no live page".to_owned();
        }
        if lower.contains("create media download spool dir") {
            return "could not prepare the download staging area".to_owned();
        }
        if lower.contains("create media download destination dir") {
            return "could not open the download folder".to_owned();
        }
        if lower.contains("write media download request")
            || lower.contains(".download.json")
            || lower.contains('/')
            || lower.contains('\\')
        {
            return "could not save the download request".to_owned();
        }
        sentence_case_ascii(trimmed)
    }

    fn download_observed_media_assets_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        self.download_observed_assets_to_dirs(MediaAssetSelection::All, spool_dir, dest_dir)
    }

    fn download_observed_image_assets_to_dirs(
        &mut self,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        self.download_observed_assets_to_dirs(MediaAssetSelection::Images, spool_dir, dest_dir)
    }

    fn download_observed_assets_to_dirs(
        &mut self,
        selection: MediaAssetSelection,
        spool_dir: PathBuf,
        dest_dir: PathBuf,
    ) -> Result<Vec<String>, String> {
        let Some((url, title, engine, resources)) = self.tabs.get(self.active).and_then(|tab| {
            let url = tab.session.nav().url.trim().to_owned();
            if url.is_empty() || tab.session.is_crashed() {
                None
            } else {
                Some((
                    url,
                    tab.session.title().to_owned(),
                    tab.engine,
                    tab.session.recent_resource_requests(),
                ))
            }
        }) else {
            return Err("no live page to download from".to_owned());
        };
        let now = unix_ms();
        std::fs::create_dir_all(&spool_dir)
            .map_err(|err| format!("create media download spool dir: {err}"))?;
        std::fs::create_dir_all(&dest_dir)
            .map_err(|err| format!("create media download destination dir: {err}"))?;
        let requests = active_page_media_asset_requests_with_selection(
            &url, &title, engine, now, &resources, selection,
        )?;
        if requests.is_empty() {
            return Err(selection.empty_error().to_owned());
        }
        let mut sources = Vec::with_capacity(requests.len());
        let mut blocked = 0usize;
        for (index, body) in requests.into_iter().enumerate() {
            let request_url = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["asset_url"].as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| url.clone());
            if let Some(block) = self.managed_policy_block_for(&request_url) {
                blocked += 1;
                self.block_managed_download(block);
                continue;
            }
            if let Some(rule) = self.safe_browsing_download_block_for(&request_url) {
                blocked += 1;
                self.block_safe_browsing_download(&request_url, &rule);
                continue;
            }
            if self.insecure_download_block_for(&request_url) {
                blocked += 1;
                self.block_insecure_download(&request_url);
                continue;
            }
            let path = spool_dir.join(media_asset_request_filename_for(
                &url,
                &title,
                &request_url,
                now,
                index + 1,
            ));
            std::fs::write(&path, body)
                .map_err(|err| format!("write media download request {}: {err}", path.display()))?;
            sources.push(path.to_string_lossy().to_string());
        }
        if sources.is_empty() {
            let suffix = if blocked == 1 { "" } else { "s" };
            return Err(format!(
                "browser policy blocked {blocked} selected download{suffix}"
            ));
        }
        enqueue_browser_output_batch(
            self.transfers.as_ref(),
            &sources,
            dest_dir.to_string_lossy().as_ref(),
        )
    }

    /// B2 — a browser download was intercepted by the engine (which cancelled its
    /// own write). A filename [`download_is_dangerous`] flags is parked in
    /// [`Self::pending_dangerous_download`] instead of being silently handed to
    /// the ledger — the downloads drawer surfaces a Keep/Discard warning, and
    /// only Keep resumes this same path via [`Self::enqueue_download_to_ledger`].
    /// A safe filename submits immediately, unchanged from before.
    fn submit_download_to_ledger(&mut self, id: u64, url: &str, filename: &str) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        let filename = resolve_download_filename(url, filename);
        if let Some(block) = self.managed_policy_block_for(url) {
            self.block_managed_download(block);
            return;
        }
        if let Some(rule) = self.safe_browsing_download_block_for(url) {
            self.block_safe_browsing_download(url, &rule);
            return;
        }
        if self.insecure_download_block_for(url) {
            self.block_insecure_download(url);
            return;
        }
        if download_is_dangerous(&filename) || download_url_path_is_dangerous(url) {
            let pending = PendingDangerousDownload {
                id,
                url: url.to_owned(),
                filename,
            };
            self.publish_download_danger(&pending, "prompt", unix_ms());
            self.pending_dangerous_download = Some(pending);
            self.downloads_open = true;
            return;
        }
        self.enqueue_download_to_ledger(id, url, &filename);
    }

    /// The user confirmed **Keep** on a dangerous-download warning — proceed
    /// exactly as a safe download would.
    fn keep_pending_dangerous_download(&mut self) {
        if let Some(pending) = self.pending_dangerous_download.take() {
            self.publish_download_danger(&pending, "keep", unix_ms());
            if let Some(block) = self.managed_policy_block_for(&pending.url) {
                self.block_managed_download(block);
                return;
            }
            if let Some(rule) = self.safe_browsing_download_block_for(&pending.url) {
                self.block_safe_browsing_download(&pending.url, &rule);
                return;
            }
            if self.insecure_download_block_for(&pending.url) {
                self.block_insecure_download(&pending.url);
                return;
            }
            self.enqueue_download_to_ledger(pending.id, &pending.url, &pending.filename);
        }
    }

    /// The user chose **Discard** on a dangerous-download warning — drop it
    /// with no ledger job ever created.
    fn discard_pending_dangerous_download(&mut self) {
        if let Some(pending) = self.pending_dangerous_download.take() {
            self.publish_download_danger(&pending, "discard", unix_ms());
        }
    }

    /// Write the `.download.json` manifest and enqueue the mesh Transfers job
    /// for a download already cleared to proceed — the daemon's
    /// browser-download lane fetches `asset_url` into the mesh share (the
    /// downloads drawer already renders the resulting `browser_download`
    /// ledger row).
    fn enqueue_download_to_ledger(&mut self, id: u64, url: &str, filename: &str) {
        self.enqueue_download_to_ledger_dirs(
            id,
            url,
            filename,
            browser_media_spool_dir(),
            browser_capture_dir(),
        );
    }

    fn enqueue_download_to_ledger_dirs(
        &mut self,
        id: u64,
        url: &str,
        filename: &str,
        spool: PathBuf,
        dest: PathBuf,
    ) {
        if std::fs::create_dir_all(&spool).is_err() || std::fs::create_dir_all(&dest).is_err() {
            self.capture_notice =
                Some("Download failed: could not prepare the transfer staging area".into());
            return;
        }
        let body = serde_json::json!({
            "op": "browser_media_download_request",
            "asset_url": url,
            "suggested_filename": filename,
        })
        .to_string();
        let path = spool.join(format!("browser-download-{id}-{}.download.json", unix_ms()));
        if std::fs::write(&path, body).is_err() {
            self.capture_notice =
                Some("Download failed: could not write the transfer request".into());
            return;
        }
        match enqueue_browser_output(
            self.transfers.as_ref(),
            &path.to_string_lossy(),
            dest.to_string_lossy().as_ref(),
        ) {
            Ok(job_id) => {
                self.download_source_urls.insert(job_id, url.to_owned());
                self.downloads_open = true;
                self.refresh_downloads();
                self.capture_notice = Some(format!("Downloading {filename} to the mesh share"));
            }
            Err(err) => self.capture_notice = Some(format!("Download failed: {err}")),
        }
    }

    /// Hide one ledger job from the Browser's downloads view without touching
    /// the ledger job itself (the drawer's per-item "Remove from list").
    fn dismiss_download(&mut self, id: &str) {
        self.dismissed_download_ids.insert(id.to_owned());
        self.download_source_urls.remove(id);
        self.download_jobs.retain(|job| job.id != id);
    }

    /// Hide every job currently visible in the downloads drawer (the header's
    /// "Clear all"). New downloads after this point are unaffected.
    fn dismiss_all_downloads(&mut self) {
        for job in &self.download_jobs {
            self.dismissed_download_ids.insert(job.id.clone());
            self.download_source_urls.remove(&job.id);
        }
        self.download_jobs.clear();
    }

    fn record_capture_success(&mut self, label: &str, path: &Path) {
        let notice = format!("{label}: {}", browser_output_label(path));
        self.capture_notice = Some(notice.clone());
        let body = browser_notify_body(Severity::Info, &notice, Some(&path.to_string_lossy()));
        publish_to_bus(self.bus_root.as_deref(), EVENT_NOTIFY_BROWSER, &body);
    }

    fn capture_active_viewport_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let bytes = encode_color_image_png(frame)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = capture_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_full_page_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let bytes = encode_color_image_png(frame)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_full_page_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_mhtml_to_dir(&self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let png = encode_color_image_png(frame)?;
        let mhtml =
            mhtml_capture_document(&tab.session.nav().url, tab.session.title(), now_ms, &png);
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = capture_mhtml_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, mhtml)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_annotated_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_annotated_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_callout_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_callout_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_callout_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn capture_active_freehand_viewport_to_dir(
        &self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let now_ms = unix_ms();
        let caption =
            capture_annotation_caption(&tab.session.nav().url, tab.session.title(), now_ms);
        let annotated = annotate_freehand_capture_image(frame, &caption)?;
        let bytes = encode_color_image_png(&annotated)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_freehand_filename_for(&tab.session.nav().url, tab.session.title(), now_ms);
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn start_region_capture(&mut self) {
        if !self.active_tab_has_frame() {
            self.capture_notice = Some("Capture failed: no painted page".to_owned());
            return;
        }
        self.capture_region_mode = true;
        self.capture_region_start = None;
        self.capture_region_current = None;
        self.capture_notice = Some("Drag a page region to capture".to_owned());
    }

    fn cancel_region_capture(&mut self) {
        self.capture_region_mode = false;
        self.capture_region_start = None;
        self.capture_region_current = None;
    }

    fn capture_active_region_to_dir(
        &self,
        dir: impl AsRef<Path>,
        region: PixelRegion,
    ) -> Result<PathBuf, String> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Err("no active tab".to_owned());
        };
        if tab.session.is_crashed() {
            return Err("active tab is crashed".to_owned());
        }
        let Some(frame) = tab.last_frame.as_ref() else {
            return Err("the active tab has not painted yet".to_owned());
        };
        let cropped = crop_color_image(frame, region)?;
        let bytes = encode_color_image_png(&cropped)?;
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name =
            capture_region_filename_for(&tab.session.nav().url, tab.session.title(), unix_ms());
        let path = dir.join(name);
        std::fs::write(&path, bytes)
            .map_err(|err| format!("could not write {}: {err}", path.display()))?;
        Ok(path)
    }

    fn print_active_page(&mut self) {
        match self.queue_active_page_cups_print_to_dir(browser_print_spool_dir()) {
            Ok(_path) => {
                self.capture_notice = Some("Print job queued".to_owned());
            }
            Err(err) => {
                self.capture_notice = Some(format!("Print failed: {err}"));
            }
        }
    }

    fn toggle_print_settings(&mut self) {
        self.print_settings_open = !self.print_settings_open;
        if self.print_settings_open {
            self.refresh_cups_printers();
        }
    }

    fn handle_pdf_event(&mut self, path: String, ok: bool) -> String {
        if let Some(request) = self.pending_cups_prints.remove(&path) {
            if !ok {
                return "Print failed: PDF could not be created".to_owned();
            }
            return match submit_pdf_to_cups(
                Path::new(&request.path),
                &request.title,
                &request.settings,
            ) {
                Ok(job) if job.trim().is_empty() || job.contains('/') || job.contains('\\') => {
                    "Print job submitted".to_owned()
                }
                Ok(job) => format!("Print job submitted: {}", job.trim()),
                Err(err) => printer_error_label(&err).map_or_else(
                    || "Print failed".to_owned(),
                    |label| format!("Print failed: {label}"),
                ),
            };
        }
        if ok {
            let saved = self.pending_saved_pdfs.remove(&path).unwrap_or_else(|| {
                let (url, title) = self
                    .tabs
                    .get(self.active)
                    .map(|tab| {
                        (
                            tab.session.nav().url.clone(),
                            tab.session.title().to_owned(),
                        )
                    })
                    .unwrap_or_default();
                SavedPdf {
                    path: PathBuf::from(&path),
                    url,
                    title,
                }
            });
            let notice = format!("PDF saved: {}", browser_output_label(&saved.path));
            self.last_saved_pdf = Some(saved);
            notice
        } else {
            let saved_path = self
                .pending_saved_pdfs
                .remove(&path)
                .map(|saved| saved.path)
                .unwrap_or_else(|| PathBuf::from(&path));
            format!("PDF save failed: {}", browser_output_label(&saved_path))
        }
    }

    fn open_last_saved_pdf(&mut self) {
        match self.last_saved_pdf_viewer_url() {
            Ok(url) => {
                self.capture_notice = Some("Opening PDF in Chromium viewer".to_owned());
                self.request_new_tab_with_url(BrowserEngine::Cef, url);
            }
            Err(err) => {
                self.capture_notice = Some(format!("PDF viewer failed: {err}"));
            }
        }
    }

    fn last_saved_pdf_viewer_url(&self) -> Result<String, String> {
        let Some(saved) = &self.last_saved_pdf else {
            return Err("no saved PDF".to_owned());
        };
        let path = &saved.path;
        if !pdf_file_looks_readable(path) {
            return Err("saved PDF is not readable".to_owned());
        }
        file_url_for_path(path)
    }

    fn save_active_page_pdf(&mut self) {
        match self.save_active_page_pdf_to_dir(browser_pdf_dir()) {
            Ok(path) => {
                self.capture_notice = Some(format!(
                    "PDF save requested: {}",
                    browser_output_label(&path)
                ));
            }
            Err(err) => {
                self.capture_notice = Some(pdf_save_failed_notice(&err));
            }
        }
    }

    fn save_active_page_pdf_to_dir(&mut self, dir: impl AsRef<Path>) -> Result<PathBuf, String> {
        if !self.can_drive_page_tools() {
            return Err("no live page".to_owned());
        }
        let (url, title) = {
            let Some(tab) = self.tabs.get(self.active) else {
                return Err("no active tab".to_owned());
            };
            (
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
            )
        };
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let name = pdf_filename_for(&url, &title, unix_ms());
        let path = dir.join(name);
        let key = path.to_string_lossy().into_owned();
        self.pending_saved_pdfs.insert(
            key.clone(),
            SavedPdf {
                path: path.clone(),
                url,
                title,
            },
        );
        if let Some(tab) = self.active_tab() {
            tab.session.save_pdf(key);
        }
        Ok(path)
    }

    fn can_drive_page_tools(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.internal_page.is_none() && !tab.session.is_crashed())
    }

    fn set_page_zoom(&mut self, percent: u16) {
        if !self.can_drive_page_tools() {
            return;
        }
        let percent = percent.clamp(PAGE_ZOOM_MIN, PAGE_ZOOM_MAX);
        self.page_zoom_percent = percent;
        if let Some(tab) = self.active_tab() {
            tab.session.set_zoom(percent);
        }
        self.publish_session_snapshot();
    }

    fn zoom_in(&mut self) {
        let next = self
            .page_zoom_percent
            .saturating_add(PAGE_ZOOM_STEP)
            .min(PAGE_ZOOM_MAX);
        self.set_page_zoom(next);
    }

    fn zoom_out(&mut self) {
        let next = self
            .page_zoom_percent
            .saturating_sub(PAGE_ZOOM_STEP)
            .max(PAGE_ZOOM_MIN);
        self.set_page_zoom(next);
    }

    fn reset_zoom(&mut self) {
        self.set_page_zoom(100);
    }

    fn set_active_tab_muted(&mut self, muted: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.muted = muted;
            tab.session.set_audio_muted(muted);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_mute(&mut self) {
        let muted = self.tabs.get(self.active).is_some_and(|tab| tab.muted);
        self.set_active_tab_muted(!muted);
    }

    pub(crate) fn toggle_active_tab_media_playback(&mut self) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.session.toggle_media_playback();
        }
    }

    #[cfg(test)]
    pub(crate) fn active_tab_media_transport(
        &mut self,
        action: mde_web_preview_client::MediaTransportAction,
    ) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.session.media_transport(action);
        }
    }

    fn set_active_tab_autoplay_blocked(&mut self, blocked: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.autoplay_blocked = blocked;
            tab.session.set_autoplay_blocked(blocked);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_autoplay_blocked(&mut self) {
        let blocked = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.autoplay_blocked);
        self.set_active_tab_autoplay_blocked(!blocked);
    }

    fn set_active_tab_force_dark(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.force_dark = enabled;
            tab.session.set_force_dark(enabled);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_force_dark(&mut self) {
        let enabled = self.tabs.get(self.active).is_some_and(|tab| tab.force_dark);
        self.set_active_tab_force_dark(!enabled);
    }

    fn set_active_tab_reader_mode(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.reader_mode = enabled;
            tab.session.set_reader_mode(enabled);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_reader_mode(&mut self) {
        let enabled = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.reader_mode);
        self.set_active_tab_reader_mode(!enabled);
    }

    /// Add the drafted host + CSS as a user site style (the safe, CSS-only userscript
    /// slice) and re-inject if userscripts are on. Blank drafts are ignored; a
    /// successful add clears the drafts.
    fn add_user_site_style(&mut self) {
        let host = self.site_style_host_draft.trim().to_owned();
        let css = self.site_style_css_draft.trim().to_owned();
        if host.is_empty() || css.is_empty() {
            return;
        }
        self.user_site_styles.push(UserSiteStyle { host, css });
        self.site_style_host_draft.clear();
        self.site_style_css_draft.clear();
        self.reinject_user_scripts_if_active();
    }

    fn remove_user_site_style(&mut self, index: usize) {
        if index < self.user_site_styles.len() {
            self.user_site_styles.remove(index);
            self.reinject_user_scripts_if_active();
        }
    }

    /// Re-push the userscript bundle to the active tab when its userscripts toggle is
    /// on, so a change to the user site styles takes effect immediately.
    fn reinject_user_scripts_if_active(&mut self) {
        if self.tabs.get(self.active).is_some_and(|t| t.user_scripts) {
            self.set_active_tab_user_scripts(true);
        }
    }

    fn set_active_tab_user_scripts(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        let bundle = if enabled {
            curated_userscript_bundle(&self.user_site_styles)
        } else {
            String::new()
        };
        if let Some(tab) = self.active_tab() {
            tab.user_scripts = enabled;
            tab.session.set_user_scripts(enabled, bundle);
        }
        self.publish_session_snapshot();
    }

    fn toggle_active_tab_user_scripts(&mut self) {
        let enabled = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| tab.user_scripts);
        self.set_active_tab_user_scripts(!enabled);
    }

    fn set_active_tab_user_agent(&mut self, user_agent: UserAgentOverride) {
        if !self.can_drive_page_tools() {
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.user_agent = user_agent;
            tab.session.set_user_agent(user_agent.value());
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_user_agent(&mut self) {
        let user_agent = self
            .tabs
            .get(self.active)
            .map_or(UserAgentOverride::Default, |tab| tab.user_agent)
            .next();
        self.set_active_tab_user_agent(user_agent);
    }

    fn set_active_tab_device_profile(&mut self, device_profile: DeviceProfile) {
        if !self.can_drive_page_tools() {
            return;
        }
        let (width, height, scale_percent, touch) = device_profile.dimensions();
        if let Some(tab) = self.active_tab() {
            tab.device_profile = device_profile;
            tab.session.set_device_profile(
                device_profile.wire(),
                width,
                height,
                scale_percent,
                touch,
            );
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_device_profile(&mut self) {
        let device_profile = self
            .tabs
            .get(self.active)
            .map_or(DeviceProfile::Default, |tab| tab.device_profile)
            .next();
        self.set_active_tab_device_profile(device_profile);
    }

    fn request_active_voice_command(&mut self, mode: VoiceCommandMode) {
        if !self.can_drive_page_tools() {
            self.capture_notice = Some(format!("{} unavailable: no live page", mode.label()));
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            self.capture_notice = Some(format!("{} unavailable: no live page", mode.label()));
            return;
        };
        let body = browser_voice_command_body(
            mode,
            self.active,
            tab.engine,
            &tab.session.nav().url,
            tab.session.title(),
            &self.address,
            tab.page_focused,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            ACTION_BROWSER_VOICE_COMMAND,
            &body,
        );
        self.capture_notice = Some(format!("{}: sent voice input request", mode.label()));
    }

    fn handle_page_text_event(&mut self, id: u64, text: String) {
        if let Some(tab_index) = self.pending_spell_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Spelling: no page text found".to_owned());
                return;
            }
            self.capture_notice = Some("Spelling: checking page text".to_owned());
            self.spellcheck.start(id, tab_index, text);
            return;
        }
        if let Some(request) = self.pending_read_aloud_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Read aloud: no page text found".to_owned());
                return;
            }
            let body = browser_read_aloud_body(&request, &text);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_READ_ALOUD, &body);
            self.capture_notice =
                Some("Read aloud: sent page text to the speech service".to_owned());
            return;
        }
        if let Some(request) = self.pending_translate_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Translate: no page text found".to_owned());
                return;
            }
            let body = browser_translate_body(&request, &text);
            publish_to_bus(self.bus_root.as_deref(), ACTION_BROWSER_TRANSLATE, &body);
            self.capture_notice = Some("Translate: sent page text to translation".to_owned());
            return;
        }
        if let Some(request) = self.pending_offline_cache_requests.remove(&id) {
            if text.trim().is_empty() {
                self.capture_notice = Some("Offline cache: no page text found".to_owned());
                return;
            }
            let body = browser_offline_cache_body(&request, &text);
            publish_to_bus(
                self.bus_root.as_deref(),
                ACTION_BROWSER_OFFLINE_CACHE,
                &body,
            );
            self.capture_notice = Some("Offline cache: saved page snapshot".to_owned());
        }
    }

    fn handle_passkey_event(&mut self, tab_id: u64, engine: BrowserEngine, body: &str) {
        match browser_passkey_body(engine, body) {
            Ok(handoff_body) => {
                let Some(client_request_id) = passkey_client_request_id(body) else {
                    self.capture_notice =
                        Some(passkey_page_request_notice("missing client request id"));
                    return;
                };
                if self.pending_passkey_consent.is_some() {
                    self.complete_passkey_denial(
                        tab_id,
                        &client_request_id,
                        "Another passkey ceremony is already waiting for approval",
                    );
                    self.capture_notice =
                        Some("Passkey: another approval is already pending".to_owned());
                    return;
                }
                match PendingPasskeyConsent::from_handoff(
                    tab_id,
                    engine,
                    handoff_body,
                    client_request_id,
                ) {
                    Ok(pending) => {
                        let notice = format!("Passkey: approval required for {}", pending.rp_id);
                        self.pending_passkey_consent = Some(pending);
                        self.capture_notice = Some(notice);
                    }
                    Err(err) => {
                        self.capture_notice = Some(passkey_page_request_notice(&err));
                    }
                }
            }
            Err(err) => {
                self.capture_notice = Some(passkey_page_request_notice(&err));
            }
        }
    }

    fn complete_passkey_denial(
        &mut self,
        tab_id: u64,
        client_request_id: &str,
        reason: &str,
    ) -> bool {
        let Some(tab_index) = self.tab_index_by_id(tab_id) else {
            return false;
        };
        let body = browser_passkey_denied_body(client_request_id, reason);
        if let Some(tab) = self.tabs.get_mut(tab_index) {
            tab.session.complete_passkey(body);
            true
        } else {
            false
        }
    }

    fn approve_pending_passkey(&mut self) {
        let Some(pending) = self.pending_passkey_consent.take() else {
            return;
        };
        if self.tab_index_by_id(pending.tab_id).is_none() {
            self.capture_notice = Some("Passkey: source tab closed".to_owned());
            return;
        }
        match browser_passkey_shell_approved_body(&pending.handoff_body) {
            Ok(handoff_body) => {
                publish_to_bus(
                    self.bus_root.as_deref(),
                    ACTION_BROWSER_PASSKEY,
                    &handoff_body,
                );
                self.pending_passkey_requests
                    .insert(pending.client_request_id.clone(), pending.tab_id);
                self.capture_notice = Some(format!("Passkey: approved for {}", pending.rp_id));
            }
            Err(_err) => {
                self.complete_passkey_denial(
                    pending.tab_id,
                    &pending.client_request_id,
                    "Passkey ceremony could not be approved",
                );
                self.capture_notice = Some("Passkey: approval could not be completed".to_owned());
            }
        }
    }

    fn deny_pending_passkey(&mut self) {
        let Some(pending) = self.pending_passkey_consent.take() else {
            return;
        };
        self.complete_passkey_denial(
            pending.tab_id,
            &pending.client_request_id,
            "Passkey ceremony denied by user",
        );
        self.capture_notice = Some(format!("Passkey: denied for {}", pending.rp_id));
    }

    fn handle_js_dialog_event(&mut self, tab_index: usize, dialog: &JsDialog) {
        let mut notice = chrome_ui::js_dialog_notice(dialog);
        if tab_index != self.active {
            notice = format!("Background tab: {notice}");
        }
        self.capture_notice = Some(notice);
    }

    fn set_active_tab_container(&mut self, container: ContainerProfile) {
        if let Some(tab) = self.active_tab() {
            tab.container = container;
        }
        self.publish_session_snapshot();
    }

    fn cycle_active_tab_container(&mut self) {
        let next = self
            .tabs
            .get(self.active)
            .map_or(ContainerProfile::None, |tab| tab.container.next());
        self.set_active_tab_container(next);
    }

    fn set_active_tab_display_target(&mut self, display_target: DisplayTarget) {
        let active = self.active;
        let body = if let Some(tab) = self.tabs.get_mut(active) {
            tab.display_target = display_target;
            Some(browser_display_target_body(active, tab, display_target))
        } else {
            None
        };
        if let Some(body) = body {
            publish_to_bus(
                self.bus_root.as_deref(),
                ACTION_BROWSER_DISPLAY_TARGET,
                &body,
            );
            self.publish_session_snapshot();
        }
    }

    fn cycle_active_tab_display_target(&mut self) {
        let next = self
            .tabs
            .get(self.active)
            .map_or(DisplayTarget::Current, |tab| tab.display_target.next());
        self.set_active_tab_display_target(next);
    }

    fn open_find_bar(&mut self) {
        if !self.can_drive_page_tools() {
            return;
        }
        self.find_open = true;
        self.publish_session_snapshot();
    }

    fn close_find_bar(&mut self) {
        self.find_open = false;
        self.last_find_query = None;
        if let Some(tab) = self.active_tab() {
            tab.session.clear_find();
        }
        self.publish_session_snapshot();
    }

    /// Apply an in-page context-menu action to the tab at `index`.
    fn apply_page_context_action(&mut self, index: usize, action: chrome_ui::PageContextAction) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };
        match action {
            chrome_ui::PageContextAction::Back => tab.session.go_back(),
            chrome_ui::PageContextAction::Forward => tab.session.go_forward(),
            chrome_ui::PageContextAction::Reload => tab.session.reload(),
            chrome_ui::PageContextAction::Edit(command) => tab.session.edit_command(command),
        }
    }

    fn submit_find(&mut self, backwards: bool) {
        let query = self.find_query.trim().to_owned();
        if query.is_empty() {
            self.last_find_query = None;
            if let Some(tab) = self.active_tab() {
                tab.session.clear_find();
            }
            return;
        }
        // Same query as last time → cycle to the next/prev match; a changed query
        // starts a fresh search from the top.
        let find_next = self.last_find_query.as_deref() == Some(query.as_str());
        self.last_find_query = Some(query.clone());
        if let Some(tab) = self.active_tab() {
            tab.session.find_in_page(query, backwards, find_next);
        }
    }

    /// The active tab's find tally `(active_ordinal, total_count)` for the counter.
    fn active_find_result(&self) -> Option<(u32, u32)> {
        self.tabs.get(self.active)?.session.find_result()
    }

    fn sync_address_from_active(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(page) = tab.internal_page {
                self.address = page.url().to_owned();
                self.last_engine_url = None;
                return;
            }
            let url = tab.session.nav().url.trim();
            if !url.is_empty() {
                self.address = url.to_owned();
            }
        }
    }

    /// Per-frame omnibox ↔ engine sync: an engine-driven navigation (redirect,
    /// page script, in-page link click) updates the address bar even though no
    /// chrome action (tab select/close/move) ran. Guarded two ways so it can
    /// run every frame from the pump: it only rewrites the address when the
    /// active tab's engine URL actually CHANGED since the last frame, and never
    /// while the omnibox itself owns keyboard focus — so it cannot clobber an
    /// in-progress operator edit, and a blurred-but-unsubmitted draft survives
    /// until the engine really moves.
    fn sync_address_on_engine_nav(&mut self) {
        if let Some(page) = self.active_internal_page() {
            if !self.omnibox_focused {
                self.address = page.url().to_owned();
            }
            self.last_engine_url = None;
            return;
        }
        let Some(url) = self
            .tabs
            .get(self.active)
            .map(|tab| tab.session.nav().url.trim().to_owned())
        else {
            self.last_engine_url = None;
            return;
        };
        if self.last_engine_url.as_deref() == Some(url.as_str()) {
            return;
        }
        if !self.omnibox_focused && !url.is_empty() {
            self.address.clone_from(&url);
        }
        // Fold the transition even when focus suppressed the rewrite: lifting
        // focus later must not retroactively apply a stale engine URL over
        // whatever the operator left in the bar.
        self.last_engine_url = Some(url);
    }

    fn poll_suggestions(&mut self) {
        self.suggestions.poll();
    }

    fn update_suggestions_for_address(&mut self) {
        self.suggestions.update_for_draft(&self.address);
        self.update_file_suggestions_for_address();
        // History matches independently of the SearXNG fetch gate
        // (`should_fetch_suggestions`): a URL-like draft that skips the search
        // round-trip should still surface a matching visit. Guarded on a
        // non-empty trimmed draft so an empty omnibox doesn't dump the whole
        // recent-visits list into the dropdown.
        let hits: Vec<String> = if self.address.trim().is_empty() {
            Vec::new()
        } else {
            self.history
                .matching(&self.address)
                .map(|v| v.url.clone())
                .take(5)
                .collect()
        };
        self.suggestions.set_history_matches(hits);
        // Bookmark matches (title OR url) — highest-signal, rendered above history.
        let bookmarks = matching_bookmarks(&self.bookmark_index, &self.address, 3);
        self.suggestions.set_bookmark_matches(bookmarks);
        // Inline top-hit: preselect the first suggestion when it is an inline
        // completion of the draft (Chrome's omnibox), so Enter accepts the completed
        // URL; otherwise nothing is preselected and arrow keys drive the highlight.
        let ordered = self.suggestions.ordered_commit_values();
        self.suggestions.selected = inline_top_hit(&ordered, &self.address);
    }

    fn update_file_suggestions_for_address(&mut self) {
        let files = if self.address.trim().is_empty() {
            Vec::new()
        } else {
            matching_file_suggestions(&self.file_omnibox_index, &self.address, 5)
        };
        self.suggestions.set_file_matches(files);
    }

    fn accept_suggestion(&mut self, suggestion: String) {
        self.address = suggestion;
        self.submit_address();
    }

    /// Whether a crashed tab's Reload asked for a respawn — drained by the shell
    /// each frame (and by the tests). The live build swaps in a fresh session via
    /// [`Self::respawn_active_with`]; the gated build acknowledges it honestly.
    pub(crate) fn take_respawn_request(&mut self) -> bool {
        std::mem::take(&mut self.respawn_requested)
    }

    /// Replace the active tab's crashed session with a fresh one (respawn-on-reload),
    /// discarding its stale texture so the new page uploads cleanly.
    #[cfg(test)]
    pub(crate) fn respawn_active_with(&mut self, session: WebSession) {
        let engine = self
            .tabs
            .get(self.active)
            .map_or(self.engine, |tab| tab.engine);
        self.respawn_active_with_engine(session, engine);
    }

    #[cfg_attr(
        not(any(test, feature = "live-helper")),
        allow(dead_code, reason = "used by live-helper respawn and Browser tests")
    )]
    fn respawn_active_with_engine(&mut self, session: WebSession, engine: BrowserEngine) {
        let mut session = session;
        let url = session.nav().url.clone();
        session.set_filter(self.compiled_request_filter_for_url(&url));
        let autoplay_blocked = self
            .tabs
            .get(self.active)
            .map_or(DEFAULT_AUTOPLAY_BLOCKED, |tab| tab.autoplay_blocked);
        session.set_autoplay_blocked(autoplay_blocked);
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.session = session;
            tab.engine = engine;
            tab.texture = None;
            tab.last_frame = None;
            tab.last_audited_resource_seq = 0;
            tab.last_audited_cert_error = None;
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
            // A fresh helper re-negotiates its viewport from scratch.
            tab.resizer = ViewportResizer::default();
        }
        self.publish_session_snapshot();
    }

    #[cfg(feature = "live-helper")]
    fn replace_tab_with_session(
        &mut self,
        index: usize,
        session: WebSession,
        engine: BrowserEngine,
    ) {
        if index >= self.tabs.len() {
            self.push_session_with_engine(session, engine);
            return;
        }
        let mut session = session;
        let url = session.nav().url.clone();
        session.set_filter(self.compiled_request_filter_for_url(&url));
        session.set_autoplay_blocked(DEFAULT_AUTOPLAY_BLOCKED);
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.session = session;
            tab.engine = engine;
            tab.internal_page = None;
            tab.internal_peer = None;
            tab.container = ContainerProfile::None;
            tab.display_target = DisplayTarget::Current;
            tab.muted = false;
            tab.autoplay_blocked = DEFAULT_AUTOPLAY_BLOCKED;
            tab.force_dark = false;
            tab.reader_mode = false;
            tab.user_scripts = false;
            tab.user_agent = UserAgentOverride::Default;
            tab.device_profile = DeviceProfile::Default;
            tab.texture = None;
            tab.last_frame = None;
            tab.last_audited_resource_seq = 0;
            tab.last_audited_cert_error = None;
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
            tab.page_focused = false;
            tab.resizer = ViewportResizer::default();
            tab.favicon_cache = None;
        }
        self.active = index.min(self.tabs.len().saturating_sub(1));
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    fn compiled_request_filter(&self) -> RequestFilter {
        RequestFilter::from_store(&self.adfilter_store)
            .with_managed_policy(self.managed_url_policy.clone())
            .with_safe_browsing(SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts))
    }

    /// Record a session-only permission grant `(origin, kind)`.
    fn grant_permission(&mut self, origin: &str, kind: u8) {
        self.granted_permissions.insert((origin.to_owned(), kind));
    }

    /// Whether `(origin, kind)` was allowed earlier this session.
    fn is_permission_granted(&self, origin: &str, kind: u8) -> bool {
        self.granted_permissions
            .contains(&(origin.to_owned(), kind))
    }

    /// The active tab's oldest pending beforeunload prompt, if any. Cloned so the
    /// UI can render without holding a mutable borrow into the tab/session.
    fn pending_before_unload_prompt(&self) -> Option<BeforeUnloadDialog> {
        self.tabs
            .get(self.active)?
            .session
            .pending_before_unload()
            .cloned()
    }

    /// Answer the active tab's pending beforeunload prompt, if it still matches
    /// the prompt id the UI rendered.
    fn answer_active_before_unload(&mut self, id: u64, proceed: bool) {
        let Some(tab) = self.tabs.get_mut(self.active) else {
            return;
        };
        if tab
            .session
            .pending_before_unload()
            .is_some_and(|prompt| prompt.id == id)
        {
            tab.session.answer_before_unload(proceed);
        }
    }

    /// Resolve the active tab's pending permission request, if any: a capability
    /// this origin was already granted this session auto-allows (answers the engine
    /// with `true`, no prompt) and returns `None`; otherwise returns `(origin, kind)`
    /// for the shell to render a prompt. Never auto-denies — a previously-blocked
    /// capability re-prompts (Chrome's behaviour).
    fn pending_permission_prompt(&mut self) -> Option<(String, u8)> {
        let (origin, kind) = self
            .tabs
            .get(self.active)?
            .session
            .pending_permission()
            .map(|req| (req.origin.clone(), req.kind))?;
        if self.is_permission_granted(&origin, kind) {
            let (engine, url, title) = self.tabs.get(self.active).map_or(
                (self.engine, String::new(), String::new()),
                |tab| {
                    (
                        tab.engine,
                        tab.session.nav().url.clone(),
                        tab.session.title().to_owned(),
                    )
                },
            );
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.session.answer_permission(true);
            }
            self.publish_permission_decision(
                engine,
                &origin,
                kind,
                true,
                "session_grant_reuse",
                &url,
                &title,
                unix_ms(),
            );
            return None;
        }
        Some((origin, kind))
    }

    /// Answer the active tab's pending permission prompt; a grant is remembered for
    /// the session so the same origin+capability won't re-prompt.
    fn answer_active_permission(&mut self, origin: &str, kind: u8, allow: bool) {
        let origin = origin.trim();
        let Some((engine, url, title)) = self.tabs.get(self.active).and_then(|tab| {
            tab.session
                .pending_permission()
                .is_some_and(|request| request.origin.trim() == origin && request.kind == kind)
                .then(|| {
                    (
                        tab.engine,
                        tab.session.nav().url.clone(),
                        tab.session.title().to_owned(),
                    )
                })
        }) else {
            return;
        };
        if allow {
            self.grant_permission(origin, kind);
        }
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.session.answer_permission(allow);
        }
        self.publish_permission_decision(
            engine,
            origin,
            kind,
            allow,
            "helper_permission_prompt",
            &url,
            &title,
            unix_ms(),
        );
    }

    /// Save (or update) a session-only login for `host`; replaces an existing entry
    /// with the same host+username. Blank host/username/password is ignored.
    fn save_login(&mut self, host: &str, username: &str, password: &str) {
        self.save_login_with_trigger(host, username, password, "password_menu");
    }

    fn save_login_with_trigger(
        &mut self,
        host: &str,
        username: &str,
        password: &str,
        trigger: &str,
    ) {
        let host = host.trim().to_ascii_lowercase();
        let username = username.trim().to_owned();
        if host.is_empty() || username.is_empty() || password.is_empty() {
            return;
        }
        let decision = if let Some(existing) = self
            .session_logins
            .iter_mut()
            .find(|l| l.host == host && l.username == username)
        {
            existing.password = password.to_owned();
            "update"
        } else {
            self.session_logins.push(StoredLogin {
                host: host.clone(),
                username,
                password: password.to_owned(),
            });
            "save"
        };
        let credential_count = self.credential_count_for_host(&host);
        let (engine, url, title) = self.credential_context_for_host(&host);
        self.publish_credential_event(
            engine,
            &url,
            &title,
            &host,
            decision,
            trigger,
            credential_count,
            unix_ms(),
        );
    }

    /// The saved logins for `host` (lowercased), in save order.
    #[cfg(test)]
    fn logins_for_host(&self, host: &str) -> Vec<&StoredLogin> {
        let host = host.trim().to_ascii_lowercase();
        self.session_logins
            .iter()
            .filter(|l| l.host == host)
            .collect()
    }

    fn credential_count_for_host(&self, host: &str) -> usize {
        let host = host.trim().to_ascii_lowercase();
        self.session_logins
            .iter()
            .filter(|l| l.host == host)
            .count()
    }

    fn credential_context_for_host(&self, host: &str) -> (BrowserEngine, String, String) {
        let host = host.trim().to_ascii_lowercase();
        let context = |tab: &Tab| {
            (
                tab.engine,
                tab.session.nav().url.trim().to_owned(),
                tab.session.title().to_owned(),
            )
        };
        if !host.is_empty() {
            if let Some(tab) = self.tabs.get(self.active).filter(|tab| {
                host_of(tab.session.nav().url.trim())
                    .is_some_and(|candidate| candidate == host.as_str())
            }) {
                return context(tab);
            }
            if let Some(tab) = self.tabs.iter().find(|tab| {
                host_of(tab.session.nav().url.trim())
                    .is_some_and(|candidate| candidate == host.as_str())
            }) {
                return context(tab);
            }
        }
        self.tabs
            .get(self.active)
            .map_or((self.engine, String::new(), String::new()), context)
    }

    /// Remove the saved login at `index` (manager delete).
    fn remove_login(&mut self, index: usize) {
        if index < self.session_logins.len() {
            let removed = self.session_logins.remove(index);
            let credential_count = self.credential_count_for_host(&removed.host);
            let (engine, url, title) = self.credential_context_for_host(&removed.host);
            self.publish_credential_event(
                engine,
                &url,
                &title,
                &removed.host,
                "delete",
                "password_menu",
                credential_count,
                unix_ms(),
            );
        }
    }

    /// Autofill the active tab's login form with a chosen credential (the engine
    /// injects the fill script). User-initiated only.
    fn fill_active_login(&mut self, expected_host: String, username: String, password: String) {
        if self.active_first_party().as_deref() != Some(expected_host.as_str()) {
            return;
        }
        let credential_count = self.credential_count_for_host(&expected_host);
        let (engine, url, title) = self.credential_context_for_host(&expected_host);
        if let Some(tab) = self.active_tab() {
            tab.session
                .fill_login(expected_host.clone(), username, password);
        } else {
            return;
        }
        self.mark_active_tab_activity();
        self.publish_credential_event(
            engine,
            &url,
            &title,
            &expected_host,
            "fill",
            "password_menu",
            credential_count,
            unix_ms(),
        );
    }

    fn active_pending_login_save(&self) -> Option<&PendingLoginSave> {
        self.pending_login_save
            .as_ref()
            .filter(|pending| self.pending_login_save_is_active(pending))
    }

    fn pending_login_save_is_active(&self, pending: &PendingLoginSave) -> bool {
        if pending.tab_id == 0 {
            return true;
        }
        self.tabs.get(self.active).is_some_and(|tab| {
            tab.id == pending.tab_id
                && host_of(tab.session.nav().url.trim())
                    .is_some_and(|host| host == pending.host.as_str())
        })
    }

    fn accept_pending_login_save(&mut self) {
        let Some(pending) = self.pending_login_save.take() else {
            return;
        };
        if !self.pending_login_save_is_active(&pending) {
            self.pending_login_save = Some(pending);
            return;
        }
        self.save_login_with_trigger(
            &pending.host,
            &pending.username,
            &pending.password,
            "auto_capture_prompt",
        );
    }

    fn dismiss_pending_login_save(&mut self) {
        let Some(pending) = self.pending_login_save.take() else {
            return;
        };
        if !self.pending_login_save_is_active(&pending) {
            self.pending_login_save = Some(pending);
            return;
        }
        let credential_count = self.credential_count_for_host(&pending.host);
        let (engine, url, title) = self.credential_context_for_host(&pending.host);
        self.publish_credential_event(
            engine,
            &url,
            &title,
            &pending.host,
            "dismiss",
            "auto_capture_prompt",
            credential_count,
            unix_ms(),
        );
    }

    /// Fold an auto-captured login (engine-supplied `origin` + page JSON carrying
    /// username/password) into a host-bound "Save password?" offer. Skipped if the
    /// exact credential is already stored, so a re-login never re-prompts.
    #[cfg(test)]
    fn handle_login_capture(&mut self, origin: &str, body: &str) {
        let tab_id = self.tabs.get(self.active).map_or(0, |tab| tab.id);
        self.handle_login_capture_from_tab(tab_id, origin, body);
    }

    fn handle_login_capture_from_tab(&mut self, tab_id: u64, origin: &str, body: &str) {
        let host = host_of(origin).unwrap_or_default();
        if host.is_empty() {
            return;
        }
        if tab_id != 0 {
            let Some(source_host) = self
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .and_then(|tab| host_of(tab.session.nav().url.trim()))
            else {
                return;
            };
            if source_host != host {
                return;
            }
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
            return;
        };
        let username = v
            .get("username")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .trim()
            .to_owned();
        let password = v
            .get("password")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_owned();
        if username.is_empty() || password.is_empty() {
            return;
        }
        if self
            .session_logins
            .iter()
            .any(|l| l.host == host && l.username == username && l.password == password)
        {
            return; // already saved — no offer
        }
        self.pending_login_save = Some(PendingLoginSave {
            tab_id,
            host,
            username,
            password,
        });
    }

    fn compiled_request_filter_for_url(&self, url: &str) -> RequestFilter {
        let mut filter = self.compiled_request_filter();
        filter.set_page(url);
        filter
    }

    fn apply_adfilter_to_open_tabs(&mut self) {
        let store = self.adfilter_store.clone();
        let managed_policy = self.managed_url_policy.clone();
        let safe_browsing = SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts);
        for tab in &mut self.tabs {
            if tab.internal_page.is_some() {
                continue;
            }
            let mut filter = RequestFilter::from_store(&store)
                .with_managed_policy(managed_policy.clone())
                .with_safe_browsing(safe_browsing.clone());
            filter.set_page(&tab.session.nav().url);
            tab.session.set_filter(filter);
        }
    }

    fn active_first_party(&self) -> Option<String> {
        let tab = self.tabs.get(self.active)?;
        if tab.internal_page.is_some() {
            return None;
        }
        let url = tab.session.nav().url.trim();
        host_of(url)
    }

    fn active_site_blocking_enabled(&self) -> bool {
        self.active_first_party()
            .is_some_and(|host| !self.adfilter_store.allowlist().is_allowed(&host))
    }

    fn filter_list_summary(&self) -> String {
        self.filter_list_source_status.summary()
    }

    fn custom_filter_rules_summary(&self) -> String {
        self.custom_filter_rules_source_status.summary()
    }

    fn safe_browsing_summary(&self) -> String {
        self.safe_browsing_source_status.summary()
    }

    fn managed_policy_summary(&self) -> String {
        self.managed_policy_source_status.summary()
    }

    fn site_data_summary(&self) -> String {
        self.site_data.summary(self.active_first_party().as_deref())
    }

    fn update_site_data_from_tabs(&mut self) {
        let hosts = self
            .tabs
            .iter()
            .filter(|tab| tab.internal_page.is_none())
            .filter_map(|tab| host_of(tab.session.nav().url.trim()))
            .collect::<Vec<_>>();
        self.site_data
            .observe_open_tabs(hosts.iter().map(String::as_str), unix_ms());
    }

    /// Record the ACTIVE tab's committed navigation into the session-only history
    /// (B3). `record` dedupes the NavState `loading:true→false` churn and reloads,
    /// and back-fills the title as it arrives; new-tab/blank pages are skipped.
    fn record_history_from_active_tab(&mut self) {
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if tab.internal_page.is_some() {
            return;
        }
        let url = tab.session.nav().url.trim().to_owned();
        let title = tab.session.title().to_owned();
        if url.is_empty() || url == NEW_TAB_URL {
            return;
        }
        self.history.record(&url, &title, unix_ms());
    }

    fn set_active_site_blocking(&mut self, enabled: bool) {
        let Some(host) = self.active_first_party() else {
            return;
        };
        let (engine, url, title) =
            self.tabs
                .get(self.active)
                .map_or((self.engine, String::new(), String::new()), |tab| {
                    (
                        tab.engine,
                        tab.session.nav().url.trim().to_owned(),
                        tab.session.title().to_owned(),
                    )
                });
        let now = unix_ms();
        if enabled {
            self.adfilter_store
                .block_site(&host, &local_hostname(), now);
            publish(ACTION_ADFILTER_BLOCK, &adfilter_domain_body(&host));
        } else {
            self.adfilter_store
                .allow_site(&host, &local_hostname(), now);
            publish(ACTION_ADFILTER_ALLOW, &adfilter_domain_body(&host));
        }
        self.apply_adfilter_to_open_tabs();
        self.publish_site_blocking(engine, &url, &title, &host, enabled, now);
    }

    fn add_custom_filter_rules(&mut self, name: &str, raw: &str, source_path: Option<&Path>) {
        let name = name.trim();
        let raw = raw.trim();
        if name.is_empty() {
            return;
        }
        let source_url = source_path.map(|path| path.to_string_lossy().into_owned());
        self.adfilter_store
            .add_source(FilterListSource::custom(name, source_url, raw, unix_ms()));
        self.apply_adfilter_to_open_tabs();
    }

    fn current_custom_filter_rule_count(&self) -> usize {
        self.adfilter_store
            .source(CUSTOM_FILTER_SOURCE_NAME)
            .map_or(0, |source| custom_filter_rule_count(&source.raw))
    }

    fn current_filter_source_count(&self) -> usize {
        self.adfilter_store.enabled_sources().count()
    }

    fn set_synced_filter_store(&mut self, store: FilterListStore) {
        let mut merged = store;
        // Preserve immediate local Browser edits (site allow/block toggles and the
        // local custom-rule source) while still letting newer synced sources win.
        merged.merge(&self.adfilter_store);
        if merged != self.adfilter_store {
            self.adfilter_store = merged;
            self.apply_adfilter_to_open_tabs();
        }
    }

    fn set_custom_filter_rules(&mut self, raw: &str, source_path: &Path) {
        let raw = raw.trim();
        let unchanged = self
            .adfilter_store
            .source(CUSTOM_FILTER_SOURCE_NAME)
            .is_some_and(|source| source.raw.trim() == raw);
        if unchanged {
            return;
        }
        self.add_custom_filter_rules(CUSTOM_FILTER_SOURCE_NAME, raw, Some(source_path));
    }

    fn set_safe_browsing_hosts(&mut self, hosts: impl IntoIterator<Item = impl AsRef<str>>) {
        self.safe_browsing_hosts = hosts
            .into_iter()
            .filter_map(|host| {
                let host = host.as_ref().trim().to_ascii_lowercase();
                (!host.is_empty()).then_some(host)
            })
            .collect();
        self.apply_adfilter_to_open_tabs();
    }

    fn set_managed_url_policy(&mut self, policy: ManagedUrlPolicy) {
        self.managed_url_policy = policy;
        self.apply_adfilter_to_open_tabs();
    }

    fn managed_policy_block_for(&self, url: &str) -> Option<ManagedPolicyBlock> {
        self.managed_url_policy
            .matches(url)
            .map(|rule| ManagedPolicyBlock {
                url: url.to_owned(),
                rule,
            })
    }

    fn safe_browsing_download_block_for(&self, url: &str) -> Option<String> {
        if self.safe_browsing_hosts.is_empty() {
            return None;
        }
        let mut filter = RequestFilter::empty()
            .with_safe_browsing(SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts));
        let decision = filter.decide(url, mde_web_preview_client::ResourceType::Document);
        decision
            .blocked_by()
            .and_then(|rule| rule.strip_prefix("safe-browsing:"))
            .map(str::to_owned)
    }

    fn insecure_download_block_for(&self, url: &str) -> bool {
        is_plain_http(url) && !plain_http_download_host_is_trusted(url)
    }

    fn block_managed_navigation(
        &mut self,
        block: ManagedPolicyBlock,
        trigger: ManagedPolicyBlockTrigger,
        engine_override: Option<BrowserEngine>,
    ) {
        self.clear_insecure_prompt();
        self.publish_managed_policy_block(&block, trigger, engine_override);
        self.managed_policy_block = Some(block.clone());
        let host = host_of(&block.url).unwrap_or_else(|| block.url.clone());
        self.capture_notice = Some(format!("Blocked by managed policy: {host}"));
        self.mark_active_tab_activity();
    }

    fn block_managed_download(&mut self, block: ManagedPolicyBlock) {
        self.publish_managed_policy_block(&block, ManagedPolicyBlockTrigger::Download, None);
        let host = host_of(&block.url).unwrap_or_else(|| block.url.clone());
        let notice = format!("Download blocked by managed policy: {host}");
        self.download_notice = Some(notice.clone());
        self.capture_notice = Some(notice);
        self.downloads_open = true;
    }

    fn block_safe_browsing_download(&mut self, url: &str, rule: &str) {
        self.publish_safe_browsing_block(url, rule, "download", unix_ms());
        let host = host_of(url).unwrap_or_else(|| url.trim().to_owned());
        let notice = format!("Download blocked by safe browsing: {host}");
        self.download_notice = Some(notice.clone());
        self.capture_notice = Some(notice);
        self.downloads_open = true;
    }

    fn block_insecure_download(&mut self, url: &str) {
        self.publish_insecure_download_block(url, "download", unix_ms());
        let host = host_of(url).unwrap_or_else(|| url.trim().to_owned());
        let notice = format!("Download blocked: insecure HTTP from {host}");
        self.download_notice = Some(notice.clone());
        self.capture_notice = Some(notice);
        self.downloads_open = true;
    }

    fn publish_managed_policy_block(
        &self,
        block: &ManagedPolicyBlock,
        trigger: ManagedPolicyBlockTrigger,
        engine_override: Option<BrowserEngine>,
    ) {
        let (engine, title) = if let Some(engine) = engine_override {
            (engine, String::new())
        } else {
            self.tabs
                .get(self.active)
                .map_or((self.engine, String::new()), |tab| {
                    (tab.engine, tab.session.title().to_owned())
                })
        };
        let body = browser_policy_block_body(
            engine,
            &block.url,
            &title,
            &block.rule,
            trigger.wire(),
            unix_ms(),
        );
        publish_to_bus(self.bus_root.as_deref(), EVENT_BROWSER_POLICY_BLOCK, &body);
    }

    fn publish_safe_browsing_block(&self, url: &str, rule: &str, trigger: &str, blocked_ms: u64) {
        let (engine, title) = self
            .tabs
            .get(self.active)
            .map_or((self.engine, String::new()), |tab| {
                (tab.engine, tab.session.title().to_owned())
            });
        let body = browser_safe_browsing_block_body(engine, url, &title, rule, trigger, blocked_ms);
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_SAFE_BROWSING_BLOCK,
            &body,
        );
    }

    fn publish_policy_source_status(&self, status: &BrowserPolicySourceStatus) {
        let body = browser_policy_source_status_body(
            status.kind.op(),
            status.kind.policy(),
            &status.source_path,
            status.state.wire(),
            status.item_count,
            status.effective_count,
            status.checked_ms,
            status.loaded_ms,
            status.error.as_deref(),
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            &status.kind.topic(&local_hostname()),
            &body,
        );
    }

    fn publish_certificate_error(&self, audit: &CertificateErrorAudit, blocked_ms: u64) {
        let body = browser_certificate_error_body(
            audit.engine,
            &audit.error.url,
            &audit.title,
            audit.error.code,
            &audit.error.message,
            blocked_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_CERTIFICATE_ERROR,
            &body,
        );
    }

    fn publish_insecure_download_block(&self, url: &str, trigger: &str, blocked_ms: u64) {
        let (engine, title) = self
            .tabs
            .get(self.active)
            .map_or((self.engine, String::new()), |tab| {
                (tab.engine, tab.session.title().to_owned())
            });
        let body = browser_insecure_download_block_body(engine, url, &title, trigger, blocked_ms);
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_INSECURE_DOWNLOAD_BLOCK,
            &body,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_insecure_navigation(
        &self,
        engine: BrowserEngine,
        url: &str,
        title: &str,
        decision: &str,
        trigger: &str,
        enforcement: &str,
        upgraded_url: Option<&str>,
        decided_ms: u64,
    ) {
        let body = browser_insecure_navigation_body(
            engine,
            url,
            title,
            decision,
            trigger,
            enforcement,
            upgraded_url,
            decided_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_INSECURE_NAVIGATION,
            &body,
        );
    }

    fn publish_mixed_content_block(&self, block: &MixedContentBlockAudit, blocked_ms: u64) {
        let body = browser_mixed_content_block_body(
            block.engine,
            &block.page_url,
            &block.url,
            &block.title,
            block.resource,
            "subresource",
            blocked_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_MIXED_CONTENT_BLOCK,
            &body,
        );
    }

    fn publish_site_blocking(
        &self,
        engine: BrowserEngine,
        url: &str,
        title: &str,
        host: &str,
        enabled: bool,
        updated_ms: u64,
    ) {
        let body = browser_site_blocking_body(engine, url, title, host, enabled, updated_ms);
        publish_to_bus(self.bus_root.as_deref(), EVENT_BROWSER_SITE_BLOCKING, &body);
    }

    fn publish_site_data_clear(
        &self,
        engine: BrowserEngine,
        url: &str,
        title: &str,
        host: &str,
        scope: &str,
        cleared_ms: u64,
    ) {
        let body = browser_site_data_clear_body(engine, url, title, host, scope, cleared_ms);
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_SITE_DATA_CLEAR,
            &body,
        );
    }

    fn publish_browsing_data_clear(
        &self,
        engine: BrowserEngine,
        active_url: &str,
        active_title: &str,
        active_host: &str,
        counts: BrowserBrowsingDataClearCounts,
        cleared_ms: u64,
    ) {
        let body = browser_browsing_data_clear_body(
            engine,
            active_url,
            active_title,
            active_host,
            counts,
            cleared_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_BROWSING_DATA_CLEAR,
            &body,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_permission_decision(
        &self,
        engine: BrowserEngine,
        origin: &str,
        kind: u8,
        allow: bool,
        enforcement: &str,
        url: &str,
        title: &str,
        decided_ms: u64,
    ) {
        let body = browser_permission_decision_body(
            engine,
            origin,
            kind,
            allow,
            enforcement,
            url,
            title,
            decided_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_PERMISSION_DECISION,
            &body,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_credential_event(
        &self,
        engine: BrowserEngine,
        url: &str,
        title: &str,
        host: &str,
        decision: &str,
        trigger: &str,
        credential_count: usize,
        updated_ms: u64,
    ) {
        let body = browser_credential_body(
            engine,
            url,
            title,
            host,
            decision,
            trigger,
            credential_count,
            updated_ms,
        );
        publish_to_bus(self.bus_root.as_deref(), EVENT_BROWSER_CREDENTIAL, &body);
    }

    fn publish_download_danger(
        &self,
        pending: &PendingDangerousDownload,
        decision: &str,
        updated_ms: u64,
    ) {
        let body = browser_download_danger_body(
            pending.id,
            &pending.url,
            &pending.filename,
            decision,
            updated_ms,
        );
        publish_to_bus(
            self.bus_root.as_deref(),
            EVENT_BROWSER_DOWNLOAD_DANGER,
            &body,
        );
    }

    /// Populate the safe-browsing blocklist from the operator-curated mesh policy
    /// file (`browser/safe-browsing-hosts.txt` under the workgroup root — the mackesd
    /// sync/operator writes it). Throttled; re-applies to open tabs only on a real
    /// change. This is the "mesh policy source" wiring that activates the blocklist.
    fn poll_safe_browsing_hosts(&mut self) {
        if self
            .safe_browsing_last_poll
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.safe_browsing_last_poll = Some(Instant::now());
        let path = default_workgroup_root().join(SAFE_BROWSING_HOSTS_PATH);
        let checked_ms = unix_ms();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let hosts = parse_safe_browsing_hosts(&text);
                let status = BrowserPolicySourceStatus::loaded(
                    BrowserPolicySourceKind::SafeBrowsing,
                    path,
                    hosts.len(),
                    checked_ms,
                );
                if hosts != self.safe_browsing_hosts {
                    self.set_safe_browsing_hosts(hosts);
                }
                self.safe_browsing_source_status = status;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.safe_browsing_source_status = self.safe_browsing_source_status.missing(
                    path,
                    checked_ms,
                    self.safe_browsing_hosts.len(),
                );
            }
            Err(err) => {
                self.safe_browsing_source_status = self.safe_browsing_source_status.error(
                    path,
                    checked_ms,
                    self.safe_browsing_hosts.len(),
                    err.to_string(),
                );
            }
        }
        self.publish_policy_source_status(&self.safe_browsing_source_status);
    }

    /// Populate the Browser blocker from the mackesd adfilter worker's converged
    /// store (`adfilter/compiled/engine.json` under the workgroup root). The local
    /// custom-rule file is polled immediately after this so operator-local rules
    /// remain layered on top.
    fn poll_filter_lists(&mut self) {
        if self
            .filter_lists_last_poll
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.filter_lists_last_poll = Some(Instant::now());
        let path = default_workgroup_root().join(ADFILTER_COMPILED_STORE_PATH);
        let checked_ms = unix_ms();
        match std::fs::read_to_string(&path) {
            Ok(text) => match FilterListStore::from_json(&text) {
                Ok(store) => {
                    let source_count = store.enabled_sources().count();
                    let status = BrowserPolicySourceStatus::loaded(
                        BrowserPolicySourceKind::FilterLists,
                        path,
                        source_count,
                        checked_ms,
                    );
                    self.set_synced_filter_store(store);
                    self.filter_list_source_status = status;
                }
                Err(err) => {
                    self.filter_list_source_status = self.filter_list_source_status.error(
                        path,
                        checked_ms,
                        self.current_filter_source_count(),
                        err.to_string(),
                    );
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.filter_list_source_status = self.filter_list_source_status.missing(
                    path,
                    checked_ms,
                    self.current_filter_source_count(),
                );
            }
            Err(err) => {
                self.filter_list_source_status = self.filter_list_source_status.error(
                    path,
                    checked_ms,
                    self.current_filter_source_count(),
                    err.to_string(),
                );
            }
        }
        self.publish_policy_source_status(&self.filter_list_source_status);
    }

    /// Populate operator custom EasyList-format rules from
    /// `browser/custom-filter-rules.txt` under the workgroup root. A loaded empty
    /// file clears the custom source; missing/error keeps the last-good source.
    fn poll_custom_filter_rules(&mut self) {
        if self
            .custom_filter_rules_last_poll
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.custom_filter_rules_last_poll = Some(Instant::now());
        let path = default_workgroup_root().join(CUSTOM_FILTER_RULES_PATH);
        let checked_ms = unix_ms();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let item_count = custom_filter_rule_count(&text);
                let status = BrowserPolicySourceStatus::loaded(
                    BrowserPolicySourceKind::CustomFilterRules,
                    path.clone(),
                    item_count,
                    checked_ms,
                );
                self.set_custom_filter_rules(&text, &path);
                self.custom_filter_rules_source_status = status;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.custom_filter_rules_source_status = self
                    .custom_filter_rules_source_status
                    .missing(path, checked_ms, self.current_custom_filter_rule_count());
            }
            Err(err) => {
                self.custom_filter_rules_source_status =
                    self.custom_filter_rules_source_status.error(
                        path,
                        checked_ms,
                        self.current_custom_filter_rule_count(),
                        err.to_string(),
                    );
            }
        }
        self.publish_policy_source_status(&self.custom_filter_rules_source_status);
    }

    /// Populate the operator-managed URL policy from
    /// `browser/managed-url-policy.txt` under the workgroup root. Lines are host
    /// suffixes or URL prefixes; changes are applied to all open tabs.
    fn poll_managed_url_policy(&mut self) {
        if self
            .managed_policy_last_poll
            .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.managed_policy_last_poll = Some(Instant::now());
        let path = default_workgroup_root().join(MANAGED_URL_POLICY_PATH);
        let checked_ms = unix_ms();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let policy = parse_managed_url_policy(&text);
                let status = BrowserPolicySourceStatus::loaded(
                    BrowserPolicySourceKind::ManagedUrl,
                    path,
                    policy.len(),
                    checked_ms,
                );
                if policy != self.managed_url_policy {
                    self.set_managed_url_policy(policy);
                }
                self.managed_policy_source_status = status;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.managed_policy_source_status = self.managed_policy_source_status.missing(
                    path,
                    checked_ms,
                    self.managed_url_policy.len(),
                );
            }
            Err(err) => {
                self.managed_policy_source_status = self.managed_policy_source_status.error(
                    path,
                    checked_ms,
                    self.managed_url_policy.len(),
                    err.to_string(),
                );
            }
        }
        self.publish_policy_source_status(&self.managed_policy_source_status);
    }
}

/// The operator-curated safe-browsing blocklist path, relative to the workgroup root.
const SAFE_BROWSING_HOSTS_PATH: &str = "browser/safe-browsing-hosts.txt";
/// The mackesd adfilter worker's converged store path, relative to the workgroup root.
const ADFILTER_COMPILED_STORE_PATH: &str = "adfilter/compiled/engine.json";
/// The operator-managed custom filter rules path, relative to the workgroup root.
const CUSTOM_FILTER_RULES_PATH: &str = "browser/custom-filter-rules.txt";
/// The stable source name used inside the Browser filter-list store.
const CUSTOM_FILTER_SOURCE_NAME: &str = "Operator custom rules";
/// The operator-managed URL policy path, relative to the workgroup root.
const MANAGED_URL_POLICY_PATH: &str = "browser/managed-url-policy.txt";

/// Count configured EasyList-style custom rule lines for operator status. This is
/// intentionally line-based; the matcher parser still owns detailed rule validity.
fn custom_filter_rule_count(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .count()
}

/// Parse the safe-browsing blocklist file: one host per line, `#` comments and blank
/// lines skipped, hosts trimmed + lowercased. Pure so the parse is unit-tested.
fn parse_safe_browsing_hosts(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Parse managed URL policy: one host suffix or URL prefix per line, `#` comments
/// and blanks skipped. Pure so unit tests cover the operator-facing format.
fn parse_managed_url_policy(text: &str) -> ManagedUrlPolicy {
    ManagedUrlPolicy::from_rules(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#')),
    )
}

fn plain_http_download_host_is_trusted(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return false;
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "mesh"
        || host.ends_with(".mesh")
        || host == "localhost"
        || host.ends_with(".localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.octets()[0] == 10 && ip.octets()[1] == 42)
}

fn browser_internal_plain_http_new_tab(url: &str) -> bool {
    url.trim_start()
        .get(..CEF_DEVTOOLS_URL.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(CEF_DEVTOOLS_URL))
}

/// The `live-helper` spawn glue: creating live [`WebSession`]s by launching the
/// sandboxed `mde-web-preview` helper (the client crate's `live-helper` API) and
/// attaching them as tabs, behind the honest deployment gates (§7). All of this is
/// compiled out of the default portable build.
#[cfg(feature = "live-helper")]
impl WebState {
    /// Record the live seat's output size (device px) so the next helper spawn
    /// pre-sizes its frame channel to it. Called each frame from the shell's Browser
    /// arm just before [`Self::ensure_live_tab`], so the very first spawn already
    /// knows the real seat.
    ///
    /// The channel is `w * h * 4` bytes of shm, so each axis is clamped to
    /// [`MAX_CHANNEL_DIM`]. Pre-sizing to the seat (the ceiling of any panel-driven
    /// resize, since the Browser panel never exceeds the screen) means a live
    /// [`WebSession::resize`] that enlarges the CEF paint always fits the channel
    /// instead of being silently dropped (`FrameChannelError::TooLarge`) — growing
    /// the channel dynamically would need re-attaching a new fd across the pinned
    /// CEF ABI, so pre-sizing is the chosen, documented alternative (browser-1).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "seat extent is rounded then clamped into [1, MAX_CHANNEL_DIM]"
    )]
    pub(crate) fn note_seat_px(&mut self, ctx: &egui::Context) {
        let size = ctx.screen_rect().size() * ctx.pixels_per_point();
        let clamp = |v: f32| -> u32 {
            if v.is_finite() {
                v.round().clamp(1.0, MAX_CHANNEL_DIM as f32) as u32
            } else {
                INIT_W
            }
        };
        self.seat_px = (clamp(size.x), clamp(size.y));
    }

    /// Ensure a live browser tab exists — spawn the sandboxed helper on first open.
    /// The shell's Browser arm calls this each frame with the live seat verdict. A
    /// no-op once a tab is open, and the real `Command::spawn` is attempted at most
    /// once (a failure surfaces an honest notice, never a per-frame spawn-storm).
    pub(crate) fn ensure_live_tab(&mut self, seat_present: bool) {
        if !self.tabs.is_empty() || self.spawn_attempted {
            return;
        }
        self.restore_startup_session_once();
        self.drain_incoming_send_tabs();
        self.cap_eager_startup_open_requests();
        if !self.open_requested.is_empty() {
            self.spawn_attempted = true;
            self.drain_live_tab_requests(seat_present);
            return;
        }
        self.spawn_attempted = true;
        self.open_with(
            seat_present,
            self.engine,
            START_URL.to_owned(),
            helper_bin_path(self.engine),
            WebSession::spawn,
        );
    }

    /// Drain the visible tab strip's new-tab request into a real helper spawn.
    pub(crate) fn drain_live_tab_requests(&mut self, seat_present: bool) {
        while let Some(intent) = self.take_open_request() {
            match intent {
                TabOpenIntent::NewForeground(engine) => {
                    self.open_with(
                        seat_present,
                        engine,
                        START_URL.to_owned(),
                        helper_bin_path(engine),
                        WebSession::spawn,
                    );
                }
                TabOpenIntent::NewForegroundUrl { engine, url } => {
                    self.open_with(
                        seat_present,
                        engine,
                        url,
                        helper_bin_path(engine),
                        WebSession::spawn,
                    );
                }
                TabOpenIntent::ReplaceActiveUrl { index, engine, url } => {
                    if let Some(session) = self.make_session(
                        seat_present,
                        engine,
                        url,
                        helper_bin_path(engine),
                        WebSession::spawn,
                    ) {
                        self.replace_tab_with_session(index, session, engine);
                    }
                }
            }
        }
    }

    /// Respawn the active crashed tab with a fresh live session (respawn-on-reload),
    /// drained by the Browser arm when [`Self::take_respawn_request`] fires. Driven
    /// by an explicit user Reload, so it is not rate-limited by the one-shot latch.
    pub(crate) fn respawn_live(&mut self) {
        // A tab was already open, so the seat gate is already proven live.
        let engine = self
            .tabs
            .get(self.active)
            .map_or(self.engine, |tab| tab.engine);
        if let Some(session) = self.make_session(
            true,
            engine,
            START_URL.to_owned(),
            helper_bin_path(engine),
            WebSession::spawn,
        ) {
            self.respawn_active_with_engine(session, engine);
        }
    }

    /// Testable core of [`Self::ensure_live_tab`]: attach a session from `spawn` as
    /// the first tab, applying the honest gates. Production passes
    /// [`WebSession::spawn`]; the tests inject a `testkit` factory so no real process
    /// is spawned (hermetic CI).
    fn open_with(
        &mut self,
        seat_present: bool,
        engine: BrowserEngine,
        url: String,
        helper_bin: std::path::PathBuf,
        spawn: impl FnOnce(&SpawnSpec) -> std::io::Result<WebSession>,
    ) {
        if let Some(block) = self.managed_policy_block_for(&url) {
            self.block_managed_navigation(
                block,
                ManagedPolicyBlockTrigger::LiveSpawn,
                Some(engine),
            );
            return;
        }
        if let Some(session) = self.make_session(seat_present, engine, url, helper_bin, spawn) {
            self.push_session_with_engine(session, engine);
        }
    }

    /// Build one live session behind the honest gates (a usable seat · the helper
    /// binary installed · the spawn succeeding), or record a NAMED notice and return
    /// `None`. Never fakes a page, never panics, never hangs — a spawn failure
    /// surfaces its reason (§7).
    fn make_session(
        &mut self,
        seat_present: bool,
        engine: BrowserEngine,
        url: String,
        helper_bin: std::path::PathBuf,
        spawn: impl FnOnce(&SpawnSpec) -> std::io::Result<WebSession>,
    ) -> Option<WebSession> {
        if !seat_present {
            self.gate_notice = Some(NO_GPU_SEAT_NOTICE.to_owned());
            return None;
        }
        if !helper_bin.exists() {
            self.gate_notice = Some("The Browser engine is not installed.".to_owned());
            return None;
        }
        if engine == BrowserEngine::Cef {
            if cef_runtime_missing_path().is_some() {
                self.gate_notice =
                    Some("The Chromium engine is not installed completely.".to_owned());
                return None;
            }
            self.refresh_security_update_status_for_launch();
            if let Some(notice) = self.cef_security_update_gate_notice() {
                self.gate_notice = Some(notice);
                return None;
            }
        }
        // Pre-size the helper's frame channel to the live seat (device px) so a
        // later resize can grow the CEF paint up to the seat without overflowing
        // the channel (browser-1, item 3); falls back to (INIT_W, INIT_H) until a
        // real seat is seen via `note_seat_px`.
        let (width, height) = self.seat_px;
        let spec = SpawnSpec {
            helper_bin,
            env: helper_env_for(engine, self.power_mode),
            url,
            width,
            height,
        };
        match spawn(&spec) {
            Ok(session) => {
                self.gate_notice = None;
                Some(session)
            }
            Err(e) => {
                self.gate_notice = Some(format!("The Browser engine failed to start: {e}"));
                None
            }
        }
    }

    #[cfg(feature = "live-helper")]
    fn cef_security_update_gate_notice(&self) -> Option<String> {
        let status = self.latest_security_update.as_ref()?;
        if status.node != local_hostname() || status.state == "current" {
            return None;
        }

        let mut notice = format!(
            "The Chromium engine needs an update before this tab can open. {}.",
            status.drawer_state_label()
        );
        if let Some(target) = status.target_chromium_label() {
            notice.push(' ');
            notice.push_str(&target);
            notice.push('.');
        }
        if let Some(installed) = status.installed_chromium_label() {
            notice.push(' ');
            notice.push_str(&installed);
            notice.push('.');
        }
        let details = status.user_facing_details();
        if details.is_empty() {
            notice.push_str(" Update verification failed.");
        } else {
            for detail in details {
                notice.push(' ');
                notice.push_str(&detail);
                notice.push('.');
            }
        }
        Some(notice)
    }
}

#[cfg(feature = "live-helper")]
fn helper_env_for(engine: BrowserEngine, power_mode: bool) -> Vec<(String, String)> {
    if engine != BrowserEngine::Cef || !power_mode {
        return Vec::new();
    }
    vec![
        (CEF_BROWSER_POWER_MODE_ENV.to_owned(), "true".to_owned()),
        (CEF_EXTENSION_POWER_MODE_ENV.to_owned(), "true".to_owned()),
    ]
}

/// Render the Browser surface into `ui`: poll every tab, upload any fresh frame on
/// the active tab, draw the navigation chrome, and paint the body (or the honest
/// crashed / loading / gated states).
pub(crate) fn web_panel(ui: &mut egui::Ui, state: &mut WebState) {
    // Tab-strip keyboard UX: consume the browser-reserved shortcuts FIRST so
    // neither chrome widgets nor the page-canvas forwarding in `paint_body`
    // ever see them.
    chrome_ui::handle_tab_keyboard(ui.ctx(), state);
    state.poll_browser_services_before_tabs();
    let tab_events = state.poll_tabs_for_panel();
    state.apply_tab_poll_events(tab_events);
    state.finish_browser_panel_poll();
    state.upload_active_frame(ui.ctx());
    state.upload_media_pip_frame(ui.ctx());
    state.request_browser_frame_repaint(ui.ctx());
    chrome_ui::install_browser_accessibility(ui.ctx(), ui.max_rect(), state);

    // Immersive/fullscreen mode: only the page body renders — no tab strip, nav bar,
    // bookmarks, or drawers. Triggered by F11 (manual, state.fullscreen) OR the page
    // itself entering HTML5 fullscreen (on_fullscreen_mode_change → the active
    // session reports it). F11/Esc exits the manual mode; the page exit clears its own.
    let page_fullscreen = state
        .tabs
        .get(state.active)
        .is_some_and(|tab| tab.session.fullscreen());
    if state.fullscreen || page_fullscreen {
        chrome_ui::active_body(ui, state);
        chrome_ui::media_pip_overlay(ui, state);
        return;
    }

    // The accelerator + omnibox-sync guards above read LAST frame's chrome
    // text-field focus; re-collect it from the chrome widgets painted below
    // (the omnibox, the find bar, and the dashboard search each OR into it).
    state.chrome_edit_focus = false;
    state.omnibox_focused = false;

    if state.vertical_tabs {
        let panel_rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        if panel_rect.is_positive() {
            ui.allocate_rect(panel_rect, egui::Sense::hover());
            let rail_right = (panel_rect.left() + CHROME_TAB_RAIL_W).min(panel_rect.right());
            let rail_rect = egui::Rect::from_min_max(
                panel_rect.min,
                egui::pos2(rail_right, panel_rect.bottom()),
            );
            let content_left = (rail_right + CHROME_GAP).min(panel_rect.right());
            let content_rect = egui::Rect::from_min_max(
                egui::pos2(content_left, panel_rect.top()),
                panel_rect.max,
            );

            let mut rail_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rail_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            chrome_ui::scope(&mut rail_ui, |ui| {
                chrome_ui::tab_strip(ui, state);
            });

            if content_rect.is_positive() {
                let mut content_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(content_rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                chrome_ui::scope(&mut content_ui, |ui| {
                    // The navigation chrome (back / forward / reload / address bar),
                    // wired to the active session's control socket.
                    chrome_ui::nav_chrome(ui, state);
                    chrome_ui::bookmarks_bar(ui, state);
                    chrome_ui::find_chrome(ui, state);
                });
                chrome_ui::insecure_prompt(&mut content_ui, state);
                chrome_ui::capture_notice(&mut content_ui, state);
                chrome_ui::drawer_stack(&mut content_ui, state);
                content_ui.add_space(CHROME_GAP);
                chrome_ui::active_body(&mut content_ui, state);
                chrome_ui::media_pip_overlay(&mut content_ui, state);
            }
        }
    } else {
        let panel_rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        if panel_rect.is_positive() {
            ui.allocate_rect(panel_rect, egui::Sense::hover());
            let mut panel_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(panel_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            panel_ui.set_clip_rect(panel_rect);
            chrome_ui::scope(&mut panel_ui, |ui| {
                // First-class tab strip (BROWSER-DD-2): switch/close existing isolated
                // sessions and expose a real new-tab intent for the live-helper path.
                chrome_ui::tab_strip(ui, state);
                ui.add_space(CHROME_GAP);

                // The navigation chrome (back / forward / reload / address bar), wired
                // to the active session's control socket.
                chrome_ui::nav_chrome(ui, state);
                chrome_ui::bookmarks_bar(ui, state);
                chrome_ui::find_chrome(ui, state);
            });
            chrome_ui::insecure_prompt(&mut panel_ui, state);
            chrome_ui::capture_notice(&mut panel_ui, state);
            chrome_ui::drawer_stack(&mut panel_ui, state);
            panel_ui.add_space(CHROME_GAP);
            chrome_ui::active_body(&mut panel_ui, state);
            chrome_ui::media_pip_overlay(&mut panel_ui, state);
        }
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_owned();
    }

    if max_chars <= 3 {
        return ".".repeat(max_chars);
    }

    let mut out = s.chars().take(max_chars - 3).collect::<String>();
    out.push_str("...");
    out
}

fn browser_output_label(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let label = raw
        .rsplit(['/', '\\'])
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("saved file");
    ellipsize(label, 48)
}

fn pdf_save_failed_notice(err: &str) -> String {
    match err {
        "no live page" => "PDF failed: no live page".to_owned(),
        "no active tab" => "PDF failed: no active tab".to_owned(),
        _ if err.starts_with("could not create ") => {
            "PDF failed: could not prepare the PDF folder".to_owned()
        }
        _ => "PDF failed: could not save the page".to_owned(),
    }
}

fn media_metadata_chip_label(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let title = value
        .get("title")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            value
                .get("source_url")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })?;
    let artist = value
        .get("artist")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let label = artist.map_or_else(
        || format!("Now: {title}"),
        |artist| format!("Now: {title} - {artist}"),
    );
    Some(ellipsize(&label, 32))
}

fn tab_media_is_playing(tab: &Tab) -> bool {
    if tab.session.audible() {
        return true;
    }
    tab.session
        .media_metadata()
        .and_then(|metadata| serde_json::from_str::<serde_json::Value>(&metadata.body).ok())
        .and_then(|value| value.get("paused").and_then(serde_json::Value::as_bool))
        .is_some_and(|paused| !paused)
}

/// The crash reason string for a session, or empty if it has not crashed.
fn crash_reason(session: &WebSession) -> String {
    match session.state() {
        SessionState::Crashed { reason } => reason.clone(),
        _ => String::new(),
    }
}

/// Whether a tab should show the TLS/certificate-error interstitial instead
/// of its normal frame — the same precedence `active_body` encodes: crashed
/// wins if a tab is somehow both crashed and cert-blocked, so this is only
/// `true` once `is_crashed` has been ruled out. Pure + testable in isolation
/// from the egui paint path.
fn shows_cert_interstitial(is_crashed: bool, cert_error: Option<&CertError>) -> bool {
    !is_crashed && cert_error.is_some()
}

/// What "Back to safety" does on the cert-error interstitial — go back if the
/// tab has history, otherwise there is nowhere honest to land it but closed
/// (there is no "proceed anyway" past a blocked certificate). Pure decision,
/// factored out of `active_body` so the choice is unit-testable on its own.
enum CertErrorBackAction {
    GoBack,
    CloseTab,
}

fn cert_error_back_action(can_back: bool) -> CertErrorBackAction {
    if can_back {
        CertErrorBackAction::GoBack
    } else {
        CertErrorBackAction::CloseTab
    }
}

// ── BOOKMARKS-10: mesh integration (Send-in-Chat + copy-URL + add-from-page) ──
//
// The Browser surface composes the live page with two mesh services by REUSING
// their existing Bus verbs (§6 JSON boundaries — a local mirror of each topic,
// never a dep on the mackesd worker, never a re-derived store):
//
//   * add-from-page → `action/bookmarks/add` — the BOOKMARKS-2 worker (the
//                      single writer of this node's op-log segment + HLC clock,
//                      the services/desktop tier boundary) drains it, mints the
//                      real `Op::Add`, persists it, and Syncthing-syncs it across
//                      the mesh. `source` is omitted → the honest `Source::Manual`.
//   * Send-in-Chat  → `action/chat/send` — the SAME verb the shell's Chat composer
//                      and Files' `chat_bridge` publish; a link rides the
//                      NOTIFY-CHAT `Clipboard` message-kind (a `kind` wins over
//                      `text` in the worker), addressed to this node's own Chat
//                      contact (the notification hub where its clips land).
//   * copy-URL      → the shell clipboard — egui's output command (the DRM /
//                      windowed backend owns the wire), the same one-click re-copy
//                      the Chat clipboard card uses.

/// The mackesd bookmarks worker's add verb (`action/bookmarks/<verb>`, §9).
const ACTION_BOOKMARKS_ADD: &str = "action/bookmarks/add";

/// The mackesd chat worker's send verb (reused, never re-invented).
const ACTION_CHAT_SEND: &str = "action/chat/send";

/// The mackesd adfilter worker's mesh-wide per-site allowlist verbs.
const ACTION_ADFILTER_ALLOW: &str = "action/adfilter/allow";
const ACTION_ADFILTER_BLOCK: &str = "action/adfilter/block";

/// Browser-to-platform display placement handoff. The shell owns tab intent; the
/// compositor/display owner drains this stream when it can perform the output
/// migration.
const ACTION_BROWSER_DISPLAY_TARGET: &str = "action/browser/display-target";

/// Browser external-protocol handoff for schemes without a dedicated worker yet.
const ACTION_BROWSER_PROTOCOL: &str = "action/browser/protocol";

/// Browser page-share handoff for platform targets whose receivers live outside
/// the Browser surface.
const ACTION_BROWSER_SHARE: &str = "action/browser/share";

/// Browser follow-me/send-tab handoff. The session-sync owner drains this stream
/// and routes by concrete `target_id` into the node or paired-phone handoff
/// substrate; Browser only owns the tab metadata and stable user action.
const ACTION_BROWSER_SEND_TAB: &str = "action/browser/send-tab";

/// Browser passkey/WebAuthn ceremony handoff. The helper observes page WebAuthn
/// calls; Browser adds local source metadata; the daemon passkey owner validates
/// and persists the pending ceremony.
const ACTION_BROWSER_PASSKEY: &str = "action/browser/passkey";

/// Browser read-aloud handoff. The Browser owns page text extraction; the TTS
/// owner drains this bounded text request and performs speech synthesis/playback.
const ACTION_BROWSER_READ_ALOUD: &str = "action/browser/read-aloud";

/// Browser translation handoff. The Browser owns page text extraction; the
/// offline/mesh translation owner drains this bounded private request.
const ACTION_BROWSER_TRANSLATE: &str = "action/browser/translate";

/// Browser offline-cache handoff. The Browser owns page text extraction; the
/// cache owner persists a private local copy and mirrors it onto the mesh file
/// plane without re-enabling helper disk cache.
const ACTION_BROWSER_OFFLINE_CACHE: &str = "action/browser/offline-cache";

/// Browser voice-command/dictation handoff. The Browser owns active-tab context;
/// the STT owner drains this request, captures audio, and publishes/apply commands.
const ACTION_BROWSER_VOICE_COMMAND: &str = "action/browser/voice-command";

/// Browser prompted sensitive-device permission decision. The current helpers
/// enforce deny-all; this stream records the prompt decision and gives the later
/// engine permission hook a typed contract.
const ACTION_BROWSER_PERMISSION_PROMPT: &str = "action/browser/permission-prompt";

/// Browser media-control action prefix. KDC/MPRIS and future desktop media
/// surfaces publish node-addressed transport requests here; the shell applies
/// them to the active/audible Browser media tab.
const ACTION_BROWSER_MEDIA_CONTROL_PREFIX: &str = "action/browser/media-control/";

/// Browser voice-command transcript result prefix, owned by the daemon STT worker.
const EVENT_BROWSER_VOICE_COMMAND_PREFIX: &str = "event/browser-voice-command/";

/// Browser read-aloud status prefix, owned by the daemon TTS worker.
const STATE_BROWSER_READ_ALOUD_PREFIX: &str = "state/browser-read-aloud/";

/// Browser voice-command status prefix, owned by the daemon STT worker.
const STATE_BROWSER_VOICE_COMMAND_PREFIX: &str = "state/browser-voice-command/";

/// Browser passkey/WebAuthn status prefix, owned by the daemon passkey worker.
const STATE_BROWSER_PASSKEYS_PREFIX: &str = "state/browser-passkeys/";

/// Browser passkey/WebAuthn completion-event prefix, owned by the daemon worker.
const EVENT_BROWSER_PASSKEYS_PREFIX: &str = "event/browser-passkeys/";

/// Browser translation result prefix, owned by the daemon translation worker.
const EVENT_BROWSER_TRANSLATE_PREFIX: &str = "event/browser-translate/";

/// Browser platform-share route event prefix, owned by the daemon share worker.
const EVENT_BROWSER_SHARE_PREFIX: &str = "event/browser-share/";

/// Browser offline-cache record prefix, owned by the daemon cache worker.
const EVENT_BROWSER_OFFLINE_CACHE_PREFIX: &str = "event/browser-offline-cache/";

/// Browser CEF security-update status prefix, owned by the daemon updater worker.
const STATE_BROWSER_SECURITY_UPDATE_PREFIX: &str = "state/browser-security-update/";

/// Browser-owned retained page/media-session status for desktop media bridges.
const STATE_BROWSER_MEDIA_PREFIX: &str = "state/browser-media/";

/// Browser safe-browsing source-read status prefix.
const STATE_BROWSER_SAFE_BROWSING_SOURCE_PREFIX: &str = "state/browser-safe-browsing-source/";

/// Browser managed URL policy source-read status prefix.
const STATE_BROWSER_MANAGED_POLICY_SOURCE_PREFIX: &str = "state/browser-managed-url-policy-source/";

/// Browser custom filter rules source-read status prefix.
const STATE_BROWSER_CUSTOM_FILTER_RULES_SOURCE_PREFIX: &str =
    "state/browser-custom-filter-rules-source/";

/// Browser synced filter-list source-read status prefix.
const STATE_BROWSER_FILTER_LIST_SOURCE_PREFIX: &str = "state/browser-filter-list-source/";

/// Browser managed-policy block audit event. Operators/admin tooling can consume
/// this without scraping UI notices.
const EVENT_BROWSER_POLICY_BLOCK: &str = "event/browser/policy-block";

/// Browser safe-browsing block audit event. Operators/admin tooling can consume
/// unsafe-host blocks without scraping interstitials or download notices.
const EVENT_BROWSER_SAFE_BROWSING_BLOCK: &str = "event/browser/safe-browsing-block";

/// Browser TLS/certificate-error block audit event. Operators/admin tooling can
/// consume top-level certificate failures without scraping interstitials.
const EVENT_BROWSER_CERTIFICATE_ERROR: &str = "event/browser/certificate-error";

/// Browser insecure-download block audit event. Operators/admin tooling can
/// distinguish transport hard-blocks from content/policy blocks.
const EVENT_BROWSER_INSECURE_DOWNLOAD_BLOCK: &str = "event/browser/insecure-download-block";

/// Browser plain-HTTP top-level navigation audit event. Operators/admin tooling
/// can distinguish prompts, user continues, upgrades, cancels, and session-HSTS
/// auto-upgrades.
const EVENT_BROWSER_INSECURE_NAVIGATION: &str = "event/browser/insecure-navigation";

/// Browser mixed-content block audit event. Operators/admin tooling can consume
/// secure-page downgrade blocks without scraping resource manifests.
const EVENT_BROWSER_MIXED_CONTENT_BLOCK: &str = "event/browser/mixed-content-block";

/// Browser per-site blocker toggle audit event. Operators/admin tooling can
/// distinguish user privacy overrides from synced filter-list policy.
const EVENT_BROWSER_SITE_BLOCKING: &str = "event/browser/site-blocking";

/// Browser current-site data clear audit event. This records the session-memory
/// reset that actually happened under the no-persistent-cookie threat model.
const EVENT_BROWSER_SITE_DATA_CLEAR: &str = "event/browser/site-data-clear";

/// Browser all-session browsing-data clear audit event. Operators/admin tooling can
/// distinguish a full in-memory session wipe from a single-site reset.
const EVENT_BROWSER_BROWSING_DATA_CLEAR: &str = "event/browser/browsing-data-clear";

/// Browser runtime permission decision audit event. This records actual page
/// capability decisions, including session-grant reuse auto-allows.
const EVENT_BROWSER_PERMISSION_DECISION: &str = "event/browser/permission-decision";

/// Browser permission revocation audit event. This records explicit current-site
/// permission forgetting, including session-grant removal.
const EVENT_BROWSER_PERMISSION_REVOKE: &str = "event/browser/permission-revoke";

/// Browser session credential action audit event. This records save/update/delete
/// and fill decisions without username, password, or per-credential identifiers.
const EVENT_BROWSER_CREDENTIAL: &str = "event/browser/credential";

/// Browser dangerous-download audit event. Operators/admin tooling can distinguish
/// the warning prompt from the user's eventual Keep or Discard decision.
const EVENT_BROWSER_DOWNLOAD_DANGER: &str = "event/browser/download-danger";

/// Browser follow-me session snapshot. The sync owner drains this stream into the
/// Nebula+Syncthing session store and later drives startup restore; Browser only
/// publishes the state it already owns.
const ACTION_BROWSER_SESSION_SYNC: &str = "action/browser/session-sync";

/// Daemon-owned Browser session-sync snapshot subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SESSION_SYNC_SUBDIR` without creating
/// a desktop-shell dependency on the daemon crate.
#[cfg(any(test, feature = "live-helper"))]
const SESSION_SYNC_SUBDIR: &str = "browser-session-sync";

/// Daemon-owned latest snapshot filename. The file body is the Browser snapshot
/// JSON itself, so startup restore can feed it straight into the parser.
#[cfg(any(test, feature = "live-helper"))]
const SESSION_SYNC_LATEST_FILE: &str = "latest.json";

/// Daemon-owned send-tab outbox subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SEND_TAB_OUTBOX_SUBDIR`.
const SEND_TAB_OUTBOX_SUBDIR: &str = "browser-send-tab";

/// Shell-owned replay ledger for processed send-tab records. This is intentionally
/// separate from the daemon outbox so a surviving or unlinkable JSON record cannot
/// reopen tabs after the shell restarts.
const SEND_TAB_CONSUMED_SUBDIR: &str = "browser-send-tab-consumed";

/// Browser idle-tab suspension handoff for deeper engine/process orchestration.
const ACTION_BROWSER_TAB_SUSPEND: &str = "action/browser/tab-suspend";

/// Existing Voice handoff used by Chat's Call action; Browser `tel:` URLs reuse
/// it instead of creating a second dial verb.
const ACTION_VOICE_DIAL: &str = "action/voice/dial";

/// Browser-originated notifications folded by Chat's alert lane into the unified
/// Notifications feed.
const EVENT_NOTIFY_BROWSER: &str = "event/notify/browser";

/// The mesh-hosted SearXNG endpoint used by the first-cut omnibox when the draft
/// is plain search text rather than a URL. This is intentionally mesh-local, not a
/// public search provider default.
const DEFAULT_SEARCH_URL: &str = "https://search.mesh/search";

/// A user-selectable search engine (Tier-2 "configurable search engines"): a display
/// `name`, an omnibox `keyword` shortcut (type "`kw` query" to use it — Chrome's
/// per-keyword search / tab-to-search), and a `template` whose `%s` is replaced by
/// the percent-encoded query. Session-only for now; the model is the extension point
/// for an operator-managed list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SearchEngine {
    name: String,
    keyword: String,
    template: String,
}

/// The default engine set: the mesh SearXNG search plus its image/video category
/// shortcuts (SearXNG honors `&categories=`). The first entry is the fallback default
/// when no keyword matches; the rest are keyword shortcuts. Mesh-local by design (no
/// public provider default), matching [`DEFAULT_SEARCH_URL`].
fn default_search_engines() -> Vec<SearchEngine> {
    vec![
        SearchEngine {
            name: "Mesh Search".to_owned(),
            keyword: "s".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?q=%s"),
        },
        SearchEngine {
            name: "Mesh Images".to_owned(),
            keyword: "img".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?categories=images&q=%s"),
        },
        SearchEngine {
            name: "Mesh Videos".to_owned(),
            keyword: "vid".to_owned(),
            template: format!("{DEFAULT_SEARCH_URL}?categories=videos&q=%s"),
        },
    ]
}

/// If `draft` begins with a configured engine's keyword followed by a query
/// ("`img` sunset"), return that engine's URL with `%s` replaced by the
/// percent-encoded query; else `None` (the caller falls back to the default router).
/// Pure + unit-tested.
fn keyword_search_target(draft: &str, engines: &[SearchEngine]) -> Option<String> {
    let (kw, rest) = draft.trim().split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    let engine = engines
        .iter()
        .find(|e| e.keyword.eq_ignore_ascii_case(kw))?;
    Some(engine.template.replace("%s", &percent_encode_query(rest)))
}

/// Mesh-local SearXNG autocomplete endpoint. SearXNG deployments commonly expose
/// autocomplete through this path; the parser below accepts the JSON shapes used
/// by SearXNG/OpenSearch-style providers so the shell does not bake in one brittle
/// response variant.
const DEFAULT_SUGGEST_URL: &str = "https://search.mesh/autocompleter";

/// Keep a live suggestion request tight; this is interactive chrome, not page load.
const SUGGEST_TIMEOUT: Duration = Duration::from_millis(900);

type SuggestionResult = Result<(String, Vec<String>), String>;
type SpellcheckResult = (u64, Result<Vec<SpellMiss>, String>);

mod content_tools;
use content_tools::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceCommandMode {
    Command,
    Dictation,
}

impl VoiceCommandMode {
    fn wire(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Dictation => "dictation",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Command => "Voice command",
            Self::Dictation => "Dictation",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "command" => Some(Self::Command),
            "dictation" => Some(Self::Dictation),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VoiceTranscriptResult {
    host: String,
    mode: VoiceCommandMode,
    tab_index: usize,
    focus: String,
    transcript: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserReadAloudStatus {
    node: String,
    last_title: Option<String>,
    last_url: Option<String>,
    state: String,
    last_error: Option<String>,
    accepted: u64,
    spoken: u64,
    rejected: u64,
    last_request_ms: Option<u64>,
    updated_ms: u64,
}

impl BrowserReadAloudStatus {
    #[cfg(test)]
    fn is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn is_actionable(&self) -> bool {
        matches!(self.state.as_str(), "speaking" | "unavailable" | "error")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "spoken" => ChipTone::Ok,
            "speaking" => ChipTone::Info,
            "unavailable" | "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "idle" => "Read aloud idle".to_owned(),
            "speaking" => "Reading aloud".to_owned(),
            "spoken" => "Read aloud complete".to_owned(),
            "unavailable" => "Read aloud unavailable".to_owned(),
            "error" => "Read aloud error".to_owned(),
            other => format!("Read aloud {}", sentence_case_ascii(other)),
        }
    }

    fn user_facing_error(&self) -> Option<String> {
        self.last_error
            .as_deref()
            .and_then(speech_status_error_label)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserVoiceCommandStatus {
    node: String,
    last_url: Option<String>,
    last_mode: Option<String>,
    state: String,
    last_error: Option<String>,
    accepted: u64,
    transcribed: u64,
    rejected: u64,
    last_transcript_chars: Option<u64>,
    last_request_ms: Option<u64>,
    updated_ms: u64,
}

impl BrowserVoiceCommandStatus {
    #[cfg(test)]
    fn is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn is_actionable(&self) -> bool {
        matches!(self.state.as_str(), "listening" | "unavailable" | "error")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "transcribed" => ChipTone::Ok,
            "listening" => ChipTone::Info,
            "unavailable" | "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        let prefix = match self.last_mode.as_deref() {
            Some("dictation") => "Dictation",
            _ => "Voice",
        };
        match self.state.as_str() {
            "idle" => format!("{prefix} idle"),
            "listening" => format!("{prefix} listening"),
            "transcribed" => format!("{prefix} captured"),
            "unavailable" => format!("{prefix} unavailable"),
            "error" => format!("{prefix} error"),
            other => format!("{prefix} {}", sentence_case_ascii(other)),
        }
    }

    fn user_facing_error(&self) -> Option<String> {
        self.last_error
            .as_deref()
            .and_then(speech_status_error_label)
    }
}

fn speech_status_error_label(detail: &str) -> Option<String> {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("stt runtime") && lower.contains("not configured") {
        return Some("Voice input is not configured".to_owned());
    }
    if lower.contains("tts runtime") && lower.contains("not configured") {
        return Some("Read aloud is not configured".to_owned());
    }
    if lower.contains("runtime") && lower.contains("not configured") {
        return Some("Speech service is not configured".to_owned());
    }

    Some(sentence_case_ascii(trimmed))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserPasskeyStatus {
    node: String,
    last_request_id: Option<String>,
    last_host: Option<String>,
    last_ceremony: Option<String>,
    last_rp_id: Option<String>,
    state: String,
    mirrored: bool,
    last_error: Option<String>,
    accepted: u64,
    rejected: u64,
    last_pending_ms: Option<u64>,
    hardware_state: String,
    hardware_key_count: u64,
    hardware_readable_count: u64,
    hardware_ctaphid_state: String,
    hardware_ctaphid_init_frame_count: u64,
    hardware_probe_ms: u64,
    updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserPasskeyCompletion {
    client_request_id: String,
    body: String,
}

impl BrowserPasskeyStatus {
    #[cfg(test)]
    fn ceremony_is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    #[cfg(test)]
    fn hardware_is_visible(&self) -> bool {
        self.hardware_state != "unknown"
    }

    #[cfg(test)]
    fn ctaphid_is_visible(&self) -> bool {
        self.hardware_ctaphid_state == "init_request_ready"
            && self.hardware_ctaphid_init_frame_count > 0
    }

    #[cfg(test)]
    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "pending" => ChipTone::Info,
            "created" | "asserted" => ChipTone::Ok,
            "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    #[cfg(test)]
    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "pending" => "Passkey pending".to_owned(),
            "created" => "Passkey created".to_owned(),
            "asserted" => "Passkey asserted".to_owned(),
            "error" => "Passkey error".to_owned(),
            other => format!("Passkey {other}"),
        }
    }

    #[cfg(test)]
    fn hardware_tone(&self) -> ChipTone {
        match self.hardware_state.as_str() {
            "ready" => ChipTone::Ok,
            "present_permission_denied" => ChipTone::Warn,
            "unavailable" => ChipTone::Neutral,
            _ => ChipTone::Neutral,
        }
    }

    #[cfg(test)]
    fn hardware_chip_label(&self) -> String {
        match self.hardware_state.as_str() {
            "ready" => "Security key ready".to_owned(),
            "present_permission_denied" => "Security key blocked".to_owned(),
            "unavailable" => "Security key unavailable".to_owned(),
            other => format!("Security key {other}"),
        }
    }

    #[cfg(test)]
    fn ctaphid_tone(&self) -> ChipTone {
        match self.hardware_ctaphid_state.as_str() {
            "init_request_ready" => ChipTone::Info,
            _ => ChipTone::Neutral,
        }
    }

    #[cfg(test)]
    fn ctaphid_chip_label(&self) -> String {
        match self.hardware_ctaphid_state.as_str() {
            "init_request_ready" => "CTAP INIT framed".to_owned(),
            other => format!("CTAP HID {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserTranslationResult {
    host: String,
    tab_index: usize,
    engine: BrowserEngine,
    url: String,
    title: String,
    source_lang: String,
    target_lang: String,
    translation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserShareRouteResult {
    host: String,
    target: BrowserShareTarget,
    url: String,
    title: String,
    preview: String,
    request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserQrShareResult {
    host: String,
    url: String,
    title: String,
    preview: String,
    request_id: String,
    modules: Vec<Vec<bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserOfflineCacheResult {
    host: String,
    cache_id: String,
    tab_index: usize,
    engine: BrowserEngine,
    url: String,
    title: String,
    text: String,
    viewport: Option<OfflineCacheViewportImage>,
    resources: Vec<OfflineCacheResource>,
    archive_mhtml: Option<OfflineCacheArchive>,
    pdf_snapshot: Option<OfflineCachePdf>,
    cached_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserSecurityUpdateStatus {
    node: String,
    state: String,
    expected_cef_version: Option<String>,
    expected_chromium_version: Option<String>,
    expected_channel: Option<String>,
    active_runtime: Option<String>,
    installed_version: Option<String>,
    installed_chromium: Option<String>,
    libcef_present: bool,
    updater_state: String,
    last_update_ms: Option<u64>,
    last_update_exit_code: Option<i32>,
    last_update_error: Option<String>,
    last_error: Option<String>,
    updated_ms: u64,
}

impl BrowserSecurityUpdateStatus {
    fn is_actionable(&self) -> bool {
        self.state != "current" || !matches!(self.updater_state.as_str(), "idle" | "attempted")
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "current" if matches!(self.updater_state.as_str(), "idle" | "attempted") => {
                ChipTone::Ok
            }
            "missing" | "mismatch" | "manifest_missing" => ChipTone::Warn,
            _ if self.updater_state == "installing" => ChipTone::Info,
            _ if self.updater_state == "failed" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn drawer_state_label(&self) -> &'static str {
        if self.updater_state == "installing" {
            return "Installing update";
        }
        if self.state == "current" && self.updater_state == "failed" {
            return "Update check failed";
        }
        match self.state.as_str() {
            "current" => "Current",
            "missing" => "Install needed",
            "mismatch" => "Update needed",
            "manifest_missing" => "Update details missing",
            _ => "Needs attention",
        }
    }

    fn updater_label(&self) -> &'static str {
        match self.updater_state.as_str() {
            "attempted" => "Checked for updates",
            "failed" => "Update failed",
            "idle" => "Ready to update",
            "installing" => "Installing update",
            _ => "Update status unavailable",
        }
    }

    fn target_chromium_label(&self) -> Option<String> {
        self.expected_chromium_version
            .as_deref()
            .filter(|version| !version.trim().is_empty())
            .map(|version| format!("Target Chromium {}", version.trim()))
            .or_else(|| {
                self.expected_cef_version
                    .as_deref()
                    .filter(|version| !version.trim().is_empty())
                    .map(|version| format!("Target engine {}", version.trim()))
            })
    }

    fn installed_chromium_label(&self) -> Option<String> {
        self.installed_chromium
            .as_deref()
            .filter(|version| !version.trim().is_empty())
            .map(|version| format!("Installed Chromium {}", version.trim()))
            .or_else(|| {
                self.installed_version
                    .as_deref()
                    .filter(|version| !version.trim().is_empty())
                    .map(|version| format!("Installed engine {}", version.trim()))
            })
            .or_else(|| {
                if self
                    .active_runtime
                    .as_deref()
                    .is_some_and(|runtime| !runtime.trim().is_empty())
                {
                    Some("Engine files detected".to_owned())
                } else if self.libcef_present {
                    None
                } else {
                    Some("Engine files missing".to_owned())
                }
            })
    }

    fn channel_label(&self) -> Option<String> {
        self.expected_channel
            .as_deref()
            .filter(|channel| !channel.trim().is_empty())
            .map(|channel| format!("{} channel", sentence_case_ascii(channel.trim())))
    }

    fn user_facing_details(&self) -> Vec<String> {
        let mut details = Vec::new();
        for raw in [
            self.last_update_error.as_deref(),
            self.last_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let Some(detail) = browser_security_update_detail_label(raw) else {
                continue;
            };
            if !details.iter().any(|existing| existing == &detail) {
                details.push(detail);
            }
        }
        details
    }

    #[cfg(test)]
    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "current" => "Chromium current".to_owned(),
            "missing" => "Chromium missing".to_owned(),
            "mismatch" => "Chromium mismatch".to_owned(),
            "manifest_missing" => "Chromium update details".to_owned(),
            other => format!("Chromium {}", sentence_case_ascii(other)),
        }
    }
}

fn browser_security_update_detail_label(detail: &str) -> Option<String> {
    let trimmed = detail.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("sha256") && lower.contains("mismatch") {
        return Some("Downloaded update did not pass verification".to_owned());
    }
    if lower.contains("active cef runtime") && lower.contains("packaged manifest") {
        return Some("Installed Chromium files do not match this build".to_owned());
    }
    if lower.contains("packaged manifest") {
        return Some("Installed Chromium files do not match this build".to_owned());
    }
    if lower.contains("/opt/") || lower.contains('\\') || lower.contains("runtime path") {
        return Some("Chromium engine update could not be verified".to_owned());
    }
    if lower.contains("libcef") {
        return Some("Chromium engine files are incomplete".to_owned());
    }
    let label = trimmed
        .replace("CEF", "Chromium")
        .replace("cef", "Chromium")
        .replace("packaged manifest", "this build")
        .replace("manifest", "update details");
    Some(sentence_case_ascii(&label))
}

fn sentence_case_ascii(text: &str) -> String {
    let trimmed = text.trim();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(trimmed.len());
    out.extend(first.to_uppercase());
    out.extend(chars);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserVoiceAction {
    NewTab,
    CloseTab,
    Back,
    Forward,
    Reload,
    ReadAloud,
    Find(String),
}

#[derive(Default)]
struct SpellcheckState {
    in_flight: Option<u64>,
    tab_index: Option<usize>,
    rx: Option<mpsc::Receiver<SpellcheckResult>>,
}

impl SpellcheckState {
    fn poll(&mut self) -> Option<(u64, usize, Result<Vec<SpellMiss>, String>)> {
        let rx = self.rx.take()?;
        match rx.try_recv() {
            Ok((id, result)) => {
                self.in_flight = None;
                let tab_index = self.tab_index.take().unwrap_or_default();
                Some((id, tab_index, result))
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
                None
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                let id = self.in_flight.take().unwrap_or_default();
                let tab_index = self.tab_index.take().unwrap_or_default();
                Some((id, tab_index, Err("Spell-check unavailable".to_owned())))
            }
        }
    }

    fn start(&mut self, id: u64, tab_index: usize, text: String) {
        let (tx, rx) = mpsc::channel();
        self.in_flight = Some(id);
        self.tab_index = Some(tab_index);
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = spell::run_hunspell(spell::HUNSPELL, &text)
                .map_err(|state| state.notice().to_owned());
            let _ = tx.send((id, result));
        });
    }
}

fn spellcheck_notice(result: Result<Vec<SpellMiss>, String>) -> String {
    match result {
        Ok(misses) if misses.is_empty() => "Spelling: no misspellings found".to_owned(),
        Ok(misses) => {
            let count = misses.len();
            let plural = if count == 1 { "" } else { "s" };
            let first = misses
                .first()
                .map(|miss| {
                    let suggestion = miss
                        .suggestions
                        .first()
                        .map_or(String::new(), |s| format!(" -> {s}"));
                    format!("; first: {}{}", miss.word, suggestion)
                })
                .unwrap_or_default();
            format!("Spelling: {count} possible misspelling{plural}{first}")
        }
        Err(err) => spellcheck_error_label(&err).map_or_else(
            || "Spelling unavailable".to_owned(),
            |label| format!("Spelling unavailable: {label}"),
        ),
    }
}

fn spellcheck_highlight_words(misses: &[SpellMiss]) -> Vec<String> {
    let mut words = BTreeSet::new();
    for miss in misses {
        let word = miss.word.trim();
        if word.len() < 2 || word.len() > 64 {
            continue;
        }
        words.insert(word.to_owned());
        if words.len() >= 64 {
            break;
        }
    }
    words.into_iter().collect()
}

fn spellcheck_occurrence_index(misses: &[SpellMiss], row_index: usize) -> u16 {
    let Some(current) = misses.get(row_index) else {
        return 0;
    };
    let word = current.word.trim();
    if word.is_empty() {
        return 0;
    }
    let prior = misses
        .iter()
        .take(row_index)
        .filter(|miss| miss.word.trim().eq_ignore_ascii_case(word))
        .count();
    prior.min(u16::MAX as usize) as u16
}

#[derive(Default)]
struct SuggestionState {
    draft: String,
    items: Vec<String>,
    notice: Option<String>,
    in_flight: Option<String>,
    rx: Option<mpsc::Receiver<SuggestionResult>>,
    /// Matching browsing-history visit URLs for the current draft — most-recent-first,
    /// capped (see [`WebState::update_suggestions_for_address`]). Rendered ABOVE the
    /// SearXNG `items` in the omnibox dropdown (Chrome-style). Set independently of the
    /// search-suggestion fetch gate ([`should_fetch_suggestions`]), so a URL-like draft
    /// that skips the SearXNG round-trip still surfaces a matching visit. Session-only —
    /// mirrors [`HistoryStore`], never persisted.
    history: Vec<String>,
    /// Matching bookmarks (`{title, url}`) for the current draft — rendered ABOVE
    /// history in the dropdown (Chrome ranks a saved bookmark above a mere visit).
    bookmarks: Vec<BookmarkBarLink>,
    /// Matching local files supplied by the shell Files model. Rendered after
    /// bookmarks and before browsing history so local paths remain high-signal
    /// without displacing saved page destinations.
    files: Vec<BrowserFileSuggestion>,
    /// Keyboard-highlighted suggestion — a flat index into
    /// [`Self::ordered_commit_values`] (bookmarks, files, history, then search).
    /// `None` = nothing highlighted (Enter submits the typed draft). Reset whenever
    /// the draft changes; moved by Up/Down while the omnibox has focus.
    selected: Option<usize>,
}

/// Next keyboard-highlight index after moving `delta` (±1) over `len` suggestions,
/// wrapping at both ends; from nothing highlighted, Down picks the first and Up the
/// last (Chrome's omnibox behavior). Pure so the traversal is unit-tested directly.
fn next_selection(current: Option<usize>, delta: i32, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        None => {
            if delta > 0 {
                0
            } else {
                len - 1
            }
        }
        Some(cur) => (cur as i32 + delta).rem_euclid(len as i32) as usize,
    })
}

/// The index to preselect for Chrome's "inline top-hit": `Some(0)` when the first
/// suggestion is an inline completion of the draft (the trimmed draft is a
/// case-insensitive prefix of it AND the suggestion adds more), so Enter accepts the
/// completed URL. `None` otherwise (nothing preselected; arrows drive selection).
/// Pure so the preselect rule is unit-tested directly.
fn inline_top_hit(ordered: &[String], draft: &str) -> Option<usize> {
    let d = draft.trim().to_lowercase();
    if d.is_empty() {
        return None;
    }
    let top = ordered.first()?;
    (top.to_lowercase().starts_with(&d) && top.trim().len() > d.len()).then_some(0)
}

/// The grey inline-completion tail painted inside the focused omnibox. This is
/// stricter than [`inline_top_hit`]: the keyboard preselect can tolerate
/// whitespace/case oddities, but a visual overlay must line up with the exact
/// draft buffer TextEdit is painting, so only no-trim, char-boundary prefixes
/// get a visible tail.
fn inline_completion_tail(ordered: &[String], draft: &str) -> Option<String> {
    if draft.is_empty() || draft.trim() != draft {
        return None;
    }
    let top = ordered.first()?;
    if !top.to_lowercase().starts_with(&draft.to_lowercase()) {
        return None;
    }
    top.get(draft.len()..)
        .filter(|tail| !tail.is_empty())
        .map(ToOwned::to_owned)
}

impl SuggestionState {
    fn clear(&mut self) {
        self.draft.clear();
        self.items.clear();
        self.notice = None;
        self.in_flight = None;
        self.rx = None;
        self.history.clear();
        self.bookmarks.clear();
        self.files.clear();
        self.selected = None;
    }

    /// Replace the history-match list (see [`SuggestionState::history`]).
    fn set_history_matches(&mut self, matches: Vec<String>) {
        self.history = matches;
    }

    /// Replace the bookmark-match list (see [`SuggestionState::bookmarks`]).
    fn set_bookmark_matches(&mut self, matches: Vec<BookmarkBarLink>) {
        self.bookmarks = matches;
    }

    /// Replace the local file-match list (see [`SuggestionState::files`]).
    fn set_file_matches(&mut self, matches: Vec<BrowserFileSuggestion>) {
        self.files = matches;
    }

    /// Browser's local contribution to the shared unified-omnibox model, in the
    /// same render/commit order the dropdown exposes: bookmarks, local files,
    /// history, then deduped web suggestions. The payload is the exact value
    /// accepted on Enter.
    fn ordered_search_items(&self) -> Vec<SearchItem<String>> {
        let mut items: Vec<SearchItem<String>> = Vec::new();
        items.extend(self.bookmarks.iter().enumerate().map(|(idx, bookmark)| {
            SearchItem::new(
                SearchDomain::BrowserBookmark,
                bookmark.title.clone(),
                bookmark.url.clone(),
                bookmark.url.clone(),
            )
            .with_source_rank(idx)
        }));
        let file_offset = items.len();
        items.extend(self.files.iter().enumerate().map(|(idx, file)| {
            SearchItem::new(
                SearchDomain::File,
                file.title.clone(),
                file.path.display().to_string(),
                file.url.clone(),
            )
            .with_source_rank(file_offset + idx)
        }));
        let history_offset = items.len();
        items.extend(self.history.iter().enumerate().map(|(idx, url)| {
            SearchItem::new(
                SearchDomain::BrowserHistory,
                url.clone(),
                url.clone(),
                url.clone(),
            )
            .with_source_rank(history_offset + idx)
        }));
        let search_offset = items.len();
        items.extend(
            chrome_ui::dedup_search_items(&self.items, &self.history)
                .into_iter()
                .enumerate()
                .map(|(idx, suggestion)| {
                    SearchItem::new(
                        SearchDomain::WebSuggestion,
                        suggestion.clone(),
                        format!(
                            "{DEFAULT_SEARCH_URL}?q={}",
                            percent_encode_query(suggestion)
                        ),
                        suggestion.clone(),
                    )
                    .with_source_rank(search_offset + idx)
                }),
        );
        items
    }

    /// The flat suggestion list in RENDER order (bookmarks, history, deduped search)
    /// as the strings that get committed on Enter — the index space for [`Self::selected`].
    fn ordered_commit_values(&self) -> Vec<String> {
        self.ordered_search_items()
            .into_iter()
            .map(|item| item.payload)
            .collect()
    }

    /// Move the keyboard highlight by `delta` (±1), wrapping over the current list.
    fn move_selection(&mut self, delta: i32) {
        let len = self.ordered_commit_values().len();
        self.selected = next_selection(self.selected, delta, len);
    }

    /// The commit value under the keyboard highlight, if any (Enter accepts it
    /// instead of the typed draft).
    fn selected_value(&self) -> Option<String> {
        self.selected
            .and_then(|i| self.ordered_commit_values().into_iter().nth(i))
    }

    /// The visible inline completion tail for the current draft, if the current
    /// selected suggestion is the top-hit preselect.
    fn inline_completion_tail(&self) -> Option<String> {
        (self.selected == Some(0))
            .then(|| self.ordered_commit_values())
            .and_then(|ordered| inline_completion_tail(&ordered, &self.draft))
    }

    fn poll(&mut self) {
        let Some(rx) = self.rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((draft, items))) => {
                if draft == self.draft {
                    self.items = items;
                    self.notice = None;
                }
                self.in_flight = None;
            }
            Ok(Err(err)) => {
                self.items.clear();
                self.notice = Some(err);
                self.in_flight = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.rx = Some(rx);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.items.clear();
                self.notice = Some("Suggestions unavailable".to_owned());
                self.in_flight = None;
            }
        }
    }

    fn update_for_draft(&mut self, draft: &str) {
        self.poll();
        let draft = draft.trim();
        if !should_fetch_suggestions(draft) {
            self.clear();
            return;
        }
        if self.draft != draft {
            self.draft = draft.to_owned();
            self.items.clear();
            self.notice = None;
        }
        if self.in_flight.as_deref() == Some(draft) {
            return;
        }
        self.in_flight = Some(draft.to_owned());
        let request = draft.to_owned();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = fetch_suggestions(&request).map(|items| (request.clone(), items));
            let _ = tx.send(result);
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MeshServiceShortcut {
    label: &'static str,
    url: &'static str,
    hint: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpeedDialEntry {
    label: String,
    url: String,
    hint: String,
}

impl SpeedDialEntry {
    fn new(label: impl Into<String>, url: impl Into<String>, hint: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
            hint: hint.into(),
        }
    }
}

const NEW_TAB_SERVICES: [MeshServiceShortcut; 4] = [
    MeshServiceShortcut {
        label: "Search",
        url: "https://search.mesh/",
        hint: "Open the mesh SearXNG front page",
    },
    MeshServiceShortcut {
        label: "Music",
        url: "http://music.mesh:4533/",
        hint: "Open the active-active Navidrome mesh service",
    },
    MeshServiceShortcut {
        label: "Horizon",
        url: "https://horizon.mesh/",
        hint: "Open the optional OpenStack dashboard when enabled",
    },
    MeshServiceShortcut {
        label: "Keystone",
        url: "http://keystone.mesh:5000/v3",
        hint: "Open the OpenStack identity API endpoint",
    },
];

fn default_speed_dial() -> Vec<SpeedDialEntry> {
    NEW_TAB_SERVICES
        .iter()
        .map(|service| SpeedDialEntry::new(service.label, service.url, service.hint))
        .collect()
}

fn default_session_restore_roots() -> Vec<PathBuf> {
    vec![local_session_sync_root(), default_workgroup_root()]
}

fn local_session_sync_root() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map_or_else(
        || {
            std::env::var_os("HOME").map_or_else(
                || PathBuf::from("/var/lib/mde/browser-session-sync"),
                |home| {
                    PathBuf::from(home)
                        .join(".local")
                        .join("share")
                        .join("mde")
                        .join("browser-session-sync")
                },
            )
        },
        |data_home| {
            PathBuf::from(data_home)
                .join("mde")
                .join("browser-session-sync")
        },
    )
}

#[cfg(any(test, feature = "live-helper"))]
fn session_sync_latest_path(root: &Path, host: &str) -> PathBuf {
    root.join(SESSION_SYNC_SUBDIR)
        .join(sanitize_session_host(host))
        .join(SESSION_SYNC_LATEST_FILE)
}

fn send_tab_inbox_dir(root: &Path, host: &str) -> PathBuf {
    root.join(SEND_TAB_OUTBOX_SUBDIR)
        .join("node")
        .join(sanitize_session_host(host))
}

fn send_tab_consumed_dir(root: &Path, host: &str) -> PathBuf {
    root.join(SEND_TAB_CONSUMED_SUBDIR)
        .join(sanitize_session_host(host))
}

fn send_tab_consumed_path(root: &Path, host: &str, record_id: &str) -> PathBuf {
    send_tab_consumed_dir(root, host).join(format!("{record_id}.seen"))
}

fn send_tab_record_is_consumed(roots: &[PathBuf], host: &str, record_id: &str) -> bool {
    roots
        .iter()
        .any(|root| send_tab_consumed_path(root, host, record_id).is_file())
}

fn write_send_tab_consumed_marker(roots: &[PathBuf], host: &str, record_id: &str) -> bool {
    for root in roots {
        let path = send_tab_consumed_path(root, host, record_id);
        let Some(parent) = path.parent() else {
            continue;
        };
        if std::fs::create_dir_all(parent).is_err() {
            continue;
        }
        if std::fs::write(&path, b"processed\n").is_ok() {
            return true;
        }
    }
    false
}

fn send_tab_consumed_record_id(relative_key: &str, body: &str) -> String {
    let mut hash = FNV1A64_OFFSET;
    hash = fnv1a64_update(hash, relative_key.as_bytes());
    hash = fnv1a64_update(hash, b"\0");
    hash = fnv1a64_update(hash, body.as_bytes());
    format!("{hash:016x}")
}

const FNV1A64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A64_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV1A64_PRIME);
    }
    hash
}

fn sanitize_session_host(host: &str) -> String {
    host.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                Some(c)
            } else if c.is_ascii_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserSendTabOpenDecision {
    Open(BrowserEngine, String),
    Consume,
}

fn browser_send_tab_open_intent(
    body: &str,
    host: &str,
) -> Result<BrowserSendTabOpenDecision, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("send-tab JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_send_tab") {
        return Err("send-tab has the wrong op".to_owned());
    }
    if v.get("target").and_then(serde_json::Value::as_str) != Some("node") {
        return Err("send-tab is not node-addressed".to_owned());
    }
    let target_id = v
        .get("target_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|target_id| !target_id.is_empty())
        .ok_or_else(|| "send-tab is missing target_id".to_owned())?;
    if sanitize_session_host(target_id) != sanitize_session_host(host) {
        return Err("send-tab is for a different node".to_owned());
    }
    if v.get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|source_host| !source_host.is_empty())
        .is_some_and(|source_host| {
            sanitize_session_host(source_host) == sanitize_session_host(host)
        })
    {
        return Ok(BrowserSendTabOpenDecision::Consume);
    }
    let engine = v
        .get("engine")
        .and_then(serde_json::Value::as_str)
        .and_then(BrowserEngine::from_wire)
        .ok_or_else(|| "send-tab has an unsupported engine".to_owned())?;
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "send-tab is missing url".to_owned())?;
    Ok(BrowserSendTabOpenDecision::Open(engine, url.to_owned()))
}

fn incoming_send_tab_files(root: &Path, host: &str) -> Vec<PathBuf> {
    let inbox = send_tab_inbox_dir(root, host);
    let Ok(sources) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for source in sources.filter_map(Result::ok) {
        let source_path = source.path();
        if !source_path.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&source_path) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn cleanup_empty_send_tab_source_dirs(root: &Path, host: &str) {
    let inbox = send_tab_inbox_dir(root, host);
    let Ok(sources) = std::fs::read_dir(&inbox) else {
        return;
    };
    for source in sources.filter_map(Result::ok) {
        let source_path = source.path();
        if !source_path.is_dir() {
            continue;
        }
        let is_empty = std::fs::read_dir(&source_path)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if is_empty {
            let _ = std::fs::remove_dir(&source_path);
        }
    }
}

#[cfg(any(test, feature = "live-helper"))]
fn speed_dial_from_settings(settings: &serde_json::Value) -> Option<Vec<SpeedDialEntry>> {
    let entries = settings.get("speed_dial")?.as_array()?;
    let restored = entries
        .iter()
        .filter_map(|entry| {
            let label = entry
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            let url = entry
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .trim();
            if label.is_empty() || url.is_empty() {
                return None;
            }
            let hint = entry
                .get("hint")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|hint| !hint.is_empty())
                .unwrap_or(url);
            Some(SpeedDialEntry::new(label, url, hint))
        })
        .take(32)
        .collect::<Vec<_>>();
    (!restored.is_empty()).then_some(restored)
}

mod wire;
use wire::*;

mod mpris;
pub(crate) use mpris::BrowserMprisHandle;

/// Start the Browser's freedesktop MPRIS bridge for this shell session.
pub(crate) fn spawn_browser_mpris() -> BrowserMprisHandle {
    mpris::spawn(mde_bus::client_data_dir(), local_hostname())
}

/// Mint the transfer job a completed browser download or scraper output uses once
/// the helper has materialized the file locally. The browser does not crawl or move
/// bytes itself here; it hands each resulting file to the daemon-owned Transfers
/// queue, preserving one download manager and one ledger (TRANSFERS-10).
fn browser_output_transfer_job(source: &str, dest: &str) -> TransferJob {
    TransferJob::new(
        source.trim(),
        dest.trim(),
        TransferMethod::BrowserDownload,
        TransferPolicy {
            bwlimit: None,
            verify: true,
        },
    )
}

/// Enqueue one materialized browser output into the shared Transfers queue.
///
/// # Errors
/// Returns an honest validation/dispatch error when either path is empty or the
/// transfer client cannot write the daemon inbox.
fn enqueue_browser_output(
    transfers: &dyn TransfersClient,
    source: &str,
    dest: &str,
) -> Result<String, String> {
    if source.trim().is_empty() || dest.trim().is_empty() {
        return Err("browser transfer enqueue requires source and destination paths".into());
    }
    let job = browser_output_transfer_job(source, dest);
    let id = job.id.clone();
    transfers.dispatch(&TransferVerb::Submit(job))?;
    Ok(id)
}

/// Enqueue every file produced by a Power-mode scrape/export batch. Each file
/// becomes its own `browser_download` job, so progress, verify, notify, pause, and
/// history stay in the same Transfers surface as ordinary downloads.
///
/// # Errors
/// Returns the first validation/dispatch error; any ids returned before that point
/// were already handed to the transfer worker.
fn enqueue_browser_output_batch(
    transfers: &dyn TransfersClient,
    sources: &[String],
    dest: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::with_capacity(sources.len());
    for source in sources {
        ids.push(enqueue_browser_output(transfers, source, dest)?);
    }
    Ok(ids)
}

/// Publish `body` on `topic` via the persist-first path (the same discipline as
/// the shell's Chat composer + Files' `chat_bridge`). Best-effort: no Bus on this
/// node / a transient open failure is a silent no-op — the honest solo-host
/// state, never a panic.
fn publish(topic: &str, body: &str) {
    publish_to_bus(mde_bus::client_data_dir().as_deref(), topic, body);
}

fn publish_to_bus(root: Option<&Path>, topic: &str, body: &str) {
    let Some(root) = root else { return };
    let Ok(persist) = Persist::open(root.to_path_buf()) else {
        return;
    };
    let _ = persist.write(topic, Priority::Default, None, Some(body));
}

/// The local hostname — the mesh identity a Send-in-Chat addresses (lock 2/21:
/// the hostname *is* the chat contact username). `$HOSTNAME` →
/// `/proc/sys/kernel/hostname` → `/etc/hostname` → `"localhost"`.
fn local_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    for path in ["/proc/sys/kernel/hostname", "/etc/hostname"] {
        if let Ok(h) = std::fs::read_to_string(path) {
            let h = h.trim();
            if !h.is_empty() {
                return h.to_string();
            }
        }
    }
    "localhost".to_string()
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

mod capture;
use capture::*;

/// Whether the compact toolbar's reload slot should present as a real Stop
/// control instead of Reload.
///
/// Only CEF exposes a genuine cancel-load hook (`cef_browser_t::stop_load`,
/// offset-verified against the pinned CEF 149 headers — see
/// `mde-web-cef::cef_browser::apply_control_frame`). Servo's embedding API does
/// not: the official `servo`/`servo-embedder-traits`/`servo-constellation-traits`
/// 0.3.0 crates.io publications this workspace pins were inspected directly
/// (DD-2, 2026-07-10) — `WebView`'s and `Servo`'s complete public method sets
/// carry no stop/cancel-navigation method, the `WebDriverCommandMsg` relay
/// `Servo::execute_webdriver_command` accepts has no stop/cancel-navigation
/// variant (only `LoadUrl`/`Refresh`/`GoBack`/`GoForward` affect navigation),
/// and the `EmbedderToConstellationMessage` channel `WebView::load`/`reload`
/// send on (unreachable anyway — `WebView::inner()` is a private accessor)
/// has no such variant either. Returning `true` for a non-CEF engine would
/// paint a Stop button that silently does nothing when clicked, which is worse
/// than the honest Reload it would replace, so this stays a hard per-engine
/// check rather than a capability guess.
fn can_show_stop_control(
    has_tab: bool,
    crashed: bool,
    loading: bool,
    engine: Option<BrowserEngine>,
) -> bool {
    has_tab && !crashed && loading && engine == Some(BrowserEngine::Cef)
}

/// Stable egui id for the omnibox TextEdit, so its keyboard focus can be
/// tracked across frames (and driven by the tests).
fn omnibox_widget_id() -> egui::Id {
    egui::Id::new("browser-omnibox")
}

fn short_transfer_name(job: &TransferJob) -> String {
    job.source
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .map_or_else(|| job.id.clone(), ToOwned::to_owned)
}

fn spellcheck_results_text(misses: &[SpellMiss]) -> String {
    misses
        .iter()
        .map(|miss| {
            let suggestions = if miss.suggestions.is_empty() {
                "no suggestions".to_owned()
            } else {
                miss.suggestions.join(", ")
            };
            format!(
                "{} [{}..{}]: {}",
                miss.word, miss.chars.start, miss.chars.end, suggestions
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn offline_cache_viewport_texture(
    ctx: &egui::Context,
    cache_id: &str,
    viewport: &OfflineCacheViewportImage,
) -> Option<TextureHandle> {
    let data_sig = offline_cache_viewport_data_sig(&viewport.data_base64);
    let key = egui::Id::new(("browser-offline-cache-viewport", cache_id, data_sig));
    if let Some(cached) = ctx.data_mut(|data| data.get_temp::<OfflineCacheViewportTexture>(key)) {
        if cached.data_sig == data_sig {
            return cached.texture;
        }
    }

    let texture = base64::engine::general_purpose::STANDARD
        .decode(viewport.data_base64.as_str())
        .ok()
        .and_then(|bytes| crate::chooser::decode_png_rgba(&bytes))
        .filter(|image| image.size == [viewport.width, viewport.height])
        .map(|image| {
            ctx.load_texture(
                format!("browser-offline-cache-viewport::{cache_id}"),
                image,
                TextureOptions::LINEAR,
            )
        });
    ctx.data_mut(|data| {
        data.insert_temp(
            key,
            OfflineCacheViewportTexture {
                data_sig,
                texture: texture.clone(),
            },
        );
    });
    texture
}

fn offline_cache_viewport_data_sig(data_base64: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    data_base64.hash(&mut hasher);
    hasher.finish()
}

fn offline_cache_viewport_display_size(
    ui: &egui::Ui,
    viewport: &OfflineCacheViewportImage,
) -> egui::Vec2 {
    let natural = egui::vec2(viewport.width as f32, viewport.height as f32);
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return egui::vec2(1.0, 1.0);
    }
    let max = egui::vec2(ui.available_width().max(1.0), 180.0);
    let scale = (max.x / natural.x).min(max.y / natural.y).min(1.0);
    natural * scale
}

mod chrome_ui;
mod menubar;

#[cfg(test)]
mod tests {
    use super::chrome_ui::{browser_input_event, frame_target_device_px, map_pointer_to_frame};
    use super::*;
    use mde_egui::egui::{pos2, vec2, Rect};
    use mde_web_preview_client::{scm, testkit, wire, EditCommand};
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// A headless 960×640 shell body, mirroring the VDI + shell render tests.
    fn body_input() -> egui::RawInput {
        body_input_with_size(vec2(960.0, 640.0))
    }

    fn body_input_with_size(size: egui::Vec2) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)),
            ..Default::default()
        }
    }

    /// One Ctrl(+Shift) key press as a frame's input — drives the tab-strip
    /// accelerators through the same event path a real seat produces.
    fn ctrl_key_input(key: egui::Key, shift: bool) -> egui::RawInput {
        let mut input = body_input();
        input.events = vec![egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                ctrl: true,
                shift,
                ..egui::Modifiers::default()
            },
        }];
        input
    }

    /// Drive one headless frame of `web_panel` on the CPU tessellation path (the
    /// same `Context::run` → `tessellate` the DRM runner drives, minus the GPU).
    fn run_panel(state: &mut WebState) -> bool {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        !prims.is_empty()
    }

    /// Run frames until the active tab uploads its texture (the fake helper's frame
    /// is already buffered, so this settles immediately).
    fn run_until_texture(state: &mut WebState) -> bool {
        run_until_texture_for(state, 50)
    }

    fn run_until_texture_for(state: &mut WebState, frames: usize) -> bool {
        for _ in 0..frames {
            run_panel(state);
            if state
                .tabs
                .get(state.active)
                .is_some_and(|t| t.texture.is_some())
            {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    fn run_panel_on_ctx(ctx: &egui::Context, state: &mut WebState, input: egui::RawInput) -> bool {
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        !prims.is_empty()
    }

    fn run_panel_page_image_rect(
        ctx: &egui::Context,
        state: &mut WebState,
        input: egui::RawInput,
    ) -> Option<egui::Rect> {
        let texture_id = state
            .tabs
            .get(state.active)
            .and_then(|tab| tab.texture.as_ref())
            .map(|texture| texture.id())?;
        let out = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty(), "Browser panel produced no egui output");
        page_texture_rect(&prims, texture_id)
    }

    fn run_panel_page_image_rect_with_reserved_shell_chrome(
        ctx: &egui::Context,
        state: &mut WebState,
        input: egui::RawInput,
        left_gutter: f32,
        bottom_strut: f32,
    ) -> Option<egui::Rect> {
        let texture_id = state
            .tabs
            .get(state.active)
            .and_then(|tab| tab.texture.as_ref())
            .map(|texture| texture.id())?;
        let out = ctx.run(input, |ctx| {
            if bottom_strut > 0.0 {
                egui::TopBottomPanel::bottom("browser-test-taskbar-strut")
                    .exact_height(bottom_strut)
                    .show_separator_line(false)
                    .show(ctx, |_| {});
            }
            if left_gutter > 0.0 {
                egui::SidePanel::left("browser-test-dock-gutter")
                    .exact_width(left_gutter)
                    .resizable(false)
                    .show_separator_line(false)
                    .show(ctx, |_| {});
            }
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty(), "Browser panel produced no egui output");
        page_texture_rect(&prims, texture_id)
    }

    fn page_texture_rect(
        prims: &[egui::epaint::ClippedPrimitive],
        texture_id: egui::TextureId,
    ) -> Option<egui::Rect> {
        let mut rect = egui::Rect::NOTHING;
        for clipped in prims {
            if let egui::epaint::Primitive::Mesh(mesh) = &clipped.primitive {
                if mesh.texture_id != texture_id {
                    continue;
                }
                for vertex in &mesh.vertices {
                    rect.extend_with(vertex.pos);
                }
            }
        }
        rect.is_positive().then_some(rect)
    }

    #[cfg_attr(
        not(feature = "live-helper"),
        allow(dead_code, reason = "used by the live-helper Browser UI smoke")
    )]
    fn live_page_panel_point_for_frame(
        ctx: &egui::Context,
        state: &mut WebState,
        frame_point: egui::Pos2,
    ) -> Option<egui::Pos2> {
        let frame_size = state
            .tabs
            .get(state.active)
            .and_then(|tab| tab.last_frame.as_ref())
            .map(|frame| frame.size)?;
        let image_rect = run_panel_page_image_rect(ctx, state, body_input())?;
        Some(pos2(
            image_rect.left()
                + frame_point.x * image_rect.width() / (frame_size[0] as f32).max(1.0),
            image_rect.top()
                + frame_point.y * image_rect.height() / (frame_size[1] as f32).max(1.0),
        ))
    }

    fn run_panel_output(
        ctx: &egui::Context,
        state: &mut WebState,
        input: egui::RawInput,
    ) -> egui::FullOutput {
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        })
    }

    fn browser_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    struct EnvRestore {
        key: &'static str,
        value: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                value: std::env::var_os(key),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(value) = self.value.as_ref() {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn accesskit_nodes(
        out: &egui::FullOutput,
    ) -> Vec<(egui::accesskit::NodeId, egui::accesskit::Node)> {
        out.platform_output
            .accesskit_update
            .as_ref()
            .expect("accesskit update")
            .nodes
            .clone()
    }

    fn painted_text(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Color32)> {
        fn text_color(text: &egui::epaint::TextShape) -> egui::Color32 {
            if let Some(color) = text.override_text_color {
                return color;
            }
            text.galley
                .job
                .sections
                .iter()
                .find_map(|section| {
                    (section.format.color != egui::Color32::PLACEHOLDER)
                        .then_some(section.format.color)
                })
                .unwrap_or(text.fallback_color)
        }

        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Color32)>) {
            match shape {
                egui::Shape::Text(text) => {
                    out.push((text.galley.text().to_owned(), text_color(text)));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    #[test]
    fn curated_userscript_bundle_contains_the_first_site_fixups() {
        let bundle = curated_userscript_bundle(&[]);
        assert_eq!(CURATED_USERSCRIPT_COUNT, 100);
        assert_eq!(CURATED_USERSCRIPTS.len(), CURATED_USERSCRIPT_COUNT);
        for needle in [
            "youtube-focus",
            "npr-reader",
            "spotify-quiet",
            "wikipedia-readable",
            "nytimes-clean-reader",
            "github-readable",
            "amazon-clean-shop",
            "allrecipes-clean-recipe",
            "coursera-readable",
            "mde-browser-userscript-style",
            "__mdeBrowserUserScriptsObserver",
        ] {
            assert!(
                bundle.contains(needle),
                "missing userscript payload: {needle}"
            );
        }
    }

    #[test]
    fn curated_userscript_bundle_folds_in_user_site_styles_and_skips_blanks() {
        let styles = vec![
            UserSiteStyle {
                host: "www.Example.com".into(),
                css: "body{background:#000}".into(),
            },
            UserSiteStyle {
                host: "  ".into(),
                css: "x{y:z}".into(),
            }, // blank host → skipped
            UserSiteStyle {
                host: "site.test".into(),
                css: "   ".into(),
            }, // blank css → skipped
        ];
        let bundle = curated_userscript_bundle(&styles);
        // The user rule renders with a normalized (www-stripped, lowercased) host.
        assert!(bundle.contains("example.com"));
        assert!(bundle.contains("body{background:#000}"));
        assert!(bundle.contains("user:0"));
        // Blank host / blank CSS entries are skipped.
        assert!(!bundle.contains("user:1"));
        assert!(!bundle.contains("user:2"));
    }

    #[test]
    fn browser_page_exports_accesskit_status_and_clickable_page_region() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.tabs[state.active].force_dark = true;
        state.tabs[state.active].reader_mode = true;
        state.tabs[state.active].container = ContainerProfile::Work;
        state.tabs[state.active].display_target = DisplayTarget::Secondary;
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);

        let out = run_panel_output(&ctx, &mut state, body_input());
        let nodes = accesskit_nodes(&out);
        let browser = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser status"))
            .expect("browser status accesskit node");
        assert_eq!(browser.role(), egui::accesskit::Role::Status);
        assert_eq!(browser.live(), Some(egui::accesskit::Live::Polite));
        let browser_value = browser.value().expect("browser status value");
        assert!(browser_value.contains("Active tab 1 of 1"));
        assert!(browser_value.contains("Example"));
        assert!(browser_value.contains("https://example.test/"));
        assert!(browser_value.contains("force dark"));
        assert!(browser_value.contains("reader mode"));
        assert!(browser_value.contains("Work browsing profile"));
        assert!(browser_value.contains("opens on secondary display"));

        let page = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser page"))
            .expect("browser page accesskit node");
        assert_eq!(page.role(), egui::accesskit::Role::Button);
        let page_value = page.value().expect("browser page value");
        assert!(page_value.contains("Click the page canvas to focus keyboard input"));
        for leaked in ["CEF", "Servo", "internal", "container", "display target"] {
            assert!(
                !browser_value.contains(leaked) && !page_value.contains(leaked),
                "Browser AccessKit copy must stay user-facing; leaked {leaked:?}: {browser_value} / {page_value}"
            );
        }
    }

    #[test]
    fn browser_options_tab_accesskit_uses_user_facing_page_summary() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);
        let mut state = WebState::default();
        state.open_options_tab();

        let out = run_panel_output(&ctx, &mut state, body_input());
        let nodes = accesskit_nodes(&out);
        let browser = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser status"))
            .expect("browser status accesskit node");
        let value = browser.value().expect("browser status value");
        assert!(value.contains("Browser Options page"));
        assert!(value.contains("Browser Options"));
        assert!(value.contains(BROWSER_OPTIONS_URL));
        for leaked in [
            "Browser internal page",
            "internal",
            "container",
            "display target",
            "helper session",
        ] {
            assert!(
                !value.contains(leaked),
                "Options AccessKit summary must not expose implementation wording {leaked:?}: {value}"
            );
        }
        assert!(
            !value.contains("Untitled") && !value.contains("about:blank"),
            "Options AccessKit summary must not leak the inert helper session: {value}"
        );
    }

    #[test]
    fn browser_empty_accesskit_status_uses_user_facing_notice() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);
        let mut state = WebState::default();

        let out = run_panel_output(&ctx, &mut state, body_input());
        let nodes = accesskit_nodes(&out);
        let browser = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser status"))
            .expect("browser status accesskit node");
        let value = browser.value().expect("browser status value");
        assert!(value.contains("No active tab"));
        assert!(value.contains(chrome_ui::BROWSER_NO_LIVE_PAGE_NOTICE));
        assert!(
            !value.contains("helper session")
                && !value.contains("helper unavailable")
                && !value.contains("Servo")
                && !value.contains("BOOKMARKS"),
            "empty Browser AccessKit status must not expose helper internals: {value}"
        );
    }

    #[test]
    fn loading_tab_renders_netscape_style_globe_status() {
        let (mut session, helper) = raw_session_pair();
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: true,
                url: "https://loading.example/".to_owned(),
            },
        );
        session.poll();
        let mut state = WebState::default();
        state.push_session_with_engine(session, BrowserEngine::Cef);
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);

        let out = run_panel_output(&ctx, &mut state, body_input());
        let texts: Vec<String> = painted_text(&out.shapes)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert!(
            texts.iter().any(|text| text == "Loading the page..."),
            "loading body should still expose the concise status copy: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains('\u{2026}')),
            "loading body must not paint the unicode ellipsis glyph: {texts:?}"
        );
        assert!(
            accesskit_nodes(&out)
                .iter()
                .any(|(_, node)| node.label() == Some("Browser loading globe")),
            "loading body/toolbar should expose the Browser loading globe status"
        );
        assert!(
            chrome_ui::loading_globe_painted_shape_count() > 0,
            "Netscape-style loading globe must paint real shapes"
        );
    }

    fn write_helper_event(stream: &UnixStream, msg: &mde_web_preview_client::EventMsg) {
        let mut stream = stream;
        stream
            .write_all(&wire::frame(&msg.encode()))
            .expect("write helper event");
    }

    /// A bare socketpair session with no shm/frame plumbing — enough to drive
    /// wire-level events (favicons, nav, …) without a fake helper thread.
    /// Mirrors `the_ad_filter_blocked_count_surfaces_on_the_active_tab`'s recipe;
    /// testkit's `connect()` doesn't expose its peer socket for manual events.
    fn raw_session_pair() -> (WebSession, UnixStream) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let session = WebSession::from_stream(shell, None).expect("session");
        (session, helper)
    }

    /// Push an `EventMsg::Favicon` carrying `png_bytes` to a raw session's peer.
    /// Caller polls the session afterward to fold it in.
    fn send_favicon(peer: &UnixStream, png_bytes: &[u8]) {
        write_helper_event(
            peer,
            &mde_web_preview_client::EventMsg::Favicon {
                png: png_bytes.to_vec(),
            },
        );
    }

    /// A raw session that has already polled in one favicon's PNG bytes.
    fn session_with_favicon(png_bytes: &[u8]) -> WebSession {
        let (mut session, peer) = raw_session_pair();
        send_favicon(&peer, png_bytes);
        session.poll();
        session
    }

    /// Push an `EventMsg::CertError` to a raw session's peer. Caller polls the
    /// session afterward to fold it in.
    fn send_cert_error(peer: &UnixStream, url: &str, code: i32, message: &str) {
        write_helper_event(
            peer,
            &mde_web_preview_client::EventMsg::CertError {
                url: url.to_owned(),
                code,
                message: message.to_owned(),
            },
        );
    }

    /// A raw session that has already polled in one blocked-navigation
    /// certificate error.
    fn session_with_cert_error(url: &str, code: i32, message: &str) -> WebSession {
        let (mut session, peer) = raw_session_pair();
        send_cert_error(&peer, url, code, message);
        session.poll();
        session
    }

    fn live_page_session() -> (
        WebSession,
        UnixStream,
        mde_web_preview_client::testkit::FrameWriter,
    ) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let writer =
            testkit::FrameWriter::create(testkit::FAKE_W, testkit::FAKE_H).expect("frame writer");
        writer
            .emit(
                testkit::FAKE_W,
                testkit::FAKE_H,
                mde_web_preview_client::PixelFormat::Rgba8,
                &testkit::gradient(testkit::FAKE_W, testkit::FAKE_H),
            )
            .expect("emit frame");
        scm::send_frame_with_fd(
            &helper,
            &mde_web_preview_client::EventMsg::AttachFrame.encode(),
            writer.raw_fd(),
        )
        .expect("attach frame");
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://example.test/".to_owned(),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::Title("Example".to_owned()),
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PaintReady {
                seq: writer.sequence(),
            },
        );
        helper.set_nonblocking(true).expect("nonblocking helper");
        (
            WebSession::from_stream(shell, None).expect("session"),
            helper,
            writer,
        )
    }

    fn capture_browser_screenshot(
        name: &str,
        state: &mut WebState,
        size: egui::Vec2,
    ) -> crate::screenshot::Canvas {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let input = || egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)),
            ..Default::default()
        };
        let mut cap = crate::screenshot::Capture::new();
        let _settle = cap.frame(&ctx, input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });
        let canvas = cap.frame(&ctx, input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        });

        assert_eq!(
            (canvas.width(), canvas.height()),
            (size.x.round() as usize, size.y.round() as usize),
            "Browser screenshot canvas must match the driven viewport"
        );
        assert!(!canvas.is_blank(), "Browser screenshot must not be blank");

        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("screenshots")
            .join(name);
        canvas
            .write_png(&path)
            .expect("write the Browser visual-audit screenshot");
        println!(
            "Browser visual-audit screenshot written to {}",
            path.display()
        );
        canvas
    }

    #[test]
    fn browser_visual_audit_screenshots_cover_tab_modes_and_viewports() {
        let mut options = WebState::default();
        options.set_vertical_tabs(true);
        options.open_options_tab();
        let wide = capture_browser_screenshot(
            "browser-wide-vertical-options.png",
            &mut options,
            vec2(1280.0, 800.0),
        );
        let clear_pixels = wide.count_exact_color(Style::CAPTURE_CLEAR);
        let total_pixels = wide.width() * wide.height();
        assert!(
            clear_pixels < total_pixels / 20,
            "Browser Options screenshot must paint the full body; clear pixels: {clear_pixels}/{total_pixels}"
        );

        let (session, _helper, _writer) = live_page_session();
        let mut page = WebState::default();
        page.set_vertical_tabs(false);
        page.push_session_with_engine(session, BrowserEngine::Cef);
        let compact = capture_browser_screenshot(
            "browser-compact-horizontal-page.png",
            &mut page,
            vec2(540.0, 720.0),
        );
        assert!(
            page.tabs[page.active].texture.is_some(),
            "compact horizontal Browser screenshot must include the live page texture"
        );
        assert_eq!((compact.width(), compact.height()), (540, 720));
    }

    fn drain_control_messages(stream: &UnixStream) -> Vec<mde_web_preview_client::ControlMsg> {
        let mut rbuf = Vec::new();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            match scm::recv(stream).expect("recv controls") {
                scm::RecvOutcome::Data { bytes, .. } => {
                    rbuf.extend_from_slice(&bytes);
                    while let Some(payload) = wire::take_frame(&mut rbuf).expect("control frame") {
                        out.push(
                            mde_web_preview_client::ControlMsg::decode(&payload)
                                .expect("control decode"),
                        );
                    }
                }
                scm::RecvOutcome::WouldBlock => {
                    if !out.is_empty() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                scm::RecvOutcome::Eof => break,
            }
        }
        out
    }

    fn wait_for_fresh_frame(state: &mut WebState) -> bool {
        for _ in 0..100 {
            let Some(tab) = state.tabs.get_mut(state.active) else {
                return false;
            };
            tab.session.poll();
            if tab.session.take_frame().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn browser_capture_filename_prefers_host_and_sanitizes() {
        assert_eq!(
            capture_filename_for("https://Example.COM/some/path", "Ignored", 123),
            "mde-browser-123-example-com.png"
        );
        assert_eq!(
            capture_filename_for("about:blank", "News: Top Stories / Today", 456),
            "mde-browser-456-news-top-stories-today.png"
        );
        assert_eq!(
            capture_full_page_filename_for("https://Example.COM/some/path", "Ignored", 678),
            "mde-browser-full-page-678-example-com.png"
        );
        assert_eq!(
            capture_mhtml_filename_for("https://Example.COM/some/path", "Ignored", 679),
            "mde-browser-679-example-com.mhtml"
        );
        assert_eq!(
            capture_annotated_filename_for("https://Example.COM/some/path", "Ignored", 789),
            "mde-browser-annotated-789-example-com.png"
        );
        assert_eq!(
            capture_callout_filename_for("https://Example.COM/some/path", "Ignored", 888),
            "mde-browser-callout-888-example-com.png"
        );
        assert_eq!(
            capture_freehand_filename_for("https://Example.COM/some/path", "Ignored", 889),
            "mde-browser-freehand-889-example-com.png"
        );
        assert_eq!(
            pdf_filename_for("https://Example.COM/some/path", "Ignored", 999),
            "mde-browser-999-example-com.pdf"
        );
    }

    #[test]
    fn browser_capture_writes_latest_frame_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_until_texture(&mut state),
            "the fake helper frame should upload before capture"
        );
        assert!(
            state.active_tab_has_frame(),
            "capture is gated on a retained helper frame"
        );

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_viewport_to_dir(dir.path())
            .expect("capture writes PNG");
        assert_eq!(path.parent(), Some(dir.path()));
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("example-test") && name.ends_with(".png")),
            "capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize],
            "capture preserves the helper viewport dimensions"
        );
    }

    #[test]
    fn browser_full_page_capture_writes_named_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_full_page_to_dir(dir.path())
            .expect("full-page capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-full-page-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "full-page capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize],
            "full-page capture preserves the current helper surface until stitching lands"
        );
    }

    #[test]
    fn browser_mhtml_capture_writes_related_archive() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_mhtml_to_dir(dir.path())
            .expect("mhtml capture writes archive");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-")
                    && name.contains("example-test")
                    && name.ends_with(".mhtml")),
            "mhtml capture filename should include the current host: {}",
            path.display()
        );
        let archive = std::fs::read_to_string(&path).expect("read mhtml");
        assert!(archive.contains("Content-Type: multipart/related"));
        assert!(archive.contains("Content-Type: text/html; charset=\"utf-8\""));
        assert!(archive.contains("Content-Type: image/png"));
        assert!(archive.contains("Content-Transfer-Encoding: base64"));
        assert!(archive.contains("https://example.test/"));
        assert!(archive.contains("mde-browser-capture.png"));
        assert!(
            archive.contains("iVBORw0KGgo"),
            "the embedded related part should contain a base64 PNG payload"
        );
    }

    #[test]
    fn mhtml_capture_escapes_page_metadata() {
        let archive = mhtml_capture_document(
            "https://example.test/?q=<tag>&x=1",
            "A <Title> & \"Quote\"",
            42,
            b"not a real png for structure testing",
        );
        let archive = String::from_utf8(archive).expect("mhtml is utf8");
        assert!(archive.contains("A &lt;Title&gt; &amp; &quot;Quote&quot;"));
        assert!(archive.contains("https://example.test/?q=&lt;tag&gt;&amp;x=1"));
        assert!(!archive.contains("<Title>"));
    }

    #[test]
    fn browser_artifacts_use_canonical_quazar_browser_identity() {
        assert_eq!(browser_product_label(), "Quazar Browser");
        assert_eq!(
            browser_capture_dir()
                .file_name()
                .and_then(|name| name.to_str()),
            Some("Quazar Browser Captures")
        );
        assert_eq!(
            browser_pdf_dir().file_name().and_then(|name| name.to_str()),
            Some("Quazar Browser PDFs")
        );
        assert_eq!(cups_job_title("", "", 42), "Quazar Browser - Browser page");
        assert_eq!(
            cups_job_title("https://example.test/", "Example", 42),
            "Quazar Browser - Example"
        );

        let capture = String::from_utf8(mhtml_capture_document(
            "https://example.test/",
            "Example",
            42,
            b"png",
        ))
        .expect("capture mhtml utf8");
        assert!(capture.contains("Subject: Quazar Browser Capture - Example"));
        assert!(!capture.contains("Magic Mesh Browser"));

        let offline = String::from_utf8(offline_cache_mhtml_document(
            "https://example.test/",
            "Example",
            42,
            "cached text",
            None,
        ))
        .expect("offline mhtml utf8");
        assert!(offline.contains("Subject: Quazar Browser Offline Copy - Example"));
        assert!(!offline.contains("Magic Mesh Browser"));
    }

    #[test]
    fn browser_annotated_capture_writes_captioned_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_annotated_viewport_to_dir(dir.path())
            .expect("annotated capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-annotated-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "annotated capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "annotated capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT),
            "caption band should contain rendered annotation text"
        );
    }

    #[test]
    fn browser_callout_capture_writes_annotated_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_callout_viewport_to_dir(dir.path())
            .expect("callout capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-callout-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "callout capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "callout capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "callout capture should render a callout label into the caption band"
        );
    }

    #[test]
    fn browser_freehand_capture_writes_annotated_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_freehand_viewport_to_dir(dir.path())
            .expect("freehand capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-freehand-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "freehand capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(
            image.size,
            [
                testkit::FAKE_W as usize,
                testkit::FAKE_H as usize + ANNOTATION_BAR_HEIGHT
            ],
            "freehand capture appends a visible caption band"
        );
        let frame_pixels = (testkit::FAKE_W * testkit::FAKE_H) as usize;
        assert!(
            image.pixels[frame_pixels..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand capture should render a freehand label into the caption band"
        );
    }

    #[test]
    fn annotated_capture_preserves_frame_and_adds_caption_band() {
        let mut img = egui::ColorImage::new([32, 4], egui::Color32::RED);
        img.pixels[3] = egui::Color32::BLUE;

        let annotated = annotate_capture_image(&img, "Example | https://example.test | 123")
            .expect("valid annotation");

        assert_eq!(annotated.size, [32, 4 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(&annotated.pixels[..img.pixels.len()], &img.pixels[..]);
        assert!(
            annotated.pixels[img.pixels.len()..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT),
            "caption text should be painted into the appended band"
        );
    }

    #[test]
    fn callout_capture_draws_overlay_and_preserves_frame_area() {
        let img = egui::ColorImage::new([64, 48], egui::Color32::RED);

        let annotated =
            annotate_callout_capture_image(&img, "Example | https://example.test | 123")
                .expect("valid callout annotation");

        assert_eq!(annotated.size, [64, 48 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(
            annotated.pixels[0], img.pixels[0],
            "unannotated corners of the captured frame remain intact"
        );
        assert!(
            annotated.pixels[..(64 * 48)]
                .iter()
                .any(|pixel| *pixel == Style::ACCENT),
            "callout overlay should paint an accent rectangle or leader line"
        );
        assert!(
            annotated.pixels[(64 * 48)..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "callout label should be painted into the appended band"
        );
    }

    #[test]
    fn freehand_capture_draws_stroke_and_preserves_frame_area() {
        let img = egui::ColorImage::new([64, 48], egui::Color32::RED);

        let annotated =
            annotate_freehand_capture_image(&img, "Example | https://example.test | 123")
                .expect("valid freehand annotation");

        assert_eq!(annotated.size, [64, 48 + ANNOTATION_BAR_HEIGHT]);
        assert_eq!(
            annotated.pixels[0], img.pixels[0],
            "unannotated corners of the captured frame remain intact"
        );
        assert!(
            annotated.pixels[..(64 * 48)]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand overlay should paint a visible white stroke"
        );
        assert!(
            annotated.pixels[(64 * 48)..]
                .iter()
                .any(|pixel| *pixel == Style::TEXT_STRONG),
            "freehand label should be painted into the appended band"
        );
    }

    #[test]
    fn browser_region_capture_crops_latest_frame_png() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let dir = tempfile::tempdir().expect("temp capture dir");
        let path = state
            .capture_active_region_to_dir(
                dir.path(),
                PixelRegion {
                    x: 1,
                    y: 1,
                    width: 3,
                    height: 2,
                },
            )
            .expect("region capture writes PNG");

        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mde-browser-region-")
                    && name.contains("example-test")
                    && name.ends_with(".png")),
            "region capture filename should include the current host: {}",
            path.display()
        );
        let bytes = std::fs::read(&path).expect("read capture");
        let image = crate::chooser::decode_png_rgba(&bytes).expect("capture decodes");
        assert_eq!(image.size, [3, 2]);
    }

    #[test]
    fn browser_region_crop_validates_and_preserves_pixels() {
        let mut img = egui::ColorImage::new([4, 3], egui::Color32::TRANSPARENT);
        for y in 0..3 {
            for x in 0..4 {
                // Twelve distinct sentinel pixels (a WHITE ramp) so the crop's
                // positional preservation is provable without minting a colour.
                img.pixels[y * 4 + x] =
                    egui::Color32::WHITE.gamma_multiply((y * 4 + x + 1) as f32 / 12.0);
            }
        }

        let cropped = crop_color_image(
            &img,
            PixelRegion {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
        )
        .expect("valid crop");

        assert_eq!(cropped.size, [2, 2]);
        assert_eq!(cropped.pixels[0], img.pixels[5]);
        assert_eq!(cropped.pixels[1], img.pixels[6]);
        assert_eq!(cropped.pixels[2], img.pixels[9]);
        assert_eq!(cropped.pixels[3], img.pixels[10]);
        assert!(
            crop_color_image(
                &img,
                PixelRegion {
                    x: 3,
                    y: 2,
                    width: 2,
                    height: 1,
                },
            )
            .is_err(),
            "out-of-bounds regions are rejected"
        );
    }

    #[test]
    fn pdf_completion_events_update_the_browser_notice() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PdfSaved {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: true,
            },
        );
        run_panel(&mut state);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF saved: mde-page.pdf")
        );
        assert!(
            !state
                .capture_notice
                .as_deref()
                .unwrap_or_default()
                .contains("/tmp/"),
            "saved-PDF notice should not expose an absolute path"
        );

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PdfSaved {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: false,
            },
        );
        run_panel(&mut state);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF save failed: mde-page.pdf")
        );
        assert!(
            !state
                .capture_notice
                .as_deref()
                .unwrap_or_default()
                .contains("/tmp/"),
            "failed-PDF notice should not expose an absolute path"
        );
    }

    #[test]
    fn saved_pdf_opens_in_a_cef_viewer_tab() {
        let dir = tempfile::tempdir().expect("pdf dir");
        let path = dir.path().join("report one.pdf");
        std::fs::write(&path, b"%PDF-1.7\n% viewer fixture\n").expect("pdf fixture");
        let path_text = path.to_string_lossy().into_owned();
        let mut state = WebState::default();

        assert_eq!(
            state.handle_pdf_event(path_text.clone(), true),
            "PDF saved: report one.pdf"
        );
        state.open_last_saved_pdf();

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: format!("file://{}", path_text.replace(' ', "%20")),
            })
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Opening PDF in Chromium viewer")
        );
    }

    #[test]
    fn pdf_viewer_refuses_missing_or_non_pdf_output() {
        let dir = tempfile::tempdir().expect("pdf dir");
        let path = dir.path().join("not-pdf.pdf");
        std::fs::write(&path, b"not a pdf\n").expect("bad fixture");
        let mut state = WebState::default();
        state.last_saved_pdf = Some(SavedPdf {
            path: path.clone(),
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        });

        state.open_last_saved_pdf();

        assert_eq!(state.take_open_request(), None);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF viewer failed: saved PDF is not readable")
        );
        assert!(
            !state
                .capture_notice
                .as_deref()
                .unwrap_or_default()
                .contains(path.to_string_lossy().as_ref()),
            "viewer notice should not expose the refused path: {:?}",
            state.capture_notice
        );
    }

    #[test]
    fn browser_body_input_is_localized_and_keyboard_is_focus_gated() {
        let rect = Rect::from_min_size(pos2(20.0, 40.0), vec2(320.0, 200.0));
        // Frame device size == rect points size (a 1:1 seat), so the mapped position
        // equals the panel-local offset — pins the transform's identity case.
        let frame = [320usize, 200usize];

        let moved = browser_input_event(
            &egui::Event::PointerMoved(pos2(70.0, 90.0)),
            rect,
            frame,
            false,
            false,
        )
        .expect("pointer inside page");
        assert_eq!(moved, egui::Event::PointerMoved(pos2(50.0, 50.0)));

        let key = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        };
        assert_eq!(
            browser_input_event(&key, rect, frame, false, false),
            None,
            "address-bar/chrome keystrokes must not leak into the page"
        );
        assert_eq!(
            browser_input_event(&key, rect, frame, true, false),
            Some(key),
            "click-focused page canvas receives keyboard events"
        );
        assert_eq!(
            browser_input_event(
                &egui::Event::Text("mesh".to_owned()),
                rect,
                frame,
                true,
                false
            ),
            Some(egui::Event::Text("mesh".to_owned())),
            "committed text reaches the focused page canvas"
        );
    }

    // ─────────────── browser-1: pointer transform + viewport resize ──────────────
    //
    // The bug forwarded `(pointer - image_rect.min) * pixels_per_point` against a
    // FIXED 1280×800 frame, so clicks missed on every seat whose displayed rect
    // wasn't exactly 1280×800 device px. These pin the shared transform both the
    // live-input and region-capture paths now flow through.

    #[test]
    fn map_pointer_to_frame_scales_panel_space_into_frame_pixels() {
        // A non-zero origin (tab strip + nav chrome above/left) and a frame TWICE
        // the panel per axis — the exact non-1280×800 shape the fixed frame ignored.
        let rect = Rect::from_min_size(pos2(100.0, 40.0), vec2(800.0, 600.0));
        let frame = [1600usize, 1200usize];
        // Image top-left → frame origin.
        assert_eq!(
            map_pointer_to_frame(pos2(100.0, 40.0), rect, frame),
            pos2(0.0, 0.0)
        );
        // Image centre → frame centre.
        assert_eq!(
            map_pointer_to_frame(pos2(500.0, 340.0), rect, frame),
            pos2(800.0, 600.0)
        );
        // A quarter across the image → a quarter across the frame.
        assert_eq!(
            map_pointer_to_frame(pos2(300.0, 190.0), rect, frame),
            pos2(400.0, 300.0)
        );
        // Image bottom-right → the frame's far edge.
        assert_eq!(
            map_pointer_to_frame(pos2(900.0, 640.0), rect, frame),
            pos2(1600.0, 1200.0)
        );
    }

    #[test]
    fn map_pointer_to_frame_clamps_outside_the_image_and_survives_zero_size() {
        let rect = Rect::from_min_size(pos2(50.0, 30.0), vec2(500.0, 400.0));
        let frame = [250usize, 200usize];
        // Above/left of the image clamps to the origin (never negative).
        assert_eq!(
            map_pointer_to_frame(pos2(0.0, 0.0), rect, frame),
            pos2(0.0, 0.0)
        );
        // Far below/right clamps to the frame's far edge (never runs off).
        assert_eq!(
            map_pointer_to_frame(pos2(9000.0, 9000.0), rect, frame),
            pos2(250.0, 200.0)
        );
        // A degenerate zero-size image must not divide by zero.
        let zero = Rect::from_min_size(pos2(10.0, 10.0), vec2(0.0, 0.0));
        assert_eq!(
            map_pointer_to_frame(pos2(50.0, 50.0), zero, [640, 480]),
            pos2(0.0, 0.0)
        );
    }

    #[test]
    fn browser_click_maps_through_the_shared_frame_transform() {
        // A 4K frame displayed into a smaller, offset panel (the downscale case the
        // fixed-1280×800 path got wrong). A click at the panel centre must land at
        // the FRAME centre — proving the live path routes through the transform, not
        // a `pos * ppp` scale — and must MATCH what the region-capture path computes
        // for the same pointer (the dedup the review asked for: no divergence).
        let rect = Rect::from_min_size(pos2(64.0, 128.0), vec2(960.0, 540.0));
        let frame = [3840usize, 2160usize];
        let centre = pos2(64.0 + 480.0, 128.0 + 270.0);
        let ev = egui::Event::PointerButton {
            pos: centre,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        };
        match browser_input_event(&ev, rect, frame, true, false).expect("focused click forwards") {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                ..
            } => {
                assert_eq!(pos, pos2(1920.0, 1080.0), "click lands at the frame centre");
                // Region-capture's `pointer_to_frame` IS `map_pointer_to_frame`, so
                // both paths agree on the same pointer — one shared transform.
                assert_eq!(pos, map_pointer_to_frame(centre, rect, frame));
                assert_eq!(button, egui::PointerButton::Primary);
                assert!(pressed);
            }
            other => panic!("expected PointerButton, got {other:?}"),
        }
        // A focused pointer leaving the image reports PointerGone (hover clears).
        assert_eq!(
            browser_input_event(
                &egui::Event::PointerMoved(pos2(0.0, 0.0)),
                rect,
                frame,
                true,
                false
            ),
            Some(egui::Event::PointerGone)
        );
    }

    #[test]
    fn browser_page_drag_keeps_forwarding_clamped_moves_after_leaving_the_image() {
        let rect = Rect::from_min_size(pos2(100.0, 40.0), vec2(800.0, 600.0));
        let frame = [1600usize, 1200usize];
        let outside = pos2(1000.0, 700.0);

        assert_eq!(
            browser_input_event(
                &egui::Event::PointerMoved(outside),
                rect,
                frame,
                true,
                false
            ),
            Some(egui::Event::PointerGone),
            "a focused hover leaving the image still clears page hover state"
        );
        assert_eq!(
            browser_input_event(&egui::Event::PointerMoved(outside), rect, frame, true, true),
            Some(egui::Event::PointerMoved(pos2(1600.0, 1200.0))),
            "a captured page drag keeps moving at the clamped frame edge"
        );
    }

    #[test]
    fn frame_target_device_px_scales_by_ppp_and_clamps() {
        let rect = Rect::from_min_size(pos2(8.0, 8.0), vec2(1000.0, 500.0));
        assert_eq!(frame_target_device_px(rect, 1.0), (1000, 500));
        // A HiDPI seat scales the panel into more device pixels.
        assert_eq!(frame_target_device_px(rect, 2.0), (2000, 1000));
        // Beyond the channel ceiling clamps (an oversized panel at 2×).
        let huge = Rect::from_min_size(pos2(0.0, 0.0), vec2(3000.0, 3000.0));
        assert_eq!(
            frame_target_device_px(huge, 2.0),
            (MAX_CHANNEL_DIM, MAX_CHANNEL_DIM)
        );
        // Never smaller than 1×1.
        let z = Rect::from_min_size(pos2(0.0, 0.0), vec2(0.0, 0.0));
        assert_eq!(frame_target_device_px(z, 1.0), (1, 1));
    }

    #[test]
    fn viewport_resizer_debounces_a_changed_size_and_ignores_no_change() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        // First sighting of a new size: still settling, no resize yet.
        assert_eq!(r.observe((1200, 700), t0, d), None);
        assert!(r.is_settling());
        // Held steady but before the debounce elapses: still nothing.
        assert_eq!(
            r.observe((1200, 700), t0 + Duration::from_millis(100), d),
            None
        );
        // Settled (held ≥ debounce): committed exactly once.
        assert_eq!(
            r.observe((1200, 700), t0 + Duration::from_millis(150), d),
            Some((1200, 700))
        );
        assert!(!r.is_settling());
        // The SAME size again is a no-op — an unchanged panel never re-resizes.
        assert_eq!(r.observe((1200, 700), t0 + Duration::from_secs(1), d), None);
    }

    #[test]
    fn viewport_resizer_restarts_the_clock_while_the_size_keeps_changing() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        // A drag moves through many sizes: each new candidate restarts the debounce,
        // so no resize fires mid-drag even past the first candidate's deadline.
        assert_eq!(r.observe((1000, 600), t0, d), None);
        assert_eq!(
            r.observe((1010, 600), t0 + Duration::from_millis(100), d),
            None
        );
        assert_eq!(
            r.observe((1020, 600), t0 + Duration::from_millis(200), d),
            None
        );
        assert!(r.is_settling());
        // The drag settles on the final size; once THAT holds for the debounce it
        // commits — a single settled resize for the whole drag.
        let settled = t0 + Duration::from_millis(200);
        assert_eq!(r.observe((1020, 600), settled + d, d), Some((1020, 600)));
    }

    #[test]
    fn viewport_resizer_cancels_a_pending_change_that_reverts() {
        let mut r = ViewportResizer::default();
        let t0 = Instant::now();
        let d = Duration::from_millis(150);
        r.observe((800, 600), t0, d);
        assert_eq!(r.observe((800, 600), t0 + d, d), Some((800, 600)));
        // A transient candidate appears...
        assert_eq!(
            r.observe((801, 600), t0 + d + Duration::from_millis(10), d),
            None
        );
        assert!(r.is_settling());
        // ...but reverts to the committed size before settling: pending cancels, so
        // no spurious resize.
        assert_eq!(
            r.observe((800, 600), t0 + d + Duration::from_millis(20), d),
            None
        );
        assert!(!r.is_settling());
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn a_live_spawn_pre_sizes_the_channel_to_the_seat() {
        use std::cell::Cell;
        // A seat reporting a 1920×1080 screen: the spawn must pre-size the channel to
        // it (item 3), NOT the fixed 1280×800 that would silently drop an enlarged
        // paint on a bigger panel.
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1920.0, 1080.0))),
            ..Default::default()
        };
        let _ = ctx.run(input, |_| {});
        let ppp = ctx.pixels_per_point();

        let mut state = WebState::default();
        state.note_seat_px(&ctx);
        let seen = Cell::new((0u32, 0u32));
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            std::env::current_exe().expect("test exe"),
            |spec| {
                seen.set((spec.width, spec.height));
                let (session, _helper) = testkit::connect()?;
                Ok(session)
            },
        );
        let expect = ((1920.0 * ppp).round() as u32, (1080.0 * ppp).round() as u32);
        assert_eq!(
            seen.get(),
            expect,
            "the spawn pre-sizes the channel to the seat, not a fixed 1280×800"
        );
        assert_ne!(seen.get(), (INIT_W, INIT_H));
    }

    #[test]
    fn browser_panel_click_focuses_page_and_sends_keyboard_input_to_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(
            run_panel_on_ctx(&ctx, &mut state, body_input()),
            "the live page frame must upload before body input can be painted"
        );

        let page_point = run_panel_page_image_rect(&ctx, &mut state, body_input())
            .expect("the Browser page texture should be locatable before clicking")
            .center();
        let mut click_input = body_input();
        click_input.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click_input));
        assert!(
            state.tabs[0].page_focused,
            "clicking the rendered page must latch page keyboard focus"
        );
        let mut key_input = body_input();
        key_input.events = vec![
            egui::Event::Key {
                key: egui::Key::A,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::Text("mesh".to_owned()),
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, key_input));

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::PointerButton { pressed: true, .. }
                )
            )),
            "clicking the Browser body must send a page pointer press: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Key {
                        key: mde_web_preview_client::wire::KeyCode::A,
                        pressed: true,
                        ..
                    }
                )
            )),
            "a focused Browser body must forward key input: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(text)
                ) if text == "mesh"
            )),
            "a focused Browser body must forward committed text: {controls:?}"
        );
    }

    #[test]
    fn a_focused_page_forwards_ime_composition_to_the_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        // Focus the page body.
        let page_point = run_panel_page_image_rect(&ctx, &mut state, body_input())
            .expect("the Browser page texture should be locatable before clicking")
            .center();
        let mut click = body_input();
        click.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click));
        assert!(state.tabs[0].page_focused);

        // A preedit then a commit: the page must receive IME composition controls,
        // NOT a Text input (composition is a distinct browser-host path).
        let mut ime = body_input();
        ime.events = vec![
            egui::Event::Ime(egui::ImeEvent::Preedit("\u{4f60}".to_owned())),
            egui::Event::Ime(egui::ImeEvent::Commit("\u{4f60}\u{597d}".to_owned())),
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, ime));

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ImeSetComposition { text } if text == "\u{4f60}"
            )),
            "a preedit must forward ImeSetComposition: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ImeCommitText { text } if text == "\u{4f60}\u{597d}"
            )),
            "a commit must forward ImeCommitText: {controls:?}"
        );
    }

    #[test]
    fn page_zoom_and_find_actions_send_helper_controls() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            state.tabs[state.active].autoplay_blocked,
            "new browser tabs block autoplay by default"
        );
        let ctx = egui::Context::default();

        menubar::apply(&ctx, &mut state, menubar::MenuAction::ZoomIn);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ZoomOut);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::OpenFind);
        assert!(
            state.find_open,
            "OpenFind keeps the compact find chrome visible until close"
        );
        state.find_query = "mesh".to_owned();
        state.submit_find(false);
        state.submit_find(true);
        state.close_find_bar();
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAudioMute);
        assert!(state.tabs[state.active].muted);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAudioMute);
        assert!(!state.tabs[state.active].muted);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleMediaPlayback);
        for action in [
            mde_web_preview_client::MediaTransportAction::PlayPause,
            mde_web_preview_client::MediaTransportAction::Play,
            mde_web_preview_client::MediaTransportAction::Pause,
            mde_web_preview_client::MediaTransportAction::Stop,
            mde_web_preview_client::MediaTransportAction::Next,
            mde_web_preview_client::MediaTransportAction::Previous,
            mde_web_preview_client::MediaTransportAction::VolumeUp,
            mde_web_preview_client::MediaTransportAction::VolumeDown,
        ] {
            state.active_tab_media_transport(action);
        }
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAutoplayBlock);
        assert!(!state.tabs[state.active].autoplay_blocked);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleAutoplayBlock);
        assert!(state.tabs[state.active].autoplay_blocked);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleForceDark);
        assert!(state.tabs[state.active].force_dark);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleForceDark);
        assert!(!state.tabs[state.active].force_dark);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleReaderMode);
        assert!(state.tabs[state.active].reader_mode);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleReaderMode);
        assert!(!state.tabs[state.active].reader_mode);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleUserScripts);
        assert!(state.tabs[state.active].user_scripts);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleUserScripts);
        assert!(!state.tabs[state.active].user_scripts);
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleUserAgent);
        assert_eq!(
            state.tabs[state.active].user_agent,
            UserAgentOverride::DesktopChrome
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleUserAgent);
        assert_eq!(
            state.tabs[state.active].user_agent,
            UserAgentOverride::AndroidChrome
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleDeviceProfile);
        assert_eq!(
            state.tabs[state.active].device_profile,
            DeviceProfile::Phone
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CycleDeviceProfile);
        assert_eq!(
            state.tabs[state.active].device_profile,
            DeviceProfile::Tablet
        );
        menubar::apply(&ctx, &mut state, menubar::MenuAction::CheckSpelling);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: reading page text")
        );
        let print_dir = tempfile::tempdir().expect("print spool dir");
        let print_path = state
            .queue_active_page_cups_print_to_dir(print_dir.path())
            .expect("cups print path queued");
        let print_path_text = print_path.to_string_lossy().into_owned();
        menubar::apply(&ctx, &mut state, menubar::MenuAction::PrintPage);
        let pdf_dir = tempfile::tempdir().expect("pdf output dir");
        let pdf_path = state
            .save_active_page_pdf_to_dir(pdf_dir.path())
            .expect("pdf path queued");
        let pdf_path_text = pdf_path.to_string_lossy().into_owned();

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetZoom { percent: 110 }
            )),
            "zoom in must send a helper zoom control: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetZoom { percent: 100 }
            )),
            "zoom out must restore a helper zoom control: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::FindInPage {
                    query,
                    backwards: false,
                    ..
                } if query == "mesh"
            )),
            "forward find must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::FindInPage {
                    query,
                    backwards: true,
                    ..
                } if query == "mesh"
            )),
            "backward find must reach the helper: {controls:?}"
        );
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::ClearFind)),
            "closing find must clear the helper selection: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAudioMuted { muted: true }
            )),
            "muting the tab must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAudioMuted { muted: false }
            )),
            "unmuting the tab must reach the helper: {controls:?}"
        );
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::ToggleMediaPlayback)),
            "play/pause media must reach the helper: {controls:?}"
        );
        for action in [
            mde_web_preview_client::MediaTransportAction::PlayPause,
            mde_web_preview_client::MediaTransportAction::Play,
            mde_web_preview_client::MediaTransportAction::Pause,
            mde_web_preview_client::MediaTransportAction::Stop,
            mde_web_preview_client::MediaTransportAction::Next,
            mde_web_preview_client::MediaTransportAction::Previous,
            mde_web_preview_client::MediaTransportAction::VolumeUp,
            mde_web_preview_client::MediaTransportAction::VolumeDown,
        ] {
            assert!(
                controls.iter().any(|msg| matches!(
                    msg,
                    mde_web_preview_client::ControlMsg::MediaTransport { action: seen }
                        if *seen == action
                )),
                "{action:?} media transport must reach the helper: {controls:?}"
            );
        }
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAutoplayBlocked { blocked: true }
            )),
            "default/blocking autoplay must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAutoplayBlocked { blocked: false }
            )),
            "allowing autoplay must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetForceDark { enabled: true }
            )),
            "enabling force-dark must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetForceDark { enabled: false }
            )),
            "disabling force-dark must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetReaderMode { enabled: true }
            )),
            "enabling reader mode must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetReaderMode { enabled: false }
            )),
            "disabling reader mode must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserScripts {
                    enabled: true,
                    bundle,
                } if bundle.contains("youtube-focus")
                    && bundle.contains("npr-reader")
                    && bundle.contains("spotify-quiet")
            )),
            "enabling curated userscripts must reach the helper with the bundled library: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserScripts {
                    enabled: false,
                    bundle,
                } if bundle.is_empty()
            )),
            "disabling curated userscripts must clear the helper bundle: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserAgent { user_agent }
                    if user_agent.contains("X11; Linux x86_64")
                        && user_agent.contains("Chrome/126")
            )),
            "desktop UA override must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetUserAgent { user_agent }
                    if user_agent.contains("Android 14")
                        && user_agent.contains("Mobile Safari")
            )),
            "mobile UA override must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetDeviceProfile {
                    profile,
                    width: 390,
                    height: 844,
                    scale_percent: 300,
                    touch: true,
                } if profile == "phone"
            )),
            "phone device profile must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetDeviceProfile {
                    profile,
                    width: 820,
                    height: 1180,
                    scale_percent: 200,
                    touch: true,
                } if profile == "tablet"
            )),
            "tablet device profile must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "spellcheck must request bounded page text from the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SavePdf { path }
                    if path == &print_path_text
            )),
            "CUPS print must request a helper PDF before lp submission: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SavePdf { path } if path == &pdf_path_text
            )),
            "save-as-PDF must reach the helper with the chosen path: {controls:?}"
        );
    }

    #[test]
    fn page_context_menu_actions_send_native_helper_controls() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_page_context_action(state.active, chrome_ui::PageContextAction::Reload);
        state.apply_page_context_action(
            state.active,
            chrome_ui::PageContextAction::Edit(EditCommand::Copy),
        );
        state.apply_page_context_action(
            state.active,
            chrome_ui::PageContextAction::Edit(EditCommand::SelectAll),
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::Reload)),
            "context-menu Reload must reach the helper: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::EditCommand {
                    command: EditCommand::Copy
                }
            )),
            "context-menu Copy must send a native edit command: {controls:?}"
        );
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::EditCommand {
                    command: EditCommand::SelectAll
                }
            )),
            "context-menu Select-all must send a native edit command: {controls:?}"
        );
    }

    #[test]
    fn spellcheck_notice_summarizes_results_without_hunspell() {
        assert_eq!(
            spellcheck_notice(Ok(Vec::new())),
            "Spelling: no misspellings found"
        );
        assert_eq!(
            spellcheck_notice(Ok(vec![SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned()],
            }])),
            "Spelling: 1 possible misspelling; first: wrold -> world"
        );
        assert_eq!(
            spellcheck_notice(Err("hunspell not installed".to_owned())),
            "Spelling unavailable: Spelling dictionary is not installed"
        );
    }

    #[test]
    fn browser_spellcheck_result_model_keeps_misses_and_copy_text() {
        let misses = vec![
            SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned(), "would".to_owned()],
            },
            SpellMiss {
                chars: 12..18,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
        ];
        let result = BrowserSpellcheckResult::from_result(3, Ok(misses.clone()));
        assert!(result.is_visible());
        assert_eq!(result.tab_index, 3);
        assert_eq!(result.summary(), "2 possible misspellings");
        assert_eq!(result.misses, misses);
        assert_eq!(
            spellcheck_results_text(&result.misses),
            "wrold [0..5]: world, would\nmsh [12..18]: no suggestions"
        );

        let unavailable =
            BrowserSpellcheckResult::from_result(4, Err("hunspell not installed".to_owned()));
        assert!(unavailable.is_visible());
        assert_eq!(unavailable.tab_index, 4);
        assert_eq!(unavailable.error.as_deref(), Some("hunspell not installed"));
        assert_eq!(
            unavailable.user_facing_error().as_deref(),
            Some("Spelling dictionary is not installed")
        );
        assert_eq!(
            unavailable.summary(),
            "Spellcheck unavailable: Spelling dictionary is not installed"
        );
    }

    #[test]
    fn spellcheck_highlight_words_are_bounded_and_deduped() {
        let words = spellcheck_highlight_words(&[
            SpellMiss {
                chars: 0..5,
                word: " wrold ".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 7..12,
                word: "wrold".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 13..14,
                word: "x".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 15..20,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
        ]);
        assert_eq!(words, vec!["msh".to_owned(), "wrold".to_owned()]);
    }

    #[test]
    fn spellcheck_occurrence_index_counts_prior_matching_rows() {
        let misses = vec![
            SpellMiss {
                chars: 0..5,
                word: "wrold".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 8..12,
                word: "msh".to_owned(),
                suggestions: Vec::new(),
            },
            SpellMiss {
                chars: 16..21,
                word: "WROLD".to_owned(),
                suggestions: Vec::new(),
            },
        ];

        assert_eq!(spellcheck_occurrence_index(&misses, 0), 0);
        assert_eq!(spellcheck_occurrence_index(&misses, 1), 0);
        assert_eq!(spellcheck_occurrence_index(&misses, 2), 1);
        assert_eq!(spellcheck_occurrence_index(&misses, 99), 0);
    }

    #[test]
    fn browser_spellcheck_poll_retains_latest_page_text_result_and_marks_page() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let (tx, rx) = mpsc::channel();
        state.spellcheck.in_flight = Some(7);
        state.spellcheck.tab_index = Some(0);
        state.spellcheck.rx = Some(rx);
        tx.send((
            7,
            Ok(vec![SpellMiss {
                chars: 3..8,
                word: "wrold".to_owned(),
                suggestions: vec!["world".to_owned()],
            }]),
        ))
        .expect("send spell result");

        state.poll_spellcheck();

        let result = state
            .latest_spellcheck
            .as_ref()
            .expect("stored spellcheck result");
        assert_eq!(result.misses.len(), 1);
        assert_eq!(result.misses[0].word, "wrold");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: 1 possible misspelling; first: wrold -> world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetSpellcheckHighlights { words }
                    if words == &vec!["wrold".to_owned()]
            )),
            "spellcheck misses must be sent back to the helper as page highlights: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_sends_selected_suggestion_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction(0, "wrold", "world");

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrection {
                    word,
                    replacement,
                } if word == "wrold" && replacement == "world"
            )),
            "selected spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_all_sends_selected_suggestion_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction_all(0, "wrold", "world");

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced all wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrectionAll {
                    word,
                    replacement,
                } if word == "wrold" && replacement == "world"
            )),
            "all-occurrence spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_spellcheck_correction_at_sends_selected_occurrence_to_result_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        state.apply_spellcheck_correction_at(0, "wrold", "world", 2);

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Spelling: replaced occurrence 3 of wrold with world")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::ApplySpellcheckCorrectionAt {
                    word,
                    replacement,
                    occurrence,
                } if word == "wrold" && replacement == "world" && *occurrence == 2
            )),
            "indexed spelling suggestions must reach the helper: {controls:?}"
        );
    }

    #[test]
    fn cups_print_submission_uses_lp_title_and_pdf_path() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");
        let mut seen_program = String::new();
        let mut seen_args = Vec::new();
        let title = format!("{} - Example", browser_product_label());

        let job = submit_pdf_to_cups_with_runner(
            &path,
            &title,
            &CupsPrintSettings::default(),
            |program, args, timeout| {
                seen_program = program.to_owned();
                seen_args = args.to_vec();
                assert_eq!(timeout, CUPS_PRINT_TIMEOUT);
                Ok(ProcessOutput {
                    success: true,
                    stdout: "request id is Office-42 (1 file)\n".to_owned(),
                    stderr: String::new(),
                })
            },
        )
        .expect("lp submission succeeds");

        assert_eq!(job, "request id is Office-42 (1 file)");
        assert_eq!(seen_program, "lp");
        assert_eq!(
            seen_args,
            vec!["-t".to_owned(), title, path.to_string_lossy().into_owned(),]
        );
    }

    #[test]
    fn cups_print_submission_surfaces_lp_errors_without_a_printer() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");

        let err = submit_pdf_to_cups_with_runner(
            &path,
            "Example",
            &CupsPrintSettings::default(),
            |_program, _args, _timeout| {
                Ok(ProcessOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "lp: Error - no default destination available\n".to_owned(),
                })
            },
        )
        .expect_err("lp failure is surfaced");

        assert_eq!(err, "lp: Error - no default destination available");
    }

    #[test]
    fn printer_error_labels_hide_backend_terms_for_browser_chrome() {
        assert_eq!(
            printer_error_label("lp: Error - no default destination available").as_deref(),
            Some("No default printer is available")
        );
        assert_eq!(
            printer_error_label("CUPS service unavailable").as_deref(),
            Some("Printer service unavailable")
        );
        assert_eq!(
            printer_error_label("/tmp/page.pdf is not a file").as_deref(),
            Some("Print output was not found")
        );
        assert_eq!(
            printer_error_label("lp failed without an error message").as_deref(),
            Some("Printer did not accept the job")
        );
    }

    #[test]
    fn browser_print_pdf_events_use_user_facing_notices() {
        let mut state = WebState::default();
        let path = "/tmp/quazar-print-missing.pdf".to_owned();
        state.pending_cups_prints.insert(
            path.clone(),
            CupsPrintRequest {
                path: path.clone(),
                title: "Example".to_owned(),
                settings: CupsPrintSettings::default(),
            },
        );
        let failed_notice = state.handle_pdf_event(path.clone(), false);
        assert_eq!(failed_notice, "Print failed: PDF could not be created");
        assert!(
            !failed_notice.contains("CUPS")
                && !failed_notice.contains("lp")
                && !failed_notice.contains("/tmp/"),
            "print PDF failure leaked backend copy: {failed_notice}"
        );

        state.pending_cups_prints.insert(
            path.clone(),
            CupsPrintRequest {
                path: path.clone(),
                title: "Example".to_owned(),
                settings: CupsPrintSettings::default(),
            },
        );
        let missing_notice = state.handle_pdf_event(path, true);
        assert_eq!(missing_notice, "Print failed: Print output was not found");
        assert!(
            !missing_notice.contains("CUPS")
                && !missing_notice.contains("lp")
                && !missing_notice.contains("/tmp/"),
            "print PDF missing-output notice leaked backend copy: {missing_notice}"
        );
    }

    #[test]
    fn cups_print_submission_applies_destination_and_options() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");
        let settings = CupsPrintSettings {
            destination: Some("Office".to_owned()),
            copies: 3,
            duplex: true,
            grayscale: true,
            ..Default::default()
        };
        let mut seen_args = Vec::new();

        submit_pdf_to_cups_with_runner(&path, "Example", &settings, |_program, args, _timeout| {
            seen_args = args.to_vec();
            Ok(ProcessOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
        .expect("lp submission succeeds");

        assert_eq!(
            seen_args,
            vec![
                "-d".to_owned(),
                "Office".to_owned(),
                "-n".to_owned(),
                "3".to_owned(),
                "-o".to_owned(),
                "sides=two-sided-long-edge".to_owned(),
                "-o".to_owned(),
                "ColorModel=Gray".to_owned(),
                "-t".to_owned(),
                "Example".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
        );
    }

    #[test]
    fn cups_printer_discovery_marks_the_default_destination() {
        let printers = discover_cups_printers_with_runner(|program, args, timeout| {
            assert_eq!(program, "lpstat");
            assert_eq!(timeout, CUPS_PRINT_TIMEOUT);
            if args == ["-e"] {
                Ok(ProcessOutput {
                    success: true,
                    stdout: "Lab\nOffice\nLab\n".to_owned(),
                    stderr: String::new(),
                })
            } else if args == ["-d"] {
                Ok(ProcessOutput {
                    success: true,
                    stdout: "system default destination: Office\n".to_owned(),
                    stderr: String::new(),
                })
            } else {
                panic!("unexpected lpstat args: {args:?}");
            }
        })
        .expect("printer discovery succeeds");

        assert_eq!(
            printers,
            vec![
                CupsPrinter {
                    name: "Office".to_owned(),
                    is_default: true,
                },
                CupsPrinter {
                    name: "Lab".to_owned(),
                    is_default: false,
                },
            ]
        );
    }

    #[test]
    fn container_tabs_are_named_per_tab_and_visible_in_chrome() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);

        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, 1);
        assert_eq!(state.tabs[0].container, ContainerProfile::None);
        assert_eq!(state.tabs[1].container, ContainerProfile::None);

        state.set_active_tab_container(ContainerProfile::Work);
        assert_eq!(state.tabs[1].container, ContainerProfile::Work);
        assert_eq!(
            state.tabs[0].container,
            ContainerProfile::None,
            "container identity stays per-tab"
        );
        assert!(
            !chrome_ui::tab_label(&state.tabs[1]).contains("W "),
            "the tab title should stay clean; status belongs to the chip row"
        );
        assert!(
            chrome_ui::tab_status_chip_labels(&state.tabs[1]).contains(&"Work"),
            "the tab pill carries the Work container chip"
        );
        assert!(
            chrome_ui::tab_hover(&state.tabs[1]).contains("Container: Work"),
            "the hover text names the container"
        );

        state.cycle_active_tab_container();
        assert_eq!(state.tabs[1].container, ContainerProfile::Banking);
        state.select_tab(0);
        state.cycle_active_tab_container();
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[1].container, ContainerProfile::Banking);
    }

    #[test]
    fn display_targets_are_per_tab_and_visible_in_chrome() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);

        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.active, 1);
        assert_eq!(state.tabs[0].display_target, DisplayTarget::Current);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::Current);

        state.set_active_tab_display_target(DisplayTarget::Secondary);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::Secondary);
        assert_eq!(
            state.tabs[0].display_target,
            DisplayTarget::Current,
            "display target intent stays per-tab"
        );
        assert!(
            !chrome_ui::tab_label(&state.tabs[1]).contains("D2 "),
            "the tab title should stay clean; status belongs to the chip row"
        );
        assert!(
            chrome_ui::tab_status_chip_labels(&state.tabs[1]).contains(&"Secondary Display"),
            "the tab pill carries the secondary-display chip"
        );
        assert!(
            chrome_ui::tab_hover(&state.tabs[1]).contains("Display target: Secondary Display"),
            "the hover text names the display target"
        );

        state.cycle_active_tab_display_target();
        assert_eq!(state.tabs[1].display_target, DisplayTarget::AllDisplays);
        state.select_tab(0);
        state.cycle_active_tab_display_target();
        assert_eq!(state.tabs[0].display_target, DisplayTarget::Primary);
        assert_eq!(state.tabs[1].display_target, DisplayTarget::AllDisplays);
    }

    #[test]
    fn display_target_changes_publish_platform_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);

        state.set_active_tab_display_target(DisplayTarget::Secondary);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_DISPLAY_TARGET, None)
            .expect("list display target actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("handoff body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_display_target");
        assert_eq!(v["tab_index"], 0);
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["target"], "secondary");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");

        state.set_active_tab_display_target(DisplayTarget::AllDisplays);
        let msgs = persist
            .list_since(ACTION_BROWSER_DISPLAY_TARGET, None)
            .expect("list display target actions");
        assert_eq!(msgs.len(), 2);
        let body = msgs[1].body.as_deref().expect("second handoff body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["target"], "all_displays");
    }

    #[test]
    fn inactive_idle_tabs_publish_suspend_handoff_and_stop_once() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (first, first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(first, BrowserEngine::Servo);
        state.push_session_with_engine(second, BrowserEngine::Cef);
        assert_eq!(state.active, 1, "second tab is active");
        state.tabs[0].session.poll();
        state.tabs[1].session.poll();
        let _ = drain_control_messages(&first_helper);

        let now = Instant::now();
        state.tabs[0].last_activity = now - IDLE_TAB_SUSPEND_AFTER - Duration::from_secs(1);
        state.suspend_idle_tabs(now);
        state.suspend_idle_tabs(now + Duration::from_secs(1));

        assert!(state.tabs[0].idle_suspended);
        assert!(!state.tabs[1].idle_suspended, "active tab is not suspended");
        let controls = drain_control_messages(&first_helper);
        assert!(
            controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::Stop)),
            "inactive idle tab received a Stop control: {controls:?}"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_TAB_SUSPEND, None)
            .expect("list browser suspend actions");
        assert_eq!(msgs.len(), 1, "suspend handoff is once per idle period");
        let body = msgs[0].body.as_deref().expect("suspend body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_tab_suspend");
        assert_eq!(v["tab_index"], 0);
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["source"], "browser");
        assert_eq!(
            v["idle_after_ms"],
            u64::try_from(IDLE_TAB_SUSPEND_AFTER.as_millis()).unwrap()
        );
        assert!(
            !chrome_ui::tab_label(&state.tabs[0]).contains('\u{25D2}'),
            "the tab title should stay clean; status belongs to the chip row"
        );
        assert!(
            chrome_ui::tab_status_chip_labels(&state.tabs[0]).contains(&"Idle suspended"),
            "suspended tabs wear the idle status chip"
        );
        assert!(chrome_ui::tab_hover(&state.tabs[0]).contains("Idle suspended"));
    }

    #[test]
    fn audible_inactive_tabs_are_not_idle_suspended() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (first, first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(first, BrowserEngine::Cef);
        state.push_session_with_engine(second, BrowserEngine::Servo);
        assert_eq!(state.active, 1, "second tab is active");
        state.tabs[0].session.poll();
        let _ = drain_control_messages(&first_helper);

        write_helper_event(
            &first_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        state.tabs[0].session.poll();
        assert!(
            state.tabs[0].session.audible(),
            "precondition: inactive tab is producing audio"
        );

        let now = Instant::now();
        state.tabs[0].last_activity = now - IDLE_TAB_SUSPEND_AFTER - Duration::from_secs(1);
        state.suspend_idle_tabs(now);

        assert!(
            !state.tabs[0].idle_suspended,
            "background audio must not be stopped by idle suspension"
        );
        let controls = drain_control_messages(&first_helper);
        assert!(
            !controls
                .iter()
                .any(|msg| matches!(msg, mde_web_preview_client::ControlMsg::Stop)),
            "audible inactive tab must not receive Stop: {controls:?}"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_TAB_SUSPEND, None)
            .expect("list browser suspend actions");
        assert!(
            msgs.is_empty(),
            "audible inactive tab should not publish a suspend handoff"
        );
    }

    #[test]
    fn selecting_a_suspended_tab_reactivates_idle_state() {
        let (first, _first_helper, _first_writer) = live_page_session();
        let (second, _second_helper, _second_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.tabs[0].idle_suspended = true;
        let old_activity = state.tabs[0].last_activity;

        state.select_tab(0);

        assert_eq!(state.active, 0);
        assert!(!state.tabs[0].idle_suspended);
        assert!(state.tabs[0].last_activity >= old_activity);
    }

    #[test]
    fn no_session_paints_the_gated_empty_state() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        let out = run_panel_output(&ctx, &mut state, body_input());
        assert!(state.tabs.is_empty());
        let texts: Vec<String> = painted_text(&out.shapes)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert!(texts.iter().any(|text| text == "Browser"));
        assert!(
            texts
                .iter()
                .any(|text| text == chrome_ui::BROWSER_NO_LIVE_PAGE_NOTICE),
            "empty Browser body must show the Browser-facing unavailable notice: {texts:?}"
        );
        for forbidden in [
            "Sandboxed browser",
            "Servo",
            "BOOKMARKS",
            "helper",
            "live path",
        ] {
            assert!(
                !texts.iter().any(|text| text.contains(forbidden)),
                "empty Browser body leaked implementation copy {forbidden:?}: {texts:?}"
            );
        }
    }

    #[test]
    fn browser_default_chrome_retires_the_shared_menubar_strip() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        let out = run_panel_output(&ctx, &mut state, body_input());
        let texts: Vec<String> = painted_text(&out.shapes)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert!(
            !texts.iter().any(|text| text == "BROWSER"),
            "Browser should no longer paint the shared MENUBAR-ALL title strip: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains('\u{22EE}')),
            "Browser toolbar should not render the old dropdown ellipsis: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text == "Browser Options"),
            "Options render only after the internal tab is opened: {texts:?}"
        );
    }

    #[test]
    fn browser_options_tab_opens_focuses_and_clears_for_real_navigation() {
        let mut state = WebState::default();

        state.open_options_tab();

        assert_eq!(state.tabs.len(), 1);
        assert_eq!(
            state.active_internal_page(),
            Some(BrowserInternalPage::Options)
        );
        assert_eq!(state.address, BROWSER_OPTIONS_URL);
        assert_eq!(chrome_ui::tab_label(&state.tabs[0]), "Browser Options");

        state.open_options_tab();
        assert_eq!(
            state.tabs.len(),
            1,
            "opening Options again focuses the existing internal tab"
        );

        state.load_target("https://example.test/".to_owned());
        assert_eq!(state.active_internal_page(), None);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::ReplaceActiveUrl {
                index: 0,
                engine: BrowserEngine::Servo,
                url: "https://example.test/".to_owned(),
            }),
            "real omnibox navigation replaces the internal page with a live helper"
        );
    }

    #[test]
    fn browser_options_page_renders_command_categories_and_disabled_rows_in_chrome_text() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        state.power_mode = true;
        state.open_options_tab();
        let menu_titles: Vec<String> = menubar::chrome_menus(&state)
            .iter()
            .map(|menu| menu.title.clone())
            .collect();
        assert_eq!(
            menu_titles,
            [
                "Page".to_owned(),
                "Engine".to_owned(),
                "Edit".to_owned(),
                "View".to_owned(),
                "Power".to_owned(),
                "History".to_owned(),
                "Privacy".to_owned(),
                "Bookmarks".to_owned()
            ],
            "Options page is backed by every top-level Browser command category"
        );

        let out = run_panel_output(&ctx, &mut state, body_input());
        let texts = painted_text(&out.shapes);
        let labels: Vec<&str> = texts.iter().map(|(text, _)| text.as_str()).collect();
        for label in [
            "Navigation",
            "Engines",
            "Input",
            "Rendering",
            "Instrumentation",
        ] {
            assert!(
                labels.contains(&label),
                "Options page category rail must expose the visible {label} category: {labels:?}"
            );
        }
        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "Back" && *color == chrome_ui::CHROME_TEXT_DIM),
            "disabled command rows stay visible with Browser dim text: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "Use Chromium for New Tabs"
                    && *color == chrome_ui::CHROME_TEXT),
            "engine controls render through the Options command model: {texts:?}"
        );
    }

    #[test]
    fn browser_options_actions_dispatch_through_menubar_apply() {
        let ctx = egui::Context::default();
        let mut state = WebState::default();
        assert!(state.vertical_tabs);

        menubar::apply(&ctx, &mut state, menubar::MenuAction::ToggleVerticalTabs);
        assert!(!state.vertical_tabs);
        menubar::apply(
            &ctx,
            &mut state,
            menubar::MenuAction::SelectEngine(BrowserEngine::Cef),
        );
        assert_eq!(state.engine, BrowserEngine::Cef);
    }

    #[test]
    fn a_paint_ready_frame_uploads_to_a_texture_and_paints() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_until_texture(&mut state),
            "no frame uploaded to a texture"
        );
        assert!(state.tabs[0].texture.is_some());
        assert!(run_panel(&mut state), "the browser image produced no draw");
    }

    #[test]
    fn frame_upload_uses_arc_capture_retention_without_cpu_clone() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_until_texture(&mut state),
            "no frame uploaded to a texture"
        );
        let tab = &state.tabs[state.active];
        assert!(tab.texture.is_some(), "paint-ready frame did not upload");
        let frame = tab
            .last_frame
            .as_ref()
            .expect("paint-ready upload must retain the capture frame");
        assert!(
            !frame.pixels.is_empty(),
            "retained capture frame must keep the decoded pixels"
        );
        assert_eq!(
            std::sync::Arc::strong_count(frame),
            1,
            "Browser should retain exactly one CPU-side frame Arc for capture"
        );
    }

    #[test]
    fn tab_strip_state_switches_closes_and_requests_new_tabs() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        assert!(run_until_texture(&mut state));
        assert_eq!(state.active, 1, "new pushed tabs become foreground");

        state.select_tab(0);
        assert_eq!(state.active, 0);
        assert_eq!(state.address, "about:blank");

        state.close_tab(0);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.active, 0, "active index stays valid after close");

        state.request_new_tab(BrowserEngine::Cef);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForeground(BrowserEngine::Cef))
        );
        assert_eq!(state.take_open_request(), None, "request drains once");
    }

    #[test]
    fn tab_reorder_preserves_the_active_session_identity() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let (third, _helper3) = testkit::connect().expect("connect 3");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.push_session(third);
        assert!(run_until_texture(&mut state));
        state.select_tab(1);
        let active_title = state.tabs[state.active].session.title().to_owned();

        state.move_tab(1, 0);
        assert_eq!(state.active, 0);
        assert_eq!(state.tabs[state.active].session.title(), active_title);

        state.move_tab(0, 2);
        assert_eq!(state.active, 2);
        assert_eq!(state.tabs[state.active].session.title(), active_title);
    }

    /// A `WebState` with `n` (≤4) tabs, each tagged with a distinct container so a
    /// pinned recluster can be asserted by *identity* (tabs 0..n → Personal, Work,
    /// Banking, Research). No live helper — the pin/reorder methods never poll.
    fn tagged_tabs(n: usize) -> WebState {
        let mut state = WebState::default();
        for _ in 0..n {
            let (shell, _peer) = UnixStream::pair().expect("socketpair");
            state.push_session(WebSession::from_stream(shell, None).expect("session"));
        }
        for (i, tab) in state.tabs.iter_mut().enumerate() {
            tab.container = ContainerProfile::ALL[i + 1]; // skip None (index 0)
        }
        state
    }

    #[test]
    fn pinning_a_tab_clusters_it_to_the_front_preserving_order() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(2, true); // pin the Banking tab
                                       // The pinned tab jumps to the front; the unpinned tail keeps its order.
        assert!(state.tabs[0].pinned);
        assert!(!state.tabs[1].pinned && !state.tabs[2].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Banking);
        assert_eq!(state.tabs[1].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[2].container, ContainerProfile::Work);
    }

    #[test]
    fn pinning_tracks_the_active_tab_across_the_recluster() {
        let mut state = tagged_tabs(3);
        state.select_tab(1); // active = the Work tab
        state.set_tab_pinned(2, true); // recluster → [Banking, Personal, Work]
        assert_eq!(state.active, 2);
        assert_eq!(state.tabs[state.active].container, ContainerProfile::Work);
    }

    #[test]
    fn unpinning_returns_a_tab_to_the_front_of_the_unpinned_cluster() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned: [Personal, Work]; unpinned: [Banking]
        state.set_tab_pinned(0, false); // unpin Personal → rejoins unpinned front
        assert!(state.tabs[0].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Work); // still pinned
        assert!(!state.tabs[1].pinned);
        assert_eq!(state.tabs[1].container, ContainerProfile::Personal); // unpinned front
        assert_eq!(state.tabs[2].container, ContainerProfile::Banking);
    }

    #[test]
    fn a_drag_cannot_pull_an_unpinned_tab_ahead_of_a_pinned_one() {
        let mut state = tagged_tabs(3); // [Personal, Work, Banking]
        state.set_tab_pinned(0, true); // Personal pinned at the front
        state.move_tab(2, 0); // try to drag Banking to the very front
                              // The pinned Personal tab stays at the front — the drag snapped back.
        assert!(state.tabs[0].pinned);
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
    }

    #[test]
    fn the_audio_icon_reflects_playback_and_mute() {
        // Silent + unmuted -> no icon (the strip stays quiet).
        assert_eq!(chrome_ui::audio_icon_for(false, false), None);
        // Audibly playing -> the speaker, hover offers to mute.
        assert_eq!(
            chrome_ui::audio_icon_for(true, false),
            Some((chrome_ui::ChromeIcon::VolumeUp, "Mute tab"))
        );
        // Muted -> the muted-speaker; mute wins the icon even while audio plays.
        assert_eq!(
            chrome_ui::audio_icon_for(false, true),
            Some((chrome_ui::ChromeIcon::VolumeOff, "Unmute tab"))
        );
        assert_eq!(
            chrome_ui::audio_icon_for(true, true),
            Some((chrome_ui::ChromeIcon::VolumeOff, "Unmute tab"))
        );
    }

    #[test]
    fn media_metadata_chip_label_uses_title_artist_and_source_fallbacks() {
        assert_eq!(ellipsize("abcdef", 6), "abcdef");
        assert_eq!(ellipsize("abcdef", 5), "ab...");
        assert_eq!(ellipsize("abcdef", 3), "...");
        assert_eq!(ellipsize("abcdef", 0), "");
        assert!(ellipsize("abcdef", 5).is_ascii());
        assert!(ellipsize("abcdef", 5).chars().count() <= 5);
        assert_eq!(
            media_metadata_chip_label(r#"{"title":" Track ","artist":" Artist "}"#).as_deref(),
            Some("Now: Track - Artist")
        );
        assert_eq!(
            media_metadata_chip_label(
                r#"{"title":"","source_url":"https://media.example/song.mp3"}"#
            )
            .as_deref(),
            Some("Now: https://media.example/so...")
        );
        assert_eq!(media_metadata_chip_label(r#"{"title":"   "}"#), None);
        assert_eq!(media_metadata_chip_label("not-json"), None);
    }

    #[test]
    fn browser_output_label_hides_parent_paths() {
        assert_eq!(
            browser_output_label(Path::new("/tmp/quazar/report.pdf")),
            "report.pdf"
        );
        assert_eq!(
            browser_output_label(Path::new(r"C:\Users\Alice\capture.png")),
            "capture.png"
        );
        assert_eq!(browser_output_label(Path::new("/")), "saved file");

        let long = Path::new("/tmp/quazar/abcdefghijklmnopqrstuvwxyz0123456789-output.pdf");
        let label = browser_output_label(long);
        assert!(label.ends_with("..."));
        assert!(label.chars().count() <= 48);
        assert!(!label.contains("/tmp/"));
    }

    #[test]
    fn browser_media_toolbar_model_requires_real_media_metadata() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(session, BrowserEngine::Cef);
        state.tabs[0].session.poll();

        assert_eq!(
            chrome_ui::browser_media_toolbar_model(&state),
            None,
            "ordinary pages without media metadata must not grow toolbar chrome"
        );
    }

    #[test]
    fn browser_media_toolbar_model_reflects_paused_active_media() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(session, BrowserEngine::Cef);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Paused Track","paused":true}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();

        let model = chrome_ui::browser_media_toolbar_model(&state).expect("toolbar media model");
        assert_eq!(model.label, "Now: Paused Track");
        assert!(model.paused);
        assert!(!model.background);
        let (icon, _tip, action) = chrome_ui::media_toolbar_play_action(model.paused);
        assert_eq!(icon, chrome_ui::ChromeIcon::Play);
        assert_eq!(action, mde_web_preview_client::MediaTransportAction::Play);
    }

    #[test]
    fn browser_media_toolbar_model_selects_background_media_and_existing_transport_path() {
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();

        let model = chrome_ui::browser_media_toolbar_model(&state).expect("toolbar media model");
        assert_eq!(model.label, "Now: Background");
        assert!(!model.paused);
        assert!(model.background);
        let (icon, _tip, action) = chrome_ui::media_toolbar_play_action(model.paused);
        assert_eq!(icon, chrome_ui::ChromeIcon::Pause);
        assert_eq!(action, mde_web_preview_client::MediaTransportAction::Pause);

        assert!(state.selected_media_transport(action));
        let media_controls = drain_control_messages(&media_helper);
        assert!(
            media_controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport {
                    action: mde_web_preview_client::MediaTransportAction::Pause
                }
            )),
            "toolbar media control should reuse the selected background media path: {media_controls:?}"
        );
        let quiet_controls = drain_control_messages(&quiet_helper);
        assert!(
            quiet_controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport { .. }
            )),
            "quiet foreground tab must not receive toolbar media transport: {quiet_controls:?}"
        );
    }

    #[test]
    fn browser_media_pip_model_requires_media_metadata_and_retained_frame() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(session, BrowserEngine::Cef);
        state.media_pip_open = true;
        state.tabs[0].session.poll();

        assert_eq!(
            chrome_ui::browser_media_pip_model(&state),
            None,
            "PiP must not render for ordinary pages without media metadata"
        );

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"PiP Track","paused":true}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        assert_eq!(
            chrome_ui::browser_media_pip_model(&state),
            None,
            "metadata alone is not enough; the overlay needs a retained frame"
        );

        assert!(run_until_texture(&mut state));
        let model = chrome_ui::browser_media_pip_model(&state).expect("PiP model");
        assert_eq!(model.label, "Now: PiP Track");
        assert!(model.paused);
        assert_eq!(
            model.frame_size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize]
        );
    }

    #[test]
    fn browser_media_pip_menu_toggles_state_through_menubar_apply() {
        let ctx = egui::Context::default();
        let mut empty = WebState::default();
        menubar::apply(
            &ctx,
            &mut empty,
            menubar::MenuAction::TogglePictureInPicture,
        );
        assert!(
            !empty.media_pip_open,
            "no selected Browser media keeps PiP apply as a safe no-op"
        );

        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(session, BrowserEngine::Cef);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Menu Track","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();

        menubar::apply(
            &ctx,
            &mut state,
            menubar::MenuAction::TogglePictureInPicture,
        );
        assert!(state.media_pip_open);
        menubar::apply(
            &ctx,
            &mut state,
            menubar::MenuAction::TogglePictureInPicture,
        );
        assert!(!state.media_pip_open);
    }

    #[test]
    fn browser_media_pip_renders_background_media_frame_without_switching_tabs() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, _quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();
        state.media_pip_open = true;

        let out = run_panel_output(&ctx, &mut state, body_input());
        assert_eq!(
            state.active, 1,
            "rendering PiP must not focus the media tab"
        );
        assert!(
            state.tabs[0].texture.is_some(),
            "PiP uploads the retained background media frame for painting"
        );
        let media_texture = state.tabs[0].texture.as_ref().expect("media texture").id();
        let texts = painted_text(&out.shapes);
        assert!(
            texts.iter().any(|(text, _)| text == "Picture-in-Picture"),
            "PiP title should be painted in Browser chrome text: {texts:?}"
        );
        assert!(
            texts.iter().any(|(text, _)| text == "Now: Background"),
            "PiP should name the selected background media: {texts:?}"
        );
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(
            page_texture_rect(&prims, media_texture).is_some(),
            "PiP should paint the background media tab texture"
        );
    }

    #[test]
    fn browser_media_pip_requests_idle_repaint_when_active_tab_is_internal_page() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let (media_session, media_helper, _media_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background PiP","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        assert!(run_until_texture(&mut state));

        state.open_options_tab();
        state.media_pip_open = true;

        assert_eq!(
            state.active_internal_page(),
            Some(BrowserInternalPage::Options)
        );
        assert!(
            !state.active_live_page_needs_repaint(),
            "the regression setup must not be satisfied by the active page heartbeat"
        );
        assert!(
            state.media_pip_needs_repaint(),
            "playing background PiP media must keep polling without pointer input"
        );

        let out = run_panel_output(&ctx, &mut state, body_input());
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        assert!(
            repaint_delay <= LIVE_PAGE_REPAINT_INTERVAL,
            "background Browser PiP media must schedule frame polling without mouse input (delay {repaint_delay:?})"
        );
    }

    #[test]
    fn browser_media_status_publishes_retained_metadata_and_dedupes() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        state.tabs[0].session.poll();
        state.publish_media_status_if_changed();

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: serde_json::json!({
                    "title": " Track ",
                    "artist": " Artist ",
                    "album": " Album ",
                    "artwork_url": "https://media.example/art.png",
                    "source_url": "https://media.example/track.mp3",
                    "paused": false,
                    "duration_ms": 120000,
                    "position_ms": 42000,
                    "volume_percent": 42,
                })
                .to_string(),
            },
        );
        state.tabs[0].session.poll();
        state.publish_media_status_if_changed();
        state.publish_media_status_if_changed();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_media_status_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list browser media status");
        assert_eq!(msgs.len(), 2, "unchanged media status is de-duped");
        let latest: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("media body"))
                .expect("valid media JSON");
        assert_eq!(latest["op"], "browser_media_status");
        assert_eq!(latest["source"], "browser");
        assert_eq!(latest["state"], "playing");
        assert_eq!(latest["engine"], "cef");
        assert_eq!(latest["tab_index"], 0);
        assert_eq!(latest["tab_id"], 1);
        assert_eq!(latest["active_tab"], true);
        assert_eq!(latest["url"], "https://example.test/");
        assert_eq!(latest["page_title"], "Example");
        assert_eq!(latest["label"], "Now: Track - Artist");
        assert_eq!(latest["audible"], true);
        assert_eq!(latest["muted"], false);
        assert_eq!(latest["metadata"]["title"], "Track");
        assert_eq!(latest["metadata"]["artist"], "Artist");
        assert_eq!(latest["metadata"]["album"], "Album");
        assert_eq!(
            latest["metadata"]["artwork_url"],
            "https://media.example/art.png"
        );
        assert_eq!(
            latest["metadata"]["source_url"],
            "https://media.example/track.mp3"
        );
        assert_eq!(latest["metadata"]["paused"], false);
        assert_eq!(latest["metadata"]["duration_ms"], 120000);
        assert_eq!(latest["metadata"]["position_ms"], 42000);
        assert_eq!(latest["metadata"]["volume_percent"], 42);
    }

    #[test]
    fn browser_media_status_clears_to_idle_when_metadata_disappears() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[0].session.poll();

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Track","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.publish_media_status_if_changed();
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: String::new(),
            },
        );
        state.tabs[0].session.poll();
        state.publish_media_status_if_changed();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_media_status_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list browser media status");
        assert_eq!(msgs.len(), 2, "clear publishes exactly one idle status");
        let latest: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("media body"))
                .expect("valid media JSON");
        assert_eq!(latest["state"], "idle");
        assert!(latest["tab_index"].is_null());
        assert!(latest["tab_id"].is_null());
        assert!(latest["engine"].is_null());
        assert!(latest["label"].is_null());
        assert!(latest["metadata"].is_null());
    }

    #[test]
    fn browser_media_status_selects_audible_background_media_when_active_tab_is_quiet() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, _quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();

        state.publish_media_status_if_changed();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_media_status_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list browser media status");
        assert_eq!(msgs.len(), 1);
        let latest: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("media body"))
                .expect("valid media JSON");
        assert_eq!(latest["state"], "playing");
        assert_eq!(latest["tab_index"], 0);
        assert_eq!(latest["tab_id"], 1);
        assert_eq!(latest["engine"], "servo");
        assert_eq!(latest["active_tab"], false);
        assert_eq!(latest["audible"], true);
        assert_eq!(latest["metadata"]["title"], "Background");
    }

    #[test]
    fn browser_media_control_bus_action_drives_the_selected_media_tab() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        persist
            .write(
                &browser_media_control_topic(&local_hostname()),
                Priority::Default,
                None,
                Some(&browser_media_control_body(
                    mde_web_preview_client::MediaTransportAction::Pause,
                    None,
                    "test",
                    123,
                )),
            )
            .expect("write media control");

        state.poll_media_control_actions();

        let media_controls = drain_control_messages(&media_helper);
        assert!(
            media_controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport {
                    action: mde_web_preview_client::MediaTransportAction::Pause
                }
            )),
            "the background media tab should receive the pause action: {media_controls:?}"
        );
        let quiet_controls = drain_control_messages(&quiet_helper);
        assert!(
            quiet_controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport { .. }
            )),
            "the quiet foreground tab should not receive the media action: {quiet_controls:?}"
        );
    }

    #[test]
    fn browser_media_control_bus_volume_action_uses_selected_media_tab() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false,"volume_percent":42}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        persist
            .write(
                &browser_media_control_topic(&local_hostname()),
                Priority::Default,
                None,
                Some(&browser_media_control_body(
                    mde_web_preview_client::MediaTransportAction::VolumeUp,
                    None,
                    "test",
                    123,
                )),
            )
            .expect("write media volume control");

        state.poll_media_control_actions();

        let media_controls = drain_control_messages(&media_helper);
        assert!(
            media_controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport {
                    action: mde_web_preview_client::MediaTransportAction::VolumeUp
                }
            )),
            "the background media tab should receive the volume action: {media_controls:?}"
        );
        let quiet_controls = drain_control_messages(&quiet_helper);
        assert!(
            quiet_controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport { .. }
            )),
            "the quiet foreground tab should not receive the volume action: {quiet_controls:?}"
        );
    }

    #[test]
    fn browser_hardware_media_key_drives_selected_background_media_tab() {
        let (media_session, media_helper, _media_writer) = live_page_session();
        let (quiet_session, quiet_helper, _quiet_writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session_with_engine(media_session, BrowserEngine::Servo);
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::AudioState { audible: true },
        );
        write_helper_event(
            &media_helper,
            &mde_web_preview_client::EventMsg::MediaMetadata {
                body: r#"{"title":"Background","paused":false}"#.to_owned(),
            },
        );
        state.tabs[0].session.poll();
        state.push_session_with_engine(quiet_session, BrowserEngine::Cef);
        state.tabs[1].session.poll();
        assert_eq!(state.active, 1, "quiet foreground tab is active");

        assert!(
            state.selected_media_transport(mde_web_preview_client::MediaTransportAction::PlayPause)
        );

        let media_controls = drain_control_messages(&media_helper);
        assert!(
            media_controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport {
                    action: mde_web_preview_client::MediaTransportAction::PlayPause
                }
            )),
            "hardware media key should follow the selected background media tab: {media_controls:?}"
        );
        let quiet_controls = drain_control_messages(&quiet_helper);
        assert!(
            quiet_controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::MediaTransport { .. }
            )),
            "quiet foreground tab must not steal the hardware media key: {quiet_controls:?}"
        );
    }

    #[test]
    fn an_audible_tab_renders_the_speaker_without_panic() {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::AudioState { audible: true }.encode(),
        ))
        .expect("audio event");
        state.tabs[0].session.poll();
        assert!(state.tabs[0].session.audible(), "the tab is now audible");

        // The strip must paint the speaker glyph without panicking, muted or not.
        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        state.tabs[0].muted = true;
        run_tab_strip_frame(&ctx, &mut state, body_input());
    }

    #[test]
    fn duplicating_a_tab_enqueues_an_open_on_the_same_url() {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://dup.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();

        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.front(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://dup.example/"
            ),
            "duplicate enqueues a same-URL foreground open"
        );
    }

    #[test]
    fn close_other_tabs_keeps_only_the_target_and_pinned() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.set_tab_pinned(0, true); // Personal pinned, stays at the front
        let banking = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Banking)
            .expect("banking tab");
        state.close_other_tabs(banking);
        // Survivors: the pinned Personal tab + the kept Banking tab.
        assert_eq!(state.tabs.len(), 2);
        assert!(state
            .tabs
            .iter()
            .any(|t| t.container == ContainerProfile::Personal && t.pinned));
        assert_eq!(
            state.tabs[state.active].container,
            ContainerProfile::Banking
        );
    }

    #[test]
    fn close_tabs_to_the_right_closes_the_tail() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.close_tabs_to_the_right(1); // close Banking + Research
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.tabs[0].container, ContainerProfile::Personal);
        assert_eq!(state.tabs[1].container, ContainerProfile::Work);
    }

    #[test]
    fn close_tabs_to_the_right_spares_pinned_tabs() {
        let mut state = tagged_tabs(4);
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // Personal, Work pinned at the front
        state.close_tabs_to_the_right(0); // from the first pinned tab
        assert!(
            state
                .tabs
                .iter()
                .any(|t| t.container == ContainerProfile::Work && t.pinned),
            "the other pinned tab is spared"
        );
        assert!(
            !state
                .tabs
                .iter()
                .any(|t| t.container == ContainerProfile::Banking),
            "the unpinned tail is closed"
        );
    }

    #[test]
    fn tab_search_filters_on_title_and_url() {
        let mut state = WebState::default();
        let mut _peers = Vec::new();
        for url in [
            "https://news.example/",
            "https://mail.example/",
            "https://maps.example/",
        ] {
            let (shell, helper) = UnixStream::pair().expect("socketpair");
            helper.set_nonblocking(true).expect("helper nonblocking");
            state.push_session(WebSession::from_stream(shell, None).expect("session"));
            let idx = state.tabs.len() - 1;
            let mut peer: &UnixStream = &helper;
            peer.write_all(&wire::frame(
                &mde_web_preview_client::EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: url.to_owned(),
                }
                .encode(),
            ))
            .expect("nav");
            state.tabs[idx].session.poll();
            _peers.push(helper); // keep the peers alive so the sessions don't crash
        }

        // Empty query → the full list.
        assert_eq!(
            chrome_ui::matching_tab_indices(&state.tabs, ""),
            vec![0, 1, 2]
        );
        // A URL-substring match, case-insensitive.
        assert_eq!(
            chrome_ui::matching_tab_indices(&state.tabs, "mail"),
            vec![1]
        );
        assert_eq!(
            chrome_ui::matching_tab_indices(&state.tabs, "MAPS"),
            vec![2]
        );
        // No match → empty.
        assert!(chrome_ui::matching_tab_indices(&state.tabs, "zzz").is_empty());
    }

    #[test]
    fn permission_grant_is_remembered_and_auto_allows_next_time() {
        assert_eq!(chrome_ui::permission_kind_label(0), "know your location");
        assert_eq!(chrome_ui::permission_kind_label(2), "access the clipboard");
        assert_eq!(chrome_ui::permission_kind_label(3), "use your camera");
        assert_eq!(chrome_ui::permission_kind_label(4), "use your microphone");
        assert_eq!(
            chrome_ui::permission_kind_label(5),
            "use your camera and microphone"
        );
        assert_eq!(chrome_ui::permission_kind_site_info_label(3), "camera");
        assert_eq!(chrome_ui::permission_kind_site_info_label(4), "microphone");
        assert_eq!(
            chrome_ui::permission_kind_site_info_label(5),
            "camera and microphone"
        );

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        let mut peer: &UnixStream = &helper;
        let request = mde_web_preview_client::EventMsg::PermissionRequest {
            id: 5,
            kind: 0,
            origin: "https://maps.example".to_owned(),
        };

        peer.write_all(&wire::frame(&request.encode()))
            .expect("req");
        state.tabs[0].session.poll();
        // First time: not granted → a prompt is offered (nothing auto-answered).
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://maps.example".to_owned(), 0))
        );

        // Allow it → remembered, and the pending request is answered + cleared.
        state.answer_active_permission("https://maps.example", 0, true);
        assert!(state.is_permission_granted("https://maps.example", 0));
        assert!(state.tabs[0].session.pending_permission().is_none());

        // A second identical request auto-allows with no prompt.
        peer.write_all(&wire::frame(&request.encode()))
            .expect("req2");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            None,
            "a granted capability auto-allows, no prompt"
        );
        assert!(
            state.tabs[0].session.pending_permission().is_none(),
            "the auto-allow answered and cleared the request"
        );
    }

    #[test]
    fn runtime_permission_decisions_are_audited_and_sent_to_helper() {
        use mde_web_preview_client::{ControlMsg, EventMsg};

        let bus = tempfile::tempdir().expect("temp bus");
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://app.example/dashboard".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("App Dashboard".to_owned()));
        state.tabs[0].session.poll();

        send_permission_request(&helper, 7, 0, "https://maps.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://maps.example".to_owned(), 0))
        );
        state.answer_active_permission("https://maps.example", 0, true);
        assert!(state.is_permission_granted("https://maps.example", 0));

        send_permission_request(&helper, 8, 0, "https://maps.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            None,
            "a remembered grant auto-allows but is still audited"
        );

        send_permission_request(&helper, 9, 2, "https://clip.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://clip.example".to_owned(), 2))
        );
        state.answer_active_permission("https://clip.example", 2, false);
        assert!(!state.is_permission_granted("https://clip.example", 2));

        send_permission_request(&helper, 10, 5, "https://meet.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://meet.example".to_owned(), 5))
        );
        state.answer_active_permission("https://meet.example", 5, true);
        assert!(state.is_permission_granted("https://meet.example", 5));

        let permission_decisions = drain_control_messages(&helper)
            .into_iter()
            .filter(|msg| matches!(msg, ControlMsg::PermissionDecision { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            permission_decisions,
            vec![
                ControlMsg::PermissionDecision { id: 7, allow: true },
                ControlMsg::PermissionDecision { id: 8, allow: true },
                ControlMsg::PermissionDecision {
                    id: 9,
                    allow: false,
                },
                ControlMsg::PermissionDecision {
                    id: 10,
                    allow: true,
                },
            ]
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_PERMISSION_DECISION, None)
            .expect("list permission decision events");
        assert_eq!(msgs.len(), 4);
        let events = msgs
            .iter()
            .map(|msg| {
                serde_json::from_str::<serde_json::Value>(
                    msg.body.as_deref().expect("permission-decision body"),
                )
                .expect("permission-decision JSON")
            })
            .collect::<Vec<_>>();

        assert_eq!(events[0]["op"], "browser_permission_decision");
        assert_eq!(events[0]["permission"], "geolocation");
        assert_eq!(events[0]["permission_kind"], 0);
        assert_eq!(events[0]["decision"], "allow");
        assert_eq!(events[0]["grant_scope"], "session");
        assert_eq!(events[0]["enforcement"], "helper_permission_prompt");
        assert_eq!(events[0]["engine"], "servo");
        assert_eq!(events[0]["origin"], "https://maps.example");
        assert_eq!(events[0]["origin_host"], "maps.example");
        assert_eq!(events[0]["url"], "https://app.example/dashboard");
        assert_eq!(events[0]["title"], "App Dashboard");
        assert_eq!(events[0]["source"], "browser");
        assert_eq!(events[0]["node"], local_hostname());
        assert!(events[0]["decided_ms"].as_u64().is_some());

        assert_eq!(events[1]["permission"], "geolocation");
        assert_eq!(events[1]["decision"], "allow");
        assert_eq!(events[1]["enforcement"], "session_grant_reuse");

        assert_eq!(events[2]["permission"], "clipboard");
        assert_eq!(events[2]["permission_kind"], 2);
        assert_eq!(events[2]["decision"], "deny");
        assert_eq!(events[2]["grant_scope"], "none");
        assert_eq!(events[2]["enforcement"], "helper_permission_prompt");
        assert_eq!(events[2]["origin_host"], "clip.example");

        assert_eq!(events[3]["permission"], "camera_microphone");
        assert_eq!(events[3]["permission_kind"], 5);
        assert_eq!(events[3]["decision"], "allow");
        assert_eq!(events[3]["grant_scope"], "session");
        assert_eq!(events[3]["enforcement"], "helper_permission_prompt");
        assert_eq!(events[3]["origin_host"], "meet.example");
    }

    #[test]
    fn forgetting_site_permissions_revokes_runtime_grants_and_is_audited() {
        use mde_web_preview_client::EventMsg;

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://app.example/dashboard".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("App Dashboard".to_owned()));
        state.tabs[0].session.poll();
        state.grant_permission("https://app.example", 0);
        state.grant_permission("https://other.example", 0);
        state.site_permission_prompts.push(SitePermissionPrompt {
            host: "app.example".to_owned(),
            kind: DevicePermissionKind::Camera,
            decision: "denied",
            updated_ms: 123,
        });
        assert!(state.is_permission_granted("https://app.example", 0));

        state.forget_active_site_permissions();

        assert!(
            !state.is_permission_granted("https://app.example", 0),
            "forgetting this site must revoke its session grant"
        );
        assert!(
            state.is_permission_granted("https://other.example", 0),
            "other-site grants are untouched"
        );
        assert!(
            state
                .active_site_permission_summary()
                .is_some_and(|summary| summary.contains("app.example: forgotten")),
            "the visible permission summary reflects the forgotten state"
        );

        send_permission_request(&helper, 44, 0, "https://app.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://app.example".to_owned(), 0)),
            "a revoked grant must not auto-allow the next request"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_PERMISSION_REVOKE, None)
            .expect("list permission revoke events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("permission revoke body"))
                .expect("permission revoke JSON");
        assert_eq!(event["op"], "browser_permission_revoke");
        assert_eq!(event["decision"], "revoke");
        assert_eq!(event["enforcement"], "session_permission_store");
        assert_eq!(event["permission_policy"], "default_deny");
        assert_eq!(event["scope"], "current_site");
        assert_eq!(event["engine"], "servo");
        assert_eq!(event["url"], "https://app.example/dashboard");
        assert_eq!(event["host"], "app.example");
        assert_eq!(event["title"], "App Dashboard");
        assert_eq!(event["revoked_grants"], 1);
        assert_eq!(event["cleared_prompt_decisions"], 1);
        assert_eq!(event["source"], "browser");
        assert_eq!(event["node"], local_hostname());
        assert!(event["updated_ms"].as_u64().is_some());
    }

    #[test]
    fn session_login_store_matches_by_host_updates_and_removes() {
        let mut state = WebState::default();
        state.save_login("Mail.Example.com", " alice ", "pw1"); // host lowercased, user trimmed
        state.save_login("mail.example.com", "bob", "pw2");
        state.save_login("other.example", "carol", "pw3");
        let m = state.logins_for_host("mail.example.com");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].username, "alice");

        // Re-saving same host+username UPDATES the password (no duplicate).
        state.save_login("mail.example.com", "alice", "pw1-new");
        let m = state.logins_for_host("mail.example.com");
        assert_eq!(m.len(), 2);
        assert_eq!(
            m.iter().find(|l| l.username == "alice").unwrap().password,
            "pw1-new"
        );

        // Blank host/username/password entries are ignored.
        state.save_login("mail.example.com", "", "x");
        state.save_login("", "u", "p");
        state.save_login("mail.example.com", "dave", "");
        assert_eq!(state.logins_for_host("mail.example.com").len(), 2);

        // Remove by index.
        let before = state.session_logins.len();
        state.remove_login(0);
        assert_eq!(state.session_logins.len(), before - 1);
        state.remove_login(999); // out of range is a no-op
        assert_eq!(state.session_logins.len(), before - 1);
    }

    #[test]
    fn stored_login_debug_redacts_session_credentials() {
        let login = StoredLogin {
            host: "mail.example.com".to_owned(),
            username: "alice@example.com".to_owned(),
            password: "hunter2".to_owned(),
        };
        let debug = format!("{login:?}");
        assert!(debug.contains("mail.example.com"));
        assert!(!debug.contains("alice@example.com"));
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn pending_login_save_debug_redacts_session_credentials() {
        let pending = PendingLoginSave {
            tab_id: 7,
            host: "mail.example.com".to_owned(),
            username: "alice@example.com".to_owned(),
            password: "hunter2".to_owned(),
        };
        let debug = format!("{pending:?}");
        assert!(debug.contains("mail.example.com"));
        assert!(debug.contains("tab_id"));
        assert!(!debug.contains("alice@example.com"));
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn filling_a_login_sends_the_credential_to_the_helper() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://mail.example.com/login".to_owned(),
            },
        );
        state.tabs[state.active].session.poll();
        state.fill_active_login(
            "mail.example.com".to_owned(),
            "alice@example.com".to_owned(),
            "hunter2".to_owned(),
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|m| matches!(
                m,
                mde_web_preview_client::ControlMsg::FillLogin { expected_host, username, password }
                    if expected_host == "mail.example.com"
                        && username == "alice@example.com"
                        && password == "hunter2"
            )),
            "fill_active_login sends the chosen credential to the page: {controls:?}"
        );

        state.fill_active_login(
            "other.example".to_owned(),
            "alice@example.com".to_owned(),
            "hunter2".to_owned(),
        );
        assert!(
            drain_control_messages(&helper).is_empty(),
            "a stale host-scoped fill must not leave the shell"
        );
    }

    #[test]
    fn auto_captured_login_prompt_is_scoped_to_the_source_tab() {
        use mde_web_preview_client::EventMsg;

        let (mail_session, mail_helper) = raw_session_pair();
        let (work_session, work_helper) = raw_session_pair();
        let mut state = WebState::default();

        state.push_session(mail_session);
        write_helper_event(
            &mail_helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://mail.example.com/login".to_owned(),
            },
        );
        state.tabs[0].session.poll();
        let mail_tab_id = state.tabs[0].id;

        state.push_session(work_session);
        write_helper_event(
            &work_helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://work.example.com/".to_owned(),
            },
        );
        state.tabs[1].session.poll();
        assert_eq!(state.active, 1, "second push makes the work tab active");

        state.handle_login_capture_from_tab(
            mail_tab_id,
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw"}"#,
        );
        assert!(
            state.pending_login_save.is_some(),
            "the source tab keeps a pending save"
        );
        assert!(
            state.active_pending_login_save().is_none(),
            "the work tab must not show the mail tab's save prompt"
        );

        state.accept_pending_login_save();
        assert!(
            state.logins_for_host("mail.example.com").is_empty(),
            "a stale prompt accept must not save from the wrong active tab"
        );
        assert!(
            state.pending_login_save.is_some(),
            "the prompt is retained for when the user returns to the source tab"
        );

        state.select_tab(0);
        assert!(
            state.active_pending_login_save().is_some(),
            "returning to the source tab re-exposes the save prompt"
        );
        state.accept_pending_login_save();
        assert_eq!(state.logins_for_host("mail.example.com").len(), 1);
        assert!(state.pending_login_save.is_none());
    }

    #[test]
    fn auto_captured_login_origin_must_match_the_source_tab_host() {
        use mde_web_preview_client::EventMsg;

        let (session, helper) = raw_session_pair();
        let mut state = WebState::default();
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://mail.example.com/login".to_owned(),
            },
        );
        state.tabs[0].session.poll();
        let tab_id = state.tabs[0].id;

        state.handle_login_capture_from_tab(
            tab_id,
            "https://forged.example",
            r#"{"username":"alice","password":"pw"}"#,
        );
        assert!(
            state.pending_login_save.is_none(),
            "the shell rejects a capture whose origin host does not match the source tab"
        );

        state.handle_login_capture_from_tab(
            tab_id,
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw"}"#,
        );
        assert_eq!(
            state.pending_login_save.as_ref().map(|p| p.host.as_str()),
            Some("mail.example.com"),
            "matching source-tab origin hosts can still offer to save"
        );
    }

    #[test]
    fn closing_the_source_tab_clears_a_pending_login_save() {
        use mde_web_preview_client::EventMsg;

        let (session, helper) = raw_session_pair();
        let mut state = WebState::default();
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://mail.example.com/login".to_owned(),
            },
        );
        state.tabs[0].session.poll();
        let tab_id = state.tabs[0].id;

        state.handle_login_capture_from_tab(
            tab_id,
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw"}"#,
        );
        assert!(state.pending_login_save.is_some());

        state.close_tab(0);
        assert!(
            state.pending_login_save.is_none(),
            "closing the source tab drops its pending save prompt"
        );
    }

    #[test]
    fn auto_captured_login_offers_to_save_then_dedups() {
        let mut state = WebState::default();
        // A captured submit (engine-beaconed JSON) → a pending save offer.
        state.handle_login_capture(
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw"}"#,
        );
        let pending = state.pending_login_save.clone().expect("a save offer");
        assert_eq!(pending.tab_id, 0);
        assert_eq!(pending.host, "mail.example.com");
        assert_eq!(pending.username, "alice");

        // Accept → save + clear.
        state.accept_pending_login_save();
        assert_eq!(state.logins_for_host("mail.example.com").len(), 1);
        assert!(state.pending_login_save.is_none());

        // The SAME credential again does NOT re-offer (dedup).
        state.handle_login_capture(
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw"}"#,
        );
        assert!(
            state.pending_login_save.is_none(),
            "an already-saved credential does not re-prompt"
        );

        // A CHANGED password DOES re-offer (to update).
        state.handle_login_capture(
            "https://mail.example.com",
            r#"{"username":"alice","password":"pw2"}"#,
        );
        assert!(
            state.pending_login_save.is_some(),
            "a changed password re-offers"
        );

        // Malformed / blank captures are ignored.
        state.pending_login_save = None;
        state.handle_login_capture("https://x", "not json at all");
        state.handle_login_capture("https://x", r#"{"username":"","password":"p"}"#);
        assert!(state.pending_login_save.is_none());

        // A page-supplied origin inside the JSON is ignored; only the engine-supplied
        // event origin is reduced to the host that scopes the stored credential.
        state.handle_login_capture(
            "https://real.example",
            r#"{"origin":"https://forged.example","username":"alice","password":"pw"}"#,
        );
        assert_eq!(
            state.pending_login_save.as_ref().map(|p| p.host.as_str()),
            Some("real.example")
        );
    }

    #[test]
    fn credential_actions_publish_redacted_audit_events() {
        use mde_web_preview_client::EventMsg;

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://mail.example.com/login".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("Mail Login".to_owned()));
        state.tabs[state.active].session.poll();

        state.save_login_with_trigger(
            "mail.example.com",
            "alice@example.com",
            "hunter2",
            "password_menu",
        );
        state.save_login_with_trigger(
            "mail.example.com",
            "alice@example.com",
            "better-secret",
            "auto_capture_prompt",
        );
        state.fill_active_login(
            "mail.example.com".to_owned(),
            "alice@example.com".to_owned(),
            "better-secret".to_owned(),
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|m| matches!(
                m,
                mde_web_preview_client::ControlMsg::FillLogin { expected_host, username, password }
                    if expected_host == "mail.example.com"
                        && username == "alice@example.com"
                        && password == "better-secret"
            )),
            "fill still sends the chosen credential to the helper: {controls:?}"
        );
        state.remove_login(0);
        let tab_id = state.tabs[state.active].id;
        state.pending_login_save = Some(PendingLoginSave {
            tab_id,
            host: "mail.example.com".to_owned(),
            username: "dismiss-user".to_owned(),
            password: "dismiss-secret".to_owned(),
        });
        state.dismiss_pending_login_save();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_CREDENTIAL, None)
            .expect("list credential events");
        assert_eq!(msgs.len(), 5);

        let events = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("credential body");
                assert!(!body.contains("alice@example.com"));
                assert!(!body.contains("hunter2"));
                assert!(!body.contains("better-secret"));
                assert!(!body.contains("dismiss-user"));
                assert!(!body.contains("dismiss-secret"));
                serde_json::from_str::<serde_json::Value>(body).expect("credential JSON")
            })
            .collect::<Vec<_>>();
        let decisions = events
            .iter()
            .map(|event| event["decision"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(decisions, ["save", "update", "fill", "delete", "dismiss"]);
        let counts = events
            .iter()
            .map(|event| event["credential_count"].as_u64().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(counts, [1, 1, 1, 0, 0]);
        let triggers = events
            .iter()
            .map(|event| event["trigger"].as_str().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            triggers,
            [
                "password_menu",
                "auto_capture_prompt",
                "password_menu",
                "password_menu",
                "auto_capture_prompt"
            ]
        );
        for event in events {
            assert_eq!(event["op"], "browser_credential");
            assert_eq!(event["enforcement"], "session_credential_store");
            assert_eq!(event["privacy"], "redacted");
            assert_eq!(event["scope"], "session_only");
            assert_eq!(event["engine"], "servo");
            assert_eq!(event["url"], "https://mail.example.com/login");
            assert_eq!(event["host"], "mail.example.com");
            assert_eq!(event["title"], "Mail Login");
            assert_eq!(event["source"], "browser");
            assert_eq!(event["node"], local_hostname());
            assert!(event["updated_ms"].as_u64().is_some());
            assert!(event.get("username").is_none());
            assert!(event.get("password").is_none());
        }
    }

    #[test]
    fn js_dialog_notice_names_the_engine_auto_resolution() {
        let confirm = JsDialog {
            kind: 1,
            message: "Delete this item?".to_owned(),
            origin: "https://app.example/settings".to_owned(),
        };
        assert_eq!(
            chrome_ui::js_dialog_notice(&confirm),
            "Page confirm from app.example was cancelled: Delete this item?"
        );

        let alert = JsDialog {
            kind: 0,
            message: "Saved".to_owned(),
            origin: String::new(),
        };
        assert_eq!(
            chrome_ui::js_dialog_notice(&alert),
            "Page alert from unknown origin was accepted: Saved"
        );

        let prompt = JsDialog {
            kind: 2,
            message: "   ".to_owned(),
            origin: "not-a-url".to_owned(),
        };
        assert_eq!(
            chrome_ui::js_dialog_notice(&prompt),
            "Page prompt from not-a-url was cancelled: (empty message)"
        );
    }

    #[test]
    fn js_dialog_events_surface_as_browser_notices() {
        let (session, helper) = raw_session_pair();
        let mut state = WebState::default();
        state.push_session(session);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::JsDialog {
                kind: 1,
                message: "Delete this item?".to_owned(),
                origin: "https://app.example/settings".to_owned(),
            },
        );

        assert!(run_panel(&mut state));

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Page confirm from app.example was cancelled: Delete this item?")
        );
        assert!(
            state.tabs[state.active]
                .session
                .drain_js_dialog_events()
                .is_empty(),
            "web_panel drains JS dialog notices exactly once"
        );
    }

    #[test]
    fn before_unload_prompt_text_names_the_action_and_origin() {
        let leave = BeforeUnloadDialog {
            id: 1,
            message: "You have unsaved changes".to_owned(),
            origin: "https://editor.example/doc/1".to_owned(),
            is_reload: false,
        };
        assert_eq!(
            chrome_ui::before_unload_prompt_text(&leave),
            "editor.example wants to leave this page: You have unsaved changes"
        );
        assert_eq!(chrome_ui::before_unload_primary_label(&leave), "Leave");

        let reload = BeforeUnloadDialog {
            id: 2,
            message: "   ".to_owned(),
            origin: String::new(),
            is_reload: true,
        };
        assert_eq!(
            chrome_ui::before_unload_prompt_text(&reload),
            "unknown origin wants to reload this page: (empty message)"
        );
        assert_eq!(chrome_ui::before_unload_primary_label(&reload), "Reload");
    }

    #[test]
    fn before_unload_prompt_answers_the_active_session() {
        let (session, helper) = raw_session_pair();
        helper.set_nonblocking(true).expect("nonblocking helper");
        let mut state = WebState::default();
        state.push_session(session);
        assert_eq!(
            drain_control_messages(&helper),
            vec![mde_web_preview_client::ControlMsg::SetAutoplayBlocked { blocked: true }]
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::BeforeUnloadDialog {
                id: 7,
                message: "Draft changed".to_owned(),
                origin: "https://editor.example/doc/1".to_owned(),
                is_reload: false,
            },
        );

        assert!(run_panel(&mut state));
        assert_eq!(
            state.pending_before_unload_prompt().map(|prompt| prompt.id),
            Some(7)
        );

        state.answer_active_before_unload(7, true);
        assert_eq!(
            drain_control_messages(&helper),
            vec![mde_web_preview_client::ControlMsg::BeforeUnloadDecision {
                id: 7,
                proceed: true,
            }]
        );
        assert!(
            state.pending_before_unload_prompt().is_none(),
            "answering clears the prompt"
        );
    }

    // ----------------------------------------------------------------------
    // Adversarial stress tests (2026-07-13): try to BREAK the pinned-cluster,
    // active-session-tracking, close-set, duplicate, and permission
    // invariants. Each tab-op test asserts the ACTIVE session's *identity*
    // (its container tag) is unchanged and the pinned-first invariant holds.
    // ----------------------------------------------------------------------

    /// The pinned-first invariant: no unpinned tab may precede any pinned tab.
    fn assert_pinned_first(state: &WebState) {
        let boundary = state.tabs.iter().take_while(|t| t.pinned).count();
        assert!(
            state.tabs.iter().skip(boundary).all(|t| !t.pinned),
            "pinned tabs must all precede unpinned tabs, got {:?}",
            state
                .tabs
                .iter()
                .map(|t| (t.container, t.pinned))
                .collect::<Vec<_>>()
        );
    }

    /// The container tag of whatever session is currently active.
    fn active_container(state: &WebState) -> ContainerProfile {
        state.tabs[state.active].container
    }

    /// A `WebState` with one *live-peer* session plus the retained peer end (so
    /// the session never crash-detects). Frames written to the peer are visible
    /// after `state.tabs[0].session.poll()`.
    fn live_single_tab() -> (WebState, std::os::unix::net::UnixStream) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        (state, helper)
    }

    /// Push a `PermissionRequest` event onto the helper wire.
    fn send_permission_request(peer: &UnixStream, id: u64, kind: u8, origin: &str) {
        let mut p: &UnixStream = peer;
        p.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::PermissionRequest {
                id,
                kind,
                origin: origin.to_owned(),
            }
            .encode(),
        ))
        .expect("permission request frame");
    }

    // --- Target 1: sort_pinned_stable / set_tab_pinned ---

    #[test]
    fn pinning_the_active_tab_keeps_it_active_at_the_front() {
        let mut state = tagged_tabs(4); // [Personal, Work, Banking, Research]
        state.select_tab(1); // active = Work
        assert_eq!(active_container(&state), ContainerProfile::Work);
        state.set_tab_pinned(1, true); // pin the ACTIVE (Work) tab
        assert_pinned_first(&state);
        assert!(state.tabs[0].pinned && state.tabs[0].container == ContainerProfile::Work);
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "the active session is still Work after it reclustered to the front"
        );
    }

    #[test]
    fn pinning_last_then_first_preserves_active_identity() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(2); // active = Banking
        state.set_tab_pinned(3, true); // pin Research (last) -> reclusters
        assert_pinned_first(&state);
        assert_eq!(active_container(&state), ContainerProfile::Banking);
        // Then pin whatever unpinned tab now leads the tail (Personal).
        let personal = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Personal)
            .expect("personal");
        state.set_tab_pinned(personal, true);
        assert_pinned_first(&state);
        assert_eq!(
            active_container(&state),
            ContainerProfile::Banking,
            "active stays Banking through two pins that reordered the strip",
        );
    }

    #[test]
    fn unpinning_the_middle_of_three_pinned_keeps_the_invariant() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true);
        state.set_tab_pinned(2, true); // pinned [P, W, B], unpinned [R]
        state.select_tab(3); // active = Research (unpinned)
        state.set_tab_pinned(1, false); // unpin the MIDDLE pinned tab (Work)
        assert_pinned_first(&state);
        // Work rejoins the FRONT of the unpinned cluster, ahead of Research.
        let work = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Work)
            .expect("work");
        let research = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Research)
            .expect("research");
        assert!(!state.tabs[work].pinned && work < research);
        assert_eq!(active_container(&state), ContainerProfile::Research);
    }

    #[test]
    fn pinning_all_in_place_then_unpinning_all_tracks_active() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(1); // active = Work
        for i in 0..4 {
            state.set_tab_pinned(i, true);
        }
        assert!(state.tabs.iter().all(|t| t.pinned));
        // Pinning an already-front-clustered strip must NOT reorder it.
        assert_eq!(
            state.tabs.iter().map(|t| t.container).collect::<Vec<_>>(),
            vec![
                ContainerProfile::Personal,
                ContainerProfile::Work,
                ContainerProfile::Banking,
                ContainerProfile::Research,
            ],
        );
        assert_eq!(active_container(&state), ContainerProfile::Work);
        // Unpin every tab (drain the pinned cluster from the front).
        while let Some(i) = state.tabs.iter().position(|t| t.pinned) {
            state.set_tab_pinned(i, false);
        }
        assert!(state.tabs.iter().all(|t| !t.pinned));
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "active stays Work across pin-all then unpin-all",
        );
    }

    // --- Target 2: move_tab with pinned tabs ---

    #[test]
    fn dragging_an_unpinned_tab_to_the_front_snaps_behind_the_pins() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        state.select_tab(3); // active = Research
        state.move_tab(3, 0); // drag Research to the very front — can't leap pins
        assert_pinned_first(&state);
        assert!(
            state.tabs[0].pinned && state.tabs[1].pinned,
            "both pins still lead the strip"
        );
        assert_eq!(
            active_container(&state),
            ContainerProfile::Research,
            "the dragged Research tab stays the active session",
        );
        assert_eq!(
            state.tabs[2].container,
            ContainerProfile::Research,
            "Research landed at the FRONT of the unpinned cluster",
        );
    }

    #[test]
    fn dragging_a_pinned_tab_to_the_end_snaps_back_to_the_front() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true); // Personal pinned at the front
        state.select_tab(0); // active = the pinned Personal tab
        state.move_tab(0, 3); // drag the pinned tab to the very end
        assert_pinned_first(&state);
        assert!(state.tabs[0].pinned && state.tabs[0].container == ContainerProfile::Personal);
        assert_eq!(active_container(&state), ContainerProfile::Personal);
    }

    #[test]
    fn moving_a_tab_across_the_active_index_preserves_the_active_session() {
        // Right-of-active to left-of-active (crosses active).
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.select_tab(1); // active = Work
        state.move_tab(3, 0); // Research jumps to the front, crossing Work
        assert_eq!(active_container(&state), ContainerProfile::Work);
        // The mirror: left-of-active to right-of-active.
        let mut state = tagged_tabs(4);
        state.select_tab(2); // active = Banking
        state.move_tab(0, 3); // Personal moves to the end, crossing Banking
        assert_eq!(active_container(&state), ContainerProfile::Banking);
    }

    // --- Target 3: close_other_tabs(keep) ---

    #[test]
    fn close_other_tabs_spares_pins_on_both_sides_of_the_kept_tab() {
        // Deliberately construct a NON-front-clustered pin layout (pins straddling
        // an unpinned tab) to stress the right-to-left index math directly.
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.tabs[0].pinned = true; // Personal pinned (before keep)
        state.tabs[2].pinned = true; // Banking pinned (after keep)
        state.close_other_tabs(1); // keep the unpinned Work tab at index 1
        assert_eq!(state.tabs.len(), 3);
        let survivors: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        assert!(survivors.contains(&ContainerProfile::Personal));
        assert!(survivors.contains(&ContainerProfile::Work));
        assert!(survivors.contains(&ContainerProfile::Banking));
        assert!(!survivors.contains(&ContainerProfile::Research));
        assert_eq!(
            active_container(&state),
            ContainerProfile::Work,
            "the explicitly-kept tab ends active even with pins on both sides",
        );
    }

    #[test]
    fn close_other_tabs_keeping_a_pinned_tab_survives_all_pins() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        let work = state
            .tabs
            .iter()
            .position(|t| t.container == ContainerProfile::Work)
            .expect("work");
        state.close_other_tabs(work); // keep a PINNED tab
        assert_eq!(state.tabs.len(), 2);
        assert!(state.tabs.iter().all(|t| t.pinned));
        assert_eq!(active_container(&state), ContainerProfile::Work);
    }

    #[test]
    fn close_other_tabs_keep_first_and_keep_last_leave_one_tab() {
        let mut state = tagged_tabs(4);
        state.close_other_tabs(0); // keep index 0 (Personal)
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(active_container(&state), ContainerProfile::Personal);

        let mut state = tagged_tabs(4);
        state.close_other_tabs(3); // keep the last (Research)
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(active_container(&state), ContainerProfile::Research);
    }

    // --- Target 4: close_tabs_to_the_right(from) ---

    #[test]
    fn close_tabs_to_the_right_spares_a_pin_in_the_middle() {
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.tabs[1].pinned = true; // Work pinned at index 1 (non-front layout)
        state.close_tabs_to_the_right(0);
        let survivors: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        assert!(survivors.contains(&ContainerProfile::Personal)); // index 0, untouched
        assert!(survivors.contains(&ContainerProfile::Work)); // pinned → spared
        assert!(!survivors.contains(&ContainerProfile::Banking));
        assert!(!survivors.contains(&ContainerProfile::Research));
    }

    #[test]
    fn close_tabs_to_the_right_from_boundary_and_noop_cases() {
        // From past the pinned boundary: only the unpinned tail to the right closes.
        let mut state = tagged_tabs(4); // [P, W, B, R]
        state.set_tab_pinned(0, true);
        state.set_tab_pinned(1, true); // pinned [P, W], unpinned [B, R]
        state.close_tabs_to_the_right(2); // from Banking (first unpinned)
        assert_eq!(state.tabs.len(), 3);
        assert!(!state
            .tabs
            .iter()
            .any(|t| t.container == ContainerProfile::Research));

        // from == last index is a no-op.
        let mut state = tagged_tabs(4);
        state.close_tabs_to_the_right(3);
        assert_eq!(state.tabs.len(), 4);

        // from past the end is a no-op (early return, no panic).
        let mut state = tagged_tabs(4);
        state.close_tabs_to_the_right(99);
        assert_eq!(state.tabs.len(), 4);
    }

    // --- Target 5: duplicate_tab ---

    #[test]
    fn duplicating_enqueues_at_the_back_of_the_open_queue() {
        let (mut state, peer) = live_single_tab();
        // A prior queued open, so we can prove duplicate lands at the BACK.
        let engine = state.engine;
        state.request_new_tab(engine);
        let mut p: &UnixStream = &peer;
        p.write_all(&wire::frame(
            &mde_web_preview_client::EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://dup.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.back(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://dup.example/"
            ),
            "duplicate lands a same-URL open at the BACK of the queue, got {:?}",
            state.open_requested.back(),
        );
    }

    #[test]
    fn duplicating_a_blank_tab_enqueues_a_blank_new_tab() {
        let (mut state, _peer) = live_single_tab();
        // A fresh session has no committed URL yet.
        assert!(state.tabs[0].session.nav().url.trim().is_empty());
        state.duplicate_tab(0);
        assert!(
            matches!(
                state.open_requested.back(),
                Some(TabOpenIntent::NewForeground(_))
            ),
            "a blank tab duplicates to a blank foreground tab, got {:?}",
            state.open_requested.back(),
        );
    }

    // --- Target 6: permission grant/answer/prompt ---

    #[test]
    fn a_grant_does_not_auto_allow_a_different_capability_kind() {
        let (mut state, peer) = live_single_tab();
        // Grant (origin A, kind 0 = geolocation).
        send_permission_request(&peer, 1, 0, "https://a.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://a.example".to_owned(), 0))
        );
        state.answer_active_permission("https://a.example", 0, true);
        assert!(state.is_permission_granted("https://a.example", 0));

        // Same origin, DIFFERENT kind (1 = notifications) must STILL prompt.
        send_permission_request(&peer, 2, 1, "https://a.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://a.example".to_owned(), 1)),
            "a geolocation grant must not silently allow notifications",
        );
        assert!(
            state.tabs[0].session.pending_permission().is_some(),
            "the different-kind request was NOT auto-answered",
        );
    }

    #[test]
    fn a_grant_does_not_auto_allow_a_different_origin() {
        let (mut state, peer) = live_single_tab();
        send_permission_request(&peer, 1, 0, "https://a.example");
        state.tabs[0].session.poll();
        state.answer_active_permission("https://a.example", 0, true);
        assert!(state.is_permission_granted("https://a.example", 0));

        // DIFFERENT origin, SAME kind must still prompt.
        send_permission_request(&peer, 2, 0, "https://b.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://b.example".to_owned(), 0)),
            "a grant to a.example must not silently allow b.example",
        );
    }

    #[test]
    fn a_blocked_capability_is_not_remembered_and_reprompts() {
        let (mut state, peer) = live_single_tab();
        send_permission_request(&peer, 1, 2, "https://c.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://c.example".to_owned(), 2))
        );
        // BLOCK it.
        state.answer_active_permission("https://c.example", 2, false);
        assert!(
            !state.is_permission_granted("https://c.example", 2),
            "a block is not a grant",
        );
        assert!(
            state.tabs[0].session.pending_permission().is_none(),
            "the block answered and cleared the request",
        );

        // The very same request must prompt AGAIN (blocks are not sticky).
        send_permission_request(&peer, 2, 2, "https://c.example");
        state.tabs[0].session.poll();
        assert_eq!(
            state.pending_permission_prompt(),
            Some(("https://c.example".to_owned(), 2)),
            "a previously-blocked capability re-prompts",
        );
    }

    // --- Robustness: out-of-range tab ops must be safe no-ops ---

    #[test]
    fn out_of_range_tab_ops_are_no_ops() {
        let mut state = tagged_tabs(2); // [Personal, Work]
        let before: Vec<_> = state.tabs.iter().map(|t| t.container).collect();
        state.set_tab_pinned(9, true);
        state.move_tab(9, 0);
        state.move_tab(0, 9);
        state.move_tab(1, 1); // from == to
        state.close_tabs_to_the_right(9);
        state.close_other_tabs(9);
        state.duplicate_tab(9);
        assert_eq!(
            state.tabs.iter().map(|t| t.container).collect::<Vec<_>>(),
            before,
        );
        assert!(
            state.open_requested.is_empty(),
            "an out-of-range duplicate enqueues nothing",
        );
        assert!(state.tabs.iter().all(|t| !t.pinned));
    }

    /// Drive ONE headless frame of just the tab strip (isolating it from the full
    /// panel's polling), mirroring `middle_clicking_a_tab_pill_closes_that_tab`.
    fn run_tab_strip_frame(ctx: &egui::Context, state: &mut WebState, input: egui::RawInput) {
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| chrome_ui::tab_strip(ui, state));
        });
    }

    /// The laid-out pill centres the strip stashed on its last frame, in tab order.
    fn tab_pill_centers(ctx: &egui::Context) -> Vec<egui::Pos2> {
        ctx.data(|d| d.get_temp::<Vec<Rect>>(chrome_ui::tab_pill_rects_id()))
            .unwrap_or_default()
            .iter()
            .map(|r| r.center())
            .collect()
    }

    /// Press on `from`, drag past egui's click threshold to `to`, then release —
    /// a real pointer drag-reorder gesture routed through the tab strip.
    fn drag_pill(ctx: &egui::Context, state: &mut WebState, from: egui::Pos2, to: egui::Pos2) {
        let mut press = body_input();
        press.events = vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(ctx, state, press);

        let mut moved = body_input();
        moved.events = vec![egui::Event::PointerMoved(to)];
        run_tab_strip_frame(ctx, state, moved);

        let mut release = body_input();
        release.events = vec![egui::Event::PointerButton {
            pos: to,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        }];
        run_tab_strip_frame(ctx, state, release);
    }

    #[test]
    fn tab_drag_target_index_picks_the_nearest_slot_center() {
        let pills = vec![
            (0, Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 20.0))),
            (1, Rect::from_min_size(pos2(100.0, 0.0), vec2(100.0, 20.0))),
            (2, Rect::from_min_size(pos2(200.0, 0.0), vec2(100.0, 20.0))),
        ];
        // Horizontal centres sit at x = 50, 150, 250.
        assert_eq!(
            chrome_ui::tab_drag_target_index(
                &pills,
                pos2(260.0, 10.0),
                chrome_ui::TabAxis::Horizontal
            ),
            Some(2)
        );
        assert_eq!(
            chrome_ui::tab_drag_target_index(
                &pills,
                pos2(160.0, 10.0),
                chrome_ui::TabAxis::Horizontal
            ),
            Some(1)
        );
        assert_eq!(
            chrome_ui::tab_drag_target_index(
                &pills,
                pos2(40.0, 10.0),
                chrome_ui::TabAxis::Horizontal
            ),
            Some(0)
        );
        // The vertical axis compares Y instead of X.
        let stacked = vec![
            (0, Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 20.0))),
            (1, Rect::from_min_size(pos2(0.0, 20.0), vec2(100.0, 20.0))),
        ];
        assert_eq!(
            chrome_ui::tab_drag_target_index(
                &stacked,
                pos2(50.0, 38.0),
                chrome_ui::TabAxis::Vertical
            ),
            Some(1)
        );
        assert_eq!(
            chrome_ui::tab_drag_target_index(&[], pos2(0.0, 0.0), chrome_ui::TabAxis::Horizontal),
            None
        );
    }

    #[test]
    fn dragging_a_tab_pill_reorders_it_and_keeps_the_active_tab() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        // Distinct per-tab markers so we can follow both the dragged tab and the
        // active session across the reorder (testkit titles are identical).
        state.tabs[0].force_dark = true; // the tab we drag
        state.tabs[2].reader_mode = true; // the active session
        state.select_tab(2);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Settle one frame so the strip publishes its pill rects.
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3, "three pills laid out");

        // Drag tab 0 onto tab 1's slot.
        drag_pill(&ctx, &mut state, centers[0], centers[1]);

        // The dragged tab moved from slot 0 to slot 1 ...
        assert!(state.tabs[1].force_dark, "the dragged tab landed in slot 1");
        assert!(!state.tabs[0].force_dark);
        // ... and the SAME session stays active (a reorder below the active index
        // leaves the active index in place, still pointing at tab C).
        assert_eq!(
            state.active, 2,
            "reorder below the active index leaves it put"
        );
        assert!(
            state.tabs[state.active].reader_mode,
            "the active tab is still the same session after the reorder"
        );
    }

    #[test]
    fn dragging_a_tab_across_the_active_index_adjusts_active() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        state.tabs[1].reader_mode = true; // follow the active session (B)
        state.select_tab(1);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3);

        // Drag tab 0 (A) past the active tab to the last slot.
        drag_pill(&ctx, &mut state, centers[0], centers[2]);

        // A moved to the end; B slid one slot left but is STILL the active session.
        assert_eq!(
            state.active, 0,
            "active index follows its session across the reorder"
        );
        assert!(
            state.tabs[state.active].reader_mode,
            "the same session (B) stays active after crossing the active index"
        );
    }

    #[test]
    fn a_tiny_pointer_move_on_a_pill_activates_instead_of_reordering() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(a);
        state.push_session(b);
        state.tabs[0].force_dark = true; // marker proving the order is unchanged
        assert_eq!(state.active, 1, "pushing two tabs leaves the second active");

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        let from = centers[0];
        // A jitter well under egui's 6pt click threshold — must read as a CLICK.
        let nudged = from + vec2(2.0, 0.0);

        let mut press = body_input();
        press.events = vec![
            egui::Event::PointerMoved(from),
            egui::Event::PointerButton {
                pos: from,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(&ctx, &mut state, press);

        let mut release = body_input();
        release.events = vec![
            egui::Event::PointerMoved(nudged),
            egui::Event::PointerButton {
                pos: nudged,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        run_tab_strip_frame(&ctx, &mut state, release);

        assert_eq!(
            state.active, 0,
            "a sub-threshold move is a click, so tab 0 activates"
        );
        assert!(
            state.tabs[0].force_dark,
            "a click must NOT reorder — the order is untouched"
        );
        assert_eq!(state.tabs.len(), 2, "and nothing was closed");
    }

    #[test]
    fn dragging_a_tab_reorders_in_the_vertical_strip() {
        let (a, _ha) = testkit::connect().expect("connect a");
        let (b, _hb) = testkit::connect().expect("connect b");
        let (c, _hc) = testkit::connect().expect("connect c");
        let mut state = WebState::default();
        state.push_session(a);
        state.push_session(b);
        state.push_session(c);
        state.set_vertical_tabs(true);
        state.tabs[0].force_dark = true; // follow the dragged (and active) tab
        state.select_tab(0);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        run_tab_strip_frame(&ctx, &mut state, body_input());
        let centers = tab_pill_centers(&ctx);
        assert_eq!(centers.len(), 3, "three stacked pills laid out");

        // Drag the top pill DOWN onto the bottom slot (matched along Y).
        drag_pill(&ctx, &mut state, centers[0], centers[2]);

        assert!(
            state.tabs[2].force_dark,
            "the vertical drag moved the pill to the bottom slot"
        );
        assert_eq!(
            state.active, 2,
            "the dragged tab was active and its index followed the move"
        );
    }

    #[test]
    fn horizontal_tab_pills_shrink_to_a_floor_then_scroll_instead_of_wrapping() {
        // A roomy strip with few tabs keeps full-width pills.
        assert_eq!(
            chrome_ui::horizontal_tab_pill_width(1200.0, 2),
            CHROME_TAB_W
        );
        // A crowded strip shrinks pills to the floor (never below), so the strip
        // scrolls in ONE row instead of stacking onto a second row.
        assert_eq!(
            chrome_ui::horizontal_tab_pill_width(1200.0, 40),
            CHROME_TAB_MIN_W
        );
        // The floor holds even in an absurdly narrow strip.
        assert!(chrome_ui::horizontal_tab_pill_width(40.0, 40) >= CHROME_TAB_MIN_W);
        // More tabs never widen a pill.
        assert!(
            chrome_ui::horizontal_tab_pill_width(1200.0, 20)
                <= chrome_ui::horizontal_tab_pill_width(1200.0, 4)
        );
    }

    #[test]
    fn many_tabs_stay_on_one_scrolling_row_and_the_active_tab_stays_reachable() {
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        let mut _helpers = Vec::new();
        for _ in 0..20 {
            let (s, h) = testkit::connect().expect("connect");
            state.push_session(s);
            _helpers.push(h);
        }
        assert_eq!(state.tabs.len(), 20);

        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Measure the vertical space the strip consumes: a single scrolling row is
        // ~one tab tall (plus a scrollbar), NOT the many rows the old wrap made.
        let mut used_h = 0.0f32;
        let _ = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let top = ui.next_widget_position().y;
                chrome_ui::tab_strip(ui, &mut state);
                used_h = ui.next_widget_position().y - top;
            });
        });
        assert!(
            used_h < CHROME_TAB_H * 3.0,
            "20 tabs must stay on ONE scrolling row (strip height {used_h})"
        );

        // The far tab is still selectable — it scrolls into view and renders.
        state.select_tab(19);
        assert_eq!(state.active, 19);
        assert!(
            run_panel(&mut state),
            "the active far tab renders in the single scrolling row"
        );
    }

    #[test]
    fn horizontal_tabs_page_body_stays_within_the_visible_workspace() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let ctx = egui::Context::default();
        Style::install(&ctx);
        let rect = run_panel_page_image_rect(&ctx, &mut state, body_input())
            .expect("horizontal Browser body should paint the page texture");

        assert!(
            rect.left() >= -0.5 && rect.right() <= 960.5 && rect.width() > 900.0,
            "horizontal Browser body must not paint off the visible right edge: {rect:?}"
        );
        assert!(
            rect.top() >= 0.0 && rect.bottom() <= 640.5,
            "horizontal Browser body must remain inside the visible panel: {rect:?}"
        );
        assert!(
            rect.height() > 520.0,
            "horizontal Browser body should use the remaining workspace, not only the top slice: {rect:?}"
        );

        let shell_ctx = egui::Context::default();
        Style::install(&shell_ctx);
        let shell_rect = run_panel_page_image_rect_with_reserved_shell_chrome(
            &shell_ctx,
            &mut state,
            body_input(),
            48.0,
            48.0,
        )
        .expect("horizontal Browser body should paint inside reserved shell chrome");
        assert!(
            shell_rect.left() >= 47.5
                && shell_rect.right() <= 960.5
                && shell_rect.bottom() <= 592.5,
            "horizontal Browser body must stay inside the central workspace after shell gutters/struts: {shell_rect:?}"
        );
        assert!(
            shell_rect.width() > 840.0 && shell_rect.height() > 470.0,
            "horizontal Browser body should use the remaining central workspace, not collapse to a top slice: {shell_rect:?}"
        );

        let frame_size = state.tabs[state.active]
            .last_frame
            .as_ref()
            .expect("painted frame")
            .size;
        let right_edge_click = egui::Event::PointerButton {
            pos: pos2(rect.right() - 0.5, rect.center().y),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        };
        let Some(egui::Event::PointerButton { pos, .. }) =
            browser_input_event(&right_edge_click, rect, frame_size, true, false)
        else {
            panic!("right-edge Browser click should forward into the page");
        };
        assert!(
            pos.x >= frame_size[0] as f32 - 1.0,
            "visible right edge must map to the helper frame edge, got {pos:?} of {frame_size:?}"
        );
    }

    #[test]
    fn vertical_tabs_page_body_stays_bounded_and_uses_remaining_workspace() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.set_vertical_tabs(true);
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let ctx = egui::Context::default();
        Style::install(&ctx);
        let rect = run_panel_page_image_rect(&ctx, &mut state, body_input())
            .expect("vertical Browser body should paint the page texture");

        assert!(
            rect.left() >= 0.0 && rect.right() <= 960.5,
            "vertical Browser body must remain inside the visible width: {rect:?}"
        );
        assert!(
            rect.width() > 700.0,
            "vertical Browser body should use the workspace to the right of the tab rail: {rect:?}"
        );
        assert!(
            rect.top() >= 0.0 && rect.bottom() <= 640.5 && rect.height() > 560.0,
            "vertical Browser body must use the visible height below compact chrome: {rect:?}"
        );
    }

    #[test]
    fn live_browser_page_requests_idle_repaint_heartbeat() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        let ctx = egui::Context::default();
        Style::install(&ctx);
        let out = run_panel_output(&ctx, &mut state, body_input());
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        assert!(
            repaint_delay <= LIVE_PAGE_REPAINT_INTERVAL,
            "active live Browser pages must keep polling frames without mouse input (delay {repaint_delay:?})"
        );
    }

    #[test]
    fn closing_the_last_tab_returns_to_the_honest_empty_state() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.close_tab(0);
        assert!(state.tabs.is_empty());
        assert_eq!(state.active, 0);
        assert!(state.address.is_empty());
        assert!(run_panel(&mut state), "empty browser state draws honestly");
    }

    #[test]
    fn tab_strip_renders_with_multiple_tabs() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        assert!(run_panel(&mut state), "tab strip produced no primitives");
    }

    #[test]
    fn vertical_tab_strip_renders_with_the_same_sessions() {
        let (first, _helper1) = testkit::connect().expect("connect 1");
        let (second, _helper2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.set_vertical_tabs(true);
        assert!(
            run_panel(&mut state),
            "vertical tabs produced no primitives"
        );
        assert_eq!(state.tabs.len(), 2);
        assert!(state.vertical_tabs);
    }

    #[test]
    fn tab_strip_uses_compact_new_tab_without_engine_selector() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| chrome_ui::tab_strip(ui, &mut state));
        });
        let texts: Vec<String> = painted_text(&out.shapes)
            .into_iter()
            .map(|(text, _)| text)
            .collect();

        assert!(
            !texts.iter().any(|text| text == "CEF" || text == "Servo"),
            "tab strip should not render the old engine selector segments: {texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.contains("Chromium")),
            "CEF/Chromium detail now belongs in Options, not the tab strip: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|text| text == "Default" || text.contains("tabs") || text == "New tab"),
            "future-engine state and new-tab command belong outside the tab strip: {texts:?}"
        );

        assert!(
            chrome_ui::chrome_icon_painted_shape_count(chrome_ui::ChromeIcon::NewTab) > 0,
            "new-tab toolbar control is a painted icon button"
        );
    }

    #[test]
    fn tab_labels_and_hover_cards_name_each_tabs_engine() {
        let (cef, _cef_helper) = testkit::connect().expect("connect CEF");
        let (servo, _servo_helper) = testkit::connect().expect("connect Servo");
        let mut state = WebState::default();
        state.push_session_with_engine(cef, BrowserEngine::Cef);
        state.push_session_with_engine(servo, BrowserEngine::Servo);

        assert!(
            chrome_ui::engine_marker(state.tabs[0].engine) == "CEF",
            "CEF/Chromium tabs should carry a compact CEF badge marker"
        );
        assert!(
            chrome_ui::tab_hover(&state.tabs[0]).contains("Engine: Chromium"),
            "CEF-backed hover card should name the user-facing engine"
        );
        assert!(
            !chrome_ui::tab_hover(&state.tabs[0]).contains("CEF / Chromium"),
            "CEF-backed hover card should not expose implementation pairing"
        );
        assert!(
            chrome_ui::engine_marker(state.tabs[1].engine) == "Servo",
            "Servo tabs should carry a readable Servo badge marker"
        );
        assert!(
            chrome_ui::tab_hover(&state.tabs[1]).contains("Engine: Lightweight"),
            "Servo-backed hover card should use the user-facing engine label"
        );
        assert!(
            !chrome_ui::tab_label(&state.tabs[0]).contains("CEF"),
            "the tab title should not repeat the engine now that the badge owns it"
        );
    }

    #[test]
    fn a_well_formed_favicon_png_decodes_to_a_texture() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::RED;
        img.pixels[3] = egui::Color32::BLUE;
        let png = encode_color_image_png(&img).expect("encode a tiny favicon PNG");

        let decoded =
            crate::chooser::decode_png_rgba(&png).expect("a well-formed favicon PNG decodes");
        assert_eq!(decoded.size, [2, 2]);
    }

    #[test]
    fn garbage_favicon_bytes_fail_soft_instead_of_panicking() {
        assert!(
            crate::chooser::decode_png_rgba(b"not a png").is_none(),
            "malformed bytes decode to None, never a panic"
        );

        let mut state = WebState::default();
        state.push_session(session_with_favicon(b"not a png"));

        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(
            chrome_ui::tab_favicon_texture(&ctx, &mut state.tabs[0]).is_none(),
            "a garbage favicon resolves to no texture, falling back to the pill's own glyph"
        );
        // The failed decode is still cached (fingerprint recorded, texture None) so
        // the same garbage bytes aren't re-decoded every frame.
        let cache = state.tabs[0]
            .favicon_cache
            .as_ref()
            .expect("a decode attempt is cached even on failure");
        assert!(cache.texture.is_none());
    }

    #[test]
    fn unchanged_favicon_bytes_reuse_the_cached_texture_and_changed_bytes_invalidate_it() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::RED;
        let png_a = encode_color_image_png(&img).expect("encode favicon A");
        img.pixels[0] = egui::Color32::BLUE;
        let png_b = encode_color_image_png(&img).expect("encode favicon B");

        let (mut session, peer) = raw_session_pair();
        send_favicon(&peer, &png_a);
        session.poll();

        let mut state = WebState::default();
        state.push_session(session);

        let ctx = egui::Context::default();
        Style::install(&ctx);

        let first =
            chrome_ui::tab_favicon_texture(&ctx, &mut state.tabs[0]).expect("favicon A decodes");
        let second = chrome_ui::tab_favicon_texture(&ctx, &mut state.tabs[0])
            .expect("favicon A decodes again");
        assert_eq!(
            first.id(),
            second.id(),
            "the same favicon bytes must reuse the cached texture, not re-decode"
        );

        // A genuinely new favicon (different bytes) invalidates the cache and gets
        // its own fresh texture — proving the fingerprint gates on content, not a
        // permanent "decoded once, ever" latch.
        send_favicon(&peer, &png_b);
        state.tabs[0].session.poll();
        let third =
            chrome_ui::tab_favicon_texture(&ctx, &mut state.tabs[0]).expect("favicon B decodes");
        assert_ne!(
            second.id(),
            third.id(),
            "changed favicon bytes must decode a fresh texture"
        );
    }

    #[test]
    fn horizontal_tab_strip_renders_a_favicon_without_panicking() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::GREEN;
        let png = encode_color_image_png(&img).expect("encode favicon");

        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(session_with_favicon(&png));
        assert!(
            run_panel(&mut state),
            "the horizontal tab strip with a favicon produced no primitives"
        );
        assert!(
            state.tabs[0]
                .favicon_cache
                .as_ref()
                .is_some_and(|cache| cache.texture.is_some()),
            "the frame should have decoded + cached the favicon texture"
        );
    }

    #[test]
    fn vertical_tab_strip_renders_a_favicon_without_panicking() {
        let mut img = egui::ColorImage::new([2, 2], egui::Color32::TRANSPARENT);
        img.pixels[0] = egui::Color32::GREEN;
        let png = encode_color_image_png(&img).expect("encode favicon");

        let mut state = WebState::default();
        state.push_session(session_with_favicon(&png));
        state.set_vertical_tabs(true);
        assert!(
            run_panel(&mut state),
            "the vertical tab strip with a favicon produced no primitives"
        );
        assert!(
            state.tabs[0]
                .favicon_cache
                .as_ref()
                .is_some_and(|cache| cache.texture.is_some()),
            "the frame should have decoded + cached the favicon texture"
        );
    }

    #[test]
    fn ctrl_tab_cycles_tabs_and_ctrl_digits_jump_to_them() {
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let (third, _h3) = testkit::connect().expect("connect 3");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        state.push_session(third);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert_eq!(state.active, 2, "the newest pushed tab starts foreground");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_eq!(state.active, 0, "Ctrl+Tab wraps forward to the first tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_eq!(state.active, 1, "Ctrl+Tab advances to the next tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, true)
        ));
        assert_eq!(state.active, 0, "Ctrl+Shift+Tab cycles backwards");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, true)
        ));
        assert_eq!(state.active, 2, "Ctrl+Shift+Tab wraps back to the last tab");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num1, false)
        ));
        assert_eq!(state.active, 0, "Ctrl+1 activates the first tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num3, false)
        ));
        assert_eq!(state.active, 2, "Ctrl+3 activates the third tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num1, false)
        ));
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num9, false)
        ));
        assert_eq!(state.active, 2, "Ctrl+9 activates the LAST tab");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Num5, false)
        ));
        assert_eq!(state.active, 2, "an out-of-range Ctrl+digit is ignored");
    }

    #[test]
    fn ctrl_t_opens_a_new_tab_intent_and_never_leaks_into_the_page() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let engine = state.engine;
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        // Focus the page canvas first — the browser-reserved shortcut must
        // still win over page keyboard forwarding.
        let page_point = run_panel_page_image_rect(&ctx, &mut state, body_input())
            .expect("the Browser page texture should be locatable before clicking")
            .center();
        let mut click = body_input();
        click.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        assert!(run_panel_on_ctx(&ctx, &mut state, click));
        assert!(
            state.tabs[0].page_focused,
            "the click latches page keyboard focus"
        );
        let _ = drain_control_messages(&helper);

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, false)
        ));
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForeground(engine)),
            "Ctrl+T raises the tab strip's exact new-tab intent"
        );
        let leaked = drain_control_messages(&helper);
        assert!(
            !leaked.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Key { .. }
                )
            )),
            "a consumed browser shortcut must not be forwarded to the page: {leaked:?}"
        );
    }

    #[test]
    fn ctrl_w_closes_and_ctrl_shift_t_restores_the_closed_page() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let engine = state.tabs[0].engine;
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert!(state.tabs.is_empty(), "Ctrl+W closes the active tab");
        assert_eq!(
            state.closed_tabs.last(),
            Some(&ClosedTab {
                url: "https://example.test/".to_owned(),
                title: "Example".to_owned(),
                engine,
            }),
            "the closed page is retained in-memory for reopen"
        );

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, true)
        ));
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine,
                url: "https://example.test/".to_owned(),
            }),
            "Ctrl+Shift+T reopens the closed page on its original engine"
        );
        // The stack drains — a second restore is an honest no-op.
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, true)
        ));
        assert_eq!(state.take_open_request(), None, "the reopen stack drains");
    }

    #[test]
    fn the_reopen_stack_is_bounded_and_skips_blank_sessions() {
        // A session that never committed a URL leaves nothing to restore.
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("nonblocking helper");
        let blank = WebSession::from_stream(shell, None).expect("session");
        let mut state = WebState::default();
        state.push_session(blank);
        state.close_tab(0);
        assert!(
            state.closed_tabs.is_empty(),
            "a blank session is not reopenable"
        );

        // The stack stays bounded: a close past the cap evicts the OLDEST.
        let (session, _helper2) = testkit::connect().expect("connect");
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.closed_tabs = (0..CLOSED_TAB_STACK_CAP)
            .map(|n| ClosedTab {
                url: format!("https://mesh{n}.test/"),
                title: format!("Mesh {n}"),
                engine: BrowserEngine::Servo,
            })
            .collect();
        state.close_tab(0);
        assert_eq!(
            state.closed_tabs.len(),
            CLOSED_TAB_STACK_CAP,
            "the reopen stack stays bounded"
        );
        assert_eq!(
            state.closed_tabs.first().map(|c| c.url.as_str()),
            Some("https://mesh1.test/"),
            "the oldest retained entry is evicted first"
        );
        assert_eq!(
            state.closed_tabs.last().map(|c| c.url.as_str()),
            Some("about:blank"),
            "the newest close is retained"
        );
    }

    #[test]
    fn middle_clicking_a_tab_pill_closes_that_tab() {
        use std::cell::Cell;
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.set_vertical_tabs(false);
        state.push_session(first);
        state.push_session(second);
        // Mark the FIRST tab so the assertion can tell which one closed.
        state.tabs[0].force_dark = true;
        let ctx = egui::Context::default();
        Style::install(&ctx);

        // Probe frame: record where the first tab pill lands.
        let origin = Cell::new(pos2(0.0, 0.0));
        let _ = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                origin.set(ui.next_widget_position());
                chrome_ui::tab_strip(ui, &mut state);
            });
        });
        let point = origin.get() + vec2(CHROME_TAB_W * 0.5, CHROME_TAB_H * 0.5);

        let mut input = body_input();
        input.events = vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Middle,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Middle,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ];
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| chrome_ui::tab_strip(ui, &mut state));
        });
        assert_eq!(state.tabs.len(), 1, "middle-click closes the pill's tab");
        assert!(
            !state.tabs[0].force_dark,
            "the SECOND tab survives — middle-click closed the first pill"
        );
    }

    #[test]
    fn engine_navigation_updates_the_address_bar_only_when_not_editing() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "https://example.test/",
            "the pumped engine URL lands in the address bar with no chrome action"
        );

        // An engine-driven navigation (redirect / page script) rewrites the
        // bar on the next pump — the seam tab select/close/move never covered.
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/redirected".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(state.address, "https://example.test/redirected");

        // Focus the omnibox and start a draft: an engine navigation must NOT
        // clobber the in-progress edit.
        ctx.memory_mut(|m| m.request_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        state.address = "mesh draft".to_owned();
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/second".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "mesh draft",
            "an engine navigation never overwrites an in-progress edit"
        );

        // Blur without submitting: the missed engine URL is NOT retroactively
        // applied over the draft, but the NEXT engine navigation syncs again.
        ctx.memory_mut(|m| m.surrender_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "mesh draft",
            "blurring must not retroactively apply a stale engine URL"
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://example.test/third".to_owned(),
            },
        );
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert_eq!(
            state.address, "https://example.test/third",
            "engine navigation resumes syncing once the edit ends"
        );
    }

    #[test]
    fn open_close_shortcuts_pause_while_a_chrome_text_field_is_editing() {
        let (first, _h1) = testkit::connect().expect("connect 1");
        let (second, _h2) = testkit::connect().expect("connect 2");
        let mut state = WebState::default();
        state.push_session(first);
        state.push_session(second);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));

        ctx.memory_mut(|m| m.request_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(state.chrome_edit_focus, "omnibox focus latches the guard");

        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert_eq!(state.tabs.len(), 2, "Ctrl+W must not close a tab mid-edit");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::T, false)
        ));
        assert_eq!(
            state.take_open_request(),
            None,
            "Ctrl+T must not open a tab mid-edit"
        );
        // Tab CYCLING stays live during edits (the desktop-browser idiom).
        let before = state.active;
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::Tab, false)
        ));
        assert_ne!(
            state.active, before,
            "Ctrl+Tab keeps cycling while the omnibox is focused"
        );

        // Blur → the open/close accelerators resume.
        ctx.memory_mut(|m| m.surrender_focus(omnibox_widget_id()));
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(!state.chrome_edit_focus, "blurring releases the guard");
        assert!(run_panel_on_ctx(
            &ctx,
            &mut state,
            ctrl_key_input(egui::Key::W, false)
        ));
        assert_eq!(
            state.tabs.len(),
            1,
            "Ctrl+W closes the tab once the edit ends"
        );
    }

    #[test]
    fn new_tab_dashboard_renders_for_about_blank() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        assert_eq!(state.tabs[0].session.nav().url, "about:blank");
        assert!(run_panel(&mut state), "new-tab dashboard draws");
    }

    #[test]
    fn new_tab_dashboard_actions_use_browser_material_buttons() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::new_tab_dashboard(ui, &mut state);
            });
        });
        let texts = painted_text(&out.shapes);
        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "Search" && *color == chrome_ui::CHROME_TOOLBAR),
            "dashboard submit must use Browser primary-on text: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "Music" && *color == chrome_ui::CHROME_TEXT),
            "speed-dial shortcuts must use Browser secondary text: {texts:?}"
        );
        for label in ["Search", "Music"] {
            assert!(
                !texts.iter().any(|(text, color)| {
                    text == label
                        && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
                }),
                "dashboard action `{label}` must not inherit shared shell text colors: {texts:?}"
            );
        }
    }

    #[test]
    fn new_tab_dashboard_search_loads_mesh_searxng() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.dashboard_query = "mesh browser".to_owned();

        state.submit_dashboard_search();

        assert_eq!(state.address, "https://search.mesh/search?q=mesh+browser");
        assert_eq!(state.insecure_prompt, None);
        assert!(
            wait_for_fresh_frame(&mut state),
            "dashboard search reached the helper"
        );
    }

    #[test]
    fn new_tab_dashboard_mesh_service_shortcuts_use_the_same_load_gate() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        let music = NEW_TAB_SERVICES
            .iter()
            .find(|svc| svc.label == "Music")
            .expect("music shortcut");

        state.open_mesh_service(music.url.to_owned());

        assert_eq!(state.address, "http://music.mesh:4533/");
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://music.mesh:4533/")
        );
        assert!(
            !state.tabs[0].session.nav().loading,
            "HTTP mesh services pause at the same HTTPS prompt"
        );
    }

    #[test]
    fn explicit_http_address_prompts_before_loading() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/path".to_owned();

        state.submit_address();

        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://plain.example/path")
        );
        assert_eq!(state.address, "http://plain.example/path");
        assert!(
            !state.tabs[0].session.nav().loading,
            "HTTP prompt pauses before sending Load to the helper"
        );
        assert!(run_panel(&mut state), "the HTTP prompt renders");
    }

    #[test]
    fn http_prompt_can_upgrade_or_continue() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/path".to_owned();
        state.submit_address();
        state.upgrade_insecure_load();
        assert_eq!(state.insecure_prompt, None);
        assert_eq!(state.address, "https://plain.example/path");
        assert!(
            wait_for_fresh_frame(&mut state),
            "upgraded HTTPS load reached the helper"
        );

        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "http://plain.example/keep-http".to_owned();
        state.submit_address();
        state.continue_insecure_load();
        assert_eq!(state.insecure_prompt, None);
        assert_eq!(state.address, "http://plain.example/keep-http");
        assert!(
            wait_for_fresh_frame(&mut state),
            "continued HTTP load reached the helper"
        );
    }

    #[test]
    fn insecure_navigation_prompt_upgrade_and_hsts_are_audited() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        assert!(run_until_texture(&mut state));

        state.address = "http://plain.example/path".to_owned();
        state.submit_address();
        state.upgrade_insecure_load();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_INSECURE_NAVIGATION, None)
            .expect("list insecure navigation events");
        assert_eq!(msgs.len(), 2);
        let prompt: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("prompt body"))
                .expect("valid prompt JSON");
        assert_eq!(prompt["op"], "browser_insecure_navigation");
        assert_eq!(prompt["decision"], "prompt");
        assert_eq!(prompt["trigger"], "active_tab");
        assert_eq!(prompt["enforcement"], "navigation_prompt");
        assert_eq!(prompt["url"], "http://plain.example/path");
        assert_eq!(prompt["upgraded_url"], "https://plain.example/path");
        let upgrade: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("upgrade body"))
                .expect("valid upgrade JSON");
        assert_eq!(upgrade["decision"], "upgrade");
        assert_eq!(upgrade["trigger"], "active_tab");
        assert_eq!(upgrade["upgraded_url"], "https://plain.example/path");

        state.address = "http://plain.example/again".to_owned();
        state.submit_address();
        let msgs = persist
            .list_since(EVENT_BROWSER_INSECURE_NAVIGATION, None)
            .expect("list insecure navigation events after hsts");
        assert_eq!(msgs.len(), 3);
        let hsts: serde_json::Value =
            serde_json::from_str(msgs[2].body.as_deref().expect("hsts body"))
                .expect("valid hsts JSON");
        assert_eq!(hsts["decision"], "auto_upgrade");
        assert_eq!(hsts["enforcement"], "session_hsts");
        assert_eq!(hsts["trigger"], "active_tab");
        assert_eq!(hsts["url"], "http://plain.example/again");
        assert_eq!(hsts["upgraded_url"], "https://plain.example/again");
    }

    #[test]
    fn new_tab_http_url_prompts_before_spawn_intent_and_audits_continue() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));

        state.request_new_tab_with_url(BrowserEngine::Cef, "http://plain.example/new".to_owned());

        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://plain.example/new")
        );
        assert!(
            state.open_requested.is_empty(),
            "plain HTTP new-tab URL must not spawn before the prompt is answered"
        );

        state.continue_insecure_load();

        assert_eq!(state.insecure_prompt, None);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "http://plain.example/new".to_owned(),
            })
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_INSECURE_NAVIGATION, None)
            .expect("list insecure navigation events");
        assert_eq!(msgs.len(), 2);
        let prompt: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("prompt body"))
                .expect("valid prompt JSON");
        assert_eq!(prompt["decision"], "prompt");
        assert_eq!(prompt["trigger"], "new_tab");
        assert_eq!(prompt["engine"], "cef");
        assert_eq!(prompt["upgraded_url"], "https://plain.example/new");
        let continued: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("continue body"))
                .expect("valid continue JSON");
        assert_eq!(continued["decision"], "continue");
        assert_eq!(continued["trigger"], "new_tab");
        assert!(continued["upgraded_url"].is_null());
    }

    #[test]
    fn session_hsts_auto_upgrades_a_host_the_user_previously_upgraded() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        // First plain-http visit prompts; the user upgrades → the host is remembered.
        state.address = "http://shop.example/".to_owned();
        state.submit_address();
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://shop.example/")
        );
        state.upgrade_insecure_load();
        assert!(state.hsts_hosts.contains("shop.example"));

        // A later plain-http nav to the SAME host auto-upgrades silently (no prompt).
        state.address = "http://shop.example/cart".to_owned();
        state.submit_address();
        assert!(
            state.insecure_prompt.is_none(),
            "a remembered host auto-upgrades without re-prompting"
        );

        // A different plain-http host still prompts.
        state.address = "http://other.example/".to_owned();
        state.submit_address();
        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://other.example/")
        );
    }

    #[test]
    fn parse_managed_url_policy_accepts_hosts_and_url_prefixes() {
        let policy = parse_managed_url_policy(
            r#"
            # enterprise policy
            blocked.example
            *.wild.example
            url:https://docs.example/private/
            https://audit.example/internal/
            "#,
        );

        assert_eq!(policy.len(), 4);
        assert_eq!(
            policy.matches("https://cdn.blocked.example/").as_deref(),
            Some("host:blocked.example")
        );
        assert_eq!(
            policy.matches("https://news.wild.example/").as_deref(),
            Some("host:wild.example")
        );
        assert_eq!(
            policy
                .matches("https://docs.example/private/audit")
                .as_deref(),
            Some("url:https://docs.example/private/")
        );
        assert_eq!(
            policy
                .matches("https://audit.example/internal/report")
                .as_deref(),
            Some("url:https://audit.example/internal/")
        );
        assert!(policy.matches("https://docs.example/public/").is_none());
    }

    #[test]
    fn managed_policy_source_status_retains_last_good_on_read_error() {
        let _env = browser_env_lock();
        let _workgroup = EnvRestore::capture("MDE_WORKGROUP_ROOT");
        let workgroup = tempfile::tempdir().expect("temp workgroup");
        let browser_dir = workgroup.path().join("browser");
        std::fs::create_dir_all(&browser_dir).expect("browser policy dir");
        let source_path = browser_dir.join("managed-url-policy.txt");
        std::fs::write(&source_path, "blocked.example\n").expect("managed policy source");
        std::env::set_var("MDE_WORKGROUP_ROOT", workgroup.path());

        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));

        state.poll_managed_url_policy();

        assert_eq!(
            state
                .managed_policy_block_for("https://blocked.example/private")
                .as_ref()
                .map(|block| block.rule.as_str()),
            Some("host:blocked.example")
        );
        assert_eq!(
            state.managed_policy_source_status.state,
            BrowserPolicySourceState::Loaded
        );
        assert_eq!(state.managed_policy_source_status.item_count, 1);
        assert_eq!(state.managed_policy_source_status.effective_count, 1);
        let loaded_ms = state
            .managed_policy_source_status
            .loaded_ms
            .expect("loaded timestamp");

        std::fs::remove_file(&source_path).expect("remove policy source");
        std::fs::create_dir(&source_path).expect("directory at policy source path");
        state.managed_policy_last_poll = None;
        state.poll_managed_url_policy();

        assert_eq!(
            state
                .managed_policy_block_for("https://blocked.example/private")
                .as_ref()
                .map(|block| block.rule.as_str()),
            Some("host:blocked.example"),
            "a read error must not clear the last-good managed policy"
        );
        assert_eq!(
            state.managed_policy_source_status.state,
            BrowserPolicySourceState::Error
        );
        assert_eq!(state.managed_policy_source_status.effective_count, 1);
        assert_eq!(
            state.managed_policy_source_status.loaded_ms,
            Some(loaded_ms)
        );
        assert!(
            state.managed_policy_source_status.error.is_some(),
            "read errors should carry the filesystem error text"
        );
        assert!(state
            .managed_policy_source_status
            .summary()
            .contains("source read error"));

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_managed_policy_source_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list managed policy source status");
        assert_eq!(msgs.len(), 2);
        let error: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("error body"))
                .expect("valid error JSON");
        assert_eq!(error["op"], "browser_managed_url_policy_source_status");
        assert_eq!(error["policy"], "managed_url");
        assert_eq!(error["state"], "error");
        assert_eq!(error["effective_count"], 1);
        assert_eq!(error["loaded_ms"], loaded_ms);
        assert!(error["error"].as_str().is_some_and(|msg| !msg.is_empty()));
    }

    #[test]
    fn managed_policy_blocks_chrome_loads_before_the_helper() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.set_managed_url_policy(parse_managed_url_policy("blocked.example\n"));
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        state.address = "https://blocked.example/path".to_owned();

        state.submit_address();

        let block = state
            .managed_policy_block
            .as_ref()
            .expect("blocked navigation");
        assert_eq!(block.url, "https://blocked.example/path");
        assert_eq!(block.rule, "host:blocked.example");
        assert!(
            !state.tabs[0].session.nav().loading,
            "managed policy blocks before sending Load to the helper"
        );
        assert!(run_panel(&mut state), "managed policy interstitial paints");

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_POLICY_BLOCK, None)
            .expect("list policy block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("policy body"))
                .expect("valid JSON");
        assert_eq!(event["op"], "browser_policy_block");
        assert_eq!(event["policy"], "managed_url");
        assert_eq!(event["trigger"], "chrome_load");
        assert_eq!(event["url"], "https://blocked.example/path");
        assert_eq!(event["rule"], "host:blocked.example");
    }

    #[test]
    fn managed_policy_url_prefix_default_ports_block_chrome_loads_before_the_helper() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.set_managed_url_policy(parse_managed_url_policy(
            "url:https://portal.example:443/admin/\nurl:https://docs.example\n",
        ));
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        state.address = "https://portal.example:443/admin/users".to_owned();
        state.submit_address();

        let block = state
            .managed_policy_block
            .as_ref()
            .expect("default-port URL-prefix navigation is blocked");
        assert_eq!(block.url, "https://portal.example:443/admin/users");
        assert_eq!(block.rule, "url:https://portal.example/admin/");
        assert!(
            !state.tabs[0].session.nav().loading,
            "managed policy blocks before sending Load to the helper"
        );

        state.managed_policy_block = None;
        state.address = "https://alice@portal.example/admin/users".to_owned();
        state.submit_address();
        let block = state
            .managed_policy_block
            .as_ref()
            .expect("userinfo URL-prefix navigation is blocked");
        assert_eq!(block.url, "https://alice@portal.example/admin/users");
        assert_eq!(block.rule, "url:https://portal.example/admin/");

        state.managed_policy_block = None;
        state.address = "https://docs.example.evil/private".to_owned();
        state.submit_address();
        assert!(
            state.managed_policy_block.is_none(),
            "an authority-only URL prefix must not raw-prefix match another host"
        );
    }

    #[test]
    fn managed_policy_blocks_queued_new_tabs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.set_managed_url_policy(parse_managed_url_policy("blocked.example\n"));

        state.request_new_tab_with_url(BrowserEngine::Cef, "https://blocked.example/".to_owned());

        assert!(
            state.open_requested.is_empty(),
            "blocked new-tab opens must not reach the live-helper spawn queue"
        );
        assert_eq!(
            state.managed_policy_block.as_ref().map(|b| b.rule.as_str()),
            Some("host:blocked.example")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_POLICY_BLOCK, None)
            .expect("list policy block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("policy body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "new_tab");
        assert_eq!(event["engine"], "cef");
    }

    #[test]
    fn managed_policy_helper_document_blocks_paint_the_interstitial() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, peer) = raw_session_pair();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.set_managed_url_policy(parse_managed_url_policy("blocked.example\n"));
        state.push_session(session);

        write_helper_event(
            &peer,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 9,
                url: "https://blocked.example/".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Document,
                ),
            },
        );
        state.tabs[0].session.poll();

        assert_eq!(
            state.tabs[0].session.managed_policy_block(),
            Some("https://blocked.example/")
        );
        assert!(run_panel(&mut state), "managed policy interstitial paints");
        assert!(
            run_panel(&mut state),
            "repainting the same interstitial stays stable"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_POLICY_BLOCK, None)
            .expect("list policy block events");
        assert_eq!(msgs.len(), 1, "the interstitial is audited once");
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("policy body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "helper_document");
        assert_eq!(event["url"], "https://blocked.example/");
        assert_eq!(event["rule"], "host:blocked.example");
    }

    #[test]
    fn fullscreen_mode_renders_the_body_only_and_toggles_back() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        // Immersive mode renders the page body without the chrome (no panic).
        state.fullscreen = true;
        assert!(
            run_panel(&mut state),
            "fullscreen renders the immersive body view"
        );
        // Exiting restores the full chrome.
        state.fullscreen = false;
        assert!(
            run_panel(&mut state),
            "exiting fullscreen restores the chrome"
        );
    }

    #[test]
    fn a_crashed_tab_paints_the_honest_crashed_state() {
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);

        helper.crash();
        // Poll it to observe the crash, then render the crashed body.
        assert!(run_panel(&mut state));
        assert!(state.tabs[0].session.is_crashed());
        assert!(run_panel(&mut state), "the crashed body produced no draw");
    }

    #[test]
    fn one_tabs_crash_does_not_disturb_another() {
        let (a, helper_a) = testkit::connect().expect("connect a");
        let (b, _helper_b) = testkit::connect().expect("connect b");
        let mut state = WebState::default();
        state.push_session(a); // tab 0
        state.push_session(b); // tab 1 (active)
        run_until_texture(&mut state);

        helper_a.crash();
        run_panel(&mut state); // polls all tabs
        assert!(state.tabs[0].session.is_crashed(), "tab 0 crashed");
        assert!(!state.tabs[1].session.is_crashed(), "tab 1 unaffected");
    }

    #[test]
    fn shows_cert_interstitial_reflects_presence_and_crash_precedence() {
        let err = CertError {
            url: "https://bad.example.com/x".to_owned(),
            code: -202,
            message: "The certificate authority is not trusted".to_owned(),
        };
        assert!(
            shows_cert_interstitial(false, Some(&err)),
            "a cert error on a live tab shows the interstitial"
        );
        assert!(
            !shows_cert_interstitial(false, None),
            "no cert error renders the normal body"
        );
        assert!(
            !shows_cert_interstitial(true, Some(&err)),
            "a crash always wins over a cert error"
        );
    }

    #[test]
    fn cert_error_host_extracts_the_domain() {
        let err = CertError {
            url: "https://bad.example.com/x".to_owned(),
            code: -202,
            message: "The certificate authority is not trusted".to_owned(),
        };
        assert_eq!(chrome_ui::cert_error_host(&err), "bad.example.com");
    }

    #[test]
    fn cert_error_host_falls_back_to_the_raw_url_with_no_authority() {
        let err = CertError {
            url: "not-a-url".to_owned(),
            code: -202,
            message: "x".to_owned(),
        };
        assert_eq!(chrome_ui::cert_error_host(&err), "not-a-url");
    }

    #[test]
    fn a_cert_error_paints_the_interstitial_instead_of_a_panic() {
        let session = session_with_cert_error(
            "https://bad.example.com/",
            -202,
            "The certificate authority is not trusted",
        );
        assert_eq!(
            session.cert_error(),
            Some(&CertError {
                url: "https://bad.example.com/".to_owned(),
                code: -202,
                message: "The certificate authority is not trusted".to_owned(),
            })
        );
        let mut state = WebState::default();
        state.push_session(session);

        // A render pass must not panic with a cert error present on the active
        // (and only) tab — it paints the interstitial in place of the frame.
        assert!(run_panel(&mut state), "the interstitial produced no draw");
    }

    #[test]
    fn certificate_errors_are_audited_once() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::Title("Bad Site".to_owned()),
        );
        send_cert_error(
            &helper,
            "https://bad.example.test/login",
            -202,
            "The certificate is not trusted (unknown authority)",
        );

        assert!(run_panel(&mut state), "panel polls the cert error");
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_CERTIFICATE_ERROR, None)
            .expect("list certificate-error events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("certificate body"))
                .expect("valid JSON");
        assert_eq!(event["op"], "browser_certificate_error");
        assert_eq!(event["url"], "https://bad.example.test/login");
        assert_eq!(event["host"], "bad.example.test");
        assert_eq!(event["title"], "Bad Site");
        assert_eq!(event["code"], -202);
        assert_eq!(
            event["message"],
            "The certificate is not trusted (unknown authority)"
        );

        assert!(run_panel(&mut state), "repaint stays stable");
        let msgs = persist
            .list_since(EVENT_BROWSER_CERTIFICATE_ERROR, None)
            .expect("list certificate-error events after repaint");
        assert_eq!(msgs.len(), 1, "cert-error audit is one-shot");
    }

    #[test]
    fn a_safe_browsing_block_paints_the_interstitial_instead_of_a_panic() {
        use mde_web_preview_client::{EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        // The active tab is sitting on a benign page (with back-history)...
        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: true,
                can_forward: false,
                loading: false,
                url: "https://start.example/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();

        // ...then a top-level navigation to a mesh-flagged unsafe site is blocked,
        // arming the full-page interstitial (a Document block, not a subresource).
        state.set_safe_browsing_hosts(["malware.test"]);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 7,
                url: "https://malware.test/".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Document),
            }
            .encode(),
        ))
        .expect("document request");
        state.tabs[0].session.poll();
        assert_eq!(
            state.tabs[0].session.safe_browsing_block(),
            Some("https://malware.test/"),
            "a top-level Document block arms the interstitial"
        );

        // The render pass paints the "unsafe site blocked" interstitial in place of
        // the frame and must not panic with the block present on the active tab.
        assert!(run_panel(&mut state), "the interstitial produced no draw");
    }

    #[test]
    fn mixed_content_resource_blocks_are_audited_once() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);

        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://portal.example/dashboard".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("Portal".to_owned()));
        write_helper_event(
            &helper,
            &EventMsg::ResourceRequest {
                id: 41,
                url: "http://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            },
        );

        assert!(run_panel(&mut state), "panel polls the helper session");
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|msg| matches!(
                    msg,
                    ControlMsg::ResourceVerdict {
                        id: 41,
                        allow: false
                    }
                )),
            "mixed-content script is denied before network"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_MIXED_CONTENT_BLOCK, None)
            .expect("list mixed-content events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("mixed-content body"))
                .expect("valid JSON");
        assert_eq!(event["op"], "browser_mixed_content_block");
        assert_eq!(event["page_url"], "https://portal.example/dashboard");
        assert_eq!(event["url"], "http://cdn.example.test/app.js");
        assert_eq!(event["resource"], "script");
        assert_eq!(event["title"], "Portal");

        assert!(run_panel(&mut state), "repaint stays stable");
        let msgs = persist
            .list_since(EVENT_BROWSER_MIXED_CONTENT_BLOCK, None)
            .expect("list mixed-content events after repaint");
        assert_eq!(msgs.len(), 1, "resource block audit is one-shot");
    }

    #[test]
    fn a_permission_prompt_is_suppressed_behind_a_cert_interstitial() {
        // Defensive precedence: a tab that has BOTH a blocking cert error and a
        // pending permission must show the cert interstitial with the permission bar
        // suppressed — never paint a prompt over an interstitial, and never let the
        // combination panic. (A cert-blocked page can't really raise a request; this
        // guards the state anyway.)
        let (mut session, peer) = raw_session_pair();
        send_cert_error(&peer, "https://x.example/", -202, "bad cert");
        write_helper_event(
            &peer,
            &mde_web_preview_client::EventMsg::PermissionRequest {
                id: 3,
                kind: 0,
                origin: "https://x.example".to_owned(),
            },
        );
        session.poll();
        assert!(session.cert_error().is_some());
        assert!(session.pending_permission().is_some());

        let mut state = WebState::default();
        state.push_session(session);
        assert!(
            run_panel(&mut state),
            "the cert interstitial produced a draw"
        );
        assert!(
            state.tabs[0].session.pending_permission().is_some(),
            "the prompt is held behind the interstitial, not consumed by a stray bar"
        );
    }

    #[test]
    fn the_active_tabs_cert_error_does_not_disturb_another_tab() {
        let clean = session_with_favicon(&[0x89, b'P', b'N', b'G']);
        let blocked = session_with_cert_error("https://bad.example.com/", -202, "not trusted");
        let mut state = WebState::default();
        state.push_session(clean); // tab 0
        state.push_session(blocked); // tab 1 (active)

        assert!(run_panel(&mut state), "the interstitial produced no draw");
        assert!(
            state.tabs[0].session.cert_error().is_none(),
            "tab 0 unaffected"
        );
        assert!(
            state.tabs[1].session.cert_error().is_some(),
            "tab 1 blocked"
        );
    }

    #[test]
    fn cert_error_back_action_prefers_history_over_closing() {
        assert!(
            matches!(cert_error_back_action(true), CertErrorBackAction::GoBack),
            "with back history, \"Back to safety\" navigates back"
        );
        assert!(
            matches!(cert_error_back_action(false), CertErrorBackAction::CloseTab),
            "with no back history, \"Back to safety\" closes the tab"
        );
    }

    #[test]
    fn back_to_safety_with_no_history_closes_the_tab() {
        let session = session_with_cert_error("https://bad.example.com/", -202, "not trusted");
        assert!(
            !session.nav().can_back,
            "a raw socketpair session starts with no back history"
        );
        let mut state = WebState::default();
        state.push_session(session);
        assert_eq!(state.tabs.len(), 1);
        assert!(run_panel(&mut state), "the interstitial produced no draw");

        // No pointer harness clicks the real button here (that needs the live
        // widget rect); this proves the wiring `active_body` takes on a click —
        // the pure `cert_error_back_action` decision — matches the tab's actual
        // history state.
        let can_back = state.tabs[0].session.nav().can_back;
        match cert_error_back_action(can_back) {
            CertErrorBackAction::GoBack => panic!("expected CloseTab with no history"),
            CertErrorBackAction::CloseTab => state.close_tab(0),
        }
        assert!(state.tabs.is_empty(), "the tab closed");
    }

    #[test]
    fn browser_body_interstitials_use_browser_material_tokens() {
        let err = CertError {
            url: "https://bad.example.com/".to_owned(),
            code: -202,
            message: "not trusted".to_owned(),
        };
        let ctx = egui::Context::default();
        Style::install(&ctx);

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::cert_error_body(ui, &err, false);
            });
        });
        let texts = painted_text(&out.shapes);

        assert!(
            texts.iter().any(|(text, color)| {
                text == "Your connection is not private" && *color == chrome_ui::CHROME_ERROR
            }),
            "cert interstitial heading must use Browser Material error color: {texts:?}"
        );
        assert!(
            texts.iter().any(|(text, color)| {
                text == "bad.example.com" && *color == chrome_ui::CHROME_TEXT
            }),
            "cert interstitial host must use Browser Material text color: {texts:?}"
        );
        assert!(
            texts.iter().any(|(text, color)| {
                text == "Error code -202" && *color == chrome_ui::CHROME_TEXT_DIM
            }),
            "cert interstitial metadata must use Browser Material dim text: {texts:?}"
        );
        let assert_primary_action = |texts: &[(String, egui::Color32)], label: &str| {
            assert!(
                texts
                    .iter()
                    .any(|(text, color)| text == label && *color == chrome_ui::CHROME_TOOLBAR),
                "interstitial action `{label}` must use Browser primary-on text: {texts:?}"
            );
            assert!(
                !texts.iter().any(|(text, color)| {
                    text == label
                        && matches!(
                            *color,
                            chrome_ui::CHROME_TEXT
                                | Style::TEXT
                                | Style::TEXT_DIM
                                | Style::TEXT_STRONG
                        )
                }),
                "interstitial action `{label}` must not inherit raw/shared text colors: {texts:?}"
            );
        };
        assert_primary_action(&texts, "Back to safety");

        let mut respawn_requested = false;
        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::crashed_body(ui, "renderer exited".to_owned(), &mut respawn_requested);
            });
        });
        assert_primary_action(&painted_text(&out.shapes), "Reload");

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::safe_browsing_interstitial_body(ui, "https://blocked.example/");
            });
        });
        assert_primary_action(&painted_text(&out.shapes), "Back to safety");

        let block = ManagedPolicyBlock {
            url: "https://policy.example/".to_owned(),
            rule: "blocked-host".to_owned(),
        };
        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::managed_policy_interstitial_body(ui, &block);
            });
        });
        assert_primary_action(&painted_text(&out.shapes), "Back to safety");
    }

    #[test]
    fn browser_prompt_bars_use_material_action_buttons() {
        let prompt = BeforeUnloadDialog {
            id: 7,
            message: "Unsaved work".to_owned(),
            origin: "https://docs.example.com/edit".to_owned(),
            is_reload: false,
        };
        let ctx = egui::Context::default();
        Style::install(&ctx);

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::scope(ui, |ui| {
                    chrome_ui::permission_prompt_bar(ui, "https://camera.example", 3);
                    chrome_ui::before_unload_prompt_bar(ui, &prompt);
                    chrome_ui::login_save_prompt_bar(ui, "docs.example.com", "mm");
                });
            });
        });
        let texts = painted_text(&out.shapes);

        for label in ["Allow", "Leave", "Save"] {
            assert!(
                texts
                    .iter()
                    .any(|(text, color)| text == label && *color == chrome_ui::CHROME_TOOLBAR),
                "primary prompt action `{label}` must use Browser primary-on color: {texts:?}"
            );
        }
        for label in ["Block", "Stay", "Not now"] {
            assert!(
                texts
                    .iter()
                    .any(|(text, color)| text == label && *color == chrome_ui::CHROME_TEXT),
                "secondary prompt action `{label}` must use Browser text color: {texts:?}"
            );
        }
        assert!(
            !texts
                .iter()
                .any(|(text, color)| text == "Allow" && *color == Style::TEXT),
            "prompt buttons must not fall back to shared shell text tokens: {texts:?}"
        );
    }

    #[test]
    fn the_ad_filter_blocked_count_surfaces_on_the_active_tab() {
        use mde_web_preview_client::{
            wire, EventMsg, FilterListStore, RequestFilter, ResourceType, WebSession,
        };
        use std::io::Write as _;
        use std::os::unix::net::UnixStream;

        // A bundled-filter session over a bare socketpair (no shm needed — we only
        // drive the request-policy protocol to bump the per-page counter).
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let filter = RequestFilter::from_store(&FilterListStore::with_bundled());
        let mut session = WebSession::from_stream(shell, None)
            .expect("session")
            .with_filter(filter);

        let mut peer: &UnixStream = &helper;
        let nav = EventMsg::NavState {
            can_back: false,
            can_forward: false,
            loading: false,
            url: "https://news.example.com/".to_owned(),
        };
        peer.write_all(&wire::frame(&nav.encode())).expect("nav");
        let req = EventMsg::ResourceRequest {
            id: 1,
            url: "https://doubleclick.net/ad".to_owned(),
            resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
        };
        peer.write_all(&wire::frame(&req.encode())).expect("req");
        session.poll();
        assert_eq!(
            session.blocked_count(),
            1,
            "the tracker was blocked + counted"
        );

        let mut state = WebState::default();
        state.tabs.push(Tab {
            id: 1,
            session,
            engine: BrowserEngine::Servo,
            internal_page: None,
            internal_peer: None,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            group: None,
            pinned: false,
            muted: false,
            autoplay_blocked: false,
            force_dark: false,
            reader_mode: false,
            user_scripts: false,
            user_agent: UserAgentOverride::Default,
            device_profile: DeviceProfile::Default,
            last_activity: Instant::now(),
            idle_suspended: false,
            page_focused: false,
            texture: None,
            last_frame: None,
            last_audited_resource_seq: 0,
            last_audited_cert_error: None,
            resizer: ViewportResizer::default(),
            favicon_cache: None,
        });
        // The nav chrome (with the "N blocked" shield) renders without panicking.
        assert!(run_panel(&mut state), "the browser chrome produced no draw");
        assert_eq!(state.tabs[0].session.blocked_count(), 1);
    }

    #[test]
    fn per_site_privacy_toggle_changes_real_resource_verdicts() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("blocked request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 1,
                        allow: false
                    }
                )),
            "the bundled blocker rejects the tracker before network"
        );

        state.set_active_site_blocking(false);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 2,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("allowlisted request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(m, ControlMsg::ResourceVerdict { id: 2, allow: true })),
            "allowlisting the current site changes the actual helper verdict"
        );

        state.set_active_site_blocking(true);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 3,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Image),
            }
            .encode(),
        ))
        .expect("reblocked request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 3,
                        allow: false
                    }
                )),
            "re-enabling site blocking restores the block verdict"
        );
    }

    #[test]
    fn per_site_privacy_toggle_publishes_site_blocking_audit_events() {
        use mde_web_preview_client::EventMsg;

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/story".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("News Desk".to_owned()));
        state.tabs[0].session.poll();

        state.set_active_site_blocking(false);
        state.set_active_site_blocking(true);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_SITE_BLOCKING, None)
            .expect("list site-blocking events");
        assert_eq!(msgs.len(), 2);
        let events = msgs
            .iter()
            .map(|msg| {
                serde_json::from_str::<serde_json::Value>(
                    msg.body.as_deref().expect("site-blocking body"),
                )
                .expect("site-blocking JSON")
            })
            .collect::<Vec<_>>();

        assert_eq!(events[0]["op"], "browser_site_blocking");
        assert_eq!(events[0]["policy"], "adfilter_site_override");
        assert_eq!(events[0]["decision"], "disable");
        assert_eq!(events[0]["site_blocking"], "disabled");
        assert_eq!(events[0]["enforcement"], "request_filter");
        assert_eq!(events[0]["engine"], "servo");
        assert_eq!(events[0]["url"], "https://news.example.com/story");
        assert_eq!(events[0]["host"], "news.example.com");
        assert_eq!(events[0]["title"], "News Desk");
        assert_eq!(events[0]["source"], "browser");
        assert_eq!(events[0]["node"], local_hostname());
        assert!(events[0]["updated_ms"].as_u64().is_some());

        assert_eq!(events[1]["decision"], "enable");
        assert_eq!(events[1]["site_blocking"], "enabled");
        assert_eq!(events[1]["host"], "news.example.com");
    }

    #[test]
    fn parse_safe_browsing_hosts_skips_comments_blanks_and_lowercases() {
        let text = "# operator blocklist\nMalware.test\n\n  Phish.example  \n# note\nads.bad\n";
        assert_eq!(
            parse_safe_browsing_hosts(text),
            vec![
                "malware.test".to_string(),
                "phish.example".to_string(),
                "ads.bad".to_string(),
            ]
        );
        assert!(parse_safe_browsing_hosts("# only comments\n\n   \n").is_empty());
    }

    #[test]
    fn safe_browsing_policy_source_status_retains_last_good_on_missing_source() {
        let _env = browser_env_lock();
        let _workgroup = EnvRestore::capture("MDE_WORKGROUP_ROOT");
        let workgroup = tempfile::tempdir().expect("temp workgroup");
        let browser_dir = workgroup.path().join("browser");
        std::fs::create_dir_all(&browser_dir).expect("browser policy dir");
        let source_path = browser_dir.join("safe-browsing-hosts.txt");
        std::fs::write(&source_path, "# source\nMalware.test\n\n").expect("safe-browsing source");
        std::env::set_var("MDE_WORKGROUP_ROOT", workgroup.path());

        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));

        state.poll_safe_browsing_hosts();

        assert_eq!(state.safe_browsing_hosts, vec!["malware.test".to_owned()]);
        assert_eq!(
            state.safe_browsing_source_status.state,
            BrowserPolicySourceState::Loaded
        );
        assert_eq!(
            state.safe_browsing_source_status.summary(),
            "Safe browsing: 1 unsafe site rule loaded"
        );
        assert_eq!(state.safe_browsing_source_status.item_count, 1);
        assert_eq!(state.safe_browsing_source_status.effective_count, 1);
        let loaded_ms = state
            .safe_browsing_source_status
            .loaded_ms
            .expect("loaded timestamp");

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_safe_browsing_source_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list safe-browsing source status");
        assert_eq!(msgs.len(), 1);
        let loaded: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("loaded body"))
                .expect("valid loaded JSON");
        assert_eq!(loaded["op"], "browser_safe_browsing_source_status");
        assert_eq!(loaded["policy"], "safe_browsing");
        assert_eq!(loaded["state"], "loaded");
        assert_eq!(
            loaded["source_path"],
            source_path.to_string_lossy().as_ref()
        );
        assert_eq!(loaded["item_count"], 1);
        assert_eq!(loaded["effective_count"], 1);

        std::fs::remove_file(&source_path).expect("remove source");
        state.safe_browsing_last_poll = None;
        state.poll_safe_browsing_hosts();

        assert_eq!(
            state.safe_browsing_hosts,
            vec!["malware.test".to_owned()],
            "a missing source must not clear the last-good blocklist"
        );
        assert_eq!(
            state.safe_browsing_source_status.state,
            BrowserPolicySourceState::Missing
        );
        assert_eq!(state.safe_browsing_source_status.effective_count, 1);
        assert_eq!(state.safe_browsing_source_status.loaded_ms, Some(loaded_ms));
        assert!(state
            .safe_browsing_source_status
            .summary()
            .contains("source missing"));
        assert!(state
            .safe_browsing_source_status
            .summary()
            .contains("last-good unsafe site rule active"));

        let msgs = persist
            .list_since(&topic, None)
            .expect("list safe-browsing missing status");
        assert_eq!(msgs.len(), 2);
        let missing: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("missing body"))
                .expect("valid missing JSON");
        assert_eq!(missing["state"], "missing");
        assert_eq!(missing["effective_count"], 1);
        assert_eq!(missing["loaded_ms"], loaded_ms);
    }

    #[test]
    fn safe_browsing_hosts_change_real_resource_verdicts() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        let _ = drain_control_messages(&helper);

        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 11,
                url: "https://malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("unlisted request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 11,
                        allow: true
                    }
                )),
            "an unlisted host is allowed"
        );

        state.set_safe_browsing_hosts(["malware.test"]);
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 12,
                url: "https://cdn.malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("listed request");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 12,
                        allow: false
                    }
                )),
            "loading the mesh-hosted safe-browsing host blocks the real helper request"
        );
    }

    #[test]
    fn site_data_manager_tracks_committed_first_party_hosts() {
        use mde_web_preview_client::EventMsg;

        let (shell_a, helper_a) = UnixStream::pair().expect("socketpair a");
        let (shell_b, helper_b) = UnixStream::pair().expect("socketpair b");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell_a, None).expect("session a"));
        state.push_session(WebSession::from_stream(shell_b, None).expect("session b"));

        let mut peer_a: &UnixStream = &helper_a;
        peer_a
            .write_all(&wire::frame(
                &EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: "https://alpha.example/path".to_owned(),
                }
                .encode(),
            ))
            .expect("nav a");
        let mut peer_b: &UnixStream = &helper_b;
        peer_b
            .write_all(&wire::frame(
                &EventMsg::NavState {
                    can_back: false,
                    can_forward: false,
                    loading: false,
                    url: "https://beta.example/path".to_owned(),
                }
                .encode(),
            ))
            .expect("nav b");

        state.tabs[0].session.poll();
        state.tabs[1].session.poll();
        state.update_site_data_from_tabs();

        let summary = state.site_data_summary();
        assert!(summary.contains("2 tracked sites"), "summary = {summary}");
        assert!(summary.contains("2 open tabs"), "summary = {summary}");
        assert!(summary.is_ascii(), "summary = {summary}");
        assert!(
            !summary.contains('·'),
            "site-data summary must use ASCII separators: {summary}"
        );
    }

    #[test]
    fn clearing_current_tab_records_the_active_site_data_reset() {
        use mde_web_preview_client::EventMsg;

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/path".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        state.tabs[0].session.poll();
        state.update_site_data_from_tabs();

        state.clear_active_session_data();
        let summary = state.site_data_summary();
        assert!(
            summary.contains("news.example.com cleared 1 time"),
            "summary = {summary}"
        );
        assert!(summary.is_ascii(), "summary = {summary}");
        assert!(
            !summary.contains('·'),
            "site-data summary must use ASCII separators: {summary}"
        );
    }

    #[test]
    fn clearing_current_tab_publishes_site_data_clear_audit_event() {
        use mde_web_preview_client::EventMsg;

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://portal.example/admin".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("Admin Portal".to_owned()));
        state.tabs[0].session.poll();

        state.clear_active_session_data();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_SITE_DATA_CLEAR, None)
            .expect("list site-data clear events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("site-data body"))
                .expect("valid JSON");
        assert_eq!(event["op"], "browser_site_data_clear");
        assert_eq!(event["decision"], "clear");
        assert_eq!(event["enforcement"], "session_memory_only");
        assert_eq!(event["scope"], "current_tab");
        assert_eq!(event["engine"], "servo");
        assert_eq!(event["url"], "https://portal.example/admin");
        assert_eq!(event["host"], "portal.example");
        assert_eq!(event["title"], "Admin Portal");
        assert_eq!(event["source"], "browser");
        assert!(event["cleared_ms"].as_u64().is_some());
    }

    #[test]
    fn synced_filter_store_file_compiles_into_open_tabs_and_publishes_status() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let _env = browser_env_lock();
        let _workgroup = EnvRestore::capture("MDE_WORKGROUP_ROOT");
        let workgroup = tempfile::tempdir().expect("temp workgroup");
        let compiled_dir = workgroup.path().join("adfilter").join("compiled");
        std::fs::create_dir_all(&compiled_dir).expect("compiled adfilter dir");
        let source_path = compiled_dir.join("engine.json");
        let mut synced = FilterListStore::new();
        synced.add_source(FilterListSource::custom(
            "Synced mirror",
            Some("file:///mesh/adfilter/mirror/Synced_mirror.txt".to_owned()),
            "||ads.synced.test^\n",
            100,
        ));
        std::fs::write(
            &source_path,
            synced.to_json().expect("serialize synced filter store"),
        )
        .expect("compiled filter store");
        std::env::set_var("MDE_WORKGROUP_ROOT", workgroup.path());

        let bus = tempfile::tempdir().expect("temp bus");
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        state.add_custom_filter_rules(CUSTOM_FILTER_SOURCE_NAME, "||ads.local.test^\n", None);
        state.poll_filter_lists();

        assert_eq!(
            state.filter_list_source_status.state,
            BrowserPolicySourceState::Loaded
        );
        assert_eq!(
            state.filter_list_source_status.summary(),
            "Filter lists: 1 filter source loaded"
        );
        assert!(
            state.adfilter_store.source("Synced mirror").is_some(),
            "the worker-compiled filter source is now part of the Browser matcher"
        );
        assert!(
            state
                .adfilter_store
                .source(CUSTOM_FILTER_SOURCE_NAME)
                .is_some(),
            "loading the synced store must preserve the local operator custom source"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_filter_list_source_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list filter-list source status");
        assert_eq!(msgs.len(), 1);
        let loaded: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("loaded body"))
                .expect("valid loaded JSON");
        assert_eq!(loaded["op"], "browser_filter_list_source_status");
        assert_eq!(loaded["policy"], "filter_lists");
        assert_eq!(loaded["state"], "loaded");
        assert_eq!(loaded["item_count"], 1);
        assert_eq!(loaded["effective_count"], 1);
        let expected_source_path = source_path.to_string_lossy().into_owned();
        assert_eq!(loaded["source_path"], expected_source_path.as_str());

        let mut peer: &UnixStream = &helper;
        for (id, url) in [
            (51, "https://ads.synced.test/banner.js"),
            (52, "https://ads.local.test/banner.js"),
        ] {
            peer.write_all(&wire::frame(
                &EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
                }
                .encode(),
            ))
            .expect("resource request");
            state.tabs[0].session.poll();
            assert!(
                drain_control_messages(&helper)
                    .into_iter()
                    .any(|m| matches!(m, ControlMsg::ResourceVerdict { id: got, allow: false } if got == id)),
                "synced and preserved local filter rules must both block real resource verdicts"
            );
        }
    }

    #[test]
    fn custom_filter_rules_file_compiles_into_open_tabs_and_publishes_status() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let _env = browser_env_lock();
        let _workgroup = EnvRestore::capture("MDE_WORKGROUP_ROOT");
        let workgroup = tempfile::tempdir().expect("temp workgroup");
        let browser_dir = workgroup.path().join("browser");
        std::fs::create_dir_all(&browser_dir).expect("browser policy dir");
        let source_path = browser_dir.join("custom-filter-rules.txt");
        std::fs::write(&source_path, "# operator rules\n||ads.custom.test^\n")
            .expect("custom filter source");
        std::env::set_var("MDE_WORKGROUP_ROOT", workgroup.path());

        let bus = tempfile::tempdir().expect("temp bus");
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        state.poll_custom_filter_rules();

        assert_eq!(
            state.custom_filter_rules_source_status.state,
            BrowserPolicySourceState::Loaded
        );
        assert_eq!(
            state.custom_filter_rules_source_status.summary(),
            "Custom filters: 1 custom rule loaded"
        );
        let expected_source_url = source_path.to_string_lossy().into_owned();
        assert_eq!(
            state
                .adfilter_store
                .source(CUSTOM_FILTER_SOURCE_NAME)
                .and_then(|source| source.url.as_deref()),
            Some(expected_source_url.as_str())
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_custom_filter_rules_source_topic(&local_hostname());
        let msgs = persist
            .list_since(&topic, None)
            .expect("list custom filter status");
        assert_eq!(msgs.len(), 1);
        let loaded: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("loaded body"))
                .expect("valid loaded JSON");
        assert_eq!(loaded["op"], "browser_custom_filter_rules_source_status");
        assert_eq!(loaded["policy"], "custom_filter_rules");
        assert_eq!(loaded["state"], "loaded");
        assert_eq!(loaded["item_count"], 1);
        assert_eq!(loaded["effective_count"], 1);

        let mut peer: &UnixStream = &helper;
        peer.write_all(&wire::frame(
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://publisher.test/".to_owned(),
            }
            .encode(),
        ))
        .expect("nav");
        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 41,
                url: "https://ads.custom.test/banner.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("custom request");
        state.tabs[0].session.poll();

        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 41,
                        allow: false
                    }
                )),
            "custom EasyList-style rules are compiled into active tab verdicts"
        );

        std::fs::remove_file(&source_path).expect("remove source");
        state.custom_filter_rules_last_poll = None;
        state.poll_custom_filter_rules();
        assert_eq!(
            state.custom_filter_rules_source_status.state,
            BrowserPolicySourceState::Missing
        );
        assert!(state
            .custom_filter_rules_source_status
            .summary()
            .contains("last-good custom rule active"));

        peer.write_all(&wire::frame(
            &EventMsg::ResourceRequest {
                id: 42,
                url: "https://ads.custom.test/again.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(ResourceType::Script),
            }
            .encode(),
        ))
        .expect("custom request after missing source");
        state.tabs[0].session.poll();
        assert!(
            drain_control_messages(&helper)
                .into_iter()
                .any(|m| matches!(
                    m,
                    ControlMsg::ResourceVerdict {
                        id: 42,
                        allow: false
                    }
                )),
            "missing custom source must retain last-good active rules"
        );
    }

    #[test]
    fn reload_on_a_crashed_tab_respawns_it() {
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        state.set_active_tab_autoplay_blocked(false);
        run_until_texture(&mut state);
        helper.crash();
        run_panel(&mut state);
        assert!(state.tabs[0].session.is_crashed());

        // The Reload button on a crashed tab requests a respawn; the shell swaps in
        // a fresh session and the new page flows again.
        state.respawn_requested = true;
        assert!(state.take_respawn_request());
        let (fresh, helper2, _writer2) = live_page_session();
        state.respawn_active_with(fresh);
        assert!(
            !state.tabs[0].autoplay_blocked,
            "respawn preserves the tab's allow-autoplay override"
        );
        assert!(
            drain_control_messages(&helper2).iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::SetAutoplayBlocked { blocked: false }
            )),
            "respawned helper must receive the preserved autoplay policy"
        );
        assert!(
            !state.tabs[0].session.is_crashed(),
            "respawned session is live-ish"
        );
        assert!(
            run_until_texture(&mut state),
            "the respawned tab never uploaded a frame"
        );
    }

    // ── MENUBAR-ALL: the Browser bar dispatches its actions to real seams ───────

    #[test]
    fn the_menu_reload_on_a_live_tab_reloads_without_a_respawn() {
        // The View→Reload item on a live tab drives `WebSession::reload` (the same
        // seam the toolbar Reload button uses) and is NOT a respawn.
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Reload);
        assert!(!state.respawn_requested, "a live reload is not a respawn");
        assert!(!state.tabs[0].session.is_crashed());
    }

    #[test]
    fn the_menu_open_address_loads_the_typed_draft_on_the_live_tab() {
        // Page → Open Typed Address drives `WebSession::load` (the toolbar Go
        // button's exact seam). The fake helper answers a Load with a fresh
        // frame + PaintReady, so observing a new frame proves the menu action
        // reached the live session — not a parallel path (§6).
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state); // drain the initial frame
        state.address = "https://example.com/".to_owned();
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenAddress);
        assert!(
            wait_for_fresh_frame(&mut state),
            "OpenAddress reached the helper (a fresh frame arrived for the load)"
        );
    }

    #[test]
    fn the_menu_open_address_uses_the_http_prompt() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.address = "http://plain.example/".to_owned();
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenAddress);

        assert_eq!(
            state.insecure_prompt.as_deref(),
            Some("http://plain.example/")
        );
        assert!(
            !state.tabs[0].session.nav().loading,
            "menu OpenAddress pauses on explicit HTTP just like toolbar Go"
        );
    }

    #[test]
    fn the_privacy_menu_clear_current_tab_returns_to_the_dashboard() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.address = "https://example.com/".to_owned();
        state.submit_address();
        assert!(
            wait_for_fresh_frame(&mut state),
            "precondition: loaded page reached helper"
        );
        state.dashboard_query = "leftover".to_owned();
        state.insecure_prompt = Some("http://plain.example/".to_owned());
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::ClearCurrentTabData,
        );

        assert_eq!(state.address, NEW_TAB_URL);
        assert!(state.dashboard_query.is_empty());
        assert_eq!(state.insecure_prompt, None);
        assert!(
            state.tabs[0].autoplay_blocked,
            "clearing tab data returns the tab to the block-all autoplay default"
        );
        assert!(
            wait_for_fresh_frame(&mut state),
            "clear action loaded about:blank into the helper"
        );
        assert!(
            run_panel(&mut state),
            "new-tab dashboard renders after clear"
        );
    }

    #[test]
    fn clear_all_browsing_data_forgets_history_downloads_and_reopen_stack() {
        let mut state = WebState::default();
        state
            .history
            .record("https://visited.example/", "Visited", 1000);
        state.closed_tabs.push(ClosedTab {
            url: "https://closed.example/".into(),
            title: "Closed".into(),
            engine: BrowserEngine::Servo,
        });
        // Site data: a saved login + a permission grant must also be forgotten.
        state.save_login("saved.example", "alice", "pw");
        state.login_user_draft = "draft-user".to_owned();
        state.login_pass_draft = "draft-pass".to_owned();
        state.pending_login_save = Some(PendingLoginSave {
            tab_id: 0,
            host: "pending.example".to_owned(),
            username: "pending-user".to_owned(),
            password: "pending-pass".to_owned(),
        });
        state.grant_permission("https://granted.example", 0);
        assert!(!state.history.is_empty());
        assert_eq!(state.closed_tabs.len(), 1);
        assert_eq!(state.session_logins.len(), 1);
        assert!(state.pending_login_save.is_some());
        assert!(state.is_permission_granted("https://granted.example", 0));

        // Drive it through the real Privacy-menu action, not the private method.
        let ctx = egui::Context::default();
        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::ClearAllBrowsingData,
        );

        assert!(state.history.is_empty(), "history forgotten");
        assert!(state.closed_tabs.is_empty(), "reopen stack forgotten");
        assert!(state.session_logins.is_empty(), "saved logins forgotten");
        assert!(
            state.login_user_draft.is_empty(),
            "login user draft forgotten"
        );
        assert!(
            state.login_pass_draft.is_empty(),
            "login password draft forgotten"
        );
        assert!(
            state.pending_login_save.is_none(),
            "pending captured login forgotten"
        );
        assert!(
            !state.is_permission_granted("https://granted.example", 0),
            "permission grants forgotten"
        );
        assert_eq!(state.address, NEW_TAB_URL, "returns to the new-tab surface");
    }

    #[test]
    fn clear_all_browsing_data_publishes_full_session_audit_event() {
        use mde_web_preview_client::EventMsg;

        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper) = raw_session_pair();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        write_helper_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://portal.example/admin".to_owned(),
            },
        );
        write_helper_event(&helper, &EventMsg::Title("Admin Portal".to_owned()));
        state.tabs[0].session.poll();
        state
            .history
            .record("https://visited.example/", "Visited", 1000);
        state.download_jobs.push(transfer_fixture(
            "browser-audit",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            1010,
        ));
        state.closed_tabs.push(ClosedTab {
            url: "https://closed.example/".into(),
            title: "Closed".into(),
            engine: BrowserEngine::Servo,
        });
        state.save_login("saved.example", "alice", "pw");
        state.grant_permission("https://granted.example", 0);

        state.clear_all_browsing_data();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_BROWSING_DATA_CLEAR, None)
            .expect("list browsing-data clear events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("clear-all body"))
                .expect("valid JSON");
        assert_eq!(event["op"], "browser_browsing_data_clear");
        assert_eq!(event["decision"], "clear");
        assert_eq!(event["enforcement"], "session_memory_only");
        assert_eq!(event["scope"], "all_session");
        assert_eq!(event["engine"], "servo");
        assert_eq!(event["active_url"], "https://portal.example/admin");
        assert_eq!(event["active_host"], "portal.example");
        assert_eq!(event["active_title"], "Admin Portal");
        assert_eq!(event["history_entries"], 1);
        assert_eq!(event["downloads"], 1);
        assert_eq!(event["reopen_entries"], 1);
        assert_eq!(event["saved_logins"], 1);
        assert_eq!(event["permission_grants"], 1);
        assert_eq!(event["source"], "browser");
        assert!(event["cleared_ms"].as_u64().is_some());
    }

    #[test]
    fn the_menu_reload_on_a_crashed_tab_requests_a_respawn() {
        // On a crashed tab the SAME View→Reload item becomes a respawn request —
        // byte-identical to the toolbar Reload's crashed-tab behaviour.
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        helper.crash();
        run_panel(&mut state); // the poll observes the crash
        assert!(state.tabs[0].session.is_crashed());
        let ctx = egui::Context::default();
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Reload);
        assert!(
            state.respawn_requested,
            "menu Reload on a crashed tab requests a respawn (like the toolbar)"
        );
    }

    // ── BOOKMARKS-10: mesh integration ─────────────────────────────────────────

    #[test]
    fn bookmark_add_body_is_the_workers_add_verb_shape() {
        let body = bookmark_add_body("https://example.com/", "Example Domain");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        // `source` is omitted so the worker mints the default `Source::Manual`.
        assert!(v.get("source").is_none());
    }

    #[test]
    fn page_actions_menu_uses_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        Style::install(&ctx);

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::scope(ui, |ui| {
                    chrome_ui::page_actions_menu(
                        ui,
                        None,
                        Some(BrowserEngine::Cef),
                        false,
                        "https://example.com/",
                        "Example Domain",
                    );
                });
            });
        });
        let texts = painted_text(&out.shapes);

        for label in ["Add bookmark", "Copy URL", "Send in Chat", "Share to QR"] {
            assert!(
                texts.iter().any(|(text, color)| {
                    text.as_str() == label && *color == chrome_ui::CHROME_TEXT
                }),
                "page action `{label}` must use Browser Material text: {texts:?}"
            );
        }
        for legacy in ['\u{2606}', '\u{29C9}', '\u{1F4AC}', '\u{21AA}', '\u{21E5}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "page actions must not paint legacy glyph prefixes as text: {texts:?}"
            );
        }
        assert!(
            !texts
                .iter()
                .any(|(text, color)| { text.contains("Add bookmark") && *color == Style::TEXT }),
            "page actions must not fall back to shared shell text tokens: {texts:?}"
        );
    }

    #[test]
    fn adfilter_domain_body_matches_the_worker_allow_block_shape() {
        assert_eq!(ACTION_ADFILTER_ALLOW, "action/adfilter/allow");
        assert_eq!(ACTION_ADFILTER_BLOCK, "action/adfilter/block");
        let body = adfilter_domain_body(" news.example.com ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["domain"], "news.example.com");
    }

    #[test]
    fn browser_site_blocking_body_is_the_audit_event_shape() {
        assert_eq!(EVENT_BROWSER_SITE_BLOCKING, "event/browser/site-blocking");
        let body = browser_site_blocking_body(
            BrowserEngine::Cef,
            "https://news.example.com/story",
            "News Desk",
            "news.example.com",
            false,
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_site_blocking");
        assert_eq!(v["policy"], "adfilter_site_override");
        assert_eq!(v["decision"], "disable");
        assert_eq!(v["site_blocking"], "disabled");
        assert_eq!(v["enforcement"], "request_filter");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://news.example.com/story");
        assert_eq!(v["host"], "news.example.com");
        assert_eq!(v["title"], "News Desk");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["updated_ms"], 123);
    }

    #[test]
    fn chat_share_body_round_trips_into_a_clipboard_message_kind() {
        let body = chat_share_body("eagle", "https://example.com/", "Example Domain");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["scope"], "peer");
        assert_eq!(v["to"], "eagle");
        // Prove it's the REAL NOTIFY-CHAT message-kind (snake_case-tagged), not a
        // hand-rolled shape: the `kind` deserializes straight back into MessageKind.
        let kind: MessageKind = serde_json::from_value(v["kind"].clone()).expect("a MessageKind");
        assert!(matches!(kind, MessageKind::Clipboard { .. }));
        assert_eq!(v["kind"]["clipboard"]["preview"], "Example Domain");
        assert_eq!(v["kind"]["clipboard"]["full"], "https://example.com/");
    }

    #[test]
    fn chat_share_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = chat_share_body("eagle", "https://example.com/", "   ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["kind"]["clipboard"]["preview"], "https://example.com/");
    }

    #[test]
    fn browser_share_body_is_the_platform_handoff_shape() {
        let body = browser_share_body(
            BrowserShareTarget::Email,
            "https://example.com/",
            "Example Domain",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_share");
        assert_eq!(v["target"], "email");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        assert_eq!(v["preview"], "Example Domain");
        assert_eq!(v["source"], "browser");
        assert!(v["host"].as_str().is_some_and(|host| !host.is_empty()));
    }

    #[test]
    fn browser_share_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = browser_share_body(BrowserShareTarget::Qr, "https://example.com/", "   ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["target"], "qr");
        assert_eq!(v["title"], "");
        assert_eq!(v["preview"], "https://example.com/");
    }

    #[test]
    fn browser_send_tab_body_is_the_follow_me_handoff_shape() {
        assert_eq!(ACTION_BROWSER_SEND_TAB, "action/browser/send-tab");
        let body = browser_send_tab_body(
            BrowserSendTabTarget::Phone,
            BrowserEngine::Cef,
            "https://example.com/",
            "Example Domain",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_send_tab");
        assert_eq!(v["target"], "phone");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://example.com/");
        assert_eq!(v["title"], "Example Domain");
        assert_eq!(v["preview"], "Example Domain");
        assert_eq!(v["source"], "browser");
        assert!(v["host"].as_str().is_some_and(|host| !host.is_empty()));
    }

    #[test]
    fn browser_send_tab_node_body_has_no_default_self_destination() {
        let _env = browser_env_lock();
        let _node_target = EnvRestore::capture("MDE_BROWSER_SEND_NODE_TARGET");
        let _node_label = EnvRestore::capture("MDE_BROWSER_SEND_NODE_LABEL");
        std::env::remove_var("MDE_BROWSER_SEND_NODE_TARGET");
        std::env::remove_var("MDE_BROWSER_SEND_NODE_LABEL");
        let body = browser_send_tab_body(
            BrowserSendTabTarget::Node,
            BrowserEngine::Servo,
            "https://example.com/",
            "   ",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["target"], "node");
        assert!(v.get("target_id").is_none());
        assert!(v.get("target_label").is_none());
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["title"], "");
        assert_eq!(v["preview"], "https://example.com/");

        let bus = tempfile::tempdir().expect("temp bus");
        assert!(
            !publish_browser_send_tab(
                Some(bus.path()),
                BrowserSendTabTarget::Node,
                BrowserEngine::Servo,
                "https://example.com/",
                "Example",
            ),
            "without a remote node target, Send Tab to Node is a no-op"
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        assert!(
            persist
                .list_since(ACTION_BROWSER_SEND_TAB, None)
                .expect("list send-tab")
                .is_empty(),
            "self-target node sends must not enter the durable handoff stream"
        );
    }

    #[test]
    fn browser_send_tab_node_publish_rejects_configured_self_target() {
        let _env = browser_env_lock();
        let _node_target = EnvRestore::capture("MDE_BROWSER_SEND_NODE_TARGET");
        let _node_label = EnvRestore::capture("MDE_BROWSER_SEND_NODE_LABEL");
        std::env::set_var("MDE_BROWSER_SEND_NODE_TARGET", local_hostname());
        std::env::set_var("MDE_BROWSER_SEND_NODE_LABEL", "This node");
        let bus = tempfile::tempdir().expect("temp bus");

        assert!(!publish_browser_send_tab(
            Some(bus.path()),
            BrowserSendTabTarget::Node,
            BrowserEngine::Cef,
            "https://example.com/",
            "Example",
        ));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        assert!(persist
            .list_since(ACTION_BROWSER_SEND_TAB, None)
            .expect("list send-tab")
            .is_empty());
    }

    #[test]
    fn browser_send_tab_node_destination_can_target_a_remote_mesh_node() {
        let _env = browser_env_lock();
        let _node_target = EnvRestore::capture("MDE_BROWSER_SEND_NODE_TARGET");
        let _node_label = EnvRestore::capture("MDE_BROWSER_SEND_NODE_LABEL");
        std::env::set_var("MDE_BROWSER_SEND_NODE_TARGET", "eagle seat/1");
        std::env::set_var("MDE_BROWSER_SEND_NODE_LABEL", "Eagle Seat");

        let body = browser_send_tab_body(
            BrowserSendTabTarget::Node,
            BrowserEngine::Cef,
            "https://mesh.example/",
            "Mesh",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");

        assert_eq!(v["target"], "node");
        assert_eq!(v["target_id"], "eagle seat/1");
        assert_eq!(v["target_label"], "Eagle Seat");
        assert_eq!(v["host"], local_hostname());
        assert_eq!(v["url"], "https://mesh.example/");
    }

    #[test]
    fn browser_permission_prompt_body_records_default_deny_decision() {
        assert_eq!(
            ACTION_BROWSER_PERMISSION_PROMPT,
            "action/browser/permission-prompt"
        );
        let body = browser_permission_prompt_body(
            DevicePermissionKind::Microphone,
            BrowserEngine::Cef,
            "https://meet.example/",
            "Meeting",
            "meet.example",
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_permission_prompt");
        assert_eq!(v["permission"], "microphone");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["enforcement"], "helper_default_deny");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://meet.example/");
        assert_eq!(v["title"], "Meeting");
        assert_eq!(v["site"], "meet.example");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["updated_ms"], 123);
    }

    #[test]
    fn browser_permission_decision_body_records_runtime_decision() {
        assert_eq!(
            EVENT_BROWSER_PERMISSION_DECISION,
            "event/browser/permission-decision"
        );
        let body = browser_permission_decision_body(
            BrowserEngine::Cef,
            "https://maps.example",
            0,
            true,
            "helper_permission_prompt",
            "https://app.example/dashboard",
            "Dashboard",
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_permission_decision");
        assert_eq!(v["permission"], "geolocation");
        assert_eq!(v["permission_kind"], 0);
        assert_eq!(v["decision"], "allow");
        assert_eq!(v["grant_scope"], "session");
        assert_eq!(v["enforcement"], "helper_permission_prompt");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["origin"], "https://maps.example");
        assert_eq!(v["origin_host"], "maps.example");
        assert_eq!(v["url"], "https://app.example/dashboard");
        assert_eq!(v["title"], "Dashboard");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["decided_ms"], 123);
    }

    #[test]
    fn browser_permission_revoke_body_records_current_site_revocation() {
        assert_eq!(
            EVENT_BROWSER_PERMISSION_REVOKE,
            "event/browser/permission-revoke"
        );
        let body = browser_permission_revoke_body(
            BrowserEngine::Cef,
            "https://app.example/dashboard",
            "App Dashboard",
            "app.example",
            2,
            3,
            456,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_permission_revoke");
        assert_eq!(v["decision"], "revoke");
        assert_eq!(v["enforcement"], "session_permission_store");
        assert_eq!(v["permission_policy"], "default_deny");
        assert_eq!(v["scope"], "current_site");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://app.example/dashboard");
        assert_eq!(v["host"], "app.example");
        assert_eq!(v["title"], "App Dashboard");
        assert_eq!(v["revoked_grants"], 2);
        assert_eq!(v["cleared_prompt_decisions"], 3);
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["updated_ms"], 456);
    }

    #[test]
    fn browser_credential_body_records_redacted_session_action() {
        assert_eq!(EVENT_BROWSER_CREDENTIAL, "event/browser/credential");
        let body = browser_credential_body(
            BrowserEngine::Cef,
            "https://mail.example.com/login",
            "Mail Login",
            "mail.example.com",
            "fill",
            "password_menu",
            2,
            789,
        );
        assert!(!body.contains("alice@example.com"));
        assert!(!body.contains("hunter2"));
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_credential");
        assert_eq!(v["decision"], "fill");
        assert_eq!(v["enforcement"], "session_credential_store");
        assert_eq!(v["privacy"], "redacted");
        assert_eq!(v["scope"], "session_only");
        assert_eq!(v["trigger"], "password_menu");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://mail.example.com/login");
        assert_eq!(v["host"], "mail.example.com");
        assert_eq!(v["title"], "Mail Login");
        assert_eq!(v["credential_count"], 2);
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["updated_ms"], 789);
        assert!(v.get("username").is_none());
        assert!(v.get("password").is_none());
    }

    #[test]
    fn browser_policy_block_body_is_the_audit_event_shape() {
        assert_eq!(EVENT_BROWSER_POLICY_BLOCK, "event/browser/policy-block");
        let body = browser_policy_block_body(
            BrowserEngine::Cef,
            "https://blocked.example/private",
            "Blocked",
            "host:blocked.example",
            "chrome_load",
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_policy_block");
        assert_eq!(v["policy"], "managed_url");
        assert_eq!(v["decision"], "block");
        assert_eq!(v["enforcement"], "pre_network");
        assert_eq!(v["trigger"], "chrome_load");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://blocked.example/private");
        assert_eq!(v["host"], "blocked.example");
        assert_eq!(v["title"], "Blocked");
        assert_eq!(v["rule"], "host:blocked.example");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["blocked_ms"], 123);
    }

    #[test]
    fn browser_safe_browsing_block_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_SAFE_BROWSING_BLOCK,
            "event/browser/safe-browsing-block"
        );
        let body = browser_safe_browsing_block_body(
            BrowserEngine::Cef,
            "https://cdn.malware.test/payload",
            "Blocked",
            "malware.test",
            "download",
            456,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_safe_browsing_block");
        assert_eq!(v["policy"], "safe_browsing");
        assert_eq!(v["decision"], "block");
        assert_eq!(v["enforcement"], "pre_network");
        assert_eq!(v["trigger"], "download");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://cdn.malware.test/payload");
        assert_eq!(v["host"], "cdn.malware.test");
        assert_eq!(v["title"], "Blocked");
        assert_eq!(v["rule"], "malware.test");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["blocked_ms"], 456);
    }

    #[test]
    fn browser_policy_source_status_body_is_the_audit_state_shape() {
        assert_eq!(
            browser_safe_browsing_source_topic("node-a"),
            "state/browser-safe-browsing-source/node-a"
        );
        assert_eq!(
            browser_managed_policy_source_topic("node-a"),
            "state/browser-managed-url-policy-source/node-a"
        );
        assert_eq!(
            browser_custom_filter_rules_source_topic("node-a"),
            "state/browser-custom-filter-rules-source/node-a"
        );
        assert_eq!(
            browser_filter_list_source_topic("node-a"),
            "state/browser-filter-list-source/node-a"
        );

        let source = Path::new("/mesh/browser/safe-browsing-hosts.txt");
        let body = browser_policy_source_status_body(
            BrowserPolicySourceKind::SafeBrowsing.op(),
            BrowserPolicySourceKind::SafeBrowsing.policy(),
            source,
            BrowserPolicySourceState::Loaded.wire(),
            3,
            3,
            123,
            Some(120),
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_safe_browsing_source_status");
        assert_eq!(v["policy"], "safe_browsing");
        assert_eq!(v["state"], "loaded");
        assert_eq!(v["source_path"], "/mesh/browser/safe-browsing-hosts.txt");
        assert_eq!(v["item_count"], 3);
        assert_eq!(v["effective_count"], 3);
        assert_eq!(v["loaded_ms"], 120);
        assert!(v["error"].is_null());
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["checked_ms"], 123);

        let body = browser_policy_source_status_body(
            BrowserPolicySourceKind::ManagedUrl.op(),
            BrowserPolicySourceKind::ManagedUrl.policy(),
            Path::new("/mesh/browser/managed-url-policy.txt"),
            BrowserPolicySourceState::Error.wire(),
            0,
            2,
            456,
            Some(400),
            Some("is a directory"),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_managed_url_policy_source_status");
        assert_eq!(v["policy"], "managed_url");
        assert_eq!(v["state"], "error");
        assert_eq!(v["item_count"], 0);
        assert_eq!(v["effective_count"], 2);
        assert_eq!(v["loaded_ms"], 400);
        assert_eq!(v["error"], "is a directory");

        let body = browser_policy_source_status_body(
            BrowserPolicySourceKind::CustomFilterRules.op(),
            BrowserPolicySourceKind::CustomFilterRules.policy(),
            Path::new("/mesh/browser/custom-filter-rules.txt"),
            BrowserPolicySourceState::Loaded.wire(),
            2,
            2,
            789,
            Some(780),
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_custom_filter_rules_source_status");
        assert_eq!(v["policy"], "custom_filter_rules");
        assert_eq!(v["state"], "loaded");
        assert_eq!(v["source_path"], "/mesh/browser/custom-filter-rules.txt");
        assert_eq!(v["item_count"], 2);
        assert_eq!(v["effective_count"], 2);
        assert_eq!(v["loaded_ms"], 780);

        let body = browser_policy_source_status_body(
            BrowserPolicySourceKind::FilterLists.op(),
            BrowserPolicySourceKind::FilterLists.policy(),
            Path::new("/mesh/adfilter/compiled/engine.json"),
            BrowserPolicySourceState::Loaded.wire(),
            3,
            3,
            990,
            Some(990),
            None,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_filter_list_source_status");
        assert_eq!(v["policy"], "filter_lists");
        assert_eq!(v["state"], "loaded");
        assert_eq!(v["source_path"], "/mesh/adfilter/compiled/engine.json");
        assert_eq!(v["item_count"], 3);
        assert_eq!(v["effective_count"], 3);
    }

    #[test]
    fn browser_certificate_error_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_CERTIFICATE_ERROR,
            "event/browser/certificate-error"
        );
        let body = browser_certificate_error_body(
            BrowserEngine::Cef,
            "https://bad.example.test/login",
            "Bad Site",
            -202,
            "The certificate is not trusted (unknown authority)",
            567,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_certificate_error");
        assert_eq!(v["policy"], "tls_certificate");
        assert_eq!(v["decision"], "block");
        assert_eq!(v["enforcement"], "engine_certificate_validation");
        assert_eq!(v["reason"], "certificate_error");
        assert_eq!(v["trigger"], "top_level_navigation");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://bad.example.test/login");
        assert_eq!(v["host"], "bad.example.test");
        assert_eq!(v["title"], "Bad Site");
        assert_eq!(v["code"], -202);
        assert_eq!(
            v["message"],
            "The certificate is not trusted (unknown authority)"
        );
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["blocked_ms"], 567);
    }

    #[test]
    fn browser_insecure_download_block_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_INSECURE_DOWNLOAD_BLOCK,
            "event/browser/insecure-download-block"
        );
        let body = browser_insecure_download_block_body(
            BrowserEngine::Cef,
            "http://cdn.example.test/payload",
            "Downloads",
            "download",
            789,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_insecure_download_block");
        assert_eq!(v["policy"], "insecure_transport");
        assert_eq!(v["decision"], "block");
        assert_eq!(v["enforcement"], "pre_network");
        assert_eq!(v["reason"], "plain_http_download");
        assert_eq!(v["trigger"], "download");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "http://cdn.example.test/payload");
        assert_eq!(v["host"], "cdn.example.test");
        assert_eq!(v["title"], "Downloads");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["blocked_ms"], 789);
    }

    #[test]
    fn browser_insecure_navigation_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_INSECURE_NAVIGATION,
            "event/browser/insecure-navigation"
        );
        let body = browser_insecure_navigation_body(
            BrowserEngine::Cef,
            "http://portal.example/login",
            "Portal",
            "upgrade",
            "active_tab",
            "navigation_prompt",
            Some("https://portal.example/login"),
            790,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_insecure_navigation");
        assert_eq!(v["policy"], "insecure_transport");
        assert_eq!(v["decision"], "upgrade");
        assert_eq!(v["enforcement"], "navigation_prompt");
        assert_eq!(v["reason"], "plain_http_navigation");
        assert_eq!(v["trigger"], "active_tab");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "http://portal.example/login");
        assert_eq!(v["host"], "portal.example");
        assert_eq!(v["upgraded_url"], "https://portal.example/login");
        assert_eq!(v["title"], "Portal");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["decided_ms"], 790);
    }

    #[test]
    fn browser_mixed_content_block_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_MIXED_CONTENT_BLOCK,
            "event/browser/mixed-content-block"
        );
        let body = browser_mixed_content_block_body(
            BrowserEngine::Cef,
            "https://portal.example/dashboard",
            "http://cdn.example.test/app.js",
            "Portal",
            mde_web_preview_client::resource_to_wire(mde_web_preview_client::ResourceType::Script),
            "subresource",
            987,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_mixed_content_block");
        assert_eq!(v["policy"], "mixed_content");
        assert_eq!(v["decision"], "block");
        assert_eq!(v["enforcement"], "pre_network");
        assert_eq!(v["reason"], "plain_http_subresource");
        assert_eq!(v["trigger"], "subresource");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["page_url"], "https://portal.example/dashboard");
        assert_eq!(v["page_host"], "portal.example");
        assert_eq!(v["url"], "http://cdn.example.test/app.js");
        assert_eq!(v["host"], "cdn.example.test");
        assert_eq!(v["title"], "Portal");
        assert_eq!(v["resource"], "script");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["blocked_ms"], 987);
    }

    #[test]
    fn browser_site_data_clear_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_SITE_DATA_CLEAR,
            "event/browser/site-data-clear"
        );
        let body = browser_site_data_clear_body(
            BrowserEngine::Cef,
            "https://portal.example/admin",
            "Admin Portal",
            "portal.example",
            "current_tab",
            456,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_site_data_clear");
        assert_eq!(v["decision"], "clear");
        assert_eq!(v["enforcement"], "session_memory_only");
        assert_eq!(v["scope"], "current_tab");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["url"], "https://portal.example/admin");
        assert_eq!(v["host"], "portal.example");
        assert_eq!(v["title"], "Admin Portal");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["cleared_ms"], 456);
    }

    #[test]
    fn browser_browsing_data_clear_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_BROWSING_DATA_CLEAR,
            "event/browser/browsing-data-clear"
        );
        let body = browser_browsing_data_clear_body(
            BrowserEngine::Cef,
            "https://portal.example/admin",
            "Admin Portal",
            "portal.example",
            BrowserBrowsingDataClearCounts {
                history_entries: 2,
                downloads: 3,
                reopen_entries: 4,
                saved_logins: 5,
                permission_grants: 6,
            },
            789,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_browsing_data_clear");
        assert_eq!(v["decision"], "clear");
        assert_eq!(v["enforcement"], "session_memory_only");
        assert_eq!(v["scope"], "all_session");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["active_url"], "https://portal.example/admin");
        assert_eq!(v["active_host"], "portal.example");
        assert_eq!(v["active_title"], "Admin Portal");
        assert_eq!(v["history_entries"], 2);
        assert_eq!(v["downloads"], 3);
        assert_eq!(v["reopen_entries"], 4);
        assert_eq!(v["saved_logins"], 5);
        assert_eq!(v["permission_grants"], 6);
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["cleared_ms"], 789);
    }

    #[test]
    fn browser_download_danger_body_is_the_audit_event_shape() {
        assert_eq!(
            EVENT_BROWSER_DOWNLOAD_DANGER,
            "event/browser/download-danger"
        );
        let body = browser_download_danger_body(
            42,
            "https://files.example.test/setup.exe",
            "setup.exe",
            "discard",
            123,
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_download_danger");
        assert_eq!(v["decision"], "discard");
        assert_eq!(v["enforcement"], "dangerous_file_gate");
        assert_eq!(v["reason"], "dangerous_extension");
        assert_eq!(v["download_id"], 42);
        assert_eq!(v["url"], "https://files.example.test/setup.exe");
        assert_eq!(v["host"], "files.example.test");
        assert_eq!(v["filename"], "setup.exe");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["node"], local_hostname());
        assert_eq!(v["updated_ms"], 123);
    }

    #[test]
    fn browser_power_mode_view_source_opens_source_in_a_foreground_tab() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::TogglePowerMode,
        );
        assert!(state.power_mode);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::OpenViewSource);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "view-source:https://example.test/".to_owned(),
            })
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Power mode: opening page source")
        );
    }

    #[test]
    fn browser_power_mode_device_permission_prompt_records_default_deny_handoff() {
        let (session, _helper, _writer) = live_page_session();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        state.power_mode = true;
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::PromptCameraPermission,
        );

        assert!(
            state
                .active_site_permission_summary()
                .is_some_and(|summary| summary.contains(
                    "example.test: camera denied; sensitive prompts stay blocked by default"
                ) && !summary.contains("helper")),
            "prompt history should be reflected in the active-site permission summary"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some(
                "Camera prompt denied for example.test; sensitive prompts stay blocked by default"
            )
        );
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| !notice.contains("helper")),
            "permission prompt notice must not expose helper internals"
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let entries = persist
            .list_since(ACTION_BROWSER_PERMISSION_PROMPT, None)
            .expect("read permission prompt action");
        assert_eq!(entries.len(), 1);
        let body = entries[0].body.as_deref().expect("permission prompt body");
        let v: serde_json::Value = serde_json::from_str(body).expect("permission prompt body JSON");
        assert_eq!(v["op"], "browser_permission_prompt");
        assert_eq!(v["permission"], "camera");
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["enforcement"], "helper_default_deny");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["site"], "example.test");
        assert_eq!(v["url"], "https://example.test/");
    }

    #[test]
    fn chromium_devtools_target_json_selects_active_page_frontend() {
        let body = serde_json::json!([
            {
                "type": "page",
                "url": "https://other.example/",
                "devtoolsFrontendUrl": "/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/OTHER",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/OTHER"
            },
            {
                "type": "page",
                "url": "https://example.test/app",
                "webSocketDebuggerUrl": "ws://127.0.0.1:9222/devtools/page/ACTIVE"
            }
        ])
        .to_string();

        let selected =
            chromium_devtools_frontend_from_list("https://example.test/app", &body).unwrap();

        assert_eq!(
            selected.as_deref(),
            Some("http://127.0.0.1:9222/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/ACTIVE")
        );
        let fallback =
            chromium_devtools_frontend_from_list("https://missing.example/", &body).unwrap();
        assert_eq!(
            fallback.as_deref(),
            Some("http://127.0.0.1:9222/devtools/inspector.html?ws=127.0.0.1:9222/devtools/page/OTHER")
        );
    }

    #[test]
    fn browser_power_mode_chromium_devtools_opens_cef_debug_portal() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.power_mode = true;
        state.tabs[state.active].engine = BrowserEngine::Cef;
        let ctx = egui::Context::default();

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::OpenChromiumDevtools,
        );

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: CEF_DEVTOOLS_URL.to_owned(),
            })
        );
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice
                    .starts_with("Power mode: opening Chromium DevTools target list")),
            "unexpected DevTools notice: {:?}",
            state.capture_notice
        );

        state.tabs[state.active].engine = BrowserEngine::Servo;
        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::OpenChromiumDevtools,
        );
        assert_eq!(state.take_open_request(), None);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Chromium DevTools requires a live Chromium tab")
        );
    }

    // ── DD-2: the compact toolbar's Stop control stays CEF-only ────────────────
    //
    // Servo's embedding API (the pinned `servo`/`servo-embedder-traits`/
    // `servo-constellation-traits` 0.3.0 crates.io publications) exposes no
    // stop/cancel-navigation primitive anywhere in its reachable surface
    // (investigated 2026-07-10 — see `can_show_stop_control`'s doc comment).
    // These lock in the honest degrade: a loading Servo tab must never present
    // a Stop control that would silently do nothing when clicked.

    #[test]
    fn a_loading_cef_tab_shows_a_real_stop_control() {
        assert!(
            can_show_stop_control(true, false, true, Some(BrowserEngine::Cef)),
            "CEF has a real cef_browser_t::stop_load hook, so Stop must be offered"
        );
    }

    #[test]
    fn a_loading_servo_tab_never_shows_a_fake_stop_control() {
        assert!(
            !can_show_stop_control(true, false, true, Some(BrowserEngine::Servo)),
            "Servo exposes no cancel-load hook (DD-2 2026-07-10) — a Stop button \
             here would do nothing when clicked, so it must stay honest Reload"
        );
    }

    #[test]
    fn stop_control_still_requires_a_live_loading_uncrashed_tab() {
        // Even for CEF, Stop is gated on every other precondition: no tab, no
        // in-flight load, and a crashed tab (which shows a respawn Reload
        // instead) must all fall back to the honest Reload control too.
        assert!(
            !can_show_stop_control(false, false, true, Some(BrowserEngine::Cef)),
            "no tab ⇒ no Stop"
        );
        assert!(
            !can_show_stop_control(true, false, false, Some(BrowserEngine::Cef)),
            "not loading ⇒ no Stop"
        );
        assert!(
            !can_show_stop_control(true, true, true, Some(BrowserEngine::Cef)),
            "a crashed tab shows a respawn Reload, never Stop"
        );
        assert!(
            !can_show_stop_control(true, false, true, None),
            "no active engine ⇒ no Stop"
        );
    }

    #[test]
    fn browser_session_sync_body_carries_tabs_settings_and_downloads() {
        assert_eq!(ACTION_BROWSER_SESSION_SYNC, "action/browser/session-sync");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.tabs[state.active].container = ContainerProfile::Work;
        state.tabs[state.active].display_target = DisplayTarget::Secondary;
        state.tabs[state.active].muted = true;
        state.tabs[state.active].autoplay_blocked = true;
        state.tabs[state.active].force_dark = true;
        state.tabs[state.active].user_scripts = true;
        state.tabs[state.active].user_agent = UserAgentOverride::AndroidChrome;
        state.tabs[state.active].device_profile = DeviceProfile::Phone;
        state.vertical_tabs = true;
        state.page_zoom_percent = 125;
        state.downloads_open = true;
        state.power_mode = true;
        state.speed_dial = vec![SpeedDialEntry::new(
            "Ops",
            "https://ops.mesh/",
            "Open the mesh ops console",
        )];
        let mut job = browser_output_transfer_job("/tmp/source.bin", "/tmp/dest.bin");
        job.state = TransferState::Running;
        job.progress = Some(42);
        state.download_jobs.push(job);

        let body = browser_session_sync_body(&state);
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_session_sync");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["active_index"], 0);
        assert_eq!(v["settings"]["vertical_tabs"], true);
        assert_eq!(v["settings"]["page_zoom_percent"], 125);
        assert_eq!(v["settings"]["downloads_open"], true);
        assert_eq!(v["settings"]["power_mode"], true);
        assert_eq!(v["settings"]["speed_dial"][0]["label"], "Ops");
        assert_eq!(v["settings"]["speed_dial"][0]["url"], "https://ops.mesh/");
        assert_eq!(
            v["settings"]["speed_dial"][0]["hint"],
            "Open the mesh ops console"
        );
        assert_eq!(v["tabs"][0]["engine"], "servo");
        assert_eq!(v["tabs"][0]["container"], "work");
        assert_eq!(v["tabs"][0]["display_target"], "secondary");
        assert_eq!(v["tabs"][0]["url"], "https://example.test/");
        assert_eq!(v["tabs"][0]["muted"], true);
        assert_eq!(v["tabs"][0]["autoplay_blocked"], true);
        assert_eq!(v["tabs"][0]["force_dark"], true);
        assert_eq!(v["tabs"][0]["user_scripts"], true);
        assert_eq!(v["tabs"][0]["user_agent"], "android_chrome");
        assert_eq!(v["tabs"][0]["device_profile"], "phone");
        assert_eq!(v["downloads"][0]["method"], "browser_download");
        assert_eq!(v["downloads"][0]["state"], "running");
        assert_eq!(v["downloads"][0]["progress"], 42);
    }

    #[test]
    fn browser_session_restore_enqueues_tabs_with_the_active_tab_last() {
        let body = serde_json::json!({
            "op": "browser_session_sync",
            "active_index": 1,
            "settings": {
                "future_engine": "cef",
                "vertical_tabs": true,
                "page_zoom_percent": 135,
                "find_open": true,
                "downloads_open": true,
                "power_mode": true,
                "speed_dial": [
                    {"label": "Ops", "url": "https://ops.mesh/", "hint": "Open ops"},
                    {"label": "", "url": "https://drop.example/", "hint": "drop"},
                    {"label": "No URL", "url": "", "hint": "drop"}
                ],
            },
            "tabs": [
                {"index": 0, "engine": "servo", "url": "https://first.example/"},
                {"index": 1, "engine": "cef", "url": "https://active.example/"},
                {"index": 2, "engine": "servo", "url": "https://last.example/"},
            ],
            "downloads": [],
        })
        .to_string();
        let mut state = WebState::default();

        let restored = state
            .restore_session_sync_snapshot(&body)
            .expect("restore snapshot");

        assert_eq!(restored, 3);
        assert_eq!(state.engine, BrowserEngine::Cef);
        assert!(state.vertical_tabs);
        assert_eq!(state.page_zoom_percent, 135);
        assert!(state.find_open);
        assert!(state.downloads_open);
        assert!(state.power_mode);
        assert_eq!(
            state.speed_dial,
            vec![SpeedDialEntry::new("Ops", "https://ops.mesh/", "Open ops")]
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://first.example/".to_owned(),
            })
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://last.example/".to_owned(),
            })
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://active.example/".to_owned(),
            })
        );
        assert_eq!(state.take_open_request(), None);
    }

    #[test]
    fn browser_default_uses_vertical_tabs() {
        assert!(
            WebState::default().vertical_tabs,
            "Browser defaults to the compact vertical tab rail"
        );
    }

    #[test]
    fn browser_session_restore_missing_vertical_tabs_uses_default() {
        let body = serde_json::json!({
            "op": "browser_session_sync",
            "settings": {"future_engine": "cef"},
            "tabs": [],
            "downloads": [],
        })
        .to_string();
        let mut state = WebState::default();
        state.set_vertical_tabs(false);

        state
            .restore_session_sync_snapshot(&body)
            .expect("restore snapshot");

        assert!(
            state.vertical_tabs,
            "old snapshots without vertical_tabs adopt the new default"
        );
    }

    #[test]
    fn browser_session_restore_preserves_explicit_horizontal_tabs() {
        let body = serde_json::json!({
            "op": "browser_session_sync",
            "settings": {"vertical_tabs": false},
            "tabs": [],
            "downloads": [],
        })
        .to_string();
        let mut state = WebState::default();

        state
            .restore_session_sync_snapshot(&body)
            .expect("restore snapshot");

        assert!(
            !state.vertical_tabs,
            "an explicit user-synced horizontal preference still wins"
        );
    }

    #[test]
    fn browser_session_restore_caps_eager_tabs_and_keeps_the_active_tab() {
        let active_index = (MAX_EAGER_BROWSER_STARTUP_OPEN_TABS + 3) as u64;
        let tabs: Vec<_> = (0..(MAX_EAGER_BROWSER_STARTUP_OPEN_TABS + 6))
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "engine": "cef",
                    "url": format!("https://restore.example/{index}")
                })
            })
            .collect();
        let body = serde_json::json!({
            "op": "browser_session_sync",
            "active_index": active_index,
            "settings": {"future_engine": "cef"},
            "tabs": tabs,
            "downloads": [],
        })
        .to_string();
        let mut state = WebState::default();

        let restored = state
            .restore_session_sync_snapshot(&body)
            .expect("restore snapshot");

        assert_eq!(restored, MAX_EAGER_BROWSER_STARTUP_OPEN_TABS);
        let mut restored_urls = Vec::new();
        while let Some(TabOpenIntent::NewForegroundUrl { url, .. }) = state.take_open_request() {
            restored_urls.push(url);
        }
        assert_eq!(restored_urls.len(), MAX_EAGER_BROWSER_STARTUP_OPEN_TABS);
        assert_eq!(
            restored_urls.last().map(String::as_str),
            Some("https://restore.example/11"),
            "the saved active tab stays last so it becomes foreground"
        );
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("skipped 6 older tabs")),
            "oversized restore should explain the cap: {:?}",
            state.capture_notice
        );
    }

    #[test]
    fn browser_startup_open_queue_caps_poisoned_send_tab_replay() {
        let mut state = WebState::default();
        for index in 0..(MAX_EAGER_BROWSER_STARTUP_OPEN_TABS + 5) {
            state.request_new_tab_with_url(
                BrowserEngine::Cef,
                format!("https://send.example/{index}"),
            );
        }

        state.cap_eager_startup_open_requests();

        let mut restored_urls = Vec::new();
        while let Some(TabOpenIntent::NewForegroundUrl { url, .. }) = state.take_open_request() {
            restored_urls.push(url);
        }
        assert_eq!(restored_urls.len(), MAX_EAGER_BROWSER_STARTUP_OPEN_TABS);
        assert_eq!(
            restored_urls.last().map(String::as_str),
            Some("https://send.example/7")
        );
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("skipped 5 queued tabs")),
            "oversized startup queue should explain the cap: {:?}",
            state.capture_notice
        );
    }

    #[test]
    fn browser_session_restore_rejects_the_wrong_snapshot_shape() {
        let mut state = WebState::default();
        assert!(state.restore_session_sync_snapshot("{}").is_err());
        assert!(state
            .restore_session_sync_snapshot(r#"{"op":"browser_send_tab","tabs":[]}"#)
            .is_err());
    }

    #[test]
    fn browser_startup_restore_reads_daemon_latest_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = session_sync_latest_path(root.path(), &host);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "op": "browser_session_sync",
                "active_index": 0,
                "settings": {
                    "future_engine": "cef",
                    "speed_dial": [
                        {"label": "Ops", "url": "https://ops.mesh/", "hint": "Open ops"}
                    ],
                },
                "tabs": [
                    {"index": 0, "engine": "cef", "url": "https://restored.mesh/"}
                ],
                "downloads": [],
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.restore_startup_session_once(), Some(1));

        assert_eq!(state.engine, BrowserEngine::Cef);
        assert_eq!(
            state.speed_dial,
            vec![SpeedDialEntry::new("Ops", "https://ops.mesh/", "Open ops")]
        );
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://restored.mesh/".to_owned(),
            })
        );
        assert_eq!(state.restore_startup_session_once(), None);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn browser_startup_restore_blocks_cef_when_security_update_status_is_mismatch() {
        use std::cell::Cell;

        let _env = browser_env_lock();
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let root = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = session_sync_latest_path(root.path(), &host);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "op": "browser_session_sync",
                "active_index": 0,
                "settings": {"future_engine": "cef"},
                "tabs": [
                    {"index": 0, "engine": "cef", "url": "https://restored.mesh/"}
                ],
                "downloads": [],
            })
            .to_string(),
        )
        .unwrap();

        let runtime = make_fake_cef_runtime("mde-shell-cef-mismatch-test");
        std::env::set_var(CEF_ROOT_ENV, &runtime);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_security_update_status_topic(&host);
        let status = serde_json::json!({
            "node": host,
            "state": "mismatch",
            "expected_cef_version": "149.0.6",
            "expected_chromium_version": "149.0.7827.201",
            "expected_channel": "stable",
            "active_runtime": runtime.display().to_string(),
            "installed_version": "148.0.0",
            "installed_chromium": "148.0.0.1",
            "libcef_present": true,
            "updater_state": "failed",
            "last_update_error": "sha256 mismatch",
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(&topic, Priority::Min, None, Some(&status))
            .expect("write security status");

        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_session_restore_roots(vec![root.path().to_path_buf()]);
        assert_eq!(state.restore_startup_session_once(), Some(1));
        let TabOpenIntent::NewForegroundUrl { engine, url } =
            state.take_open_request().expect("restored CEF tab intent")
        else {
            panic!("restored session should enqueue a URL tab");
        };

        let spawned = Cell::new(false);
        state.open_with(
            true,
            engine,
            url,
            std::env::current_exe().expect("test exe path"),
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not run for mismatched CEF",
                ))
            },
        );

        assert!(!spawned.get(), "mismatched CEF must gate before spawn");
        assert!(state.tabs.is_empty());
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(notice.contains("needs an update"), "{notice}");
        assert!(notice.contains("Update needed"), "{notice}");
        assert!(
            notice.contains("Target Chromium 149.0.7827.201"),
            "{notice}"
        );
        assert!(notice.contains("Installed Chromium 148.0.0.1"), "{notice}");
        assert!(
            notice.contains("Downloaded update did not pass verification"),
            "{notice}"
        );
        for raw in ["state:", "reason:", "CEF", "149.0.6", "sha256 mismatch"] {
            assert!(
                !notice.contains(raw),
                "launch gate leaked raw engine update copy {raw:?}: {notice}"
            );
        }

        std::env::remove_var(CEF_ROOT_ENV);
        let _ = std::fs::remove_dir_all(runtime);
    }

    #[test]
    fn browser_startup_restore_host_path_matches_the_daemon_sanitizer() {
        assert_eq!(sanitize_session_host("work station/1"), "work-station1");
        assert_eq!(
            session_sync_latest_path(Path::new("/mesh"), "work station/1"),
            PathBuf::from("/mesh/browser-session-sync/work-station1/latest.json")
        );
        assert_eq!(
            send_tab_inbox_dir(Path::new("/mesh"), "work station/1"),
            PathBuf::from("/mesh/browser-send-tab/node/work-station1")
        );
    }

    #[test]
    fn browser_send_tab_outbox_enqueues_local_node_tabs_and_unlinks_records() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = send_tab_inbox_dir(root.path(), &host)
            .join("source-node")
            .join("01Send.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "node",
                "target_id": host,
                "target_label": host,
                "engine": "cef",
                "url": "https://handoff.mesh/",
                "title": "Handoff",
                "preview": "Handoff",
                "source": "browser",
                "host": "source-node"
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.drain_incoming_send_tabs(), 1);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://handoff.mesh/".to_owned(),
            })
        );
        assert!(
            !path.exists(),
            "consumed send-tab records are unlinked so they do not replay"
        );
    }

    #[test]
    fn browser_send_tab_outbox_consumes_self_originated_records_without_opening_tabs() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let source = sanitize_session_host(&host);
        let body = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host.clone(),
            "target_label": host.clone(),
            "engine": "cef",
            "url": "https://self-loop.mesh/",
            "title": "Self loop",
            "preview": "Self loop",
            "source": "browser",
            "host": host.clone()
        })
        .to_string();
        let local_path = send_tab_inbox_dir(local.path(), &host)
            .join(&source)
            .join("01Self.json");
        let share_path = send_tab_inbox_dir(share.path(), &host)
            .join(&source)
            .join("01Self.json");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(share_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, &body).unwrap();
        std::fs::write(&share_path, &body).unwrap();
        let mut state = WebState::default().with_session_restore_roots(vec![
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        ]);

        assert_eq!(state.drain_incoming_send_tabs(), 0);

        assert_eq!(state.take_open_request(), None);
        assert!(
            !local_path.exists(),
            "local self-send poison is consumed during drain"
        );
        assert!(
            !share_path.exists(),
            "shared duplicate self-send poison is consumed during drain"
        );
    }

    #[test]
    fn browser_send_tab_outbox_rejects_phone_and_other_node_records() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let local_dir = send_tab_inbox_dir(root.path(), &host).join("source-node");
        std::fs::create_dir_all(&local_dir).unwrap();
        std::fs::write(
            local_dir.join("phone.json"),
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "phone",
                "target_id": host,
                "engine": "cef",
                "url": "https://phone.mesh/"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            local_dir.join("other.json"),
            serde_json::json!({
                "op": "browser_send_tab",
                "target": "node",
                "target_id": "other-node",
                "engine": "servo",
                "url": "https://other.mesh/"
            })
            .to_string(),
        )
        .unwrap();
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.drain_incoming_send_tabs(), 0);
        assert_eq!(state.take_open_request(), None);
        assert!(
            !local_dir.join("phone.json").exists(),
            "wrong-inbox phone records are tombstoned instead of retried forever"
        );
        assert!(
            !local_dir.join("other.json").exists(),
            "misrouted node records are tombstoned instead of retried forever"
        );
    }

    #[test]
    fn browser_send_tab_outbox_dedupes_local_and_shared_records() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let body = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host,
            "engine": "servo",
            "url": "https://dedupe.mesh/",
            "host": "source-node"
        })
        .to_string();
        let local_path = send_tab_inbox_dir(local.path(), &host)
            .join("source-node")
            .join("01Same.json");
        let share_path = send_tab_inbox_dir(share.path(), &host)
            .join("source-node")
            .join("01Same.json");
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(share_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, &body).unwrap();
        std::fs::write(&share_path, &body).unwrap();
        let mut state = WebState::default().with_session_restore_roots(vec![
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        ]);

        assert_eq!(state.drain_incoming_send_tabs(), 1);

        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://dedupe.mesh/".to_owned(),
            })
        );
        assert_eq!(state.take_open_request(), None);
        assert!(!local_path.exists());
        assert!(!share_path.exists());
    }

    #[test]
    fn browser_send_tab_outbox_tombstone_prevents_surviving_record_replay() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let body = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host,
            "engine": "cef",
            "url": "https://survives-unlink.mesh/",
            "host": "source-node"
        })
        .to_string();
        let share_path = send_tab_inbox_dir(share.path(), &host)
            .join("source-node")
            .join("01Replay.json");
        std::fs::create_dir_all(share_path.parent().unwrap()).unwrap();
        std::fs::write(&share_path, &body).unwrap();
        let roots = vec![local.path().to_path_buf(), share.path().to_path_buf()];
        let mut state = WebState::default().with_session_restore_roots(roots.clone());

        assert_eq!(state.drain_incoming_send_tabs(), 1);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://survives-unlink.mesh/".to_owned(),
            })
        );

        let rel_key = PathBuf::from("source-node")
            .join("01Replay.json")
            .to_string_lossy()
            .to_string();
        let record_id = send_tab_consumed_record_id(&rel_key, &body);
        assert!(
            send_tab_consumed_path(local.path(), &host, &record_id).is_file(),
            "processed send-tab records get a local replay tombstone"
        );

        std::fs::create_dir_all(share_path.parent().unwrap()).unwrap();
        std::fs::write(&share_path, &body).unwrap();
        let mut restarted = WebState::default().with_session_restore_roots(roots);

        assert_eq!(restarted.drain_incoming_send_tabs(), 0);
        assert_eq!(restarted.take_open_request(), None);
        assert!(
            !share_path.exists(),
            "a replay-suppressed surviving record is still unlinked when possible"
        );
    }

    #[test]
    fn browser_send_tab_tombstone_does_not_hide_a_new_body_at_the_same_path() {
        let local = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let path = send_tab_inbox_dir(local.path(), &host)
            .join("source-node")
            .join("01StablePath.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let first = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host,
            "engine": "servo",
            "url": "https://first.mesh/",
            "host": "source-node"
        })
        .to_string();
        std::fs::write(&path, &first).unwrap();
        let roots = vec![local.path().to_path_buf()];
        let mut state = WebState::default().with_session_restore_roots(roots.clone());
        assert_eq!(state.drain_incoming_send_tabs(), 1);
        assert_eq!(
            state.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Servo,
                url: "https://first.mesh/".to_owned(),
            })
        );

        let second = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "target_id": host,
            "engine": "cef",
            "url": "https://second.mesh/",
            "host": "source-node"
        })
        .to_string();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &second).unwrap();
        let mut restarted = WebState::default().with_session_restore_roots(roots);

        assert_eq!(restarted.drain_incoming_send_tabs(), 1);
        assert_eq!(
            restarted.take_open_request(),
            Some(TabOpenIntent::NewForegroundUrl {
                engine: BrowserEngine::Cef,
                url: "https://second.mesh/".to_owned(),
            })
        );
    }

    #[test]
    fn browser_send_tab_outbox_tombstones_malformed_self_loop_records() {
        let root = tempfile::tempdir().unwrap();
        let host = local_hostname();
        let source = sanitize_session_host(&host);
        let path = send_tab_inbox_dir(root.path(), &host)
            .join(&source)
            .join("01BadSelf.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "op": "browser_send_tab",
            "target": "node",
            "engine": "cef",
            "url": "https://self-loop.mesh/",
            "source": "browser",
            "host": host.clone()
        })
        .to_string();
        std::fs::write(&path, &body).unwrap();
        let rel_key = PathBuf::from(&source)
            .join("01BadSelf.json")
            .to_string_lossy()
            .to_string();
        let record_id = send_tab_consumed_record_id(&rel_key, &body);
        let mut state =
            WebState::default().with_session_restore_roots(vec![root.path().to_path_buf()]);

        assert_eq!(state.drain_incoming_send_tabs(), 0);

        assert_eq!(state.take_open_request(), None);
        assert!(!path.exists(), "malformed self-send poison is removed");
        assert!(
            send_tab_consumed_path(root.path(), &host, &record_id).is_file(),
            "malformed self-send poison is tombstoned for restart"
        );
        assert!(
            !send_tab_inbox_dir(root.path(), &host)
                .join(&source)
                .exists(),
            "empty self-source inbox dirs are cleaned up"
        );
    }

    #[test]
    fn omnibox_resolves_urls_hosts_and_searches() {
        assert_eq!(
            omnibox_target(" https://example.com/a "),
            Some("https://example.com/a".to_owned())
        );
        assert_eq!(
            omnibox_target("about:blank"),
            Some("about:blank".to_owned())
        );
        assert_eq!(
            omnibox_target("data:text/html,hi"),
            Some("data:text/html,hi".to_owned())
        );
        assert_eq!(
            omnibox_target("example.com"),
            Some("https://example.com".to_owned())
        );
        assert_eq!(
            omnibox_target("localhost:8080/admin"),
            Some("https://localhost:8080/admin".to_owned())
        );
        assert_eq!(
            omnibox_target("10.42.0.1:4533"),
            Some("https://10.42.0.1:4533".to_owned())
        );
        assert_eq!(
            omnibox_target("mesh browser status"),
            Some("https://search.mesh/search?q=mesh+browser+status".to_owned())
        );
        assert_eq!(
            omnibox_target("a+b & c"),
            Some("https://search.mesh/search?q=a%2Bb+%26+c".to_owned())
        );
        assert_eq!(omnibox_target("  "), None);
    }

    // ── OMNIBOX-STYLE — Chrome-style omnibox display + security chip ───────────

    #[test]
    fn omnibox_display_elides_https_and_strips_www() {
        let display = chrome_ui::omnibox_display("https://www.example.com/x");
        assert_eq!(display.scheme_shown, None, "https:// is elided");
        assert_eq!(display.host, "example.com");
        assert_eq!(display.host_emphasis, 0..display.host.len());
        assert_eq!(&display.host[display.host_emphasis.clone()], "example.com");
        assert_eq!(display.rest, "/x");
        assert_eq!(display.security, chrome_ui::SecurityLevel::Secure);
    }

    #[test]
    fn omnibox_display_keeps_http_scheme_as_a_downgrade_signal() {
        let display = chrome_ui::omnibox_display("http://example.com");
        assert_eq!(display.scheme_shown, Some("http://".to_owned()));
        assert_eq!(display.host, "example.com");
        assert_eq!(display.rest, "");
        assert_eq!(display.security, chrome_ui::SecurityLevel::NotSecure);
    }

    #[test]
    fn omnibox_display_treats_mesh_scheme_as_trusted() {
        let display = chrome_ui::omnibox_display("mesh://music.mesh");
        assert_eq!(display.scheme_shown, Some("mesh://".to_owned()));
        assert_eq!(display.host, "music.mesh");
        assert_eq!(display.security, chrome_ui::SecurityLevel::Mesh);
    }

    #[test]
    fn omnibox_display_emphasizes_the_registrable_domain_under_a_subdomain() {
        let display = chrome_ui::omnibox_display("https://foo.bar.example.com/p");
        assert_eq!(display.host, "foo.bar.example.com");
        assert_eq!(&display.host[display.host_emphasis.clone()], "example.com");
        assert_eq!(display.rest, "/p");
    }

    #[test]
    fn omnibox_display_emphasizes_a_full_two_level_suffix_registrable_domain() {
        let display = chrome_ui::omnibox_display("https://foo.co.uk/p");
        assert_eq!(display.host, "foo.co.uk");
        assert_eq!(&display.host[display.host_emphasis.clone()], "foo.co.uk");
        assert_eq!(display.rest, "/p");
    }

    #[test]
    fn omnibox_display_neutral_scheme_stays_unmodified() {
        let display = chrome_ui::omnibox_display("about:blank");
        assert_eq!(display.scheme_shown, Some("about:blank".to_owned()));
        assert_eq!(display.host, "");
        assert_eq!(display.security, chrome_ui::SecurityLevel::Neutral);
    }

    #[test]
    fn omnibox_display_empty_url_shown_as_neutral_with_no_scheme() {
        let display = chrome_ui::omnibox_display("   ");
        assert_eq!(display.scheme_shown, None);
        assert_eq!(display.host, "");
        assert_eq!(display.security, chrome_ui::SecurityLevel::Neutral);
    }

    #[test]
    fn omnibox_layout_job_covers_the_full_text_for_an_elided_https_url() {
        let font_id = chrome_ui::font_id(CHROME_FONT);
        let job = chrome_ui::omnibox_layout_job("https://www.example.com/x", font_id);
        // The elided job's text is shorter than the raw address (no `https://`,
        // no `www.`) — that mismatch is exactly why the styled read-out is
        // painted as an overlay rather than fed into the TextEdit's own
        // layouter (which must stay 1:1 with the buffer for cursor mapping).
        assert_eq!(job.text, "example.com/x");
    }

    #[test]
    fn omnibox_layout_job_uses_browser_material_text_runs() {
        let font_id = chrome_ui::font_id(CHROME_FONT);
        let job = chrome_ui::omnibox_layout_job("https://www.sub.example.com/x", font_id);
        let colors: Vec<egui::Color32> = job
            .sections
            .iter()
            .map(|section| section.format.color)
            .collect();

        assert!(
            colors.contains(&chrome_ui::CHROME_TEXT_DIM),
            "scheme, subdomain, and path should use Browser dim text: {colors:?}"
        );
        assert!(
            colors.contains(&chrome_ui::CHROME_TEXT),
            "registrable domain should use Browser primary text: {colors:?}"
        );
        assert!(
            !colors.contains(&Style::TEXT_DIM) && !colors.contains(&Style::TEXT_STRONG),
            "omnibox read-out must not fall back to shared shell text tokens: {colors:?}"
        );
    }

    #[test]
    fn security_level_tones_map_to_browser_material_colors() {
        assert_eq!(
            chrome_ui::tone_color(chrome_ui::SecurityLevel::Secure.tone()),
            chrome_ui::CHROME_TEXT_DIM
        );
        assert_eq!(
            chrome_ui::tone_color(chrome_ui::SecurityLevel::NotSecure.tone()),
            chrome_ui::CHROME_WARN
        );
        assert_eq!(
            chrome_ui::tone_color(chrome_ui::SecurityLevel::Mesh.tone()),
            chrome_ui::CHROME_PRIMARY
        );
        assert_eq!(
            chrome_ui::tone_color(chrome_ui::SecurityLevel::Neutral.tone()),
            chrome_ui::CHROME_TEXT_DIM
        );
    }

    // ── SECURITY-INFO — the site-info panel opened by the security chip ────────

    #[test]
    fn security_headline_maps_each_level_to_plain_language_copy() {
        assert_eq!(
            chrome_ui::security_headline(chrome_ui::SecurityLevel::Secure),
            "Connection is secure"
        );
        assert_eq!(
            chrome_ui::security_headline(chrome_ui::SecurityLevel::NotSecure),
            "Your connection to this site is not secure"
        );
        assert_eq!(
            chrome_ui::security_headline(chrome_ui::SecurityLevel::Mesh),
            "Mesh service: trusted overlay"
        );
        for level in [
            chrome_ui::SecurityLevel::Secure,
            chrome_ui::SecurityLevel::NotSecure,
            chrome_ui::SecurityLevel::Mesh,
            chrome_ui::SecurityLevel::Neutral,
        ] {
            assert!(chrome_ui::security_headline(level).is_ascii());
            assert!(chrome_ui::SecurityLevel::label(level).is_ascii());
            assert!(!chrome_ui::security_headline(level).contains('\u{2014}'));
            assert!(!chrome_ui::SecurityLevel::label(level).contains('\u{2014}'));
        }
        assert_eq!(
            chrome_ui::security_headline(chrome_ui::SecurityLevel::Neutral),
            "About this page"
        );
    }

    #[test]
    fn site_info_summary_host_matches_the_omnibox_displays_host_and_emphasis() {
        let url = "https://foo.example.com/x";
        let display = chrome_ui::omnibox_display(url);
        let summary = chrome_ui::site_info_summary(url);
        assert_eq!(summary.host, display.host);
        assert_eq!(summary.host_emphasis, display.host_emphasis);
        assert_eq!(summary.host, "foo.example.com");
        assert_eq!(&summary.host[summary.host_emphasis.clone()], "example.com");
    }

    #[test]
    fn site_info_summary_surfaces_idn_homograph_warning() {
        // A punycode/IDN host (xn-- prefix) trips the spoofing warning...
        let punycode = chrome_ui::site_info_summary("https://xn--pple-43d.com/")
            .confusable
            .expect("punycode host warns");
        assert_eq!(
            punycode,
            "Punycode/IDN address (xn--): verify this is the site you expect"
        );
        assert!(punycode.is_ascii());
        assert!(!punycode.contains('\u{2014}'));
        // ...a look-alike Cyrillic 'а' (U+0430) mixed with Latin trips it too...
        let confusable =
            chrome_ui::confusable_warning("\u{0430}pple.com").expect("confusable host warns");
        assert_eq!(
            confusable,
            "Look-alike letters (Cyrillic/Greek): this site may impersonate another site"
        );
        assert!(confusable.is_ascii());
        assert!(!confusable.contains('\u{2014}'));
        // ...and a plain ASCII host does not.
        assert!(chrome_ui::site_info_summary("https://example.com/")
            .confusable
            .is_none());
        assert!(chrome_ui::confusable_warning("apple.com").is_none());
    }

    #[test]
    fn site_info_summary_shows_a_cert_line_only_for_https() {
        let cert_line = chrome_ui::site_info_summary("https://example.com/")
            .cert_line
            .expect("HTTPS page reports a certificate line");
        assert_eq!(cert_line, "Certificate: valid; the connection is encrypted");
        assert!(cert_line.is_ascii());
        assert!(!cert_line.contains('\u{2014}'));
        assert!(chrome_ui::site_info_summary("http://example.com/")
            .cert_line
            .is_none());
        assert!(chrome_ui::site_info_summary("mesh://svc.mesh/")
            .cert_line
            .is_none());
        assert!(chrome_ui::site_info_summary("about:blank")
            .cert_line
            .is_none());
    }

    #[test]
    fn site_info_resource_summary_groups_active_page_resource_blocks() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                seq: 1,
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
                blocked_by: None,
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 2,
                url: "http://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 3,
                url: "http://media.example.test/poster.jpg".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 4,
                url: "https://tracker.example.test/pixel.gif".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
                blocked_by: Some("google-analytics.com".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 5,
                url: "https://cdn.malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: false,
                blocked_by: Some("safe-browsing:malware.test".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 6,
                url: "https://admin.example.test/private/audit.json".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
                allowed: false,
                blocked_by: Some(
                    "managed-policy:url:https://admin.example.test/private/".to_owned(),
                ),
            },
        ];

        let summary = chrome_ui::site_info_resource_summary(&recent);

        assert_eq!(summary.mixed_content_blocks, 2);
        assert_eq!(
            summary.mixed_content_hosts,
            vec![
                "cdn.example.test".to_owned(),
                "media.example.test".to_owned()
            ]
        );
        assert_eq!(summary.tracker_blocks, 1);
        assert_eq!(
            summary.tracker_hosts,
            vec!["tracker.example.test".to_owned()]
        );
        assert_eq!(summary.safe_browsing_blocks, 1);
        assert_eq!(summary.safe_browsing_hosts, vec!["malware.test".to_owned()]);
        assert_eq!(summary.managed_policy_blocks, 1);
        assert_eq!(
            summary.managed_policy_rules,
            vec!["url:https://admin.example.test/private/".to_owned()]
        );
    }

    #[test]
    fn site_info_permission_summary_surfaces_active_site_permission_posture() {
        let (mut session, _helper, _writer) = live_page_session();
        session.poll();
        let mut state = WebState::default();
        state.push_session(session);
        state.grant_permission("https://example.test", 0);
        state.grant_permission("https://example.test", 2);
        state.grant_permission("https://other.example", 0);
        state.site_permission_prompts.push(SitePermissionPrompt {
            host: "example.test".to_owned(),
            kind: DevicePermissionKind::Camera,
            decision: "denied",
            updated_ms: 7,
        });
        state.site_permission_prompts.push(SitePermissionPrompt {
            host: "other.example".to_owned(),
            kind: DevicePermissionKind::Microphone,
            decision: "denied",
            updated_ms: 8,
        });

        let summary =
            chrome_ui::site_info_permission_summary(&state).expect("active site permissions");

        assert_eq!(summary.host, "example.test");
        assert!(!summary.forgotten);
        assert_eq!(
            summary.session_grants,
            vec!["clipboard".to_owned(), "geolocation".to_owned()]
        );
        assert_eq!(summary.denied_prompts, vec!["camera denied".to_owned()]);

        state
            .forgotten_permission_sites
            .push("example.test".to_owned());
        assert!(
            chrome_ui::site_info_permission_summary(&state)
                .expect("active site permissions")
                .forgotten
        );
    }

    #[test]
    fn site_info_panel_opens_from_the_security_chip_and_renders_without_panicking() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        // Establish the live page (https://example.test/) in the chrome bar.
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        // Force the popup open the same way the chip's click handler does —
        // `security_chip_popup_id` is a fixed key, not a ui-path-derived one,
        // so the test doesn't need to replay the chrome bar's exact layout.
        ctx.memory_mut(|mem| mem.open_popup(chrome_ui::security_chip_popup_id()));
        // A second frame with the panel open must still paint, not panic.
        assert!(run_panel_on_ctx(&ctx, &mut state, body_input()));
        assert!(ctx.memory(|mem| mem.is_popup_open(chrome_ui::security_chip_popup_id())));
    }

    #[test]
    fn site_info_panel_renders_with_active_page_resource_and_permission_state() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                seq: 1,
                url: "http://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 2,
                url: "https://tracker.example.test/pixel.gif".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
                blocked_by: Some("google-analytics.com".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 3,
                url: "https://cdn.malware.test/payload.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: false,
                blocked_by: Some("safe-browsing:malware.test".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 4,
                url: "https://admin.example.test/private/audit.json".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
                allowed: false,
                blocked_by: Some(
                    "managed-policy:url:https://admin.example.test/private/".to_owned(),
                ),
            },
        ];
        let permissions = chrome_ui::SiteInfoPermissionSummary {
            host: "example.test".to_owned(),
            forgotten: true,
            session_grants: vec!["geolocation".to_owned()],
            denied_prompts: vec!["camera denied".to_owned()],
        };

        let out = ctx.run(body_input(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                chrome_ui::site_info_panel(
                    ui,
                    "https://example.test/",
                    &recent,
                    Some(&permissions),
                );
            });
        });
        let texts = painted_text(&out.shapes);
        let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
        assert!(!prims.is_empty());

        assert!(
            texts.iter().any(|(text, color)| {
                text == "Connection is secure" && *color == chrome_ui::CHROME_TEXT_DIM
            }),
            "security headline should use Browser dim token: {texts:?}"
        );
        assert!(
            texts.iter().any(|(text, color)| {
                text == "example.test" && *color == chrome_ui::CHROME_TEXT
            }),
            "site host emphasis should use Browser text token: {texts:?}"
        );
        for warning in [
            "Insecure content blocked",
            "Unsafe content blocked",
            "Managed policy blocked",
            "permissions were forgotten",
        ] {
            assert!(
                texts.iter().any(|(text, color)| {
                    text.contains(warning) && *color == chrome_ui::CHROME_WARN
                }),
                "`{warning}` should use Browser warn token: {texts:?}"
            );
        }
        assert!(
            texts.iter().any(|(text, color)| {
                text.contains("Sensitive capabilities are blocked by default")
                    && *color == chrome_ui::CHROME_TEXT_DIM
            }),
            "site-info explanatory copy should use Browser dim token: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                lower.contains("helper") || lower.contains("default deny")
            }),
            "site-info panel must not expose implementation policy wording: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(_, color)| *color == Style::TEXT_DIM
                || *color == Style::TEXT_STRONG
                || *color == Style::WARN),
            "site-info panel must not fall back to shared shell text tokens: {texts:?}"
        );
    }

    #[test]
    fn external_tel_urls_handoff_to_voice_without_helper_navigation() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        state.address = "tel:+15551234567".to_owned();
        state.submit_address();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_VOICE_DIAL, None)
            .expect("list voice dial actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("dial body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["peer"], "+15551234567");
        assert_eq!(v["host"], local_hostname());
        assert_eq!(v["source"], "browser");
        assert_eq!(v["url"], "tel:+15551234567");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Handed tel link to Voice")
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .all(|msg| !matches!(msg, mde_web_preview_client::ControlMsg::Load(_))),
            "external protocol handoff must not navigate the helper: {controls:?}"
        );
    }

    #[test]
    fn mailto_and_magnet_urls_publish_browser_protocol_handoffs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        state.address = "mailto:ops@example.test?subject=mesh".to_owned();
        state.submit_address();
        state.address = "magnet:?xt=urn:btih:0123456789abcdef".to_owned();
        state.submit_address();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_PROTOCOL, None)
            .expect("list browser protocol actions");
        assert_eq!(msgs.len(), 2);

        let mail: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("mailto body"))
                .expect("valid JSON");
        assert_eq!(mail["op"], "browser_protocol_handoff");
        assert_eq!(mail["source"], "browser");
        assert_eq!(mail["host"], local_hostname());
        assert_eq!(mail["scheme"], "mailto");
        assert_eq!(mail["target"], "email");
        assert_eq!(mail["url"], "mailto:ops@example.test?subject=mesh");

        let magnet: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("magnet body"))
                .expect("valid JSON");
        assert_eq!(magnet["scheme"], "magnet");
        assert_eq!(magnet["target"], "transfers");
        assert_eq!(magnet["source"], "browser");
        assert_eq!(magnet["host"], local_hostname());
        assert_eq!(magnet["url"], "magnet:?xt=urn:btih:0123456789abcdef");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Handed magnet link to Transfers")
        );

        let controls = drain_control_messages(&helper);
        assert!(
            controls
                .iter()
                .all(|msg| !matches!(msg, mde_web_preview_client::ControlMsg::Load(_))),
            "external protocol handoff must not navigate the helper: {controls:?}"
        );
    }

    #[test]
    fn browser_share_menu_actions_publish_platform_handoffs() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToPeer);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToPhone);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToEmail);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ShareToQr);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SHARE, None)
            .expect("list browser share actions");
        assert_eq!(msgs.len(), 4);
        let targets: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("share body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_share");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["url"], "https://example.test/");
                v["target"].as_str().expect("target").to_owned()
            })
            .collect();
        assert_eq!(targets, ["peer", "phone", "email", "qr"]);
    }

    #[test]
    fn browser_share_route_result_parser_accepts_daemon_qr_routes() {
        let body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HSHARE",
            "host": local_hostname(),
            "target": "qr",
            "url": "https://example.test/share",
            "title": "Example",
            "preview": "Example",
            "routed_ms": 123,
            "updated_ms": 123,
        })
        .to_string();

        let route = parse_share_route_result(&body).expect("valid share route");
        assert_eq!(route.host, local_hostname());
        assert_eq!(route.target, BrowserShareTarget::Qr);
        assert_eq!(route.url, "https://example.test/share");
        let qr = qr_share_result(route).expect("QR route renders");
        assert!(
            qr.modules.len() >= 21,
            "a real QR matrix is generated, not a placeholder"
        );
        assert!(
            qr.modules.iter().flatten().any(|dark| *dark),
            "QR matrix has dark modules"
        );

        let bad_source = body.replace("browser_share", "cloud_share");
        assert!(parse_share_route_result(&bad_source).is_err());
        let bad_target = body.replace(r#""target":"qr""#, r#""target":"fax""#);
        assert!(parse_share_route_result(&bad_target).is_err());
    }

    #[test]
    fn browser_qr_share_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_share_result_topic(&local_hostname());
        let peer_body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HPEER",
            "host": local_hostname(),
            "target": "peer",
            "url": "https://example.test/peer",
            "title": "Peer",
            "preview": "Peer",
            "routed_ms": 123,
            "updated_ms": 123,
        })
        .to_string();
        let qr_body = serde_json::json!({
            "op": "browser_share_routed",
            "source": "browser_share",
            "node": local_hostname(),
            "request_id": "01HQR",
            "host": local_hostname(),
            "target": "qr",
            "url": "https://example.test/qr",
            "title": "QR",
            "preview": "QR",
            "routed_ms": 124,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&peer_body))
            .expect("write peer share result");
        persist
            .write(&topic, Priority::Default, None, Some(&qr_body))
            .expect("write qr share result");

        state.poll_share_results();
        let latest = state.latest_qr_share.as_ref().expect("QR share displayed");
        assert_eq!(latest.url, "https://example.test/qr");
        assert_eq!(latest.request_id, "01HQR");
        assert_eq!(state.capture_notice.as_deref(), Some("QR share ready"));

        state.latest_qr_share = None;
        state.share_result_last_poll = None;
        state.poll_share_results();
        assert!(
            state.latest_qr_share.is_none(),
            "cursor prevents replaying the same QR share event"
        );
    }

    #[test]
    fn browser_send_tab_menu_actions_publish_follow_me_handoffs() {
        let _env = browser_env_lock();
        let _node_target = EnvRestore::capture("MDE_BROWSER_SEND_NODE_TARGET");
        let _node_label = EnvRestore::capture("MDE_BROWSER_SEND_NODE_LABEL");
        std::env::set_var("MDE_BROWSER_SEND_NODE_TARGET", "eagle seat/1");
        std::env::set_var("MDE_BROWSER_SEND_NODE_LABEL", "Eagle Seat");
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        run_until_texture(&mut state);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::SendTabToNode);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::SendTabToPhone);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SEND_TAB, None)
            .expect("list browser send-tab actions");
        assert_eq!(msgs.len(), 2);
        let targets: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("send-tab body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_send_tab");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["engine"], "cef");
                assert_eq!(v["url"], "https://example.test/");
                if v["target"] == "node" {
                    assert_eq!(v["target_id"], "eagle seat/1");
                    assert_eq!(v["target_label"], "Eagle Seat");
                }
                v["target"].as_str().expect("target").to_owned()
            })
            .collect();
        assert_eq!(targets, ["node", "phone"]);
    }

    #[test]
    fn browser_read_aloud_requests_page_text_and_publishes_tts_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::ReadAloud);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Read aloud: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "read aloud must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Hello from the page.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Read aloud: sent page text to the speech service")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_READ_ALOUD, None)
            .expect("list browser read-aloud actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("read-aloud body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_read_aloud");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["text"], "Hello from the page.");
        assert_eq!(v["text_chars"], 20);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn browser_read_aloud_body_clamps_page_text_for_the_bus() {
        let request = ReadAloudRequest {
            tab_index: 3,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
        };
        let body = browser_read_aloud_body(
            &request,
            &format!("{}tail", "x".repeat(READ_ALOUD_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_read_aloud");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            READ_ALOUD_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(READ_ALOUD_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn browser_translate_requests_page_text_and_publishes_private_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::TranslatePage);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translate: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "translate must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Bonjour from the page.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translate: sent page text to translation")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_TRANSLATE, None)
            .expect("list browser translate actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("translate body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_translate");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["source_lang"], "auto");
        assert_eq!(v["target_lang"], "en");
        assert_eq!(v["text"], "Bonjour from the page.");
        assert_eq!(v["text_chars"], 22);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn browser_translate_body_clamps_page_text_for_the_bus() {
        let request = TranslateRequest {
            tab_index: 2,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
            source_lang: "auto".to_owned(),
            target_lang: "es".to_owned(),
        };
        let body = browser_translate_body(
            &request,
            &format!("{}tail", "x".repeat(TRANSLATE_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_translate");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["target_lang"], "es");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            TRANSLATE_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(TRANSLATE_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn browser_offline_cache_requests_page_text_and_publishes_private_snapshot() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let ctx = egui::Context::default();
        let pdf_dir = tempfile::tempdir().expect("pdf fixture dir");
        let pdf_path = pdf_dir.path().join("mde-browser-current.pdf");
        std::fs::write(&pdf_path, b"%PDF-1.7\n% offline cache fixture\n").expect("pdf fixture");
        state.last_saved_pdf = Some(SavedPdf {
            path: pdf_path,
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        });
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 77,
                url: "https://example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 78,
                url: "https://www.google-analytics.com/collect".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);

        super::menubar::apply(
            &ctx,
            &mut state,
            super::menubar::MenuAction::SaveOfflineCopy,
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache: reading page text")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageText {
                    id: 1,
                    max_bytes: 65536,
                }
            )),
            "offline cache must request bounded page text from the helper: {controls:?}"
        );

        state.handle_page_text_event(1, "  Cached page body.  ".to_owned());
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache: saved page snapshot")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_OFFLINE_CACHE, None)
            .expect("list browser offline-cache actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("offline-cache body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_offline_cache");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["url"], "https://example.test/");
        assert_eq!(v["title"], "Example");
        assert_eq!(v["text"], "Cached page body.");
        assert_eq!(v["text_chars"], 17);
        assert_eq!(v["truncated"], false);
        let viewport = v["viewport_image"]
            .as_object()
            .expect("offline cache carries viewport image");
        assert_eq!(viewport["mime"], "image/png");
        assert_eq!(viewport["width"], testkit::FAKE_W);
        assert_eq!(viewport["height"], testkit::FAKE_H);
        let viewport_bytes = base64::engine::general_purpose::STANDARD
            .decode(viewport["data"].as_str().expect("viewport data"))
            .expect("viewport base64 decodes");
        assert!(viewport_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let archive = v["archive_mhtml"]
            .as_object()
            .expect("offline cache carries MHTML archive");
        assert_eq!(archive["mime"], "multipart/related");
        assert!(archive["filename"]
            .as_str()
            .expect("archive filename")
            .ends_with(".mhtml"));
        let archive_bytes = base64::engine::general_purpose::STANDARD
            .decode(archive["data"].as_str().expect("archive data"))
            .expect("archive base64 decodes");
        assert_eq!(
            archive["bytes"].as_u64().expect("archive bytes") as usize,
            archive_bytes.len()
        );
        let archive_text = String::from_utf8(archive_bytes).expect("archive is utf8");
        assert!(archive_text.starts_with("MIME-Version: 1.0\r\n"));
        assert!(archive_text.contains("multipart/related"));
        assert!(archive_text.contains("Cached page body."));
        let pdf = v["pdf_snapshot"]
            .as_object()
            .expect("offline cache carries current-page PDF snapshot");
        assert_eq!(pdf["mime"], "application/pdf");
        assert_eq!(pdf["filename"], "mde-browser-current.pdf");
        let pdf_bytes = base64::engine::general_purpose::STANDARD
            .decode(pdf["data"].as_str().expect("pdf data"))
            .expect("pdf base64 decodes");
        assert_eq!(
            pdf["bytes"].as_u64().expect("pdf bytes") as usize,
            pdf_bytes.len()
        );
        assert!(pdf_bytes.starts_with(b"%PDF-"));
        let resources = v["resource_manifest"]
            .as_array()
            .expect("offline cache carries resource manifest");
        assert_eq!(resources.len(), 2);
        assert_eq!(resources[0]["url"], "https://example.test/app.js");
        assert_eq!(resources[0]["resource"], "script");
        assert_eq!(resources[0]["allowed"], true);
        assert!(resources[0]["blocked_by"].is_null());
        assert_eq!(
            resources[1]["url"],
            "https://www.google-analytics.com/collect"
        );
        assert_eq!(resources[1]["resource"], "script");
        assert_eq!(resources[1]["allowed"], false);
        assert!(
            resources[1]["blocked_by"]
                .as_str()
                .is_some_and(|rule| rule.contains("google")),
            "blocked resource keeps the matching rule: {:?}",
            resources[1]["blocked_by"]
        );
    }

    #[test]
    fn browser_offline_cache_body_clamps_page_text_for_the_bus() {
        let request = OfflineCacheRequest {
            tab_index: 4,
            engine: BrowserEngine::Cef,
            url: "https://long.example/".to_owned(),
            title: "Long".to_owned(),
            viewport: None,
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let body = browser_offline_cache_body(
            &request,
            &format!("{}tail", "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)),
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_offline_cache");
        assert_eq!(v["privacy"], "offline_or_mesh_only");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["text"].as_str().expect("text").chars().count(),
            OFFLINE_CACHE_TEXT_MAX_CHARS
        );
        assert_eq!(
            v["text_chars"],
            u64::try_from(OFFLINE_CACHE_TEXT_MAX_CHARS).expect("fits")
        );
        assert_eq!(v["truncated"], true);
        let archive = v["archive_mhtml"].as_object().expect("MHTML archive");
        let archive_bytes = base64::engine::general_purpose::STANDARD
            .decode(archive["data"].as_str().expect("archive data"))
            .expect("archive base64 decodes");
        assert!(
            archive_bytes.len() <= OFFLINE_CACHE_MHTML_MAX_BYTES,
            "archive is bounded"
        );
    }

    #[test]
    fn browser_offline_cache_result_parser_is_private_and_bounded() {
        let viewport = offline_cache_viewport_image(&egui::ColorImage {
            size: [1, 1],
            pixels: vec![egui::Color32::RED],
        })
        .expect("small viewport encodes");
        let archive_request = OfflineCacheRequest {
            tab_index: 2,
            engine: BrowserEngine::Cef,
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
            viewport: Some(viewport.clone()),
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let archive = offline_cache_mhtml_archive(&archive_request, "Cached archive body", 123)
            .expect("archive encodes");
        let pdf_bytes = b"%PDF-1.7\n% cached parser fixture\n";
        let pdf_data = base64::engine::general_purpose::STANDARD.encode(pdf_bytes);
        let body = serde_json::json!({
            "op": "browser_offline_cache_record",
            "source": "browser_offline_cache",
            "node": local_hostname(),
            "cache_id": "cache-123",
            "host": local_hostname(),
            "privacy": "offline_or_mesh_only",
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "text": format!("{}tail", "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)),
            "text_chars": OFFLINE_CACHE_TEXT_MAX_CHARS + 4,
            "viewport_image": {
                "mime": viewport.mime,
                "width": viewport.width,
                "height": viewport.height,
                "data": viewport.data_base64,
            },
            "archive_mhtml": {
                "mime": archive.mime,
                "filename": archive.filename,
                "bytes": archive.bytes,
                "data": archive.data_base64,
            },
            "resource_manifest": [
                {
                    "url": "https://example.test/app.js",
                    "resource": "script",
                    "allowed": true,
                },
                {
                    "url": "https://tracker.example/pixel.gif",
                    "resource": "image",
                    "allowed": false,
                }
            ],
            "pdf_snapshot": {
                "mime": "application/pdf",
                "filename": "mde-browser-123-example.test.pdf",
                "bytes": pdf_bytes.len(),
                "data": pdf_data.clone(),
            },
            "cached_ms": 123,
        })
        .to_string();
        let result = parse_offline_cache_result(&body).expect("valid offline-cache result");
        assert_eq!(result.host, local_hostname());
        assert_eq!(result.cache_id, "cache-123");
        assert_eq!(result.tab_index, 2);
        assert_eq!(result.engine, BrowserEngine::Cef);
        assert_eq!(result.cached_ms, Some(123));
        let viewport = result.viewport.as_ref().expect("viewport image retained");
        assert_eq!(viewport.mime, "image/png");
        assert_eq!((viewport.width, viewport.height), (1, 1));
        let archive = result.archive_mhtml.as_ref().expect("archive retained");
        assert_eq!(archive.mime, "multipart/related");
        assert!(archive.filename.ends_with(".mhtml"));
        assert_eq!(
            offline_cache_archive_bytes(archive).unwrap().len(),
            archive.bytes
        );
        assert_eq!(result.resources.len(), 2);
        assert_eq!(result.resources[0].resource, "script");
        assert!(result.resources[0].allowed);
        assert_eq!(result.resources[1].resource, "image");
        assert!(!result.resources[1].allowed);
        let pdf = result.pdf_snapshot.as_ref().expect("PDF snapshot retained");
        assert_eq!(pdf.mime, "application/pdf");
        assert_eq!(pdf.filename, "mde-browser-123-example.test.pdf");
        assert_eq!(offline_cache_pdf_bytes(pdf).unwrap(), pdf_bytes);
        assert_eq!(result.text.chars().count(), OFFLINE_CACHE_TEXT_MAX_CHARS);
        assert!(!result.text.ends_with("tail"));

        let bad_source = body.replace("browser_offline_cache", "cloud_cache");
        assert!(parse_offline_cache_result(&bad_source).is_err());
        let bad_privacy = body.replace("offline_or_mesh_only", "public");
        assert!(parse_offline_cache_result(&bad_privacy).is_err());
        let bad_engine = body.replace(r#""engine":"cef""#, r#""engine":"webkit""#);
        assert!(parse_offline_cache_result(&bad_engine).is_err());
        let empty = body.replace(
            &format!(
                r#""text":"{}tail""#,
                "x".repeat(OFFLINE_CACHE_TEXT_MAX_CHARS)
            ),
            r#""text":"   ""#,
        );
        assert!(parse_offline_cache_result(&empty).is_err());
        let bad_archive_name = body.replace(".mhtml", "../bad.mhtml");
        assert!(parse_offline_cache_result(&bad_archive_name).is_err());
        let bad_resource = body.replace(r#""resource":"script""#, r#""resource":"cookie""#);
        assert!(parse_offline_cache_result(&bad_resource).is_err());
        let bad_pdf = body.replace(
            &pdf_data,
            &base64::engine::general_purpose::STANDARD.encode(b"not a pdf"),
        );
        assert!(parse_offline_cache_result(&bad_pdf).is_err());
    }

    #[test]
    fn browser_offline_cache_viewport_texture_decodes_and_caches_png() {
        let mut image = egui::ColorImage::new([4, 3], egui::Color32::TRANSPARENT);
        for y in 0..3 {
            for x in 0..4 {
                image.pixels[y * 4 + x] =
                    egui::Color32::WHITE.gamma_multiply((y * 4 + x + 1) as f32 / 12.0);
            }
        }
        let viewport = offline_cache_viewport_image(&image).expect("small viewport encodes");
        let ctx = egui::Context::default();

        let first = offline_cache_viewport_texture(&ctx, "cache-texture", &viewport)
            .expect("viewport texture decodes");
        assert_eq!(first.size(), [4, 3]);
        let second = offline_cache_viewport_texture(&ctx, "cache-texture", &viewport)
            .expect("cached viewport texture is reused");
        assert_eq!(first.id(), second.id());

        let mut mismatched = viewport.clone();
        mismatched.width = 5;
        assert!(
            offline_cache_viewport_texture(&ctx, "cache-texture-mismatch", &mismatched).is_none(),
            "decoded PNG dimensions must match the advertised viewport metadata"
        );
    }

    #[test]
    fn browser_offline_cache_archive_saves_valid_mhtml_to_disk() {
        let request = OfflineCacheRequest {
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            viewport: None,
            resources: Vec::new(),
            pdf_snapshot: None,
        };
        let archive =
            offline_cache_mhtml_archive(&request, "Archived text", 123).expect("archive encodes");
        let mut state = WebState::default();
        state.latest_offline_cache = Some(BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-archive".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            text: "Archived text".to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: Some(archive.clone()),
            pdf_snapshot: None,
            cached_ms: Some(123),
        });
        let dir = tempfile::tempdir().expect("temp archive dir");

        let path = state
            .save_latest_offline_cache_archive_to_dir(dir.path())
            .expect("archive saves");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(archive.filename.as_str())
        );
        let saved = std::fs::read(&path).expect("read saved archive");
        assert_eq!(saved.len(), archive.bytes);
        let saved = String::from_utf8(saved).expect("archive is utf8");
        assert!(saved.starts_with("MIME-Version: 1.0\r\n"));
        assert!(saved.contains("Archived text"));
    }

    #[test]
    fn browser_offline_cache_archive_missing_notice_stays_user_facing() {
        let mut state = WebState::default();
        state.latest_offline_cache = Some(BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-no-archive".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            text: "Archived text".to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: None,
            pdf_snapshot: None,
            cached_ms: Some(123),
        });

        state.save_latest_offline_cache_archive();

        let notice = state.capture_notice.as_deref().expect("archive notice");
        assert_eq!(
            notice,
            "Offline archive failed: offline copy has no saved archive"
        );
        assert!(
            !notice.contains("MHTML") && !notice.contains("mhtml"),
            "offline archive notice must not expose archive implementation details: {notice}"
        );
    }

    #[test]
    fn browser_offline_cache_pdf_snapshot_saves_valid_pdf_to_disk() {
        let pdf_bytes = b"%PDF-1.7\n% cached PDF fixture\n";
        let pdf = OfflineCachePdf {
            mime: "application/pdf".to_owned(),
            filename: "mde-browser-123-archive.example.pdf".to_owned(),
            bytes: pdf_bytes.len(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(pdf_bytes),
        };
        let mut state = WebState::default();
        state.latest_offline_cache = Some(BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-pdf".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://archive.example/".to_owned(),
            title: "Archive".to_owned(),
            text: "Archived text".to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: None,
            pdf_snapshot: Some(pdf.clone()),
            cached_ms: Some(123),
        });
        let dir = tempfile::tempdir().expect("temp pdf dir");

        let path = state
            .save_latest_offline_cache_pdf_to_dir(dir.path())
            .expect("PDF saves");

        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(pdf.filename.as_str())
        );
        assert_eq!(std::fs::read(&path).expect("read saved PDF"), pdf_bytes);
    }

    #[test]
    fn browser_security_update_status_parser_surfaces_updater_posture() {
        let body = serde_json::json!({
            "node": local_hostname(),
            "state": "mismatch",
            "expected_cef_version": "149.0.6",
            "expected_chromium_version": "149.0.7827.201",
            "expected_channel": "stable",
            "active_runtime": "/opt/mde/cef",
            "installed_version": "old",
            "installed_chromium": "old",
            "libcef_present": true,
            "updater_state": "failed",
            "last_update_ms": 123,
            "last_update_exit_code": 69,
            "last_update_error": "installer unavailable",
            "last_error": "active CEF runtime does not match packaged manifest",
            "updated_ms": 124,
        })
        .to_string();

        let status = parse_security_update_status(&body).expect("valid security status");

        assert_eq!(status.node, local_hostname());
        assert_eq!(status.state, "mismatch");
        assert_eq!(
            status.expected_chromium_version.as_deref(),
            Some("149.0.7827.201")
        );
        assert_eq!(status.installed_version.as_deref(), Some("old"));
        assert!(status.libcef_present);
        assert_eq!(status.updater_state, "failed");
        assert_eq!(status.last_update_exit_code, Some(69));
        assert!(status.is_actionable());
        assert!(parse_security_update_status(r#"{"node":"n","state":"pretend"}"#).is_err());
        assert!(parse_security_update_status(r#"{"state":"current"}"#).is_err());
    }

    #[test]
    fn browser_speech_status_parsers_surface_worker_posture() {
        let read_body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HTTS",
            "last_host": local_hostname(),
            "last_url": "https://example.test/",
            "last_title": "Example",
            "state": "unavailable",
            "last_error": "piper voice model is not installed",
            "accepted": 2,
            "spoken": 1,
            "rejected": 0,
            "last_request_ms": 123,
            "updated_ms": 124,
        })
        .to_string();
        let read_status = parse_read_aloud_status(&read_body).expect("read-aloud status");
        assert_eq!(read_status.node, local_hostname());
        assert_eq!(read_status.state, "unavailable");
        assert_eq!(read_status.last_title.as_deref(), Some("Example"));
        assert_eq!(read_status.accepted, 2);
        assert!(read_status.is_visible());
        assert!(read_status.is_actionable());
        assert_eq!(read_status.chip_label(), "Read aloud unavailable");
        assert!(parse_read_aloud_status(r#"{"node":"n","state":"pretend"}"#).is_err());

        let voice_body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HSTT",
            "last_host": local_hostname(),
            "last_url": "https://example.test/",
            "last_mode": "dictation",
            "state": "listening",
            "last_error": null,
            "accepted": 3,
            "transcribed": 2,
            "rejected": 1,
            "last_transcript_chars": 17,
            "last_request_ms": 223,
            "updated_ms": 224,
        })
        .to_string();
        let voice_status = parse_voice_command_status(&voice_body).expect("voice status");
        assert_eq!(voice_status.node, local_hostname());
        assert_eq!(voice_status.state, "listening");
        assert_eq!(voice_status.last_mode.as_deref(), Some("dictation"));
        assert_eq!(voice_status.last_transcript_chars, Some(17));
        assert!(voice_status.is_visible());
        assert!(voice_status.is_actionable());
        assert_eq!(voice_status.chip_label(), "Dictation listening");
        assert!(parse_voice_command_status(
            r#"{"node":"n","state":"listening","last_mode":"song"}"#
        )
        .is_err());
    }

    #[test]
    fn browser_passkey_status_parser_surfaces_ceremony_posture() {
        let body = serde_json::json!({
            "node": local_hostname(),
            "last_request_id": "01HPASSKEY",
            "last_host": local_hostname(),
            "last_ceremony": "create",
            "last_rp_id": "example.test",
            "state": "pending",
            "mirrored": true,
            "last_error": null,
            "accepted": 1,
            "rejected": 0,
            "last_pending_ms": 333,
            "hardware_state": "ready",
            "hardware_key_count": 1,
            "hardware_readable_count": 1,
            "hardware_ctaphid_state": "init_request_ready",
            "hardware_ctaphid_init_frame_count": 1,
            "hardware_probe_ms": 332,
            "updated_ms": 334,
        })
        .to_string();
        let status = parse_passkey_status(&body).expect("passkey status");
        assert_eq!(status.node, local_hostname());
        assert_eq!(status.state, "pending");
        assert_eq!(status.last_ceremony.as_deref(), Some("create"));
        assert_eq!(status.last_rp_id.as_deref(), Some("example.test"));
        assert!(status.mirrored);
        assert!(status.ceremony_is_visible());
        assert!(status.hardware_is_visible());
        assert_eq!(status.chip_label(), "Passkey pending");
        assert_eq!(status.tone(), ChipTone::Info);
        assert_eq!(status.hardware_state, "ready");
        assert_eq!(status.hardware_key_count, 1);
        assert_eq!(status.hardware_readable_count, 1);
        assert_eq!(status.hardware_chip_label(), "Security key ready");
        assert_eq!(status.hardware_tone(), ChipTone::Ok);
        assert!(status.ctaphid_is_visible());
        assert_eq!(status.hardware_ctaphid_state, "init_request_ready");
        assert_eq!(status.hardware_ctaphid_init_frame_count, 1);
        assert_eq!(status.ctaphid_chip_label(), "CTAP INIT framed");
        assert_eq!(status.ctaphid_tone(), ChipTone::Info);
        let body = body.replace(r#""state":"pending""#, r#""state":"asserted""#);
        let asserted = parse_passkey_status(&body).expect("asserted passkey status");
        assert_eq!(asserted.chip_label(), "Passkey asserted");
        assert_eq!(asserted.tone(), ChipTone::Ok);
        let old_body = serde_json::json!({
            "node": "n",
            "state": "idle",
            "hardware_state": "unknown",
        })
        .to_string();
        let old_status = parse_passkey_status(&old_body).expect("old passkey status");
        assert_eq!(old_status.hardware_ctaphid_state, "unknown");
        assert_eq!(old_status.hardware_ctaphid_init_frame_count, 0);
        assert!(!old_status.ctaphid_is_visible());
        assert!(parse_passkey_status(r#"{"node":"n","state":"signed"}"#).is_err());
        assert!(
            parse_passkey_status(r#"{"node":"n","state":"pending","last_ceremony":"delete"}"#)
                .is_err()
        );
        assert!(
            parse_passkey_status(r#"{"node":"n","state":"idle","hardware_state":"wedged"}"#)
                .is_err()
        );
        assert!(parse_passkey_status(
            r#"{"node":"n","state":"idle","hardware_ctaphid_state":"wedged"}"#
        )
        .is_err());
        assert!(parse_passkey_status(
            r#"{"node":"n","state":"idle","hardware_ctaphid_state":"init_request_ready"}"#
        )
        .is_err());
        assert!(
            parse_passkey_status(
                r#"{"node":"n","state":"idle","hardware_ctaphid_state":"unavailable","hardware_ctaphid_init_frame_count":1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn browser_speech_statuses_are_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let host = local_hostname();
        let read_body = serde_json::json!({
            "node": host,
            "last_request_id": "01HTTS",
            "last_host": host,
            "last_url": "https://example.test/",
            "last_title": "Example",
            "state": "speaking",
            "last_error": null,
            "accepted": 1,
            "spoken": 0,
            "rejected": 0,
            "last_request_ms": 123,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(
                &browser_read_aloud_status_topic(&host),
                Priority::Min,
                None,
                Some(&read_body),
            )
            .expect("write read-aloud status");
        let voice_body = serde_json::json!({
            "node": host,
            "last_request_id": "01HSTT",
            "last_host": host,
            "last_url": "https://example.test/",
            "last_mode": "command",
            "state": "unavailable",
            "last_error": "STT runtime is not configured",
            "accepted": 1,
            "transcribed": 0,
            "rejected": 0,
            "last_transcript_chars": null,
            "last_request_ms": 223,
            "updated_ms": 224,
        })
        .to_string();
        persist
            .write(
                &browser_voice_command_status_topic(&host),
                Priority::Min,
                None,
                Some(&voice_body),
            )
            .expect("write voice status");

        state.poll_speech_statuses();

        let read_status = state
            .latest_read_aloud_status
            .as_ref()
            .expect("read-aloud status");
        assert_eq!(read_status.state, "speaking");
        assert_eq!(read_status.chip_label(), "Reading aloud");
        let voice_status = state
            .latest_voice_command_status
            .as_ref()
            .expect("voice status");
        assert_eq!(voice_status.state, "unavailable");
        assert_eq!(
            voice_status.last_error.as_deref(),
            Some("STT runtime is not configured")
        );
        assert_eq!(voice_status.chip_label(), "Voice unavailable");
    }

    #[test]
    fn browser_passkey_status_is_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let host = local_hostname();
        let body = serde_json::json!({
            "node": host,
            "last_request_id": "01HPASSKEY",
            "last_host": host,
            "last_ceremony": "get",
            "last_rp_id": "login.example.test",
            "state": "error",
            "mirrored": false,
            "last_error": "rp_id must match the origin host or a parent domain",
            "accepted": 1,
            "rejected": 1,
            "last_pending_ms": 444,
            "hardware_state": "present_permission_denied",
            "hardware_key_count": 1,
            "hardware_readable_count": 0,
            "hardware_ctaphid_state": "unavailable",
            "hardware_ctaphid_init_frame_count": 0,
            "hardware_probe_ms": 443,
            "updated_ms": 445,
        })
        .to_string();
        persist
            .write(
                &browser_passkey_status_topic(&host),
                Priority::Min,
                None,
                Some(&body),
            )
            .expect("write passkey status");

        state.poll_passkey_status();

        let status = state
            .latest_passkey_status
            .as_ref()
            .expect("passkey status");
        assert_eq!(status.state, "error");
        assert_eq!(status.last_ceremony.as_deref(), Some("get"));
        assert_eq!(
            status.last_error.as_deref(),
            Some("rp_id must match the origin host or a parent domain")
        );
        assert_eq!(status.hardware_state, "present_permission_denied");
        assert_eq!(status.hardware_key_count, 1);
        assert_eq!(status.hardware_readable_count, 0);
        assert_eq!(status.hardware_ctaphid_state, "unavailable");
        assert_eq!(status.hardware_ctaphid_init_frame_count, 0);
        assert!(!status.ctaphid_is_visible());
        assert_eq!(status.chip_label(), "Passkey error");
        assert_eq!(status.tone(), ChipTone::Warn);
        assert_eq!(status.hardware_chip_label(), "Security key blocked");
        assert_eq!(status.hardware_tone(), ChipTone::Warn);
    }

    #[test]
    fn browser_passkey_body_adds_browser_metadata_to_helper_event() {
        let body = browser_passkey_body(
            BrowserEngine::Cef,
            r#"{
                "ceremony":"create",
                "origin":"https://login.example/register",
                "rp_id":"login.example",
                "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                "client_request_id":"mde-pk-test-1",
                "user_handle_b64url":"user_handle_123",
                "user_name":"MDE User",
                "timeout_ms":60000,
                "user_present":true
            }"#,
        )
        .expect("passkey body");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["op"], "browser_passkey");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["ceremony"], "create");
        assert_eq!(v["rp_id"], "login.example");
        assert_eq!(v["client_request_id"], "mde-pk-test-1");
        assert_eq!(v["user_name"], "MDE User");
        assert_eq!(v["timeout_ms"], 60000);
        // security-2: the user-presence signal is forwarded to the daemon.
        assert_eq!(v["user_present"], true);

        // A helper event with no presence signal forwards user_present=false, so
        // the daemon will not set the UP bit.
        let no_presence = browser_passkey_body(
            BrowserEngine::Cef,
            r#"{
                "ceremony":"get",
                "origin":"https://login.example/",
                "rp_id":"login.example",
                "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456"
            }"#,
        )
        .expect("passkey body");
        let no_presence: serde_json::Value =
            serde_json::from_str(&no_presence).expect("valid JSON");
        assert_eq!(no_presence["user_present"], false);
        assert!(browser_passkey_body(
            BrowserEngine::Servo,
            r#"{"ceremony":"delete","origin":"https://login.example","rp_id":"login.example","challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456"}"#
        )
        .is_err());
    }

    #[test]
    fn browser_passkey_helper_event_requires_shell_approval_before_daemon_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        let tab_id = state.tabs[0].id;

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PasskeyRequest {
                body: r#"{
                    "ceremony":"get",
                    "origin":"https://login.example/auth",
                    "rp_id":"login.example",
                    "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                    "client_request_id":"mde-pk-test-2",
                    "allow_credentials":["credential_id_123456"],
                    "timeout_ms":45000
                }"#
                .to_owned(),
            },
        );
        run_until_texture(&mut state);

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: approval required for login.example")
        );
        let pending = state
            .pending_passkey_consent
            .as_ref()
            .expect("passkey waits for shell approval");
        assert_eq!(pending.tab_id, tab_id);
        assert_eq!(pending.client_request_id, "mde-pk-test-2");
        assert_eq!(pending.rp_id, "login.example");
        assert_eq!(
            chrome_ui::passkey_consent_prompt_text(pending, Some(tab_id)),
            "login.example wants to use a passkey on login.example via Chromium"
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        assert!(
            persist
                .list_since(ACTION_BROWSER_PASSKEY, None)
                .expect("list passkey actions before approval")
                .is_empty(),
            "the daemon must not see a passkey ceremony before shell approval"
        );

        state.approve_pending_passkey();
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: approved for login.example")
        );
        let msgs = persist
            .list_since(ACTION_BROWSER_PASSKEY, None)
            .expect("list passkey actions");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("passkey body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["op"], "browser_passkey");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["engine"], "cef");
        assert_eq!(v["ceremony"], "get");
        assert_eq!(v["origin"], "https://login.example/auth");
        assert_eq!(v["rp_id"], "login.example");
        assert_eq!(v["client_request_id"], "mde-pk-test-2");
        assert_eq!(v["allow_credentials"][0], "credential_id_123456");
        assert_eq!(v["shell_consent"], true);
        assert_eq!(v["presence_source"], "browser_shell_prompt");
        assert_eq!(
            v["user_present"], true,
            "the Browser shell approval click is the user-presence signal"
        );
        assert_eq!(
            state.pending_passkey_requests.get("mde-pk-test-2"),
            Some(&tab_id)
        );
    }

    #[test]
    fn malformed_passkey_request_notice_uses_page_copy() {
        let mut state = WebState::default();

        state.handle_passkey_event(
            1,
            BrowserEngine::Cef,
            r#"{
                "ceremony":"get",
                "origin":"https://login.example/auth",
                "rp_id":"login.example",
                "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456"
            }"#,
        );

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: the passkey request was incomplete")
        );
        let notice = state.capture_notice.as_deref().unwrap_or_default();
        for forbidden in ["helper", "handoff", "request id", "ceremony", "JSON"] {
            assert!(
                !notice.contains(forbidden),
                "malformed passkey notice leaked implementation copy {forbidden:?}: {notice}"
            );
        }

        state.handle_passkey_event(1, BrowserEngine::Cef, r#"{"ceremony":"delete"}"#);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: this passkey action is not supported")
        );

        state.handle_passkey_event(1, BrowserEngine::Cef, r#"{"#);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: the passkey request could not be read")
        );
    }

    #[test]
    fn browser_passkey_daemon_completion_returns_to_helper_page() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PasskeyRequest {
                body: r#"{
                    "ceremony":"get",
                    "origin":"https://login.example/auth",
                    "rp_id":"login.example",
                    "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                    "client_request_id":"mde-pk-test-3",
                    "allow_credentials":["credential_id_123456"]
                }"#
                .to_owned(),
            },
        );
        run_until_texture(&mut state);
        let tab_id = state.tabs[0].id;
        state.approve_pending_passkey();
        assert_eq!(
            state.pending_passkey_requests.get("mde-pk-test-3"),
            Some(&tab_id),
            "the pending route uses the stable source tab id"
        );
        let _ = drain_control_messages(&helper);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let completion = serde_json::json!({
            "op": "browser_passkey_assertion",
            "source": "browser_passkeys",
            "node": local_hostname(),
            "request_id": "01HPASSKEY",
            "client_request_id": "mde-pk-test-3",
            "host": local_hostname(),
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://login.example/auth",
            "rp_id": "login.example",
            "credential_id_b64url": "credential_id_123456",
            "user_handle_b64url": "user_handle_123",
            "authenticator_data_b64url": "auth_data_123456",
            "client_data_json_b64url": "client_data_123456",
            "signature_b64url": "signature_123456",
            "sign_count": 1,
            "updated_ms": 777,
        })
        .to_string();
        persist
            .write(
                &browser_passkey_event_topic(&local_hostname()),
                Priority::Default,
                None,
                Some(&completion),
            )
            .expect("write passkey completion");

        state.passkey_result_last_poll = None;
        state.poll_passkey_results();
        let controls = drain_control_messages(&helper);
        let Some(mde_web_preview_client::ControlMsg::CompletePasskey { body }) =
            controls.iter().find(|msg| {
                matches!(
                    msg,
                    mde_web_preview_client::ControlMsg::CompletePasskey { .. }
                )
            })
        else {
            panic!("expected CompletePasskey control, got {controls:?}");
        };
        let returned: serde_json::Value = serde_json::from_str(body).expect("completion JSON");
        assert_eq!(returned["client_request_id"], "mde-pk-test-3");
        assert_eq!(returned["op"], "browser_passkey_assertion");
        assert!(
            !state.pending_passkey_requests.contains_key("mde-pk-test-3"),
            "completion removes the pending route"
        );
    }

    #[test]
    fn browser_passkey_denial_rejects_the_page_without_daemon_handoff() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::PasskeyRequest {
                body: r#"{
                    "ceremony":"create",
                    "origin":"https://login.example/register",
                    "rp_id":"login.example",
                    "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                    "client_request_id":"mde-pk-test-deny",
                    "user_handle_b64url":"user_handle_123456",
                    "user_name":"MDE User"
                }"#
                .to_owned(),
            },
        );
        run_until_texture(&mut state);
        state.deny_pending_passkey();
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: denied for login.example")
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        assert!(
            persist
                .list_since(ACTION_BROWSER_PASSKEY, None)
                .expect("list passkey actions")
                .is_empty(),
            "denied passkey ceremonies must not reach the daemon"
        );
        assert!(state.pending_passkey_consent.is_none());
        assert!(state.pending_passkey_requests.is_empty());
        let controls = drain_control_messages(&helper);
        let Some(mde_web_preview_client::ControlMsg::CompletePasskey { body }) =
            controls.iter().find(|msg| {
                matches!(
                    msg,
                    mde_web_preview_client::ControlMsg::CompletePasskey { .. }
                )
            })
        else {
            panic!("expected CompletePasskey denial control, got {controls:?}");
        };
        let returned: serde_json::Value = serde_json::from_str(body).expect("denial JSON");
        assert_eq!(returned["op"], "browser_passkey_denied");
        assert_eq!(returned["client_request_id"], "mde-pk-test-deny");
        assert_eq!(returned["error"], "Passkey ceremony denied by user");
    }

    #[test]
    fn browser_passkey_duplicate_pending_request_is_denied_without_replacing_prompt() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session_with_engine(session, BrowserEngine::Cef);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);

        for client_request_id in ["mde-pk-first", "mde-pk-second"] {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::PasskeyRequest {
                    body: format!(
                        r#"{{
                            "ceremony":"get",
                            "origin":"https://login.example/auth",
                            "rp_id":"login.example",
                            "challenge_b64url":"abcdefghijklmnopqrstuvwxyz123456",
                            "client_request_id":"{client_request_id}"
                        }}"#
                    ),
                },
            );
            run_until_texture(&mut state);
        }

        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Passkey: another approval is already pending")
        );
        let pending = state
            .pending_passkey_consent
            .as_ref()
            .expect("first request still waits for approval");
        assert_eq!(pending.client_request_id, "mde-pk-first");
        let controls = drain_control_messages(&helper);
        let denial = controls
            .iter()
            .find_map(|msg| match msg {
                mde_web_preview_client::ControlMsg::CompletePasskey { body } => {
                    Some(serde_json::from_str::<serde_json::Value>(body).expect("denial JSON"))
                }
                _ => None,
            })
            .expect("second request denied");
        assert_eq!(denial["client_request_id"], "mde-pk-second");
        assert_eq!(
            denial["error"],
            "Another passkey ceremony is already waiting for approval"
        );

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        assert!(
            persist
                .list_since(ACTION_BROWSER_PASSKEY, None)
                .expect("list passkey actions before approval")
                .is_empty(),
            "neither request reaches the daemon before approval"
        );
        state.approve_pending_passkey();
        let msgs = persist
            .list_since(ACTION_BROWSER_PASSKEY, None)
            .expect("list passkey actions after approval");
        assert_eq!(msgs.len(), 1);
        let approved: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("passkey body"))
                .expect("approved JSON");
        assert_eq!(approved["client_request_id"], "mde-pk-first");
    }

    #[test]
    fn browser_offline_cache_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_offline_cache_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_offline_cache_record",
            "source": "browser_offline_cache",
            "node": local_hostname(),
            "cache_id": "cache-456",
            "host": local_hostname(),
            "privacy": "offline_or_mesh_only",
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "text": "Cached page text.",
            "text_chars": 17,
            "cached_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write offline-cache result");

        state.poll_offline_cache_results();
        let latest = state
            .latest_offline_cache
            .as_ref()
            .expect("offline-cache result displayed");
        assert_eq!(latest.cache_id, "cache-456");
        assert_eq!(latest.text, "Cached page text.");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Offline cache ready: 17 characters")
        );

        state.latest_offline_cache = None;
        state.offline_cache_result_last_poll = None;
        state.poll_offline_cache_results();
        assert!(
            state.latest_offline_cache.is_none(),
            "cursor prevents replaying the same offline-cache record"
        );
    }

    #[test]
    fn browser_security_update_status_is_displayed_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_security_update_status_topic(&local_hostname());
        let body = serde_json::json!({
            "node": local_hostname(),
            "state": "current",
            "expected_cef_version": "149.0.6",
            "expected_chromium_version": "149.0.7827.201",
            "expected_channel": "stable",
            "active_runtime": "/opt/mde/cef",
            "installed_version": "149.0.6",
            "installed_chromium": "149.0.7827.201",
            "libcef_present": true,
            "updater_state": "attempted",
            "last_update_ms": 123,
            "last_update_exit_code": 0,
            "updated_ms": 124,
        })
        .to_string();
        persist
            .write(&topic, Priority::Min, None, Some(&body))
            .expect("write security status");

        state.poll_security_update_status();

        let status = state
            .latest_security_update
            .as_ref()
            .expect("security status");
        assert_eq!(status.state, "current");
        assert_eq!(status.updater_state, "attempted");
        assert_eq!(status.last_update_exit_code, Some(0));
        assert!(!status.is_actionable());
    }

    fn offline_cache_result(url: &str, text: &str) -> BrowserOfflineCacheResult {
        BrowserOfflineCacheResult {
            host: local_hostname(),
            cache_id: "cache-fallback".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Servo,
            url: url.to_owned(),
            title: "Cached Example".to_owned(),
            text: text.to_owned(),
            viewport: None,
            resources: Vec::new(),
            archive_mhtml: None,
            pdf_snapshot: None,
            cached_ms: Some(123),
        }
    }

    #[cfg(feature = "live-helper")]
    fn seed_gate_notice_for_test(state: &mut WebState) {
        state.gate_notice = Some("Browser page unavailable".to_owned());
    }

    #[cfg(not(feature = "live-helper"))]
    fn seed_gate_notice_for_test(_state: &mut WebState) {}

    #[test]
    fn browser_offline_cache_indexes_records_for_gated_page_fallback() {
        let mut state = WebState::default();
        state.address = "https://example.test/".to_owned();
        seed_gate_notice_for_test(&mut state);
        state.apply_offline_cache_result(offline_cache_result(
            "https://example.test/",
            "Cached fallback body.",
        ));

        let fallback = state
            .offline_cache_fallback_for_unavailable()
            .expect("matching offline fallback");
        assert_eq!(fallback.text, "Cached fallback body.");
        assert_eq!(
            state
                .cached_snapshot_for_url(" https://example.test/ ")
                .expect("trimmed cache URL lookup")
                .cache_id,
            "cache-fallback"
        );
        assert!(
            run_panel(&mut state),
            "gated Browser state renders cached fallback body"
        );
    }

    #[test]
    fn browser_offline_cache_copy_uses_material_action_button() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        state.address = "https://example.test/".to_owned();
        seed_gate_notice_for_test(&mut state);
        state.apply_offline_cache_result(offline_cache_result(
            "https://example.test/",
            "Cached fallback body.",
        ));

        let out = run_panel_output(&ctx, &mut state, body_input());
        let texts = painted_text(&out.shapes);
        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "Copy" && *color == chrome_ui::CHROME_TEXT),
            "offline-cache Copy action must use Browser Material text: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|(text, color)| text == "Copy" && *color == Style::TEXT),
            "offline-cache Copy action must not use shared shell button text: {texts:?}"
        );
    }

    #[test]
    fn browser_offline_cache_matches_canonical_url_aliases() {
        let mut state = WebState::default();
        state.apply_offline_cache_result(offline_cache_result(
            "HTTPS://Example.Test:443/search?b=2&a=1#section",
            "Canonical cached fallback.",
        ));

        let fallback = state
            .cached_snapshot_for_url("https://example.test/search?a=1&b=2#other")
            .expect("query-order and fragment-insensitive cache URL lookup");
        assert_eq!(fallback.text, "Canonical cached fallback.");
        assert_eq!(
            state
                .cached_snapshot_for_url("https://EXAMPLE.TEST/search?b=2&a=1")
                .expect("host casing cache URL lookup")
                .cache_id,
            "cache-fallback"
        );
    }

    #[test]
    fn browser_offline_cache_url_canonicalizer_is_conservative() {
        assert_eq!(
            canonical_http_cache_url("HTTP://Example.Test:80"),
            Some("http://example.test/".to_owned())
        );
        assert_eq!(
            canonical_http_cache_url("https://example.test/path?z=9&a=1&z=8#top"),
            Some("https://example.test/path?a=1&z=8&z=9".to_owned())
        );
        assert_eq!(
            canonical_http_cache_url("ftp://example.test/path"),
            None,
            "non-HTTP schemes stay exact-only"
        );
        assert_eq!(
            canonical_http_cache_url("https://user@example.test/path"),
            None,
            "userinfo stays exact-only instead of broadening private URLs"
        );
    }

    #[test]
    fn browser_offline_cache_renders_for_matching_crashed_tab() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.apply_offline_cache_result(offline_cache_result(
            "https://example.test/",
            "Cached crash fallback.",
        ));
        drop(helper);
        for _ in 0..20 {
            state.tabs[0].session.poll();
            if state.tabs[0].session.is_crashed() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(state.tabs[0].session.is_crashed(), "test tab crashed");
        let fallback = state
            .offline_cache_fallback_for_unavailable()
            .expect("crashed tab uses matching offline cache");
        assert_eq!(fallback.text, "Cached crash fallback.");
        assert!(
            run_panel(&mut state),
            "crashed Browser tab renders cached fallback body"
        );
        assert!(
            !state.respawn_requested,
            "rendering the offline fallback does not auto-respawn the crashed helper"
        );
    }

    #[test]
    fn browser_translation_result_parser_is_private_and_bounded() {
        let body = serde_json::json!({
            "op": "browser_translation",
            "source": "browser_translate",
            "node": local_hostname(),
            "request_id": "01HTRANSLATE",
            "host": local_hostname(),
            "tab_index": 2,
            "engine": "cef",
            "url": "https://example.test/",
            "title": "Example",
            "source_lang": "auto",
            "target_lang": "es",
            "translation": format!("{}tail", "x".repeat(TRANSLATION_RESULT_MAX_CHARS)),
            "translation_chars": TRANSLATION_RESULT_MAX_CHARS + 4,
            "updated_ms": 123,
        })
        .to_string();
        let result = parse_translation_result(&body).expect("valid translation result");
        assert_eq!(result.host, local_hostname());
        assert_eq!(result.tab_index, 2);
        assert_eq!(result.engine, BrowserEngine::Cef);
        assert_eq!(result.source_lang, "auto");
        assert_eq!(result.target_lang, "es");
        assert_eq!(
            result.translation.chars().count(),
            TRANSLATION_RESULT_MAX_CHARS
        );
        assert!(!result.translation.ends_with("tail"));

        let bad_source = body.replace("browser_translate", "cloud_translate");
        assert!(parse_translation_result(&bad_source).is_err());
        let bad_engine = body.replace(r#""engine":"cef""#, r#""engine":"webkit""#);
        assert!(parse_translation_result(&bad_engine).is_err());
        let empty = body.replace(
            &format!(
                r#""translation":"{}tail""#,
                "x".repeat(TRANSLATION_RESULT_MAX_CHARS)
            ),
            r#""translation":"   ""#,
        );
        assert!(parse_translation_result(&empty).is_err());
    }

    #[test]
    fn browser_translation_results_are_displayed_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_translation_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_translation",
            "source": "browser_translate",
            "node": local_hostname(),
            "request_id": "01HTRANSLATE",
            "host": local_hostname(),
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "source_lang": "auto",
            "target_lang": "es",
            "translation": "Hola desde la pagina.",
            "translation_chars": 21,
            "updated_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write translation result");

        state.poll_translation_results();
        let latest = state
            .latest_translation
            .as_ref()
            .expect("translation result displayed");
        assert_eq!(latest.translation, "Hola desde la pagina.");
        assert_eq!(latest.target_lang, "es");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Translation ready: 21 characters")
        );

        state.latest_translation = None;
        state.translation_result_last_poll = None;
        state.poll_translation_results();
        assert!(
            state.latest_translation.is_none(),
            "cursor prevents replaying the same translation"
        );
    }

    #[test]
    fn browser_voice_command_menu_publishes_stt_handoffs_with_tab_context() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        state.tabs[state.active].page_focused = true;
        run_until_texture(&mut state);
        // Draft an address AFTER the surface settled (the per-frame engine
        // sync has folded the committed URL) — the handoff must carry the
        // operator's draft distinctly from the engine URL.
        state.address = "https://example.test/current".to_owned();
        let ctx = egui::Context::default();

        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::VoiceCommand);
        super::menubar::apply(&ctx, &mut state, super::menubar::MenuAction::Dictate);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_VOICE_COMMAND, None)
            .expect("list browser voice command actions");
        assert_eq!(msgs.len(), 2);
        let modes: Vec<String> = msgs
            .iter()
            .map(|msg| {
                let body = msg.body.as_deref().expect("voice command body");
                let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
                assert_eq!(v["op"], "browser_voice_command");
                assert_eq!(v["source"], "browser");
                assert_eq!(v["engine"], "cef");
                assert_eq!(v["url"], "https://example.test/");
                assert_eq!(v["title"], "Example");
                assert_eq!(v["address"], "https://example.test/current");
                assert_eq!(v["focus"], "page");
                assert_eq!(v["max_transcript_chars"], 4096);
                v["mode"].as_str().expect("mode").to_owned()
            })
            .collect();
        assert_eq!(modes, ["command", "dictation"]);
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Dictation: sent voice input request")
        );
    }

    #[test]
    fn browser_voice_transcript_parser_and_command_classifier_are_conservative() {
        let body = serde_json::json!({
            "op": "browser_voice_transcript",
            "source": "browser_voice_command",
            "node": local_hostname(),
            "request_id": "01HVOICE",
            "host": local_hostname(),
            "mode": "command",
            "tab_index": 2,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "address": "https://example.test/",
            "focus": "chrome",
            "transcript": "Search this page for mesh policy.",
            "transcript_chars": 33,
            "updated_ms": 123,
        })
        .to_string();
        let result = parse_voice_transcript_result(&body).expect("valid voice result");
        assert_eq!(result.mode, VoiceCommandMode::Command);
        assert_eq!(result.tab_index, 2);
        assert_eq!(
            voice_command_action(&result.transcript),
            Some(BrowserVoiceAction::Find("mesh policy".to_owned()))
        );
        assert_eq!(
            voice_command_action("find in page status beacon"),
            Some(BrowserVoiceAction::Find("status beacon".to_owned()))
        );
        assert_eq!(
            voice_command_action("open a new tab"),
            Some(BrowserVoiceAction::NewTab)
        );
        assert_eq!(
            voice_command_action("please send my passwords"),
            None,
            "unsupported transcripts must not become browser actions"
        );
        let bad = body.replace("browser_voice_transcript", "browser_voice_action");
        assert!(parse_voice_transcript_result(&bad).is_err());
    }

    #[test]
    fn browser_voice_command_results_are_applied_once_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let topic = browser_voice_command_result_topic(&local_hostname());
        let body = serde_json::json!({
            "op": "browser_voice_transcript",
            "source": "browser_voice_command",
            "node": local_hostname(),
            "request_id": "01HVOICE",
            "host": local_hostname(),
            "mode": "command",
            "tab_index": 0,
            "engine": "servo",
            "url": "https://example.test/",
            "title": "Example",
            "address": "https://example.test/",
            "focus": "chrome",
            "transcript": "new tab",
            "transcript_chars": 7,
            "updated_ms": 123,
        })
        .to_string();
        persist
            .write(&topic, Priority::Default, None, Some(&body))
            .expect("write result");

        state.poll_voice_command_results();
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForeground(_))
            ),
            "voice command result should enqueue one foreground tab"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Voice command: new tab")
        );

        state.voice_command_result_last_poll = None;
        state.poll_voice_command_results();
        assert!(
            state.take_open_request().is_none(),
            "cursor prevents replaying the same transcript"
        );
    }

    #[test]
    fn browser_voice_dictation_result_inserts_text_only_when_page_is_focused() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.tabs[state.active].page_focused = true;
        let _ = drain_control_messages(&helper);

        state.apply_voice_transcript_result(VoiceTranscriptResult {
            host: local_hostname(),
            mode: VoiceCommandMode::Dictation,
            tab_index: 0,
            focus: "page".to_owned(),
            transcript: "hello mesh".to_owned(),
        });

        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(text)
                ) if text == "hello mesh"
            )),
            "page-focused dictation should be forwarded as committed text: {controls:?}"
        );

        state.tabs[state.active].page_focused = false;
        state.apply_voice_transcript_result(VoiceTranscriptResult {
            host: local_hostname(),
            mode: VoiceCommandMode::Dictation,
            tab_index: 0,
            focus: "page".to_owned(),
            transcript: "ignored".to_owned(),
        });
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().all(|msg| !matches!(
                msg,
                mde_web_preview_client::ControlMsg::Input(
                    mde_web_preview_client::InputEvent::Text(_)
                )
            )),
            "dictation without page focus must not type into the helper"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Dictation result ready: focus the page before dictating")
        );
    }

    #[test]
    fn browser_session_sync_publishes_once_until_the_snapshot_changes() {
        let bus = tempfile::tempdir().expect("temp bus");
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        state.push_session(session);
        state.publish_session_snapshot();
        state.publish_session_snapshot();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(ACTION_BROWSER_SESSION_SYNC, None)
            .expect("list browser session sync");
        assert_eq!(msgs.len(), 1, "unchanged snapshots are de-duped");

        state.set_vertical_tabs(false);
        state.publish_session_snapshot();
        let msgs = persist
            .list_since(ACTION_BROWSER_SESSION_SYNC, None)
            .expect("list browser session sync after change");
        assert_eq!(msgs.len(), 2, "a changed setting emits a new snapshot");
        let latest: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("sync body")).expect("valid JSON");
        assert_eq!(latest["settings"]["vertical_tabs"], false);
    }

    #[test]
    fn browser_capture_success_publishes_notify_feed_event() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let path = PathBuf::from("/tmp/mde-browser-capture.png");

        state.record_capture_success("Captured web archive", &path);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_NOTIFY_BROWSER, None)
            .expect("list browser notify events");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("notify body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["severity"], "info");
        assert_eq!(v["source"], "browser");
        assert_eq!(
            v["summary"],
            "Captured web archive: mde-browser-capture.png"
        );
        assert!(
            !v["summary"]
                .as_str()
                .expect("summary string")
                .contains("MHTML"),
            "capture summary must keep archive implementation terminology out of Browser chrome"
        );
        assert_eq!(v["detail"], "/tmp/mde-browser-capture.png");
        assert_eq!(v["action"], "action/shell/goto/browser");
    }

    #[test]
    fn suggestions_only_fetch_for_plain_search_drafts() {
        assert!(should_fetch_suggestions("mesh browser"));
        assert!(!should_fetch_suggestions("https://example.com"));
        assert!(!should_fetch_suggestions("example.com"));
        assert!(!should_fetch_suggestions("localhost:8080"));
        assert!(!should_fetch_suggestions("about:blank"));
        assert!(!should_fetch_suggestions("   "));
        assert_eq!(
            suggestions_url("a+b & c"),
            "https://search.mesh/autocompleter?q=a%2Bb+%26+c"
        );
    }

    #[test]
    fn suggestions_parser_accepts_searxng_and_opensearch_shapes() {
        assert_eq!(
            parse_suggestions_json("mesh", r#"["mesh",["mesh network","mesh browser"]]"#)
                .expect("opensearch shape"),
            ["mesh network".to_owned(), "mesh browser".to_owned()]
        );
        assert_eq!(
            parse_suggestions_json(
                "mesh",
                r#"{"suggestions":["mesh browser",{"value":"mesh terminal"},{"text":"mesh files"}]}"#
            )
            .expect("object shape"),
            [
                "mesh browser".to_owned(),
                "mesh terminal".to_owned(),
                "mesh files".to_owned()
            ]
        );
        assert_eq!(
            parse_suggestions_json("mesh", r#"["mesh","mesh browser","mesh browser",""]"#)
                .expect("dedupe"),
            ["mesh browser".to_owned()]
        );
    }

    #[test]
    fn accepting_a_suggestion_uses_the_normal_omnibox_load_path() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        state.accept_suggestion("mesh browser".to_owned());

        assert_eq!(state.address, "https://search.mesh/search?q=mesh+browser");
        assert!(
            wait_for_fresh_frame(&mut state),
            "accepted suggestion reached the helper through submit_address"
        );
    }

    #[test]
    fn history_matches_feed_the_omnibox_suggestions_even_for_url_like_drafts() {
        let mut state = WebState::default();
        state
            .history
            .record("https://example.com/mesh-docs", "Mesh Docs", 1);
        state.history.record("https://other.test/", "Other", 2);

        // A URL-like draft skips the SearXNG fetch gate entirely (no thread is
        // spawned) — the history match must still surface independently of it.
        state.address = "https://example.com/mesh".to_owned();
        assert!(!should_fetch_suggestions(&state.address));
        state.update_suggestions_for_address();
        assert_eq!(
            state.suggestions.history,
            ["https://example.com/mesh-docs".to_owned()]
        );

        // An empty (or whitespace-only) draft shows no history matches at all,
        // even though the store still holds visits.
        state.address = "   ".to_owned();
        state.update_suggestions_for_address();
        assert!(state.suggestions.history.is_empty());
    }

    #[test]
    fn accepting_a_history_hit_uses_the_normal_omnibox_load_path() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));

        // History suggestions are plain visit-URL strings, flowing through the
        // exact same accept path as a search suggestion.
        state.accept_suggestion("https://example.com/visited".to_owned());

        assert_eq!(state.address, "https://example.com/visited");
        assert!(
            wait_for_fresh_frame(&mut state),
            "accepted history suggestion reached the helper through submit_address"
        );
    }

    #[test]
    fn dedup_search_items_omits_entries_already_shown_as_history_hits() {
        let items = vec![
            "https://example.com/mesh".to_owned(),
            "mesh browser".to_owned(),
        ];
        let history = vec!["https://example.com/mesh".to_owned()];

        let deduped: Vec<String> = chrome_ui::dedup_search_items(&items, &history)
            .into_iter()
            .cloned()
            .collect();

        assert_eq!(deduped, vec!["mesh browser".to_owned()]);
    }

    #[test]
    fn next_selection_wraps_and_seeds_from_none() {
        // From nothing highlighted: Down picks the first, Up the last.
        assert_eq!(next_selection(None, 1, 3), Some(0));
        assert_eq!(next_selection(None, -1, 3), Some(2));
        // Wrap at both ends and step in the middle.
        assert_eq!(next_selection(Some(2), 1, 3), Some(0));
        assert_eq!(next_selection(Some(0), -1, 3), Some(2));
        assert_eq!(next_selection(Some(1), 1, 3), Some(2));
        // An empty list highlights nothing.
        assert_eq!(next_selection(Some(0), 1, 0), None);
        assert_eq!(next_selection(None, 1, 0), None);
    }

    #[test]
    fn inline_top_hit_preselects_only_a_genuine_completion() {
        let list = vec![
            "https://example.com/".to_string(),
            "https://other.com/".to_string(),
        ];
        // Draft is a prefix of the top hit → preselect it (case-insensitive).
        assert_eq!(inline_top_hit(&list, "https://exa"), Some(0));
        assert_eq!(inline_top_hit(&list, "HTTPS://EXA"), Some(0));
        // Empty draft → nothing.
        assert_eq!(inline_top_hit(&list, "  "), None);
        // Draft equals the top (nothing left to complete) → nothing.
        assert_eq!(inline_top_hit(&list, "https://example.com/"), None);
        // Draft is not a prefix of the top → nothing (arrows still work).
        assert_eq!(inline_top_hit(&list, "other"), None);
        // Empty list → nothing.
        assert_eq!(inline_top_hit(&[], "http"), None);
    }

    #[test]
    fn inline_completion_tail_is_only_visible_when_it_can_align_with_the_draft() {
        let list = vec!["https://example.com/".to_string()];

        assert_eq!(
            inline_completion_tail(&list, "https://exa").as_deref(),
            Some("mple.com/")
        );
        assert_eq!(
            inline_completion_tail(&list, "HTTPS://EXA").as_deref(),
            Some("mple.com/")
        );
        assert_eq!(inline_completion_tail(&list, " https://exa"), None);
        assert_eq!(inline_completion_tail(&list, "https://exa "), None);
        assert_eq!(inline_completion_tail(&list, "https://example.com/"), None);
        assert_eq!(inline_completion_tail(&list, "https://other"), None);
    }

    #[test]
    fn focused_omnibox_paints_the_inline_completion_tail() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        state.address = "https://exa".to_owned();
        state.omnibox_focused = true;
        state.suggestions.draft = state.address.clone();
        state.suggestions.history = vec!["https://example.com/".to_owned()];
        state.suggestions.selected = Some(0);
        let ctx = egui::Context::default();
        Style::install(&ctx);
        ctx.memory_mut(|mem| mem.request_focus(omnibox_widget_id()));

        let out = run_panel_output(&ctx, &mut state, body_input());
        let texts = painted_text(&out.shapes);

        assert!(
            texts
                .iter()
                .any(|(text, color)| text == "mple.com/" && *color == chrome_ui::CHROME_TEXT_DIM),
            "focused omnibox should paint the grey inline completion tail: {texts:?}"
        );
    }

    #[test]
    fn keyword_search_target_routes_configured_shortcuts() {
        let engines = default_search_engines();
        // "img sunset" → the mesh image-category search.
        assert_eq!(
            keyword_search_target("img sunset", &engines),
            Some("https://search.mesh/search?categories=images&q=sunset".to_owned())
        );
        // Case-insensitive keyword; the query is percent-encoded (space → '+').
        assert_eq!(
            keyword_search_target("VID a b", &engines),
            Some("https://search.mesh/search?categories=videos&q=a+b".to_owned())
        );
        // An unknown leading word is NOT a keyword → None (default router handles it).
        assert_eq!(keyword_search_target("cat videos", &engines), None);
        // A bare keyword with no query → None.
        assert_eq!(keyword_search_target("img", &engines), None);
        assert_eq!(keyword_search_target("img   ", &engines), None);
    }

    #[test]
    fn browser_shell_omnibox_items_include_bookmarks_history_and_web_action() {
        let mut state = WebState::default();
        state.bookmark_index = vec![BookmarkBarLink {
            title: "Mesh Docs".into(),
            url: "https://docs.mesh/".into(),
        }];
        state
            .history
            .record("https://history.mesh/", "Mesh History", 10);

        let items = state.search_omnibox_items("mesh");
        let domains: Vec<SearchDomain> = items.iter().map(|item| item.domain).collect();

        assert!(domains.contains(&SearchDomain::BrowserBookmark));
        assert!(domains.contains(&SearchDomain::BrowserHistory));
        assert!(domains.contains(&SearchDomain::WebSuggestion));
        assert!(items
            .iter()
            .any(|item| { item.domain == SearchDomain::WebSuggestion && item.payload == "mesh" }));
    }

    #[test]
    fn browser_omnibox_accepts_file_candidates_from_the_files_model() {
        let temp = tempfile::tempdir().expect("file suggestion dir");
        let file = temp.path().join("home-notes.md");
        std::fs::write(&file, b"notes").expect("file suggestion fixture");
        let file_target = FileSearchTarget {
            pane: 0,
            row: 0,
            path: Some(file.clone()),
        };
        let mut state = WebState::default();
        state.set_file_omnibox_items(vec![
            SearchItem::new(
                SearchDomain::File,
                "home-notes.md",
                file.display().to_string(),
                file_target.clone(),
            ),
            SearchItem::new(
                SearchDomain::File,
                "virtual-row",
                "local:home/virtual-row",
                FileSearchTarget {
                    pane: 0,
                    row: 1,
                    path: None,
                },
            ),
            SearchItem::new(
                SearchDomain::File,
                "home-notes duplicate",
                file.display().to_string(),
                file_target,
            ),
        ]);

        state.address = "home-notes".to_owned();
        state.update_suggestions_for_address();

        assert_eq!(state.file_omnibox_index.len(), 1);
        assert_eq!(state.suggestions.files.len(), 1);
        let file_hit = &state.suggestions.files[0];
        assert_eq!(file_hit.title, "home-notes.md");
        assert_eq!(file_hit.path, file);
        assert!(file_hit.url.starts_with("file://"));
        assert_eq!(
            state.suggestions.ordered_search_items()[0].domain,
            SearchDomain::File,
            "file-only suggestions should flow through the shared Browser search adapter"
        );
    }

    #[test]
    fn browser_shell_omnibox_target_opens_a_foreground_tab_when_empty() {
        let mut state = WebState::default();

        state.open_search_omnibox_target("mesh browser");

        assert!(matches!(
            state.open_requested.back(),
            Some(TabOpenIntent::NewForegroundUrl { url, .. })
                if url == "https://search.mesh/search?q=mesh+browser"
        ));
    }

    #[test]
    fn tab_group_color_cycles_over_the_palette() {
        // Distinct colors for successive groups, wrapping at the palette length (5).
        assert_eq!(tab_group_color(0), chrome_ui::tab_group_color(0));
        assert_eq!(tab_group_color(4), chrome_ui::tab_group_color(4));
        assert_ne!(tab_group_color(0), tab_group_color(1));
        assert_eq!(tab_group_color(0), tab_group_color(5));
        assert_eq!(tab_group_color(1), tab_group_color(6));
    }

    #[test]
    fn new_group_from_tab_assigns_then_ungroup_detaches() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(state.tabs[0].group.is_none());

        state.new_group_from_tab(0);
        assert_eq!(state.tabs[0].group, Some(0));
        assert_eq!(state.tab_groups.len(), 1);
        assert_eq!(state.tab_groups[0].color, tab_group_color(0));

        state.ungroup_tab(0);
        assert!(state.tabs[0].group.is_none());
        // The group itself remains so existing indices stay stable.
        assert_eq!(state.tab_groups.len(), 1);
    }

    #[test]
    fn suggestion_selection_commits_the_highlighted_value_in_render_order() {
        let mut s = SuggestionState::default();
        s.set_bookmark_matches(vec![BookmarkBarLink {
            title: "BM".into(),
            url: "https://bm.example/".into(),
        }]);
        s.set_file_matches(vec![BrowserFileSuggestion {
            title: "home-notes.md".into(),
            path: PathBuf::from("/home/me/home-notes.md"),
            url: "file:///home/me/home-notes.md".into(),
        }]);
        s.set_history_matches(vec!["https://hist.example/".into()]);
        s.items = vec!["search term".into()];
        // Render order: bookmark, file, history, deduped search.
        assert_eq!(
            s.ordered_commit_values(),
            vec![
                "https://bm.example/".to_string(),
                "file:///home/me/home-notes.md".to_string(),
                "https://hist.example/".to_string(),
                "search term".to_string(),
            ]
        );
        // Nothing highlighted → Enter submits the typed draft (no committed value).
        assert!(s.selected_value().is_none());
        // Arrow down walks the list; the committed value follows the highlight.
        s.move_selection(1);
        assert_eq!(s.selected_value().as_deref(), Some("https://bm.example/"));
        s.move_selection(1);
        assert_eq!(
            s.selected_value().as_deref(),
            Some("file:///home/me/home-notes.md")
        );
        s.move_selection(1);
        assert_eq!(s.selected_value().as_deref(), Some("https://hist.example/"));
        s.move_selection(1); // -> search
        s.move_selection(1); // wraps back to the first
        assert_eq!(s.selected_value().as_deref(), Some("https://bm.example/"));
    }

    #[test]
    fn browser_suggestions_emit_shared_search_items_in_commit_order() {
        let mut s = SuggestionState::default();
        s.set_bookmark_matches(vec![BookmarkBarLink {
            title: "Mesh Bookmark".into(),
            url: "https://bookmark.example/mesh".into(),
        }]);
        s.set_file_matches(vec![BrowserFileSuggestion {
            title: "mesh-plan.md".into(),
            path: PathBuf::from("/home/me/mesh-plan.md"),
            url: "file:///home/me/mesh-plan.md".into(),
        }]);
        s.set_history_matches(vec!["https://history.example/mesh".into()]);
        s.items = vec!["https://history.example/mesh".into(), "mesh browser".into()];

        let items = s.ordered_search_items();
        let domains: Vec<SearchDomain> = items.iter().map(|item| item.domain).collect();
        let payloads: Vec<&str> = items.iter().map(|item| item.payload.as_str()).collect();
        let targets: Vec<&str> = items.iter().map(|item| item.target.as_str()).collect();

        assert_eq!(
            domains,
            [
                SearchDomain::BrowserBookmark,
                SearchDomain::File,
                SearchDomain::BrowserHistory,
                SearchDomain::WebSuggestion,
            ],
            "Browser adapter must expose bookmarks, files, history, then deduped web suggestions"
        );
        assert_eq!(
            payloads,
            [
                "https://bookmark.example/mesh",
                "file:///home/me/mesh-plan.md",
                "https://history.example/mesh",
                "mesh browser",
            ],
            "payloads remain the exact values committed by Enter/click"
        );
        assert_eq!(
            targets[3],
            "https://search.mesh/search?q=mesh+browser",
            "web suggestion rows carry their real search target while keeping the typed commit payload"
        );
    }

    #[test]
    fn thumbnail_size_preserves_aspect_and_caps_width() {
        // Wider than the cap → scaled down, aspect preserved (240 * 800/1280 = 150).
        let s = chrome_ui::thumbnail_size(egui::vec2(1280.0, 800.0), 240.0);
        assert!((s.x - 240.0).abs() < 0.01);
        assert!((s.y - 150.0).abs() < 0.01);
        // Already narrower than the cap → not upscaled.
        let s2 = chrome_ui::thumbnail_size(egui::vec2(100.0, 50.0), 240.0);
        assert!((s2.x - 100.0).abs() < 0.01 && (s2.y - 50.0).abs() < 0.01);
        // A degenerate frame yields no thumbnail.
        assert_eq!(
            chrome_ui::thumbnail_size(egui::vec2(0.0, 800.0), 240.0),
            egui::Vec2::ZERO
        );
    }

    #[derive(Clone, Default)]
    struct RecordingTransfers {
        jobs: std::sync::Arc<std::sync::Mutex<Vec<TransferJob>>>,
        verbs: std::sync::Arc<std::sync::Mutex<Vec<TransferVerb>>>,
    }

    impl RecordingTransfers {
        fn with_jobs(jobs: Vec<TransferJob>) -> Self {
            Self {
                jobs: std::sync::Arc::new(std::sync::Mutex::new(jobs)),
                verbs: std::sync::Arc::default(),
            }
        }

        fn verbs(&self) -> Vec<TransferVerb> {
            self.verbs.lock().unwrap().clone()
        }

        fn set_jobs(&self, jobs: Vec<TransferJob>) {
            *self.jobs.lock().unwrap() = jobs;
        }
    }

    impl TransfersClient for RecordingTransfers {
        fn jobs(&self) -> Vec<TransferJob> {
            self.jobs.lock().unwrap().clone()
        }

        fn worker_present(&self) -> bool {
            true
        }

        fn dispatch(&self, verb: &TransferVerb) -> Result<(), String> {
            self.verbs.lock().unwrap().push(verb.clone());
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingDownloadOpener {
        opened: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>,
        revealed: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>,
    }

    impl RecordingDownloadOpener {
        fn opened(&self) -> Vec<PathBuf> {
            self.opened.lock().unwrap().clone()
        }

        fn revealed(&self) -> Vec<PathBuf> {
            self.revealed.lock().unwrap().clone()
        }
    }

    impl DownloadOpener for RecordingDownloadOpener {
        fn open_path(&self, path: &Path) -> Result<(), String> {
            self.opened.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }

        fn reveal_path(&self, path: &Path) -> Result<(), String> {
            self.revealed.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }

    fn browser_download_fixture(
        id: &str,
        source: &Path,
        dest: &Path,
        state: TransferState,
    ) -> TransferJob {
        let mut job = TransferJob::new(
            source.to_string_lossy().into_owned(),
            dest.to_string_lossy().into_owned(),
            TransferMethod::BrowserDownload,
            TransferPolicy {
                bwlimit: None,
                verify: true,
            },
        );
        job.id = id.to_owned();
        job.state = state;
        job
    }

    fn write_browser_download_manifest(
        dir: &Path,
        asset_url: &str,
        suggested_filename: &str,
        kind: Option<&str>,
    ) -> PathBuf {
        let path = dir.join("asset.download.json");
        let mut body = serde_json::json!({
            "op": "browser_media_download_request",
            "asset_url": asset_url,
            "suggested_filename": suggested_filename,
        });
        if let Some(kind) = kind {
            body["kind"] = serde_json::Value::String(kind.to_owned());
        }
        std::fs::write(&path, body.to_string()).expect("write browser download manifest");
        path
    }

    #[test]
    fn browser_download_enqueue_submits_a_verified_browser_transfer() {
        let transfers = RecordingTransfers::default();
        let id = enqueue_browser_output(&transfers, "/tmp/helper/file.bin", "/home/mm/Downloads")
            .expect("enqueue");
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.id, id);
        assert_eq!(job.source, "/tmp/helper/file.bin");
        assert_eq!(job.dest, "/home/mm/Downloads");
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert!(job.policy.verify, "browser outputs should be verified");
    }

    #[test]
    fn scraper_output_batch_enqueues_one_transfer_per_file() {
        let transfers = RecordingTransfers::default();
        let ids = enqueue_browser_output_batch(
            &transfers,
            &[
                "/tmp/scrape/page.json".to_owned(),
                "/tmp/scrape/page.md".to_owned(),
            ],
            "/home/mm/Exports",
        )
        .expect("enqueue batch");
        assert_eq!(ids.len(), 2);
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 2);
        for verb in verbs.iter() {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, "/home/mm/Exports");
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn active_page_scrape_export_writes_formats_and_queues_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        state.tabs[state.active].engine = BrowserEngine::Cef;
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 501,
                url: "https://example.test/products/page-2.html".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 502,
                url: "https://example.test/assets/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 503,
                url: "https://cdn.example.test/ignored.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
            },
        );
        run_panel(&mut state);
        let spool = tempfile::tempdir().expect("scrape spool dir");
        let dest = tempfile::tempdir().expect("scrape destination dir");

        let ids = state
            .export_active_page_metadata_scrape_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("export active page scrape");

        assert_eq!(ids.len(), 3);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read scrape spool")
            .map(|entry| entry.expect("scrape file").path())
            .collect::<Vec<_>>();
        files.sort();
        let exts = files
            .iter()
            .map(|path| path.extension().and_then(|ext| ext.to_str()).unwrap_or(""))
            .collect::<Vec<_>>();
        assert_eq!(exts, ["csv", "json", "md"]);
        let json = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                .expect("json export"),
        )
        .expect("read json export");
        let v: serde_json::Value = serde_json::from_str(&json).expect("scrape JSON");
        assert_eq!(v["op"], "browser_active_page_scrape");
        assert_eq!(v["engine"], "cef");
        assert_eq!(
            v["scope"],
            "active_page_metadata_with_crawl_seed_text_and_dom"
        );
        assert_eq!(v["extracted_text_status"], "not_requested");
        assert_eq!(v["dom_extract_status"], "not_requested");
        assert_eq!(v["dom_link_count"], 0);
        assert_eq!(v["dom_heading_count"], 0);
        assert_eq!(v["crawl_seed_count"], 2);
        assert_eq!(v["crawl_manifest_status"], "ready");
        assert_eq!(v["crawl_execution_status"], "not_started");
        assert_eq!(v["crawl_manifest_max_depth"], 1);
        assert_eq!(v["crawl_manifest_count"], 2);
        let seed = v["crawl_seed"].as_array().expect("crawl seed array");
        assert!(seed
            .iter()
            .any(|item| item["url"] == "https://example.test/products/page-2.html"));
        assert!(seed
            .iter()
            .any(|item| item["url"] == "https://example.test/assets/app.js"));
        assert!(
            !seed
                .iter()
                .any(|item| item["url"] == "https://cdn.example.test/ignored.js"),
            "cross-origin telemetry must not become a crawl seed"
        );
        let crawl_manifest = v["crawl_manifest"].as_array().expect("crawl manifest");
        assert!(crawl_manifest.iter().any(|item| {
            item["url"] == "https://example.test/products/page-2.html"
                && item["source"] == "telemetry"
                && item["depth"] == 1
        }));
        assert!(
            !crawl_manifest
                .iter()
                .any(|item| item["url"] == "https://cdn.example.test/ignored.js"),
            "cross-origin telemetry must not become a crawl manifest target"
        );
        let csv = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "csv"))
                .expect("csv export"),
        )
        .expect("read csv export");
        assert!(
            csv.contains("captured_ms,engine,title,url,scope,seed_url,seed_resource,seed_allowed,text_status,text_chars,text_truncated,text,dom_kind,dom_url,dom_text,dom_level,dom_same_origin,dom_rel,dom_target")
        );
        assert!(csv.contains("\"Example\""));
        assert!(csv.contains("\"not_requested\""));
        assert!(csv.contains("\"https://example.test/products/page-2.html\""));
        assert!(csv.contains("crawl_manifest"));
        assert!(csv.contains("crawl_target"));
        assert!(!csv.contains("cdn.example.test"));
        let md = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "md"))
                .expect("markdown export"),
        )
        .expect("read markdown export");
        assert!(md.contains("# Example"));
        assert!(md.contains(
            "active page metadata with bounded crawl seed, extracted text, DOM links/headings"
        ));
        assert!(md.contains("Visible page text was not requested"));
        assert!(md.contains("DOM links were not requested"));
        assert!(md.contains("## Crawl Manifest"));
        assert!(md.contains("source=telemetry"));
        assert!(md.contains("https://example.test/assets/app.js"));
        assert!(md.contains(
            "This export records bounded same-origin crawl targets and does not recursively fetch them."
        ));
        assert!(
            [
                "follow-up",
                "hook",
                "placeholder",
                "stub",
                "helper",
                "handoff",
                "CEF",
                "Servo"
            ]
            .iter()
            .all(|term| !md.contains(term)),
            "scrape markdown must stay user-facing: {md}"
        );

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 3);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn power_scrape_export_requests_page_scrape_and_writes_dom_extract() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let _ = drain_control_messages(&helper);
        write_helper_event(
            &helper,
            &mde_web_preview_client::EventMsg::ResourceRequest {
                id: 601,
                url: "https://example.test/article/related.html".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
            },
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("scrape spool dir");
        let dest = tempfile::tempdir().expect("scrape destination dir");

        state
            .request_active_page_metadata_scrape_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("request page DOM scrape export");
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Scrape export: reading page DOM")
        );
        let controls = drain_control_messages(&helper);
        assert!(
            controls.iter().any(|msg| matches!(
                msg,
                mde_web_preview_client::ControlMsg::RequestPageScrape {
                    id: 1,
                    max_bytes: 65536,
                    max_links: 64,
                    max_headings: 32,
                }
            )),
            "scrape export must request bounded page DOM from the helper: {controls:?}"
        );
        assert_eq!(
            std::fs::read_dir(spool.path())
                .expect("read empty scrape spool")
                .count(),
            0,
            "scrape files wait for page DOM"
        );

        state.handle_page_scrape_event(
            1,
            serde_json::json!({
                "text": "  Visible article body.\n\nSecond paragraph.  ",
                "text_truncated": false,
                "article_text": "  Article lead.\n\nArticle detail.  ",
                "article_text_truncated": false,
                "article_selector": "article",
                "canonical_url": "https://example.test/article/",
                "meta_description": "An example article summary.",
                "document_lang": "en-US",
                "links": [
                    {
                        "url": "https://example.test/article/related.html",
                        "text": "Related Article",
                        "rel": "next",
                        "target": "_self"
                    },
                    {
                        "url": "https://example.test/article/part-2.html",
                        "text": "Part Two",
                        "rel": "",
                        "target": ""
                    },
                    {
                        "url": "https://elsewhere.test/",
                        "text": "External",
                        "rel": "",
                        "target": "_blank"
                    }
                ],
                "headings": [
                    {"level": 1, "text": "Story Headline"},
                    {"level": 2, "text": "Second Section"}
                ]
            })
            .to_string(),
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Power mode: queued active-page scrape export (3 files)")
        );

        let mut files = std::fs::read_dir(spool.path())
            .expect("read scrape spool")
            .map(|entry| entry.expect("scrape file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 3);
        let json = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "json"))
                .expect("json export"),
        )
        .expect("read json export");
        let v: serde_json::Value = serde_json::from_str(&json).expect("scrape JSON");
        assert_eq!(v["extracted_text_status"], "captured");
        assert_eq!(
            v["extracted_text"],
            "Visible article body.\n\nSecond paragraph."
        );
        assert_eq!(v["extracted_text_truncated"], false);
        assert_eq!(v["dom_extract_status"], "captured");
        assert_eq!(v["article_extract_status"], "captured");
        assert_eq!(v["article_text"], "Article lead.\n\nArticle detail.");
        assert_eq!(v["article_text_chars"], 30);
        assert_eq!(v["article_text_truncated"], false);
        assert_eq!(v["article_selector"], "article");
        assert_eq!(v["canonical_url"], "https://example.test/article/");
        assert_eq!(v["meta_description"], "An example article summary.");
        assert_eq!(v["document_lang"], "en-US");
        assert_eq!(v["dom_link_count"], 3);
        assert_eq!(v["dom_heading_count"], 2);
        assert_eq!(v["crawl_seed_count"], 1);
        assert_eq!(v["crawl_manifest_status"], "ready");
        assert_eq!(v["crawl_execution_status"], "not_started");
        assert_eq!(v["crawl_manifest_count"], 2);
        let links = v["dom_links"].as_array().expect("dom links");
        assert!(links.iter().any(|link| {
            link["url"] == "https://example.test/article/related.html"
                && link["text"] == "Related Article"
                && link["same_origin"] == true
        }));
        assert!(links.iter().any(|link| {
            link["url"] == "https://elsewhere.test/" && link["same_origin"] == false
        }));
        let crawl_manifest = v["crawl_manifest"].as_array().expect("crawl manifest");
        assert!(crawl_manifest.iter().any(|target| {
            target["url"] == "https://example.test/article/related.html"
                && target["source"] == "telemetry"
        }));
        assert!(crawl_manifest.iter().any(|target| {
            target["url"] == "https://example.test/article/part-2.html"
                && target["source"] == "dom_link"
        }));
        assert!(
            !crawl_manifest
                .iter()
                .any(|target| target["url"] == "https://elsewhere.test/"),
            "cross-origin DOM links stay out of the crawl manifest"
        );
        let csv = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "csv"))
                .expect("csv export"),
        )
        .expect("read csv export");
        assert!(csv.contains("\"captured\""));
        assert!(csv.contains("\"Visible article body."));
        assert!(csv.contains("dom_link"));
        assert!(csv.contains("\"Related Article\""));
        assert!(csv.contains("dom_heading"));
        assert!(csv.contains("\"Story Headline\""));
        assert!(csv.contains("dom_article"));
        assert!(csv.contains("\"Article lead."));
        assert!(csv.contains("dom_canonical"));
        assert!(csv.contains("\"https://example.test/article/\""));
        assert!(csv.contains("crawl_manifest"));
        assert!(csv.contains("\"https://example.test/article/part-2.html\""));
        assert!(csv.contains("dom_meta_description"));
        assert!(csv.contains("\"An example article summary.\""));
        assert!(csv.contains("dom_document_lang"));
        assert!(csv.contains("\"en-US\""));
        let md = std::fs::read_to_string(
            files
                .iter()
                .find(|path| path.extension().is_some_and(|ext| ext == "md"))
                .expect("markdown export"),
        )
        .expect("read markdown export");
        assert!(md.contains("## Extracted Text"));
        assert!(md.contains("Visible article body."));
        assert!(md.contains("## Article Extract"));
        assert!(md.contains("Article lead."));
        assert!(md.contains("https://example.test/article/"));
        assert!(md.contains("An example article summary."));
        assert!(md.contains("## Crawl Manifest"));
        assert!(md.contains("source=dom_link"));
        assert!(md.contains("## DOM Links"));
        assert!(md.contains("[Related Article](https://example.test/article/related.html)"));
        assert!(md.contains("## DOM Headings"));
        assert!(md.contains("h1 Story Headline"));
        assert!(md.contains(
            "This export records bounded same-origin crawl targets and does not recursively fetch them."
        ));
        assert!(
            [
                "follow-up",
                "hook",
                "placeholder",
                "stub",
                "helper",
                "handoff",
                "CEF",
                "Servo"
            ]
            .iter()
            .all(|term| !md.contains(term)),
            "scrape markdown must stay user-facing: {md}"
        );

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 3);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn scrape_export_empty_markdown_uses_user_facing_copy() {
        let docs = active_page_scrape_documents(
            "https://example.test/empty",
            "Empty Page",
            BrowserEngine::Cef,
            1234,
            &[],
            Some(""),
            Some(""),
        )
        .expect("empty scrape documents");
        let md = docs
            .iter()
            .find(|(ext, _)| *ext == "md")
            .map(|(_, body)| String::from_utf8(body.clone()).expect("markdown is utf8"))
            .expect("markdown export");

        for expected in [
            "- Engine: `Chromium`",
            "No visible page text was available for this page.",
            "No article/main-body text was available for this page.",
            "No DOM links were available for this page.",
            "No DOM headings were available for this page.",
            "No same-origin crawl targets were available for this export.",
            "No same-origin crawl seed URLs were observed for this page.",
        ] {
            assert!(md.contains(expected), "missing {expected:?}: {md}");
        }
        for forbidden in [
            "helper",
            "handoff manifest",
            "helper resource telemetry",
            "returned by the helper",
            "CEF",
        ] {
            assert!(
                !md.contains(forbidden),
                "scrape markdown leaked internal copy {forbidden:?}: {md}"
            );
        }

        let lightweight_docs = active_page_scrape_documents(
            "https://example.test/empty",
            "Empty Page",
            BrowserEngine::Servo,
            1234,
            &[],
            Some(""),
            Some(""),
        )
        .expect("empty lightweight scrape documents");
        let lightweight_md = lightweight_docs
            .iter()
            .find(|(ext, _)| *ext == "md")
            .map(|(_, body)| String::from_utf8(body.clone()).expect("markdown is utf8"))
            .expect("markdown export");
        assert!(
            lightweight_md.contains("- Engine: `Lightweight`"),
            "lightweight scrape markdown must use user-facing engine copy: {lightweight_md}"
        );
        assert!(
            !lightweight_md.contains("Servo"),
            "lightweight scrape markdown leaked raw engine copy: {lightweight_md}"
        );
    }

    #[test]
    fn media_export_notices_use_user_facing_copy() {
        let mut state = WebState::default();
        state.export_active_media_manifest();
        let no_live = state
            .capture_notice
            .as_deref()
            .expect("media export notice");
        assert_eq!(no_live, "Media export failed: no live page");

        let queued = WebState::media_export_queued_notice("browser-media-123");
        assert_eq!(queued, "Power mode: queued media list (browser-media-123)");

        let failed =
            WebState::media_export_failed_notice("write media manifest /tmp/page.json: denied");
        assert_eq!(failed, "Media export failed: could not save the media list");

        for notice in [no_live, queued.as_str(), failed.as_str()] {
            let lower = notice.to_ascii_lowercase();
            assert!(
                !lower.contains("manifest") && !lower.contains("/tmp/"),
                "media export notice leaked implementation copy: {notice}"
            );
        }
    }

    #[test]
    fn media_download_queue_notices_use_user_facing_copy() {
        let media = WebState::media_download_queue_failed_notice(
            "Media",
            "create media download spool dir: permission denied",
        );
        assert_eq!(
            media,
            "Media download queue failed: could not prepare the download staging area"
        );

        let image = WebState::media_download_queue_failed_notice(
            "Image",
            "write media download request /tmp/mde-browser-media/page.download.json: denied",
        );
        assert_eq!(
            image,
            "Image download queue failed: could not save the download request"
        );

        let no_live =
            WebState::media_download_queue_failed_notice("Media", "no live page to download from");
        assert_eq!(no_live, "Media download queue failed: no live page");

        for notice in [media.as_str(), image.as_str(), no_live.as_str()] {
            let lower = notice.to_ascii_lowercase();
            assert!(
                !lower.contains("spool")
                    && !lower.contains("manifest")
                    && !lower.contains(".download.json")
                    && !lower.contains("/tmp/"),
                "media download queue notice leaked implementation copy: {notice}"
            );
        }
    }

    #[test]
    fn media_manifest_export_sniffs_media_requests_and_queues_transfer() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/poster.webp?cache=1",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://video.example.test/master.m3u8",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            93,
            "https://video.example.test/manifest.mpd",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            94,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media spool dir");
        let dest = tempfile::tempdir().expect("media destination dir");

        let id = state
            .export_active_media_manifest_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("export media manifest");

        let mut files = std::fs::read_dir(spool.path())
            .expect("read media spool")
            .map(|entry| entry.expect("media file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].extension().and_then(|ext| ext.to_str()),
            Some("json")
        );
        let body = std::fs::read_to_string(&files[0]).expect("read media manifest");
        let v: serde_json::Value = serde_json::from_str(&body).expect("media manifest JSON");
        assert_eq!(v["op"], "browser_media_manifest");
        assert_eq!(v["scope"], "active_page_media_sniffer");
        assert_eq!(v["item_count"], 4);
        let items = v["items"].as_array().expect("items array");
        assert!(items.iter().any(|item| item["kind"] == "image"));
        assert!(items.iter().any(|item| item["kind"] == "hls"));
        assert!(items.iter().any(|item| item["kind"] == "dash"));
        assert!(items.iter().any(|item| item["kind"] == "video"));
        assert!(
            !items.iter().any(|item| item["url"]
                .as_str()
                .is_some_and(|url| url.ends_with("app.js"))),
            "non-media script requests stay out of the media manifest"
        );

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.id, id);
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
        assert_eq!(job.source, files[0].to_string_lossy().as_ref());
        assert!(job.policy.verify);
    }

    #[test]
    fn media_asset_request_marks_blocked_resources_for_ignore_blocking() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                seq: 1,
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
                blocked_by: None,
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 2,
                url: "https://video.example.test/master.m3u8".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            },
        ];

        let requests = active_page_media_asset_requests(
            "https://example.test/watch",
            "Example Video",
            BrowserEngine::Cef,
            42,
            &recent,
        )
        .expect("encode media asset requests");

        assert_eq!(requests.len(), 1, "non-media script requests stay out");
        let v: serde_json::Value =
            serde_json::from_slice(&requests[0]).expect("media request JSON");
        assert_eq!(v["op"], "browser_media_download_request");
        assert_eq!(v["asset_url"], "https://video.example.test/master.m3u8");
        assert_eq!(v["kind"], "hls");
        assert_eq!(v["allowed_by_page_filter"], false);
        assert_eq!(v["blocked_by_page_filter"], "mixed-content:http");
        assert_eq!(v["ignore_blocking"], true);
        assert_eq!(v["suggested_filename"], "master.m3u8");
        assert_eq!(v["rename_strategy"], "auto_rename_by_url_hint");
    }

    #[test]
    fn media_asset_request_selection_batches_only_observed_images() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                seq: 1,
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
                blocked_by: None,
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 2,
                url: "https://cdn.example.test/hero.png".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: true,
                blocked_by: None,
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 3,
                url: "https://cdn.example.test/photo".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            },
            mde_web_preview_client::ResourceRequestStatus {
                seq: 4,
                url: "https://video.example.test/clip.mp4".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Media,
                ),
                allowed: true,
                blocked_by: None,
            },
        ];

        let requests = active_page_media_asset_requests_with_selection(
            "https://example.test/gallery",
            "Example Gallery",
            BrowserEngine::Cef,
            42,
            &recent,
            MediaAssetSelection::Images,
        )
        .expect("encode image asset requests");

        assert_eq!(requests.len(), 2);
        let bodies = requests
            .iter()
            .map(|body| serde_json::from_slice::<serde_json::Value>(body).expect("request JSON"))
            .collect::<Vec<_>>();
        assert!(bodies.iter().all(|v| v["kind"] == "image"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/hero.png"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/photo"));
        assert!(bodies.iter().any(|v| v["ignore_blocking"] == true));
        assert!(!bodies
            .iter()
            .any(|v| v["asset_url"] == "https://video.example.test/clip.mp4"));
    }

    #[test]
    fn observed_media_download_queue_writes_request_files_and_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/poster.webp?cache=1",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://video.example.test/master.m3u8",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            93,
            "https://video.example.test/manifest.mpd",
            mde_web_preview_client::ResourceType::XmlHttpRequest,
        );
        resource(
            94,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media download spool dir");
        let dest = tempfile::tempdir().expect("media download destination dir");

        let ids = state
            .download_observed_media_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue observed media downloads");

        assert_eq!(ids.len(), 4);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read media download spool")
            .map(|entry| entry.expect("media request file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 4);
        let bodies = files
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).expect("read request file");
                serde_json::from_str::<serde_json::Value>(&body).expect("request JSON")
            })
            .collect::<Vec<_>>();
        assert!(bodies
            .iter()
            .all(|v| v["op"] == "browser_media_download_request"));
        assert!(bodies.iter().any(|v| v["kind"] == "image"));
        assert!(bodies.iter().any(|v| v["kind"] == "hls"));
        assert!(bodies.iter().any(|v| v["kind"] == "dash"));
        assert!(bodies.iter().any(|v| v["kind"] == "video"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 4);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.source.ends_with(".download.json"));
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn intercepted_download_becomes_a_browser_download_ledger_job() {
        // B2: a CEF `on_before_download` interception (surfaced by the helper as an
        // EventMsg::Download and drained in the pump) is handed to the Transfers
        // ledger — NOT saved locally. Prove the ledger job + its `.download.json`
        // manifest carry the asset URL the daemon will fetch.
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(7, "https://files.example.test/report.pdf", "report.pdf");

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert!(job.source.ends_with(".download.json"));
        assert_eq!(job.dest, browser_capture_dir().to_string_lossy().as_ref());

        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(manifest["op"], "browser_media_download_request");
        assert_eq!(
            manifest["asset_url"],
            "https://files.example.test/report.pdf"
        );
        assert_eq!(manifest["suggested_filename"], "report.pdf");
        // The interception opens the Downloads drawer so the user sees it land.
        assert!(state.downloads_open);
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn download_queue_intercepted_directory_failure_notice_stays_user_facing() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers));
        let tmp = tempfile::tempdir().expect("download staging tempdir");
        let not_a_dir = tmp.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"not a directory").expect("staging blocker file");
        let dest = tmp.path().join("captures");

        state.enqueue_download_to_ledger_dirs(
            17,
            "https://files.example.test/report.pdf",
            "report.pdf",
            not_a_dir,
            dest,
        );

        let notice = state.capture_notice.as_deref().expect("download notice");
        assert_eq!(
            notice,
            "Download failed: could not prepare the transfer staging area"
        );
        assert!(
            !notice.contains("spool"),
            "download notice must not expose transfer spool internals: {notice}"
        );
    }

    #[test]
    fn intercepted_download_without_a_filename_derives_one_from_the_url() {
        // A `Content-Disposition`-less download arrives with an empty suggested
        // name; the last non-empty URL segment becomes the filename.
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(
            9,
            "https://dl.example.test/a/b/archive.tar.gz?token=x",
            "",
        );

        let verbs = transfers.verbs();
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(manifest["suggested_filename"], "archive.tar.gz");
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn completed_media_download_target_matches_worker_destination_and_sanitizes_name() {
        let tmp = tempfile::tempdir().expect("download target tempdir");
        let dest = tmp.path().join("downloads");
        std::fs::create_dir_all(&dest).expect("download dest");
        let manifest = write_browser_download_manifest(
            tmp.path(),
            "https://media.example.test/video/poster image.jpg",
            "../poster image.jpg",
            None,
        );
        let job = browser_download_fixture("browser-media", &manifest, &dest, TransferState::Done);

        let target = completed_browser_download_target(&job).expect("completed target");

        assert_eq!(target.open, dest.join("poster-image.jpg"));
        assert_eq!(target.reveal, dest);
    }

    #[test]
    fn completed_hls_and_dash_download_targets_open_rewritten_manifests() {
        let hls_tmp = tempfile::tempdir().expect("hls target tempdir");
        let hls_dest = hls_tmp.path().join("downloads");
        std::fs::create_dir_all(&hls_dest).expect("hls dest");
        let hls_manifest = write_browser_download_manifest(
            hls_tmp.path(),
            "https://media.example.test/video/master.m3u8",
            "../master playlist.m3u8",
            Some("hls"),
        );
        let hls_job =
            browser_download_fixture("browser-hls", &hls_manifest, &hls_dest, TransferState::Done);

        let hls_target = completed_browser_download_target(&hls_job).expect("hls target");

        assert_eq!(
            hls_target.open,
            hls_dest.join("master-playlist.hls/master-playlist.m3u8")
        );
        assert_eq!(hls_target.reveal, hls_dest.join("master-playlist.hls"));

        let dash_tmp = tempfile::tempdir().expect("dash target tempdir");
        let dash_dest = dash_tmp.path().join("downloads");
        std::fs::create_dir_all(&dash_dest).expect("dash dest");
        let dash_manifest = write_browser_download_manifest(
            dash_tmp.path(),
            "https://media.example.test/video/manifest.mpd?token=x",
            "../dash manifest.mpd",
            None,
        );
        let dash_job = browser_download_fixture(
            "browser-dash",
            &dash_manifest,
            &dash_dest,
            TransferState::Done,
        );

        let dash_target = completed_browser_download_target(&dash_job).expect("dash target");

        assert_eq!(
            dash_target.open,
            dash_dest.join("dash-manifest.dash/dash-manifest.mpd")
        );
        assert_eq!(dash_target.reveal, dash_dest.join("dash-manifest.dash"));
    }

    #[test]
    fn completed_materialized_browser_output_target_matches_copy_lane() {
        let tmp = tempfile::tempdir().expect("browser output tempdir");
        let source = tmp.path().join("capture.png");
        let dest = tmp.path().join("exports");
        std::fs::write(&source, b"png").expect("source");
        std::fs::create_dir_all(&dest).expect("dest");
        let job = browser_download_fixture("browser-output", &source, &dest, TransferState::Done);

        let target = completed_browser_download_target(&job).expect("output target");

        assert_eq!(target.open, dest.join("capture.png"));
        assert_eq!(target.reveal, dest);
    }

    #[test]
    fn completed_download_open_and_reveal_use_the_resolved_output_target() {
        let tmp = tempfile::tempdir().expect("download opener tempdir");
        let dest = tmp.path().join("downloads");
        std::fs::create_dir_all(&dest).expect("dest");
        let manifest = write_browser_download_manifest(
            tmp.path(),
            "https://files.example.test/report.pdf",
            "report.pdf",
            None,
        );
        let job = browser_download_fixture("browser-done", &manifest, &dest, TransferState::Done);
        let transfers = RecordingTransfers::with_jobs(vec![job]);
        let opener = RecordingDownloadOpener::default();
        let mut state = WebState::default()
            .with_transfers(Box::new(transfers))
            .with_download_opener(Box::new(opener.clone()));

        state.open_download("browser-done");
        state.reveal_download("browser-done");

        assert_eq!(opener.opened(), vec![dest.join("report.pdf")]);
        assert_eq!(opener.revealed(), vec![dest.clone()]);
        assert_eq!(state.download_notice, None);
        assert_eq!(state.capture_notice.as_deref(), Some("Showing downloads"));
        assert!(
            !state
                .capture_notice
                .as_deref()
                .unwrap_or_default()
                .contains(tmp.path().to_string_lossy().as_ref()),
            "download reveal notice should not expose an absolute path"
        );
    }

    #[test]
    fn browser_output_notices_hide_absolute_paths() {
        let mut state = WebState::default();
        let pdf_path = "/tmp/quazar-output/report.pdf".to_owned();
        assert_eq!(
            state.handle_pdf_event(pdf_path.clone(), true),
            "PDF saved: report.pdf"
        );
        assert!(
            !state.handle_pdf_event(pdf_path, false).contains("/tmp/"),
            "PDF completion notices should use a filename label"
        );
        state.last_saved_pdf = Some(SavedPdf {
            path: PathBuf::from("/tmp/quazar-output/not-pdf.pdf"),
            url: "https://example.test/".to_owned(),
            title: "Example".to_owned(),
        });
        state.open_last_saved_pdf();
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("PDF viewer failed: saved PDF is not readable")
        );

        let bus = tempfile::tempdir().expect("temp bus");
        let mut capture_state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        capture_state.record_capture_success(
            "Captured web archive",
            Path::new("/tmp/quazar-output/capture.mhtml"),
        );
        assert_eq!(
            capture_state.capture_notice.as_deref(),
            Some("Captured web archive: capture.mhtml")
        );

        let tmp = tempfile::tempdir().expect("download tempdir");
        let dest = tmp.path().join("downloads");
        std::fs::create_dir_all(&dest).expect("dest");
        let manifest = write_browser_download_manifest(
            tmp.path(),
            "https://files.example.test/report.pdf",
            "report.pdf",
            None,
        );
        let job = browser_download_fixture("browser-done", &manifest, &dest, TransferState::Done);
        let opener = RecordingDownloadOpener::default();
        let mut download_state = WebState::default()
            .with_transfers(Box::new(RecordingTransfers::with_jobs(vec![job])))
            .with_download_opener(Box::new(opener));
        download_state.open_download("browser-done");
        assert_eq!(
            download_state.capture_notice.as_deref(),
            Some("Opening report.pdf")
        );
        download_state.reveal_download("browser-done");
        assert_eq!(
            download_state.capture_notice.as_deref(),
            Some("Showing downloads")
        );
        for notice in [
            state.capture_notice.as_deref(),
            capture_state.capture_notice.as_deref(),
            download_state.capture_notice.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(
                !notice.contains("/tmp/"),
                "visible output notice leaked an absolute path: {notice}"
            );
        }
    }

    #[test]
    fn incomplete_download_does_not_launch_the_platform_opener() {
        let tmp = tempfile::tempdir().expect("incomplete download tempdir");
        let source = tmp.path().join("capture.png");
        let dest = tmp.path().join("exports");
        std::fs::write(&source, b"png").expect("source");
        std::fs::create_dir_all(&dest).expect("dest");
        let job =
            browser_download_fixture("browser-running", &source, &dest, TransferState::Running);
        let transfers = RecordingTransfers::with_jobs(vec![job]);
        let opener = RecordingDownloadOpener::default();
        let mut state = WebState::default()
            .with_transfers(Box::new(transfers))
            .with_download_opener(Box::new(opener.clone()));

        state.open_download("browser-running");
        state.reveal_download("browser-running");

        assert!(opener.opened().is_empty());
        assert!(opener.revealed().is_empty());
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download is not complete yet")
        );
    }

    #[test]
    fn download_is_dangerous_flags_executable_and_script_extensions() {
        for filename in [
            "setup.exe",
            "Invoice.pdf.exe",
            "script.PS1",
            "x.jar",
            "installer.MSI",
            "payload.scr",
            "run.bat",
            "run.cmd",
            "legacy.com",
            "auto.pif",
            "app.msix",
            "creds.vbs",
            "creds.vbe",
            "worker.js",
            "worker.jse",
            "task.wsf",
            "help.hta",
            "panel.cpl",
            "lib.dll",
            "shortcut.lnk",
            "tweak.reg",
            "install.sh",
            "binary.run",
            "package.deb",
            "package.rpm",
            "image.dmg",
            "bundle.pkg",
            "app.apk",
            "widget.gadget",
            // A masquerading double extension from either side.
            "notes.exe.pdf",
        ] {
            assert!(
                download_is_dangerous(filename),
                "{filename} should be flagged dangerous"
            );
        }
    }

    #[test]
    fn download_is_dangerous_normalizes_encoded_and_platform_filename_tricks() {
        for filename in [
            "setup%2Eexe",
            "setup%252Eexe",
            "nested%2Fsetup.exe",
            "C:\\Users\\me\\setup.exe",
            "setup.exe...",
            "setup.exe ",
            "notes.exe .pdf",
            "setup.exe:Zone.Identifier",
            "safe.txt:evil.exe",
        ] {
            assert!(
                download_is_dangerous(filename),
                "{filename} should be flagged dangerous after normalization"
            );
        }
    }

    #[test]
    fn download_is_dangerous_allows_ordinary_files() {
        for filename in [
            "photo.jpg",
            "report.pdf",
            "archive.tar.gz",
            "data.csv",
            "notes.txt",
            "song.mp3",
            "video.mp4",
            ".bashrc",
            "README",
            "",
        ] {
            assert!(
                !download_is_dangerous(filename),
                "{filename} should NOT be flagged dangerous"
            );
        }
    }

    #[test]
    fn safe_suggested_name_still_parks_when_url_leaf_is_dangerous() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(
            10,
            "https://files.example.test/releases/setup%2Eexe?token=x",
            "report.pdf",
        );

        assert!(
            transfers.verbs().is_empty(),
            "a dangerous URL leaf must not touch the ledger before Keep/Discard"
        );
        let pending = state
            .pending_dangerous_download
            .clone()
            .expect("dangerous URL download parked pending confirmation");
        assert_eq!(pending.id, 10);
        assert_eq!(
            pending.url,
            "https://files.example.test/releases/setup%2Eexe?token=x"
        );
        assert_eq!(pending.filename, "report.pdf");
        assert!(state.downloads_open);
    }

    #[test]
    fn managed_policy_blocks_intercepted_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));
        state.set_managed_url_policy(parse_managed_url_policy(
            "url:https://files.example.test/private/\n",
        ));

        state.submit_download_to_ledger(
            15,
            "https://files.example.test/private/report.pdf",
            "report.pdf",
        );

        assert!(
            transfers.verbs().is_empty(),
            "managed policy must block the daemon transfer before any ledger job"
        );
        assert!(
            state.pending_dangerous_download.is_none(),
            "managed policy is final; it must not become a user-overridable danger prompt"
        );
        assert!(
            state.managed_policy_block.is_none(),
            "download blocks should not replace the active page with a navigation interstitial"
        );
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download blocked by managed policy: files.example.test")
        );
        assert!(state.downloads_open);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_POLICY_BLOCK, None)
            .expect("list policy block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("policy body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(
            event["url"],
            "https://files.example.test/private/report.pdf"
        );
        assert_eq!(event["rule"], "url:https://files.example.test/private/");
    }

    #[test]
    fn managed_policy_rechecks_pending_dangerous_download_on_keep() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(16, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.set_managed_url_policy(parse_managed_url_policy("files.example.test\n"));
        state.keep_pending_dangerous_download();

        assert!(
            state.pending_dangerous_download.is_none(),
            "Keep resolves the dangerous prompt even when later policy blocks it"
        );
        assert!(
            transfers.verbs().is_empty(),
            "a policy change after the prompt must still stop the transfer"
        );
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download blocked by managed policy: files.example.test")
        );
    }

    #[test]
    fn safe_browsing_blocks_intercepted_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));
        state.set_safe_browsing_hosts(["malware.test"]);

        state.submit_download_to_ledger(18, "https://cdn.malware.test/payload.pdf", "payload.pdf");

        assert!(
            transfers.verbs().is_empty(),
            "safe browsing must block the daemon transfer before any ledger job"
        );
        assert!(
            state.pending_dangerous_download.is_none(),
            "safe-browsing blocks must not become a user-overridable danger prompt"
        );
        assert!(
            state.managed_policy_block.is_none(),
            "download blocks should not replace the active page with a navigation interstitial"
        );
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download blocked by safe browsing: cdn.malware.test")
        );
        assert!(state.downloads_open);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_SAFE_BROWSING_BLOCK, None)
            .expect("list safe-browsing block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("safe-browsing body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(event["url"], "https://cdn.malware.test/payload.pdf");
        assert_eq!(event["rule"], "malware.test");
    }

    #[test]
    fn safe_browsing_rechecks_pending_dangerous_download_on_keep() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(19, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.set_safe_browsing_hosts(["files.example.test"]);
        state.keep_pending_dangerous_download();

        assert!(
            state.pending_dangerous_download.is_none(),
            "Keep resolves the dangerous prompt even when safe browsing later blocks it"
        );
        assert!(
            transfers.verbs().is_empty(),
            "a safe-browsing update after the prompt must still stop the transfer"
        );
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download blocked by safe browsing: files.example.test")
        );
    }

    #[test]
    fn safe_browsing_download_gate_keeps_mesh_overlay_exempt() {
        let mut state = WebState::default();
        state.set_safe_browsing_hosts(["media.mesh", "10.42.0.9"]);

        assert!(
            state
                .safe_browsing_download_block_for("https://media.mesh/payload.pdf")
                .is_none(),
            "safe browsing mirrors request-filter mesh host exemptions"
        );
        assert!(
            state
                .safe_browsing_download_block_for("https://10.42.0.9/payload.pdf")
                .is_none(),
            "safe browsing mirrors request-filter Nebula overlay exemptions"
        );
    }

    #[test]
    fn insecure_http_blocks_intercepted_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));

        state.submit_download_to_ledger(20, "http://cdn.example.test/payload.pdf", "payload.pdf");

        assert!(
            transfers.verbs().is_empty(),
            "insecure downloads must block before the daemon fetches public HTTP"
        );
        assert!(
            state.pending_dangerous_download.is_none(),
            "transport hard-blocks must not become user-overridable danger prompts"
        );
        assert!(
            state.managed_policy_block.is_none(),
            "download blocks should not replace the active page with a navigation interstitial"
        );
        assert_eq!(
            state.download_notice.as_deref(),
            Some("Download blocked: insecure HTTP from cdn.example.test")
        );
        assert!(state.downloads_open);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_INSECURE_DOWNLOAD_BLOCK, None)
            .expect("list insecure-download block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("insecure-download body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(event["url"], "http://cdn.example.test/payload.pdf");
        assert_eq!(event["reason"], "plain_http_download");
    }

    #[test]
    fn insecure_download_gate_keeps_mesh_overlay_exempt() {
        let state = WebState::default();

        assert!(
            !state.insecure_download_block_for("http://media.mesh/payload.pdf"),
            "mesh HTTP services are trusted overlay traffic, not public-web HTTP"
        );
        assert!(
            !state.insecure_download_block_for("http://10.42.0.9/payload.pdf"),
            "Nebula overlay HTTP services are trusted mesh traffic"
        );
        assert!(
            !state.insecure_download_block_for("http://localhost:8080/payload.pdf"),
            "localhost development/service downloads stay local"
        );
        assert!(
            state.insecure_download_block_for("http://cdn.example.test/payload.pdf"),
            "public HTTP downloads are blocked before transfer submission"
        );
        assert!(
            state.insecure_download_block_for("HTTP://cdn.example.test/payload.pdf"),
            "URL schemes are case-insensitive"
        );
        assert!(
            !state.insecure_download_block_for("https://cdn.example.test/payload.pdf"),
            "HTTPS downloads are unaffected"
        );
    }

    #[test]
    fn dangerous_url_path_check_ignores_authority_without_a_path_leaf() {
        assert!(!download_url_path_is_dangerous("https://downloads.exe"));
        assert!(download_url_path_is_dangerous(
            "https://downloads.example.test/releases/setup.exe"
        ));
    }

    #[test]
    fn dangerous_download_parks_pending_and_does_not_submit() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(11, "https://files.example.test/setup.exe", "setup.exe");

        assert!(
            transfers.verbs().is_empty(),
            "a dangerous download must not touch the ledger before Keep/Discard"
        );
        let pending = state
            .pending_dangerous_download
            .clone()
            .expect("dangerous download parked pending confirmation");
        assert_eq!(pending.id, 11);
        assert_eq!(pending.url, "https://files.example.test/setup.exe");
        assert_eq!(pending.filename, "setup.exe");
        // The drawer opens so the user actually sees the warning.
        assert!(state.downloads_open);
    }

    #[test]
    fn keeping_a_dangerous_download_submits_exactly_one_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(12, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.keep_pending_dangerous_download();

        assert!(
            state.pending_dangerous_download.is_none(),
            "Keep resolves the pending confirmation"
        );
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission on Keep");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        let body = std::fs::read_to_string(&job.source).expect("manifest written to spool");
        let manifest: serde_json::Value = serde_json::from_str(&body).expect("manifest JSON");
        assert_eq!(
            manifest["asset_url"],
            "https://files.example.test/setup.exe"
        );
        assert_eq!(manifest["suggested_filename"], "setup.exe");
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn discarding_a_dangerous_download_drops_it_with_no_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(13, "https://files.example.test/setup.exe", "setup.exe");
        assert!(state.pending_dangerous_download.is_some());

        state.discard_pending_dangerous_download();

        assert!(state.pending_dangerous_download.is_none());
        assert!(
            transfers.verbs().is_empty(),
            "Discard must never create a ledger job"
        );
    }

    #[test]
    fn dangerous_download_prompt_and_decisions_are_audited() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));

        state.submit_download_to_ledger(21, "https://files.example.test/setup.exe", "setup.exe");
        state.keep_pending_dangerous_download();
        state.submit_download_to_ledger(22, "https://files.example.test/drop.ps1", "drop.ps1");
        state.discard_pending_dangerous_download();

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "only the kept download is submitted");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        let _ = std::fs::remove_file(&job.source);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_DOWNLOAD_DANGER, None)
            .expect("list dangerous-download events");
        assert_eq!(msgs.len(), 4);
        let events = msgs
            .iter()
            .map(|msg| {
                serde_json::from_str::<serde_json::Value>(
                    msg.body.as_deref().expect("download-danger body"),
                )
                .expect("download-danger JSON")
            })
            .collect::<Vec<_>>();

        assert_eq!(events[0]["op"], "browser_download_danger");
        assert_eq!(events[0]["decision"], "prompt");
        assert_eq!(events[0]["enforcement"], "dangerous_file_gate");
        assert_eq!(events[0]["reason"], "dangerous_extension");
        assert_eq!(events[0]["download_id"], 21);
        assert_eq!(events[0]["url"], "https://files.example.test/setup.exe");
        assert_eq!(events[0]["host"], "files.example.test");
        assert_eq!(events[0]["filename"], "setup.exe");
        assert_eq!(events[0]["source"], "browser");
        assert_eq!(events[0]["node"], local_hostname());
        assert!(events[0]["updated_ms"].as_u64().is_some());

        assert_eq!(events[1]["download_id"], 21);
        assert_eq!(events[1]["decision"], "keep");
        assert_eq!(events[2]["download_id"], 22);
        assert_eq!(events[2]["decision"], "prompt");
        assert_eq!(events[3]["download_id"], 22);
        assert_eq!(events[3]["decision"], "discard");
    }

    #[test]
    fn safe_download_submits_immediately_without_parking() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(14, "https://files.example.test/report.pdf", "report.pdf");

        assert!(
            state.pending_dangerous_download.is_none(),
            "a safe download never parks for confirmation"
        );
        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1, "exactly one ledger submission, no parking");
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn drawer_remove_hides_a_job_and_clear_all_hides_every_job() {
        // `RecordingTransfers::jobs()` is a static double for the daemon-owned
        // ledger — it never shrinks on its own — so this proves dismissal is a
        // Browser-local view filter, NOT a mutation of the shared ledger.
        let a = transfer_fixture(
            "browser-a",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            10,
        );
        let b = transfer_fixture(
            "browser-b",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            20,
        );
        let transfers = RecordingTransfers::with_jobs(vec![a, b]);
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        assert_eq!(state.download_jobs.len(), 2);

        state.dismiss_download("browser-a");
        assert_eq!(state.download_jobs.len(), 1);
        assert!(!state.download_jobs.iter().any(|job| job.id == "browser-a"));
        // The ledger itself still carries both jobs — only the Browser's view
        // dropped one — and a dismissed id stays hidden across a rebuild.
        assert_eq!(transfers.jobs().len(), 2);
        state.refresh_downloads();
        assert_eq!(state.download_jobs.len(), 1);
        assert_eq!(state.download_jobs[0].id, "browser-b");

        state.dismiss_all_downloads();
        assert!(state.download_jobs.is_empty());
        state.refresh_downloads();
        assert!(
            state.download_jobs.is_empty(),
            "Clear all stays hidden across a ledger refresh too"
        );
        assert_eq!(
            transfers.jobs().len(),
            2,
            "Clear all never mutates the shared ledger"
        );
    }

    #[test]
    fn copy_link_source_url_is_tracked_per_ledger_job() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        state.submit_download_to_ledger(17, "https://files.example.test/report.pdf", "report.pdf");

        let verbs = transfers.verbs();
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected a Submit verb");
        };
        assert_eq!(
            state.download_source_urls.get(&job.id).map(String::as_str),
            Some("https://files.example.test/report.pdf")
        );
        let _ = std::fs::remove_file(&job.source);
    }

    #[test]
    fn observed_image_download_queue_writes_only_image_request_files_and_transfers() {
        let transfers = RecordingTransfers::default();
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            90,
            "https://cdn.example.test/app.js",
            mde_web_preview_client::ResourceType::Script,
        );
        resource(
            91,
            "https://cdn.example.test/hero.png",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            92,
            "https://cdn.example.test/photo",
            mde_web_preview_client::ResourceType::Image,
        );
        resource(
            93,
            "https://video.example.test/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("image download spool dir");
        let dest = tempfile::tempdir().expect("image download destination dir");

        let ids = state
            .download_observed_image_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue observed image downloads");

        assert_eq!(ids.len(), 2);
        let mut files = std::fs::read_dir(spool.path())
            .expect("read image download spool")
            .map(|entry| entry.expect("image request file").path())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files.len(), 2);
        let bodies = files
            .iter()
            .map(|path| {
                let body = std::fs::read_to_string(path).expect("read request file");
                serde_json::from_str::<serde_json::Value>(&body).expect("request JSON")
            })
            .collect::<Vec<_>>();
        assert!(bodies.iter().all(|v| v["kind"] == "image"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/hero.png"));
        assert!(bodies
            .iter()
            .any(|v| v["asset_url"] == "https://cdn.example.test/photo"));
        assert!(!bodies
            .iter()
            .any(|v| v["asset_url"] == "https://video.example.test/clip.mp4"));

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 2);
        for verb in verbs {
            let TransferVerb::Submit(job) = verb else {
                panic!("expected submit");
            };
            assert_eq!(job.method, TransferMethod::BrowserDownload);
            assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
            assert!(job.source.ends_with(".download.json"));
            assert!(job.policy.verify);
        }
    }

    #[test]
    fn managed_policy_filters_observed_media_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        state.set_managed_url_policy(parse_managed_url_policy(
            "url:https://blocked.example.test/media/\n",
        ));
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            101,
            "https://blocked.example.test/media/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        resource(
            102,
            "https://cdn.example.test/allowed.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media download spool dir");
        let dest = tempfile::tempdir().expect("media download destination dir");

        let ids = state
            .download_observed_media_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue only policy-allowed media downloads");

        assert_eq!(ids.len(), 1);
        let files = std::fs::read_dir(spool.path())
            .expect("read media download spool")
            .map(|entry| entry.expect("media request file").path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(&files[0]).expect("read request file");
        let request: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
        assert_eq!(request["asset_url"], "https://cdn.example.test/allowed.mp4");

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
        assert!(job.policy.verify);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_POLICY_BLOCK, None)
            .expect("list policy block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("policy body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(event["url"], "https://blocked.example.test/media/clip.mp4");
        assert_eq!(event["rule"], "url:https://blocked.example.test/media/");
    }

    #[test]
    fn safe_browsing_filters_observed_media_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        state.set_safe_browsing_hosts(["malware.test"]);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            111,
            "https://cdn.malware.test/media/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        resource(
            112,
            "https://cdn.example.test/allowed.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media download spool dir");
        let dest = tempfile::tempdir().expect("media download destination dir");

        let ids = state
            .download_observed_media_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue only safe-browsing-allowed media downloads");

        assert_eq!(ids.len(), 1);
        let files = std::fs::read_dir(spool.path())
            .expect("read media download spool")
            .map(|entry| entry.expect("media request file").path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(&files[0]).expect("read request file");
        let request: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
        assert_eq!(request["asset_url"], "https://cdn.example.test/allowed.mp4");

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
        assert!(job.policy.verify);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_SAFE_BROWSING_BLOCK, None)
            .expect("list safe-browsing block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("safe-browsing body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(event["url"], "https://cdn.malware.test/media/clip.mp4");
        assert_eq!(event["rule"], "malware.test");
    }

    #[test]
    fn insecure_http_filters_observed_media_downloads_before_transfer_ledger() {
        let transfers = RecordingTransfers::default();
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));
        let (session, helper, _writer) = live_page_session();
        state.push_session(session);
        run_until_texture(&mut state);
        let resource = |id, url: &str, ty| {
            write_helper_event(
                &helper,
                &mde_web_preview_client::EventMsg::ResourceRequest {
                    id,
                    url: url.to_owned(),
                    resource: mde_web_preview_client::resource_to_wire(ty),
                },
            );
        };
        resource(
            121,
            "http://cdn.example.test/media/clip.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        resource(
            122,
            "https://cdn.example.test/allowed.mp4",
            mde_web_preview_client::ResourceType::Media,
        );
        run_panel(&mut state);
        let _ = drain_control_messages(&helper);
        let spool = tempfile::tempdir().expect("media download spool dir");
        let dest = tempfile::tempdir().expect("media download destination dir");

        let ids = state
            .download_observed_media_assets_to_dirs(
                spool.path().to_path_buf(),
                dest.path().to_path_buf(),
            )
            .expect("queue only HTTPS media downloads");

        assert_eq!(ids.len(), 1);
        let files = std::fs::read_dir(spool.path())
            .expect("read media download spool")
            .map(|entry| entry.expect("media request file").path())
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        let body = std::fs::read_to_string(&files[0]).expect("read request file");
        let request: serde_json::Value = serde_json::from_str(&body).expect("request JSON");
        assert_eq!(request["asset_url"], "https://cdn.example.test/allowed.mp4");

        let verbs = transfers.verbs();
        assert_eq!(verbs.len(), 1);
        let TransferVerb::Submit(job) = &verbs[0] else {
            panic!("expected submit");
        };
        assert_eq!(job.method, TransferMethod::BrowserDownload);
        assert_eq!(job.dest, dest.path().to_string_lossy().as_ref());
        assert!(job.policy.verify);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_BROWSER_INSECURE_DOWNLOAD_BLOCK, None)
            .expect("list insecure-download block events");
        assert_eq!(msgs.len(), 1);
        let event: serde_json::Value =
            serde_json::from_str(msgs[0].body.as_deref().expect("insecure-download body"))
                .expect("valid JSON");
        assert_eq!(event["trigger"], "download");
        assert_eq!(event["url"], "http://cdn.example.test/media/clip.mp4");
        assert_eq!(event["reason"], "plain_http_download");
    }

    fn transfer_fixture(
        id: &str,
        method: TransferMethod,
        state: TransferState,
        updated_ms: u64,
    ) -> TransferJob {
        let mut job = TransferJob::new(
            format!("/tmp/{id}.bin"),
            "/home/mm/Downloads",
            method,
            TransferPolicy {
                bwlimit: None,
                verify: true,
            },
        );
        job.id = id.to_owned();
        job.state = state;
        job.progress = if state == TransferState::Running {
            Some(42)
        } else {
            None
        };
        job.created_ms = updated_ms.saturating_sub(10);
        job.updated_ms = updated_ms;
        job
    }

    #[test]
    fn browser_download_manager_filters_and_dispatches_shared_transfer_jobs() {
        let running = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Running,
            30,
        );
        let paused = transfer_fixture(
            "browser-paused",
            TransferMethod::BrowserDownload,
            TransferState::Paused,
            40,
        );
        let done = transfer_fixture(
            "browser-done",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            50,
        );
        let http = transfer_fixture("http", TransferMethod::Http, TransferState::Running, 60);
        let transfers = RecordingTransfers::with_jobs(vec![done, http, paused, running]);
        let mut state = WebState::default().with_transfers(Box::new(transfers.clone()));

        let ids = state
            .download_jobs
            .iter()
            .map(|job| job.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["browser-running", "browser-paused", "browser-done"]);
        assert_eq!(state.download_counts(), (2, 3));

        state.dispatch_download_verb(TransferVerb::Pause("browser-running".to_owned()));
        state.dispatch_download_verb(TransferVerb::Resume("browser-paused".to_owned()));
        state.dispatch_download_verb(TransferVerb::Cancel("browser-done".to_owned()));

        assert_eq!(
            transfers.verbs(),
            [
                TransferVerb::Pause("browser-running".to_owned()),
                TransferVerb::Resume("browser-paused".to_owned()),
                TransferVerb::Cancel("browser-done".to_owned())
            ]
        );
    }

    #[test]
    fn browser_download_progress_summary_folds_active_ledger_jobs_for_shell_chrome() {
        let running = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Running,
            30,
        );
        let queued = transfer_fixture(
            "browser-queued",
            TransferMethod::BrowserDownload,
            TransferState::Queued,
            40,
        );
        let done = transfer_fixture(
            "browser-done",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            50,
        );
        let http = transfer_fixture("http", TransferMethod::Http, TransferState::Running, 60);
        let state =
            WebState::default().with_transfers(Box::new(RecordingTransfers::with_jobs(vec![
                done, http, queued, running,
            ])));

        let summary = state
            .operation_progress_summary()
            .expect("active Browser downloads produce shell progress");
        assert_eq!(summary.active, 2);
        assert_eq!(summary.known_progress, 1);
        assert_eq!(summary.fraction, Some(0.42));
        assert_eq!(summary.label, "2 browser downloads");
    }

    #[test]
    fn browser_download_completion_publishes_notify_feed_event_once() {
        let bus = tempfile::tempdir().expect("temp bus");
        let running = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Running,
            10,
        );
        let done = transfer_fixture(
            "browser-running",
            TransferMethod::BrowserDownload,
            TransferState::Done,
            20,
        );
        let transfers = RecordingTransfers::with_jobs(vec![running]);
        let mut state = WebState::default()
            .with_bus_root(Some(bus.path().to_path_buf()))
            .with_transfers(Box::new(transfers.clone()));

        transfers.set_jobs(vec![done]);
        state.refresh_downloads();
        state.refresh_downloads();

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_NOTIFY_BROWSER, None)
            .expect("list browser notify events");
        assert_eq!(msgs.len(), 1, "download completion is announced once");
        let body = msgs[0].body.as_deref().expect("notify body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["severity"], "info");
        assert_eq!(v["source"], "browser");
        assert_eq!(
            v["summary"],
            "Browser download complete: browser-running.bin"
        );
        assert_eq!(
            v["detail"],
            "/tmp/browser-running.bin -> /home/mm/Downloads"
        );
        assert_eq!(v["action"], "action/shell/goto/browser");
    }

    #[test]
    fn the_live_page_url_and_title_feed_the_page_actions() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        // The three page-actions all read the active session's live nav + title
        // (the testkit helper reports `about:blank` for both).
        let tab = &state.tabs[0];
        assert_eq!(tab.session.nav().url, "about:blank");
        assert_eq!(tab.session.title(), "about:blank");
        // The chrome now carrying the page-actions star menu still renders.
        assert!(
            run_panel(&mut state),
            "the page-actions chrome produced no draw"
        );
    }

    // ── live-helper: the real spawn/attach/pump glue ────────────────────────────
    //
    // Exercised through the SAME `open_with` seam the shell's Browser arm drives,
    // but with a `testkit` factory injected in place of the real `Command::spawn`,
    // so the spawn→attach→pump path runs hermetically — no Servo, no real process.

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_spawns_attaches_and_pumps_a_frame() {
        use std::cell::RefCell;
        // Hold the fake helpers so the attached session stays live through the pump.
        let helpers: RefCell<Vec<testkit::FakeHelper>> = RefCell::new(Vec::new());
        let mut state = WebState::default();
        // A real, existing path passes the "helper installed" gate; the injected
        // factory returns a testkit session instead of exec'ing Servo.
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| {
                let (session, helper) = testkit::connect()?;
                helpers.borrow_mut().push(helper);
                Ok(session)
            },
        );
        assert_eq!(
            state.tabs.len(),
            1,
            "the live open attached exactly one tab"
        );
        assert!(
            state.gate_notice.is_none(),
            "a successful open clears the gate notice"
        );
        assert!(
            run_until_texture(&mut state),
            "the live tab pumped no frame into the texture path"
        );
        assert!(state.tabs[0].texture.is_some());
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_with_no_seat_stays_the_honest_empty_state() {
        use std::cell::Cell;
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            false,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not be called without a seat",
                ))
            },
        );
        assert!(!spawned.get(), "no seat must not spawn a helper");
        assert!(state.tabs.is_empty(), "no tab attaches without a seat");
        assert!(
            state.gate_notice.is_some(),
            "the no-seat gate is named honestly"
        );
        assert_eq!(state.gate_notice.as_deref(), Some(NO_GPU_SEAT_NOTICE));
        assert!(NO_GPU_SEAT_NOTICE.is_ascii());
        assert!(
            !NO_GPU_SEAT_NOTICE.contains('\u{2014}'),
            "no-seat gate copy must avoid typographic dash glyphs"
        );
        assert!(
            !NO_GPU_SEAT_NOTICE.contains("sandboxed")
                && !NO_GPU_SEAT_NOTICE.contains("helper")
                && !NO_GPU_SEAT_NOTICE.contains("Servo"),
            "no-seat gate copy must stay Browser-facing: {NO_GPU_SEAT_NOTICE}"
        );
        // The panel draws the honest gated EmptyState, never a fake page.
        assert!(run_panel(&mut state));
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn live_open_with_an_absent_helper_stays_the_honest_empty_state() {
        use std::cell::Cell;
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let missing = std::path::PathBuf::from("/nonexistent/mde-web-preview-xyz");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            missing,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not run with an absent helper",
                ))
            },
        );
        assert!(!spawned.get(), "an absent helper binary must not spawn");
        assert!(state.tabs.is_empty());
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice == "The Browser engine is not installed.",
            "the absent-helper gate names it honestly: {notice}"
        );
        assert!(
            !notice.contains("helper") && !notice.contains("mde-web-preview"),
            "absent engine gate must not expose helper internals: {notice}"
        );
        assert!(run_panel(&mut state));
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn a_spawn_failure_surfaces_an_honest_reason_not_a_hang() {
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            bin,
            |_spec| Err(std::io::Error::other("exec denied by sandbox")),
        );
        assert!(state.tabs.is_empty(), "a failed spawn attaches no tab");
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("Browser engine failed to start") && notice.contains("exec denied"),
            "a spawn failure surfaces its reason: {notice}"
        );
        assert!(
            !notice.contains("helper"),
            "spawn failure notice must not expose helper internals: {notice}"
        );
        assert!(run_panel(&mut state), "the honest failure notice draws");
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn helper_bin_path_defaults_and_honors_engine_env_overrides() {
        use std::path::PathBuf;
        let _env = browser_env_lock();
        let _servo_helper = EnvRestore::capture(SERVO_HELPER_BIN_ENV);
        let _cef_helper = EnvRestore::capture(CEF_HELPER_BIN_ENV);
        std::env::remove_var(SERVO_HELPER_BIN_ENV);
        std::env::remove_var(CEF_HELPER_BIN_ENV);
        assert_eq!(
            helper_bin_path(BrowserEngine::Servo),
            PathBuf::from(DEFAULT_SERVO_HELPER_BIN)
        );
        assert_eq!(
            helper_bin_path(BrowserEngine::Cef),
            PathBuf::from(DEFAULT_CEF_HELPER_BIN)
        );
        std::env::set_var(SERVO_HELPER_BIN_ENV, "/opt/mde/mde-web-preview");
        std::env::set_var(CEF_HELPER_BIN_ENV, "/opt/mde/mde-web-cef");
        assert_eq!(
            helper_bin_path(BrowserEngine::Servo),
            PathBuf::from("/opt/mde/mde-web-preview")
        );
        assert_eq!(
            helper_bin_path(BrowserEngine::Cef),
            PathBuf::from("/opt/mde/mde-web-cef")
        );
        std::env::remove_var(SERVO_HELPER_BIN_ENV);
        std::env::remove_var(CEF_HELPER_BIN_ENV);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn default_engine_prefers_cef_only_when_helper_and_runtime_are_installed() {
        let _env = browser_env_lock();
        let _cef_helper = EnvRestore::capture(CEF_HELPER_BIN_ENV);
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let dir = tempfile::tempdir().expect("tempdir");
        let helper = dir.path().join("mde-web-cef");
        let runtime = dir.path().join("cef");
        std::fs::write(&helper, b"helper").expect("helper marker");
        std::fs::create_dir_all(runtime.join(CEF_RELEASE_DIR)).expect("release dir");
        std::fs::create_dir_all(runtime.join(CEF_RESOURCES_DIR)).expect("resources dir");
        std::fs::write(runtime.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"cef")
            .expect("libcef marker");
        std::fs::write(runtime.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu")
            .expect("icu marker");
        std::fs::write(
            runtime.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK),
            b"pak",
        )
        .expect("pak marker");

        std::env::set_var(CEF_HELPER_BIN_ENV, &helper);
        std::env::set_var(CEF_ROOT_ENV, &runtime);
        assert_eq!(
            WebState::default().engine,
            BrowserEngine::Cef,
            "a Workstation with the packaged CEF helper/runtime should default to Chromium"
        );

        std::fs::remove_file(runtime.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK))
            .expect("remove resources marker");
        assert_eq!(
            WebState::default().engine,
            BrowserEngine::Servo,
            "a partial CEF runtime must fall back to Servo instead of selecting a broken default"
        );
        std::env::remove_var(CEF_HELPER_BIN_ENV);
        std::env::remove_var(CEF_ROOT_ENV);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_open_requires_the_real_cef_runtime_before_spawn() {
        use std::cell::Cell;
        let _env = browser_env_lock();
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let spawned = Cell::new(false);
        let mut state = WebState::default();
        let bin = std::env::current_exe().expect("test exe path");
        std::env::remove_var(CEF_ROOT_ENV);
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            bin,
            |_spec| {
                spawned.set(true);
                Err(std::io::Error::other(
                    "factory must not run without the CEF runtime",
                ))
            },
        );
        assert!(!spawned.get(), "missing CEF runtime must gate before spawn");
        assert!(state.tabs.is_empty());
        let notice = state.gate_notice.as_deref().unwrap_or_default();
        assert!(
            notice.contains("Chromium engine") && notice.contains("not installed completely"),
            "the Chromium runtime gate names the user-facing problem: {notice}"
        );
        assert!(
            !notice.contains("CEF")
                && !notice.contains("libcef")
                && !notice.contains("/opt/")
                && !notice.contains("runtime"),
            "Chromium runtime gate must not expose engine internals: {notice}"
        );
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_power_mode_launch_env_reaches_new_helper_spawns() {
        use std::cell::RefCell;
        let _env = browser_env_lock();
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let dir = make_fake_cef_runtime("mde-shell-cef-power-env-test");
        std::env::set_var(CEF_ROOT_ENV, &dir);

        let helpers: RefCell<Vec<testkit::FakeHelper>> = RefCell::new(Vec::new());
        let mut state = WebState::default();
        state.power_mode = true;
        let bin = std::env::current_exe().expect("test exe path");
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            bin,
            |spec| {
                assert!(spec
                    .env
                    .iter()
                    .any(|(key, value)| { key == CEF_BROWSER_POWER_MODE_ENV && value == "true" }));
                assert!(spec.env.iter().any(|(key, value)| {
                    key == CEF_EXTENSION_POWER_MODE_ENV && value == "true"
                }));
                let (session, helper) = testkit::connect()?;
                helpers.borrow_mut().push(helper);
                Ok(session)
            },
        );
        assert_eq!(state.tabs.len(), 1);

        let _ = std::fs::remove_dir_all(dir);
        std::env::remove_var(CEF_ROOT_ENV);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_live_open_uses_the_browser_ui_spawn_path_and_pumps_a_frame() {
        use std::cell::RefCell;
        let _env = browser_env_lock();
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let dir = make_fake_cef_runtime("mde-shell-cef-open-test");
        std::env::set_var(CEF_ROOT_ENV, &dir);

        let helpers: RefCell<Vec<testkit::FakeHelper>> = RefCell::new(Vec::new());
        let mut state = WebState::default();
        state.select_engine(BrowserEngine::Cef);
        let bin = std::env::current_exe().expect("test exe path");
        let expected_bin = bin.clone();
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            bin,
            |spec| {
                assert_eq!(spec.helper_bin, expected_bin);
                assert_eq!(spec.url, START_URL);
                assert_eq!((spec.width, spec.height), (INIT_W, INIT_H));
                assert!(spec.env.is_empty(), "default CEF spawn stays conservative");
                let (session, helper) = testkit::connect()?;
                helpers.borrow_mut().push(helper);
                Ok(session)
            },
        );

        assert_eq!(state.tabs.len(), 1, "CEF live open attached one tab");
        assert_eq!(state.tabs[0].engine, BrowserEngine::Cef);
        assert!(
            state.gate_notice.is_none(),
            "successful CEF open clears the gate"
        );
        assert!(
            run_until_texture(&mut state),
            "CEF-selected Browser UI path did not pump a frame"
        );
        assert!(state.tabs[0].texture.is_some());

        std::env::remove_var(CEF_ROOT_ENV);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_live_browser_ui_renders_and_operates_a_real_page_when_farm_smoke_is_enabled() {
        let _env = browser_env_lock();
        if std::env::var_os("MDE_CEF_LIVE_UI_SMOKE").is_none() {
            return;
        }

        let helper_bin = helper_bin_path(BrowserEngine::Cef);
        assert!(
            helper_bin.exists(),
            "MDE_WEB_CEF_BIN must point at a built mde-web-cef helper for the live smoke: {}",
            helper_bin.display()
        );
        assert_eq!(
            cef_runtime_missing_path(),
            None,
            "MDE_CEF_ROOT must point at a complete pinned CEF runtime"
        );

        let server = LiveHttpServer::start();
        let url = server.url.clone();
        let mut state = WebState::default();
        state.select_engine(BrowserEngine::Cef);
        state.open_with(
            true,
            BrowserEngine::Cef,
            START_URL.to_owned(),
            helper_bin,
            WebSession::spawn,
        );

        assert_eq!(state.tabs.len(), 1, "CEF live smoke attached one tab");
        assert_eq!(state.tabs[0].engine, BrowserEngine::Cef);
        assert!(
            run_until_texture_for(&mut state, 600),
            "CEF did not produce the initial Browser UI frame"
        );

        state.tabs[0].texture = None;
        state.address = url.clone();
        state.submit_address();
        assert!(
            state.insecure_prompt.is_some(),
            "the live HTTP smoke should exercise the Browser HTTPS prompt seam"
        );
        state.continue_insecure_load();
        assert!(
            run_until_texture_for(&mut state, 300),
            "CEF did not render the live HTTP page through the Browser UI texture path"
        );
        assert!(
            server.hits() > 0,
            "CEF did not fetch the live smoke page at {url}"
        );
        assert!(
            wait_for_live_title_contains(&mut state, "mde-browser-ui-p0-k0-t_", 200),
            "CEF live Browser UI smoke did not observe the input-probe page title"
        );

        let ctx = egui::Context::default();
        Style::install(&ctx);
        drive_live_page_input_from_shell_ui(&ctx, &mut state);
        assert!(
            wait_for_live_title_contains(&mut state, "mde-browser-ui-p1-k1-tm", 200),
            "CEF live Browser UI smoke did not observe pointer/key/text response through the shell panel"
        );
        assert!(
            wait_for_live_page_text_contains(&mut state, "P:1 K:1 T:m", 200),
            "CEF live Browser UI smoke did not read the final page input state over the helper wire"
        );

        if let Some(public_url) = std::env::var("MDE_CEF_LIVE_UI_PUBLIC_URL")
            .ok()
            .filter(|url| !url.trim().is_empty())
        {
            state.tabs[0].texture = None;
            state.address = public_url.clone();
            state.submit_address();
            assert!(
                state.insecure_prompt.is_none(),
                "public CEF smoke URLs must be HTTPS or otherwise pre-approved: {public_url}"
            );
            assert!(
                run_until_texture_for(&mut state, 600),
                "CEF did not render the public Browser UI smoke URL: {public_url}"
            );
        }
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn servo_live_browser_ui_renders_and_operates_a_real_page_when_farm_smoke_is_enabled() {
        let _env = browser_env_lock();
        if std::env::var_os("MDE_SERVO_LIVE_UI_SMOKE").is_none() {
            return;
        }

        let helper_bin = helper_bin_path(BrowserEngine::Servo);
        assert!(
            helper_bin.exists(),
            "MDE_WEB_PREVIEW_BIN must point at a built mde-web-preview helper for the live smoke: {}",
            helper_bin.display()
        );

        let server = LiveHttpServer::start();
        let url = server.url.clone();
        let mut state = WebState::default();
        state.select_engine(BrowserEngine::Servo);
        state.open_with(
            true,
            BrowserEngine::Servo,
            START_URL.to_owned(),
            helper_bin,
            WebSession::spawn,
        );

        assert_eq!(state.tabs.len(), 1, "Servo live smoke attached one tab");
        assert_eq!(state.tabs[0].engine, BrowserEngine::Servo);
        assert!(
            run_until_texture_for(&mut state, 900),
            "Servo did not produce the initial Browser UI frame"
        );

        state.tabs[0].texture = None;
        state.address = url.clone();
        state.submit_address();
        assert!(
            state.insecure_prompt.is_some(),
            "the live HTTP smoke should exercise the Browser HTTPS prompt seam"
        );
        state.continue_insecure_load();
        assert!(
            run_until_texture_for(&mut state, 900),
            "Servo did not render the live HTTP page through the Browser UI texture path"
        );
        assert!(
            server.hits() > 0,
            "Servo did not fetch the live smoke page at {url}"
        );
        assert!(
            wait_for_live_page_text_contains(&mut state, "P:0 K:0 T:_", 400),
            "Servo live Browser UI smoke did not read the input-probe page text"
        );

        let ctx = egui::Context::default();
        Style::install(&ctx);
        drive_live_page_input_from_shell_ui(&ctx, &mut state);
        assert!(
            wait_for_live_page_text_contains(&mut state, "P:1 K:1 T:m", 400),
            "Servo live Browser UI smoke did not observe pointer/key/text response through the shell panel"
        );
    }

    #[cfg(feature = "live-helper")]
    fn drive_live_page_input_from_shell_ui(ctx: &egui::Context, state: &mut WebState) {
        let page_point = live_page_panel_point_for_frame(ctx, state, pos2(80.0, 80.0))
            .expect("live Browser UI smoke could not locate the painted page image");
        let modifiers = egui::Modifiers::default();
        let mut click_input = body_input();
        click_input.events = vec![
            egui::Event::PointerMoved(page_point),
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers,
            },
            egui::Event::PointerButton {
                pos: page_point,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers,
            },
        ];
        assert!(
            run_panel_on_ctx(ctx, state, click_input),
            "live Browser UI smoke click frame produced no egui output"
        );
        assert!(
            state.tabs[state.active].page_focused,
            "clicking the live page canvas must latch Browser page focus"
        );
        assert!(
            wait_for_live_page_text_contains(state, "P:1 K:0 T:_", 200),
            "live Browser UI smoke click did not reach the page before keyboard input"
        );

        let mut text_input = body_input();
        text_input.events = vec![
            egui::Event::Key {
                key: egui::Key::M,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            },
            egui::Event::Text("m".to_owned()),
            egui::Event::Key {
                key: egui::Key::M,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers,
            },
        ];
        assert!(
            run_panel_on_ctx(ctx, state, text_input),
            "live Browser UI smoke text frame produced no egui output"
        );
    }

    #[cfg(feature = "live-helper")]
    fn wait_for_live_title_contains(state: &mut WebState, needle: &str, frames: usize) -> bool {
        for _ in 0..frames {
            if let Some(tab) = state.tabs.get_mut(state.active) {
                tab.session.poll();
                if tab.session.title().contains(needle) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(feature = "live-helper")]
    fn wait_for_live_page_text_contains(state: &mut WebState, needle: &str, frames: usize) -> bool {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_LIVE_PAGE_TEXT_ID: AtomicU64 = AtomicU64::new(0xCE_F1);
        let first_request_id = NEXT_LIVE_PAGE_TEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut request_id = first_request_id;
        if let Some(tab) = state.tabs.get_mut(state.active) {
            let _ = tab.session.drain_page_text_events();
            tab.session.request_page_text(request_id, 2048);
        }
        for frame in 0..frames {
            if let Some(tab) = state.tabs.get_mut(state.active) {
                tab.session.poll();
                for event in tab.session.drain_page_text_events() {
                    if event.id >= first_request_id && event.text.contains(needle) {
                        return true;
                    }
                }
                if frame % 5 == 4 {
                    request_id = NEXT_LIVE_PAGE_TEXT_ID.fetch_add(1, Ordering::Relaxed);
                    tab.session.request_page_text(request_id, 2048);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_runtime_gate_accepts_the_upstream_bundle_layout() {
        let _env = browser_env_lock();
        let _cef_root = EnvRestore::capture(CEF_ROOT_ENV);
        let dir = make_fake_cef_runtime("mde-shell-cef-runtime-test");
        std::env::set_var(CEF_ROOT_ENV, &dir);
        assert_eq!(
            cef_runtime_lib(),
            dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME)
        );
        assert_eq!(cef_runtime_missing_path(), None);
        std::env::remove_var(CEF_ROOT_ENV);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(feature = "live-helper")]
    fn make_fake_cef_runtime(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(CEF_RELEASE_DIR)).expect("mkdir");
        std::fs::write(dir.join(CEF_RELEASE_DIR).join(CEF_LIB_NAME), b"test")
            .expect("libcef marker");
        std::fs::create_dir_all(dir.join(CEF_RESOURCES_DIR)).expect("resources");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_ICU_DATA), b"icu").expect("icu marker");
        std::fs::write(dir.join(CEF_RESOURCES_DIR).join(CEF_RESOURCES_PAK), b"pak")
            .expect("pak marker");
        dir
    }

    #[cfg(feature = "live-helper")]
    struct LiveHttpServer {
        url: String,
        addr: std::net::SocketAddr,
        hits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    #[cfg(feature = "live-helper")]
    impl LiveHttpServer {
        fn start() -> Self {
            use std::io::{Read, Write};
            use std::net::TcpListener;
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;
            use std::time::Duration;

            let listener = TcpListener::bind("127.0.0.1:0").expect("bind live smoke server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking live smoke server");
            let addr = listener.local_addr().expect("live smoke addr");
            let hits = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(AtomicBool::new(false));
            let server_hits = Arc::clone(&hits);
            let server_done = Arc::clone(&done);
            let handle = std::thread::spawn(move || {
                let body = LIVE_BROWSER_INPUT_SMOKE_HTML.as_bytes();
                while !server_done.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let mut buf = [0_u8; 1024];
                            let _ = stream.read(&mut buf);
                            server_hits.fetch_add(1, Ordering::SeqCst);
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{addr}/"),
                addr,
                hits,
                done,
                handle: Some(handle),
            }
        }

        fn hits(&self) -> usize {
            self.hits.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[cfg(feature = "live-helper")]
    const LIVE_BROWSER_INPUT_SMOKE_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>mde-browser-ui-p0-k0-t_</title>
<style>
html,body{margin:0;padding:0;background:#101418;color:#f4f4f4;font:16px sans-serif}
#probe{position:absolute;left:32px;top:48px;width:360px;height:128px}
#typed{position:absolute;left:40px;top:64px;width:240px;height:36px;font:18px sans-serif}
#status{position:absolute;left:40px;top:112px}
</style>
<div id="probe">
  <h1>Browser UI live smoke</h1>
  <input id="typed" autocomplete="off" value="" aria-label="Browser UI smoke input">
  <div id="status">P:0 K:0 T:_</div>
</div>
<script>
(function(){
  var state={p:0,k:0,t:"_"};
  var typed=document.getElementById("typed");
  var status=document.getElementById("status");
  function render(){
    document.title="mde-browser-ui-p"+state.p+"-k"+state.k+"-t"+state.t;
    status.textContent="P:"+state.p+" K:"+state.k+" T:"+state.t;
  }
  function focusInput(){ try { typed.focus(); } catch(_e) {} }
  document.addEventListener("pointerdown",function(){ state.p=1; focusInput(); render(); },true);
  document.addEventListener("mousedown",function(){ state.p=1; focusInput(); render(); },true);
  document.addEventListener("keydown",function(e){
    if (e && e.key && e.key.toLowerCase && e.key.toLowerCase()==="m") state.k=1;
    render();
  },true);
  document.addEventListener("keypress",function(e){
    if (e && e.key && e.key.length===1) state.t=e.key.toLowerCase();
    render();
  },true);
  typed.addEventListener("input",function(){
    var value=typed.value || "";
    state.t=(value.slice(-1) || "_").toLowerCase();
    render();
  },true);
  window.addEventListener("load",function(){ focusInput(); render(); },true);
  focusInput();
  render();
})();
</script>
"#;

    #[cfg(feature = "live-helper")]
    impl Drop for LiveHttpServer {
        fn drop(&mut self) {
            use std::net::TcpStream;
            use std::sync::atomic::Ordering;
            self.done.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(self.addr);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    // ── BOOKMARKS-BAR ─────────────────────────────────────────────────────────
    /// Build a converged daemon [`mde_bookmarks::Collection`] fixture by minting
    /// real `AddBookmark` ops (top-level, in the given order) — the exact op the
    /// mackesd bookmarks worker replays, so the fold + serialize round-trip mirrors
    /// production, not a hand-forged JSON blob.
    fn fake_bookmark_collection(entries: &[(&str, &str)]) -> mde_bookmarks::Collection {
        let author = mde_bookmarks::Author::new("tester".into(), "test-node".into());
        let mut collection = mde_bookmarks::Collection::new();
        for (i, (title, url)) in entries.iter().enumerate() {
            collection.apply(&mde_bookmarks::Op::new(
                mde_bookmarks::Hlc::new(100 + i as u64, 0, "test-node".into()),
                author.clone(),
                mde_bookmarks::OpKind::AddBookmark {
                    id: uuid::Uuid::from_u128(0x1000 + i as u128),
                    parent: None,
                    order_key: format!("a{i}"),
                    url: (*url).to_string(),
                    title: (*title).to_string(),
                    favicon_ref: None,
                    tags: Vec::new(),
                    notes: String::new(),
                    added: 100,
                    source: mde_bookmarks::Source::Manual,
                },
            ));
        }
        collection
    }

    #[test]
    fn bookmark_bar_links_fold_top_level_bookmarks_in_render_order() {
        let mut collection = fake_bookmark_collection(&[
            ("Beta", "https://beta.example/"),
            ("", "https://blank-title.example/"),
        ]);
        // A top-level folder is NOT a bar button — the bar is a flat link strip.
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(200, 0, "test-node".into()),
            mde_bookmarks::Author::new("tester".into(), "test-node".into()),
            mde_bookmarks::OpKind::AddFolder {
                id: uuid::Uuid::from_u128(0x2000),
                name: "Work".to_string(),
                parent: None,
                order_key: "a9".to_string(),
            },
        ));

        let links = bookmark_bar_links_from(&collection);
        assert_eq!(links.len(), 2, "the folder is omitted from the bar");
        assert_eq!(links[0].title, "Beta");
        assert_eq!(links[0].url, "https://beta.example/");
        // A blank stored title falls back to the URL so the button stays legible.
        assert_eq!(links[1].title, "https://blank-title.example/");
    }

    #[test]
    fn all_bookmarked_urls_includes_nested_folder_bookmarks_and_normalizes_slash() {
        let mut collection = fake_bookmark_collection(&[("Top", "https://top.example/")]);
        let author = mde_bookmarks::Author::new("tester".into(), "test-node".into());
        let folder_id = uuid::Uuid::from_u128(0x3000);
        // A folder, and a bookmark nested INSIDE it (parent = folder id).
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(300, 0, "test-node".into()),
            author.clone(),
            mde_bookmarks::OpKind::AddFolder {
                id: folder_id,
                name: "Work".to_string(),
                parent: None,
                order_key: "b0".to_string(),
            },
        ));
        collection.apply(&mde_bookmarks::Op::new(
            mde_bookmarks::Hlc::new(301, 0, "test-node".into()),
            author,
            mde_bookmarks::OpKind::AddBookmark {
                id: uuid::Uuid::from_u128(0x3001),
                parent: Some(folder_id),
                order_key: "a0".to_string(),
                url: "https://nested.example/page".to_string(),
                title: "Nested".to_string(),
                favicon_ref: None,
                tags: Vec::new(),
                notes: String::new(),
                added: 100,
                source: mde_bookmarks::Source::Manual,
            },
        ));

        let all = all_bookmarks(&collection);
        assert_eq!(
            all.len(),
            2,
            "top-level AND nested folder bookmark both counted"
        );
        let urls = bookmarked_url_set(&all);
        // Trailing slash normalized on the stored side.
        assert!(urls.contains("https://top.example"));
        assert!(urls.contains("https://nested.example/page"));
        // Membership key lights the star whether the live page URL has the slash or not.
        assert!(urls.contains(bookmark_membership_key("https://top.example/")));
        assert!(urls.contains(bookmark_membership_key("https://top.example")));
        assert!(!urls.contains(bookmark_membership_key("https://unbookmarked.example/")));
    }

    #[test]
    fn matching_bookmarks_ranks_title_prefix_then_url_then_substring() {
        let index = vec![
            BookmarkBarLink {
                title: "Rust docs".into(),
                url: "https://doc.rust-lang.org/".into(),
            },
            BookmarkBarLink {
                title: "News".into(),
                url: "https://rust-news.example/".into(),
            },
            BookmarkBarLink {
                title: "Crates".into(),
                url: "https://crates.io/".into(),
            },
        ];
        // Empty draft → no suggestions (don't dump the whole set).
        assert!(matching_bookmarks(&index, "  ", 5).is_empty());
        // "rust" matches the first two; title-prefix ("Rust docs") ranks above the
        // url-substring match ("News" whose URL contains rust-news).
        let hits = matching_bookmarks(&index, "rust", 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust docs", "title-prefix ranks first");
        assert_eq!(hits[1].title, "News", "url-substring match ranked lower");
        let fuzzy = matching_bookmarks(&index, "rstd", 5);
        assert_eq!(
            fuzzy.first().map(|b| b.title.as_str()),
            Some("Rust docs"),
            "bookmark autocomplete uses the shared fuzzy title tier"
        );
        // Cap is honored, and a no-match draft yields nothing.
        assert_eq!(matching_bookmarks(&index, "http", 1).len(), 1);
        assert!(matching_bookmarks(&index, "zzz", 5).is_empty());
    }

    #[test]
    fn bookmark_bar_visible_count_reserves_an_overflow_slot() {
        let (btn, gap, over) = (100.0, 2.0, 26.0);
        // Everything fits → no overflow slot, all shown.
        assert_eq!(
            chrome_ui::bookmark_bar_visible_count(3, 400.0, btn, gap, over),
            3
        );
        // Exactly the full-row width still shows them all.
        let exact = 3.0 * btn + 2.0 * gap;
        assert_eq!(
            chrome_ui::bookmark_bar_visible_count(3, exact, btn, gap, over),
            3
        );
        // Too narrow for all 4 → reserve the ">>" slot and show fewer (< total).
        let v = chrome_ui::bookmark_bar_visible_count(4, exact, btn, gap, over);
        assert!(v < 4, "an overflow split shows fewer than the total");
        assert!(v >= 1, "a comfortable width still shows some buttons");
        // A sliver of width shows none — the whole set lives in the overflow menu.
        assert_eq!(
            chrome_ui::bookmark_bar_visible_count(4, 20.0, btn, gap, over),
            0
        );
        // Empty collection → nothing.
        assert_eq!(
            chrome_ui::bookmark_bar_visible_count(0, 400.0, btn, gap, over),
            0
        );
    }

    #[test]
    fn browser_bookmarks_bar_mirrors_the_collection_from_the_bus() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let collection = fake_bookmark_collection(&[
            ("Example News", "https://news.example/"),
            ("Docs", "https://docs.example/"),
        ]);
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        persist
            .write(
                STATE_BOOKMARKS_COLLECTION,
                Priority::Default,
                None,
                Some(&serde_json::to_string(&collection).expect("serialize collection")),
            )
            .expect("write collection");

        state.poll_bookmarks_collection();
        assert_eq!(
            state
                .bookmark_bar_links
                .iter()
                .map(|l| (l.title.as_str(), l.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Example News", "https://news.example/"),
                ("Docs", "https://docs.example/"),
            ]
        );

        // The cursor prevents re-folding the same retained snapshot.
        state.bookmarks_collection_last_poll = None;
        state.poll_bookmarks_collection();
        assert_eq!(state.bookmark_bar_links.len(), 2, "no duplicate fold");

        // A newer converged snapshot replaces the row.
        let updated = fake_bookmark_collection(&[("Only One", "https://one.example/")]);
        persist
            .write(
                STATE_BOOKMARKS_COLLECTION,
                Priority::Default,
                None,
                Some(&serde_json::to_string(&updated).expect("serialize updated")),
            )
            .expect("write updated collection");
        state.bookmarks_collection_last_poll = None;
        state.poll_bookmarks_collection();
        assert_eq!(state.bookmark_bar_links.len(), 1);
        assert_eq!(state.bookmark_bar_links[0].url, "https://one.example/");
    }

    #[test]
    fn browser_bookmarks_bar_toggle_shows_and_hides_the_row() {
        let mut state = WebState::default();
        state.bookmark_bar_links = vec![BookmarkBarLink {
            title: "Example".to_owned(),
            url: "https://example.test/".to_owned(),
        }];
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);

        // Hidden by default (matching the other chrome toggles): no bar button.
        let out = run_panel_output(&ctx, &mut state, body_input());
        assert!(!state.bookmarks_bar_visible);
        assert!(
            !accesskit_nodes(&out)
                .iter()
                .any(|(_, n)| n.label() == Some("Example")),
            "a hidden bar renders no bookmark button"
        );

        // View → Show Bookmarks Bar reveals the button.
        state.toggle_bookmarks_bar();
        assert!(state.bookmarks_bar_visible);
        let out = run_panel_output(&ctx, &mut state, body_input());
        assert!(
            accesskit_nodes(&out)
                .iter()
                .any(|(_, n)| n.label() == Some("Example")),
            "a shown bar renders the bookmark button"
        );

        // Toggling again hides it.
        state.toggle_bookmarks_bar();
        assert!(!state.bookmarks_bar_visible);
    }

    #[test]
    fn browser_bookmarks_bar_overflow_menu_holds_the_extras() {
        // A narrow bar with more bookmarks than fit: the leading run shows on the
        // row and the rest live behind the ">>" menu. Assert the split via the pure
        // fit fn on the same fixed geometry the renderer uses.
        let total = 40usize;
        let narrow = 3.0 * chrome_ui::BOOKMARK_BTN_W; // room for only a couple buttons
        let visible = chrome_ui::bookmark_bar_visible_count(
            total,
            narrow,
            chrome_ui::BOOKMARK_BTN_W,
            CHROME_GAP,
            chrome_ui::BOOKMARK_OVERFLOW_W,
        );
        assert!(visible < total, "not all bookmarks fit the narrow row");
        assert!(visible >= 1, "at least one bookmark shows before the menu");
        assert!(
            total - visible >= 1,
            "the overflow menu holds the remaining bookmarks"
        );
    }

    #[test]
    fn browser_bookmark_click_navigates_active_tab_and_middle_click_opens_a_new_tab() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);

        // Plain click → navigate the active tab and sync the omnibox.
        state.open_bookmark("https://news.example/".to_owned(), false);
        assert_eq!(state.address, "https://news.example/");
        assert!(
            state.take_open_request().is_none(),
            "a plain click reuses the active tab, no new-tab intent"
        );

        // Middle click → open a new foreground tab on the preferred engine.
        state.open_bookmark("https://docs.example/".to_owned(), true);
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://docs.example/"
            ),
            "a middle click enqueues a new foreground tab for the bookmark"
        );
    }

    #[test]
    fn browser_bookmark_click_with_no_open_tab_opens_a_new_tab() {
        let mut state = WebState::default();
        assert!(state.tabs.is_empty());
        state.open_bookmark("https://news.example/".to_owned(), false);
        assert!(
            matches!(
                state.take_open_request(),
                Some(TabOpenIntent::NewForegroundUrl { url, .. }) if url == "https://news.example/"
            ),
            "with no active tab a click opens the bookmark in a new tab"
        );
    }
}
