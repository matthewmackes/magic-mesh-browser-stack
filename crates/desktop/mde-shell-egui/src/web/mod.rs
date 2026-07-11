//! The **Browser** surface — the sandboxed Servo browser rendered egui-native.
//!
//! BOOKMARKS-6 brokers the out-of-process `mde-web-preview` helper *into* the one
//! shell (the same EMBED model as the VDI Desktop surface): the helper renders
//! offscreen into a shared-memory frame; [`mde_web_preview_client`] receives that
//! frame fd over the per-session socket, maps it read-only, and hands the shell an
//! [`egui::ColorImage`]. This panel uploads that image to a `TextureHandle` on a
//! paint-ready (never a per-frame re-upload), paints it as the body, wires the
//! navigation chrome (back / forward / reload / address bar, §4 tokens) to the
//! control socket, and forwards this frame's egui input scaled by
//! `pixels_per_point`.
//!
//! ```text
//!   session.take_frame() ─▶ ColorImage ─▶ TextureHandle ─▶ ui paints the body
//!   chrome + ui.input     ───────────────────────────────▶ session control/input
//! ```
//!
//! Each tab is an independent [`WebSession`], so one page crashing surfaces an
//! honest "page crashed" state for THAT tab only (respawn-on-reload) and never
//! touches the others (per-session isolation). Spawning the live Servo helper is
//! the client crate's `live-helper` path, honest-gated to a GPU seat; with no live
//! session attached this surface shows an honest gated `EmptyState`, never a fake
//! page (§7).

use base64::Engine as _;
use mackes_mesh_types::peers::default_workgroup_root;
use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use mde_chat::{MessageKind, Severity};
use mde_editor_egui::spell::{self, SpellMiss};
use mde_egui::egui::{self, RichText, Sense, TextureHandle, TextureOptions};
use mde_egui::{muted_note, ChipTone, Style};
use mde_files_egui::transfers::{
    FileTransfers, Method as TransferMethod, TransferJob, TransferPolicy, TransferState,
    TransferVerb, TransfersClient,
};

use mde_web_preview_client::{
    host_of, FilterListSource, FilterListStore, RequestFilter, SafeBrowsingBlocklist, SessionState,
    WebSession,
};
use qrcode::QrCode;
use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

// ── live-helper: spawning the real sandboxed `mde-web-preview` helper ──────────
//
// Gated behind `mde-shell-egui`'s `live-helper` feature, which turns on the client
// crate's `live-helper` spawn API ([`WebSession::spawn`] + [`SpawnSpec`]). OFF by
// default so the shell stays portable and the Browser surface shows its honest
// gated EmptyState (§7); ON, the surface spawns the sandboxed helper on first open.
#[cfg(feature = "live-helper")]
use mde_web_preview_client::session::SpawnSpec;

/// The sandboxed Servo helper binary the RPM installs; overridable via
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

const CEF_DEVTOOLS_URL: &str = "http://127.0.0.1:9222/";
const CEF_DEVTOOLS_LIST_URL: &str = "http://127.0.0.1:9222/json/list";
const CEF_DEVTOOLS_TIMEOUT: Duration = Duration::from_millis(450);

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
/// overlays the Quasar dashboard chrome for it.
const NEW_TAB_URL: &str = "about:blank";

/// The first page a freshly spawned live tab loads.
#[cfg(feature = "live-helper")]
const START_URL: &str = NEW_TAB_URL;

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
const CHROME_TAB_CLOSE: f32 = 18.0;
const CHROME_NEW_TAB_W: f32 = 58.0;
const CHROME_OMNIBOX_H: f32 = 22.0;
const CHROME_GAP: f32 = 2.0;
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
struct Tab {
    /// The IPC + shm session driving one sandboxed helper.
    session: WebSession,
    /// Engine that owns this helper session.
    engine: BrowserEngine,
    /// Named container identity for the tab. Helpers are already one session per
    /// tab; this records the user-facing isolation bucket in the chrome.
    container: ContainerProfile,
    /// Browser UX intent for where this tab should land once the compositor-side
    /// multi-display handoff is wired. This is per-tab chrome state, not a fake
    /// output move.
    display_target: DisplayTarget,
    /// Per-tab audio mute state mirrored to the helper.
    muted: bool,
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
    last_frame: Option<egui::ColorImage>,
    /// Debounces panel-size changes into a single settled `session.resize` so a
    /// drag-resize drives the helper's CSS viewport once, not every frame
    /// (browser-1).
    resizer: ViewportResizer,
}

/// How long a new panel device size must hold steady before it is committed to the
/// helper as a `session.resize` — long enough that a drag-resize sends ONE settled
/// resize instead of one per frame, short enough to feel immediate.
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);

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

mod printing;
use printing::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SavedPdf {
    path: PathBuf,
    url: String,
    title: String,
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
const VOICE_COMMAND_RESULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TabOpenIntent {
    NewForeground(BrowserEngine),
    NewForegroundUrl { engine: BrowserEngine, url: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum BrowserEngine {
    #[default]
    Servo,
    Cef,
}

impl BrowserEngine {
    const fn label(self) -> &'static str {
        match self {
            Self::Servo => "Servo",
            Self::Cef => "CEF",
        }
    }

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

/// The Browser surface's state: the open tabs, the active one, and the address-bar
/// edit buffer.
pub(crate) struct WebState {
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
    /// Set when Reload is pressed on a *crashed* active tab — the shell (or a test)
    /// drains it and swaps in a fresh session (respawn-on-reload).
    respawn_requested: bool,
    /// Set by the visible tab strip's `+` button or the session-restore seam.
    /// Live-helper builds drain this by spawning helper tabs; portable builds
    /// surface the honest gate only.
    open_requested: VecDeque<TabOpenIntent>,
    /// Set when Browser chrome asks to open the rich Bookmarks manager surface.
    open_bookmarks_requested: bool,
    /// BROWSER-DD-2 vertical-tabs preference. This is purely chrome layout: it
    /// reuses the same tab/session operations and never creates a second tab model.
    vertical_tabs: bool,
    /// HTTPS-only prompt latch. Explicit `http://` navigations pause here until
    /// the operator upgrades to HTTPS, continues over HTTP, or cancels.
    insecure_prompt: Option<String>,
    /// Quasar new-tab dashboard search draft. This is chrome state, not page
    /// content; submitted searches load the mesh SearXNG URL into the active tab.
    dashboard_query: String,
    /// New-tab speed-dial shortcuts. These start with mesh-local defaults but are
    /// browser state so session sync can carry an operator's current dashboard.
    speed_dial: Vec<SpeedDialEntry>,
    /// Page find draft shown in the compact find bar.
    find_query: String,
    /// Whether the compact find bar is open.
    find_open: bool,
    /// Current page zoom percentage sent to the active helper.
    page_zoom_percent: u16,
    /// BROWSER-DD-2 live SearXNG suggestions for the omnibox. Suggestions are
    /// fetched off-thread from the mesh-local service; the UI only polls this
    /// small state object so typing never blocks a frame.
    suggestions: SuggestionState,
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
    /// Mesh-hosted safe-browsing host blocklists. The worker/file-sync half can
    /// replace these hosts; the Browser compiles them into every live session.
    safe_browsing_hosts: Vec<String>,
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
    /// Shared Transfers client. Browser downloads are just `browser_download`
    /// rows in the daemon-owned ledger, so Files and Browser show one queue.
    transfers: Box<dyn TransfersClient>,
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
    /// One-shot startup restore latch. The Browser reads the daemon-owned latest
    /// session-sync snapshot once, before the live-helper blank-tab fallback.
    startup_restore_attempted: bool,
    /// Candidate roots for daemon-persisted startup restore snapshots. Production
    /// probes the local durable root first, then the Syncthing-backed workgroup
    /// root; tests inject temp roots without touching operator state.
    session_restore_roots: Vec<PathBuf>,
    /// Last time the Browser scanned the daemon-owned send-tab outbox for concrete
    /// node-addressed records.
    incoming_send_tab_last_poll: Option<Instant>,
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
    /// the bridge-minted `client_request_id`.
    pending_passkey_requests: BTreeMap<String, usize>,
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
    /// Last viewport-capture result, shown inline instead of being swallowed.
    capture_notice: Option<String>,
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
            tabs: Vec::new(),
            active: 0,
            engine: preferred_default_engine(),
            address: String::new(),
            respawn_requested: false,
            open_requested: VecDeque::new(),
            open_bookmarks_requested: false,
            vertical_tabs: false,
            insecure_prompt: None,
            dashboard_query: String::new(),
            speed_dial: default_speed_dial(),
            find_query: String::new(),
            find_open: false,
            page_zoom_percent: 100,
            suggestions: SuggestionState::default(),
            spellcheck: SpellcheckState::default(),
            latest_spellcheck: None,
            next_page_text_request_id: 1,
            pending_spell_requests: BTreeMap::new(),
            pending_read_aloud_requests: BTreeMap::new(),
            pending_scrape_export_requests: BTreeMap::new(),
            pending_translate_requests: BTreeMap::new(),
            pending_offline_cache_requests: BTreeMap::new(),
            adfilter_store: FilterListStore::with_bundled(),
            safe_browsing_hosts: Vec::new(),
            forgotten_permission_sites: Vec::new(),
            site_permission_prompts: Vec::new(),
            site_data: SiteDataManager::default(),
            transfers: Box::new(FileTransfers::from_env()),
            download_jobs: Vec::new(),
            notified_downloads: BTreeSet::new(),
            power_mode: false,
            last_session_sync_body: None,
            startup_restore_attempted: false,
            session_restore_roots: default_session_restore_roots(),
            incoming_send_tab_last_poll: None,
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
            capture_notice: None,
            last_saved_pdf: None,
            pending_saved_pdfs: BTreeMap::new(),
            print_settings_open: false,
            cups_printers: Vec::new(),
            cups_notice: None,
            cups_settings: CupsPrintSettings::default(),
            pending_cups_prints: BTreeMap::new(),
            bus_root: mde_bus::client_data_dir(),
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

    /// WIN7-4 — the open-tab count, the SAME `self.tabs` length
    /// [`browser_accessibility_summary`] already folds into its "Active tab X
    /// of N" string (no second read, §7). Backs the Start Menu Browser
    /// tile's live fact.
    pub(crate) fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    #[cfg(test)]
    fn with_transfers(mut self, transfers: Box<dyn TransfersClient>) -> Self {
        self.transfers = transfers;
        self.refresh_downloads();
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
            .filter(|job| job.method == TransferMethod::BrowserDownload)
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
            if idx == self.active || tab.idle_suspended || tab.session.is_crashed() {
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

    fn dispatch_download_verb(&mut self, verb: TransferVerb) {
        match self.transfers.dispatch(&verb) {
            Ok(()) => {
                self.download_notice = None;
                self.refresh_downloads();
            }
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
        self.tabs.push(Tab {
            session,
            engine,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            muted: false,
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
            resizer: ViewportResizer::default(),
        });
        self.active = self.tabs.len() - 1;
        self.publish_session_snapshot();
    }

    /// Request a foreground tab. The surface owns the visible affordance; the shell
    /// live-helper path owns the process spawn, so tests and portable builds can
    /// assert the intent without fabricating a helper.
    fn request_new_tab(&mut self, engine: BrowserEngine) {
        self.open_requested
            .push_back(TabOpenIntent::NewForeground(engine));
    }

    fn request_new_tab_with_url(&mut self, engine: BrowserEngine, url: String) {
        self.open_requested
            .push_back(TabOpenIntent::NewForegroundUrl { engine, url });
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
                Some("Chromium DevTools unavailable: no live CEF page".to_owned());
            return;
        };
        if tab.engine != BrowserEngine::Cef || tab.session.is_crashed() {
            self.capture_notice = Some("Chromium DevTools requires a live CEF tab".to_owned());
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
        self.open_requested.clear();
        let count = restore_tabs.len();
        for (_, engine, url) in restore_tabs {
            self.request_new_tab_with_url(engine, url);
        }
        Ok(count)
    }

    /// One-shot startup restore from the daemon-owned latest snapshot files. The
    /// helper-spawn path drains the resulting open queue, so restore and ordinary
    /// new-tab creation stay on the same code path.
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
                if !seen.insert(key) {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                let Ok(body) = std::fs::read_to_string(&path) else {
                    continue;
                };
                match browser_send_tab_open_intent(&body, &sanitized_host) {
                    Ok((engine, url)) => {
                        self.request_new_tab_with_url(engine, url);
                        let _ = std::fs::remove_file(&path);
                        opened += 1;
                    }
                    Err(_) => continue,
                }
            }
        }
        opened
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
            let Some(tab_index) = self
                .pending_passkey_requests
                .remove(&completion.client_request_id)
            else {
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
        if self
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
    /// `WebSession`'s `Drop`, so this is the real process-teardown path.
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
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
        self.sync_address_from_active();
        self.publish_session_snapshot();
    }

    fn set_vertical_tabs(&mut self, enabled: bool) {
        self.vertical_tabs = enabled;
        self.publish_session_snapshot();
    }

    fn toggle_vertical_tabs(&mut self) {
        self.set_vertical_tabs(!self.vertical_tabs);
    }

    #[cfg(test)]
    #[cfg_attr(
        not(feature = "live-helper"),
        allow(dead_code, reason = "used by live-helper-only Browser tests")
    )]
    fn select_engine(&mut self, engine: BrowserEngine) {
        self.engine = engine;
    }

    fn submit_address(&mut self) {
        let crashed = self
            .tabs
            .get(self.active)
            .is_some_and(|t| t.session.is_crashed());
        if self.tabs.is_empty() || crashed {
            return;
        }
        let Some(url) = omnibox_target(&self.address) else {
            return;
        };
        self.suggestions.clear();
        self.address = url.clone();
        self.load_target(url);
    }

    fn load_target(&mut self, url: String) {
        if is_plain_http(&url) {
            self.insecure_prompt = Some(url);
            return;
        }
        if let Some(protocol) = ExternalProtocol::from_url(&url) {
            self.insecure_prompt = None;
            self.publish_external_protocol(protocol, &url);
            return;
        }
        self.insecure_prompt = None;
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(url);
        }
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
        let url = format!("{DEFAULT_SEARCH_URL}?q={}", percent_encode_query(q));
        self.address = url.clone();
        self.load_target(url);
    }

    fn open_mesh_service(&mut self, url: String) {
        self.address = url.clone();
        self.load_target(url);
    }

    fn continue_insecure_load(&mut self) {
        let Some(url) = self.insecure_prompt.take() else {
            return;
        };
        self.address = url.clone();
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(url);
        }
    }

    fn upgrade_insecure_load(&mut self) {
        let Some(url) = self.insecure_prompt.take() else {
            return;
        };
        let upgraded = https_upgrade(&url);
        self.address = upgraded.clone();
        self.mark_active_tab_activity();
        if let Some(tab) = self.active_tab() {
            tab.session.load(upgraded);
        }
    }

    fn cancel_insecure_load(&mut self) {
        self.insecure_prompt = None;
    }

    fn clear_active_session_data(&mut self) {
        let cleared_host = self.active_first_party();
        self.mark_active_tab_activity();
        self.insecure_prompt = None;
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
            tab.force_dark = false;
            tab.reader_mode = false;
            tab.user_scripts = false;
            tab.user_agent = UserAgentOverride::Default;
            tab.device_profile = DeviceProfile::Default;
            tab.session.load(NEW_TAB_URL);
            tab.session.set_zoom(self.page_zoom_percent);
            tab.session.clear_find();
            tab.session.set_audio_muted(false);
            tab.session.set_force_dark(false);
            tab.session.set_reader_mode(false);
            tab.session.set_user_scripts(false, "");
            tab.session.set_user_agent("");
            tab.session
                .set_device_profile(DeviceProfile::Default.wire(), 0, 0, 100, false);
        }
        if let Some(host) = cleared_host {
            self.site_data.mark_cleared(&host, unix_ms());
        }
    }

    fn active_tab_has_frame(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.last_frame.is_some() && !tab.session.is_crashed())
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
                self.record_capture_success("Captured MHTML", &path);
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
                self.capture_notice = Some(format!("Power mode: queued media manifest ({id})"));
                self.refresh_downloads();
            }
            Err(err) => {
                self.capture_notice = Some(format!("Media manifest failed: {err}"));
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
                self.capture_notice = Some(format!("Media download queue failed: {err}"));
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
                self.capture_notice = Some(format!("Image download queue failed: {err}"));
            }
        }
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
        for (index, body) in requests.into_iter().enumerate() {
            let request_url = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v["asset_url"].as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| url.clone());
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
        enqueue_browser_output_batch(
            self.transfers.as_ref(),
            &sources,
            dest_dir.to_string_lossy().as_ref(),
        )
    }

    fn record_capture_success(&mut self, label: &str, path: &Path) {
        let notice = format!("{label} {}", path.display());
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
            Ok(path) => {
                self.capture_notice = Some(format!("CUPS print queued {}", path.display()));
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
                return format!("CUPS print failed: PDF write failed {}", request.path);
            }
            return match submit_pdf_to_cups(
                Path::new(&request.path),
                &request.title,
                &request.settings,
            ) {
                Ok(job) => format!("CUPS print submitted {job}"),
                Err(err) => format!("CUPS print failed: {err}"),
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
            self.last_saved_pdf = Some(saved);
            format!("PDF saved {path}")
        } else {
            self.pending_saved_pdfs.remove(&path);
            format!("PDF failed {path}")
        }
    }

    fn open_last_saved_pdf(&mut self) {
        match self.last_saved_pdf_viewer_url() {
            Ok(url) => {
                self.capture_notice = Some("Opening PDF in CEF viewer".to_owned());
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
            return Err(format!("{} is not a readable PDF", path.display()));
        }
        file_url_for_path(path)
    }

    fn save_active_page_pdf(&mut self) {
        match self.save_active_page_pdf_to_dir(browser_pdf_dir()) {
            Ok(path) => {
                self.capture_notice = Some(format!("PDF requested {}", path.display()));
            }
            Err(err) => {
                self.capture_notice = Some(format!("PDF failed: {err}"));
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
            .is_some_and(|tab| !tab.session.is_crashed())
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

    fn set_active_tab_user_scripts(&mut self, enabled: bool) {
        if !self.can_drive_page_tools() {
            return;
        }
        let bundle = if enabled {
            curated_userscript_bundle()
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
        self.capture_notice = Some(format!("{}: sent STT request", mode.label()));
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
            self.capture_notice = Some("Read aloud: sent page text to TTS".to_owned());
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

    fn handle_passkey_event(&mut self, tab_index: usize, engine: BrowserEngine, body: &str) {
        match browser_passkey_body(engine, body) {
            Ok(handoff_body) => {
                let client_request_id = passkey_client_request_id(body);
                publish_to_bus(
                    self.bus_root.as_deref(),
                    ACTION_BROWSER_PASSKEY,
                    &handoff_body,
                );
                if let Some(client_request_id) = client_request_id {
                    self.pending_passkey_requests
                        .insert(client_request_id, tab_index);
                }
                self.capture_notice = Some("Passkey: sent ceremony to daemon".to_owned());
            }
            Err(err) => {
                self.capture_notice = Some(format!("Passkey: ignored helper event ({err})"));
            }
        }
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
        if let Some(tab) = self.active_tab() {
            tab.session.clear_find();
        }
        self.publish_session_snapshot();
    }

    fn submit_find(&mut self, backwards: bool) {
        let query = self.find_query.trim().to_owned();
        if query.is_empty() {
            if let Some(tab) = self.active_tab() {
                tab.session.clear_find();
            }
            return;
        }
        if let Some(tab) = self.active_tab() {
            tab.session.find_in_page(query, backwards);
        }
    }

    fn sync_address_from_active(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            let url = tab.session.nav().url.trim();
            if !url.is_empty() {
                self.address = url.to_owned();
            }
        }
    }

    fn poll_suggestions(&mut self) {
        self.suggestions.poll();
    }

    fn update_suggestions_for_address(&mut self) {
        self.suggestions.update_for_draft(&self.address);
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
        if let Some(tab) = self.tabs.get_mut(self.active) {
            tab.session = session;
            tab.engine = engine;
            tab.texture = None;
            tab.last_frame = None;
            tab.last_activity = Instant::now();
            tab.idle_suspended = false;
            // A fresh helper re-negotiates its viewport from scratch.
            tab.resizer = ViewportResizer::default();
        }
        self.publish_session_snapshot();
    }

    fn compiled_request_filter(&self) -> RequestFilter {
        RequestFilter::from_store(&self.adfilter_store)
            .with_safe_browsing(SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts))
    }

    fn compiled_request_filter_for_url(&self, url: &str) -> RequestFilter {
        let mut filter = self.compiled_request_filter();
        filter.set_page(url);
        filter
    }

    fn apply_adfilter_to_open_tabs(&mut self) {
        let store = self.adfilter_store.clone();
        let safe_browsing = SafeBrowsingBlocklist::from_hosts(&self.safe_browsing_hosts);
        for tab in &mut self.tabs {
            let mut filter =
                RequestFilter::from_store(&store).with_safe_browsing(safe_browsing.clone());
            filter.set_page(&tab.session.nav().url);
            tab.session.set_filter(filter);
        }
    }

    fn active_first_party(&self) -> Option<String> {
        let url = self.tabs.get(self.active)?.session.nav().url.trim();
        host_of(url)
    }

    fn active_site_blocking_enabled(&self) -> bool {
        self.active_first_party()
            .is_some_and(|host| !self.adfilter_store.allowlist().is_allowed(&host))
    }

    fn safe_browsing_summary(&self) -> String {
        if self.safe_browsing_hosts.is_empty() {
            "Safe browsing: no mesh-hosted unsafe hosts loaded".to_owned()
        } else {
            format!(
                "Safe browsing: {} mesh-hosted unsafe host{} loaded",
                self.safe_browsing_hosts.len(),
                if self.safe_browsing_hosts.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )
        }
    }

    fn site_data_summary(&self) -> String {
        self.site_data.summary(self.active_first_party().as_deref())
    }

    fn update_site_data_from_tabs(&mut self) {
        let hosts = self
            .tabs
            .iter()
            .filter_map(|tab| host_of(tab.session.nav().url.trim()))
            .collect::<Vec<_>>();
        self.site_data
            .observe_open_tabs(hosts.iter().map(String::as_str), unix_ms());
    }

    fn set_active_site_blocking(&mut self, enabled: bool) {
        let Some(host) = self.active_first_party() else {
            return;
        };
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
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "wired to synced browser policy follow-up")
    )]
    fn add_custom_filter_rules(&mut self, name: &str, raw: &str) {
        let name = name.trim();
        let raw = raw.trim();
        if name.is_empty() || raw.is_empty() {
            return;
        }
        self.adfilter_store
            .add_source(FilterListSource::custom(name, None, raw, unix_ms()));
        self.apply_adfilter_to_open_tabs();
    }

    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "wired to synced safe-browsing policy follow-up")
    )]
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
            self.gate_notice =
                Some("The sandboxed browser needs a GPU seat — none is available here.".to_owned());
            return None;
        }
        if !helper_bin.exists() {
            self.gate_notice = Some(format!(
                "The sandboxed browser helper is not installed ({}).",
                helper_bin.display()
            ));
            return None;
        }
        if engine == BrowserEngine::Cef {
            if let Some(missing) = cef_runtime_missing_path() {
                self.gate_notice = Some(format!(
                    "The Chromium/CEF runtime is not installed (missing {}).",
                    missing.display()
                ));
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
                self.gate_notice =
                    Some(format!("The sandboxed browser helper failed to start: {e}"));
                None
            }
        }
    }
}

/// Render the Browser surface into `ui`: poll every tab, upload any fresh frame on
/// the active tab, draw the navigation chrome, and paint the body (or the honest
/// crashed / loading / gated states).
pub(crate) fn web_panel(ui: &mut egui::Ui, state: &mut WebState) {
    state.poll_suggestions();
    state.poll_downloads();
    state.poll_incoming_send_tabs();
    state.poll_voice_command_results();
    state.poll_speech_statuses();
    state.poll_passkey_status();
    state.poll_passkey_results();
    state.poll_share_results();
    state.poll_translation_results();
    state.poll_offline_cache_results();
    state.poll_security_update_status();
    state.suspend_idle_tabs(Instant::now());

    // 1. Poll every tab so background tabs keep receiving — and so ONE tab's crash
    //    is observed here without disturbing the others (per-session isolation).
    let mut pdf_events = Vec::new();
    let mut page_text_events = Vec::new();
    let mut page_scrape_events = Vec::new();
    let mut passkey_events = Vec::new();
    for (idx, tab) in state.tabs.iter_mut().enumerate() {
        if tab.idle_suspended && idx != state.active {
            continue;
        }
        tab.session.poll();
        for event in tab.session.drain_pdf_events() {
            pdf_events.push((event.path, event.ok));
        }
        for event in tab.session.drain_page_text_events() {
            page_text_events.push((event.id, event.text));
        }
        for event in tab.session.drain_page_scrape_events() {
            page_scrape_events.push((event.id, event.body));
        }
        for event in tab.session.drain_passkey_events() {
            passkey_events.push((idx, tab.engine, event.body));
        }
    }
    let mut pdf_notice = None;
    for (path, ok) in pdf_events {
        pdf_notice = Some(state.handle_pdf_event(path, ok));
    }
    if let Some(notice) = pdf_notice {
        state.capture_notice = Some(notice);
    }
    for (id, text) in page_text_events {
        state.handle_page_text_event(id, text);
    }
    for (id, body) in page_scrape_events {
        state.handle_page_scrape_event(id, body);
    }
    for (tab_index, engine, body) in passkey_events {
        state.handle_passkey_event(tab_index, engine, &body);
    }
    state.poll_spellcheck();
    state.poll_session_snapshot();

    // 2. Upload the active tab's pending frame — ONLY when one is present, so an
    //    idle page never triggers a re-upload.
    if let Some(tab) = state.active_tab() {
        if let Some(img) = tab.session.take_frame() {
            tab.last_frame = Some(img.clone());
            match tab.texture.as_mut() {
                Some(handle) => handle.set(img, BROWSER_TEX),
                None => tab.texture = Some(ui.ctx().load_texture("web-preview", img, BROWSER_TEX)),
            }
        }
    }
    install_browser_accessibility(ui.ctx(), ui.max_rect(), state);

    // 3. The shared MENUBAR-ALL top bar — the UPPERCASE BROWSER title, the real
    //    WebSession menus (Edit / View / History / Bookmarks), and the live status
    //    cluster. It COMPLEMENTS the navigation chrome below (never replaces it —
    //    the address bar + back/forward/reload buttons stay), the same model→seam
    //    pattern every other surface uses (design: `docs/design/menubar-all.md`).
    if let Some(action) = menubar::show(state, ui) {
        menubar::apply(ui.ctx(), state, action);
    }
    ui.add_space(CHROME_GAP);

    if state.vertical_tabs {
        ui.horizontal(|ui| {
            tab_strip(ui, state);
            ui.add_space(CHROME_GAP);
            ui.vertical(|ui| {
                // The navigation chrome (back / forward / reload / address bar),
                // wired to the active session's control socket.
                nav_chrome(ui, state);
                find_chrome(ui, state);
                insecure_prompt(ui, state);
                capture_notice(ui, state);
                qr_share_drawer(ui, state);
                spellcheck_drawer(ui, state);
                speech_status_drawer(ui, state);
                security_update_drawer(ui, state);
                translation_drawer(ui, state);
                offline_cache_drawer(ui, state);
                print_settings_drawer(ui, state);
                downloads_drawer(ui, state);
                ui.add_space(CHROME_GAP);
                active_body(ui, state);
            });
        });
    } else {
        // First-class tab strip (BROWSER-DD-2): switch/close existing isolated
        // sessions and expose a real new-tab intent for the live-helper path.
        tab_strip(ui, state);
        ui.add_space(CHROME_GAP);

        // The navigation chrome (back / forward / reload / address bar), wired to
        // the active session's control socket.
        nav_chrome(ui, state);
        find_chrome(ui, state);
        insecure_prompt(ui, state);
        capture_notice(ui, state);
        qr_share_drawer(ui, state);
        spellcheck_drawer(ui, state);
        speech_status_drawer(ui, state);
        security_update_drawer(ui, state);
        translation_drawer(ui, state);
        offline_cache_drawer(ui, state);
        print_settings_drawer(ui, state);
        downloads_drawer(ui, state);
        ui.add_space(CHROME_GAP);
        active_body(ui, state);
    }
}

fn active_body(ui: &mut egui::Ui, state: &mut WebState) {
    // Read the active tab's status first so the crashed arm can set the respawn
    // flag without holding a `&mut Tab` borrow of `state`.
    let active = state.active;
    let status = state.tabs.get(active).map(|t| {
        (
            t.session.is_crashed(),
            t.texture.is_some(),
            is_new_tab_url(t.session.nav().url.trim()),
            crash_reason(&t.session),
        )
    });
    match status {
        Some((true, _, _, reason)) => {
            if let Some(snapshot) = state.offline_cache_fallback_for_unavailable().cloned() {
                cached_offline_body(ui, &snapshot, Some(reason.as_str()));
            } else {
                crashed_body(ui, reason, &mut state.respawn_requested);
            }
        }
        Some((false, _, true, _)) => new_tab_dashboard(ui, state),
        Some((false, true, false, _)) => paint_body(ui, state, active),
        Some((false, false, false, _)) => {
            // Connected, no first frame yet — an honest loading note, never a blank.
            centered(ui, |ui| {
                muted_note(ui, "Loading the page\u{2026}");
            });
        }
        None => {
            let cached = state.offline_cache_fallback_for_unavailable().cloned();
            // The honest gated body — a `live-helper` build shows the NAMED gate
            // notice (no seat · helper absent · spawn failed) when one is set; the
            // default build always shows the standard gated caption (§7).
            #[cfg(feature = "live-helper")]
            let notice = state.gate_notice.as_deref();
            #[cfg(not(feature = "live-helper"))]
            let notice: Option<&str> = None;
            if let Some(snapshot) = cached {
                cached_offline_body(ui, &snapshot, notice);
            } else {
                empty_body(ui, notice);
            }
        }
    }
}

fn tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    if state.vertical_tabs {
        vertical_tab_strip(ui, state);
    } else {
        horizontal_tab_strip(ui, state);
    }
}

fn horizontal_tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut move_tab: Option<(usize, usize)> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    ui.horizontal_wrapped(|ui| {
        for (idx, tab) in state.tabs.iter().enumerate() {
            let active = idx == state.active;
            let label = tab_label(tab);
            let tab_response = tab_pill(ui, &label, active);
            if tab_response.clicked() {
                select = Some(idx);
            }
            tab_response
                .on_hover_text(tab_hover(tab))
                .context_menu(|ui| {
                    if ui
                        .add_enabled(idx > 0, compact_menu_item("Move tab left"))
                        .clicked()
                    {
                        move_tab = Some((idx, idx - 1));
                        ui.close_menu();
                    }
                    if ui
                        .add_enabled(
                            idx + 1 < state.tabs.len(),
                            compact_menu_item("Move tab right"),
                        )
                        .clicked()
                    {
                        move_tab = Some((idx, idx + 1));
                        ui.close_menu();
                    }
                    let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                    if ui.add(compact_menu_item(mute_label)).clicked() {
                        mute_tab = Some((idx, !tab.muted));
                        ui.close_menu();
                    }
                    let dark_label = if tab.force_dark {
                        "Disable force dark"
                    } else {
                        "Enable force dark"
                    };
                    if ui.add(compact_menu_item(dark_label)).clicked() {
                        force_dark_tab = Some((idx, !tab.force_dark));
                        ui.close_menu();
                    }
                    let reader_label = if tab.reader_mode {
                        "Disable reader mode"
                    } else {
                        "Enable reader mode"
                    };
                    if ui.add(compact_menu_item(reader_label)).clicked() {
                        reader_tab = Some((idx, !tab.reader_mode));
                        ui.close_menu();
                    }
                    let scripts_label = if tab.user_scripts {
                        "Disable userscripts"
                    } else {
                        "Enable userscripts"
                    };
                    if ui.add(compact_menu_item(scripts_label)).clicked() {
                        user_scripts_tab = Some((idx, !tab.user_scripts));
                        ui.close_menu();
                    }
                    ui.separator();
                    for container in ContainerProfile::ALL {
                        if ui
                            .add_enabled(
                                tab.container != container,
                                compact_menu_item(container.label()),
                            )
                            .clicked()
                        {
                            container_tab = Some((idx, container));
                            ui.close_menu();
                        }
                    }
                    ui.separator();
                    for display_target in DisplayTarget::ALL {
                        if ui
                            .add_enabled(
                                tab.display_target != display_target,
                                compact_menu_item(display_target.label()),
                            )
                            .clicked()
                        {
                            display_tab = Some((idx, display_target));
                            ui.close_menu();
                        }
                    }
                    if ui.add(compact_menu_item("Close tab")).clicked() {
                        close = Some(idx);
                        ui.close_menu();
                    }
                });
            if inline_close_button(ui).clicked() {
                close = Some(idx);
            }
        }
        engine_new_tab_buttons(ui, state, false);
    });
    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, enabled)) = force_dark_tab {
        state.select_tab(idx);
        state.set_active_tab_force_dark(enabled);
    } else if let Some((idx, enabled)) = reader_tab {
        state.select_tab(idx);
        state.set_active_tab_reader_mode(enabled);
    } else if let Some((idx, enabled)) = user_scripts_tab {
        state.select_tab(idx);
        state.set_active_tab_user_scripts(enabled);
    } else if let Some((idx, container)) = container_tab {
        state.select_tab(idx);
        state.set_active_tab_container(container);
    } else if let Some((idx, display_target)) = display_tab {
        state.select_tab(idx);
        state.set_active_tab_display_target(display_target);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

fn vertical_tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut move_tab: Option<(usize, usize)> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(184.0);
            egui::ScrollArea::vertical()
                .id_salt("browser-vertical-tabs")
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    for (idx, tab) in state.tabs.iter().enumerate() {
                        let active = idx == state.active;
                        let label = tab_label(tab);
                        ui.horizontal(|ui| {
                            let width = (ui.available_width() - CHROME_TAB_CLOSE - CHROME_GAP)
                                .max(CHROME_NEW_TAB_W);
                            let resp = tab_pill_sized(ui, &label, active, width);
                            if resp.clicked() {
                                select = Some(idx);
                            }
                            resp.on_hover_text(tab_hover(tab)).context_menu(|ui| {
                                if ui
                                    .add_enabled(idx > 0, compact_menu_item("Move tab up"))
                                    .clicked()
                                {
                                    move_tab = Some((idx, idx - 1));
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        idx + 1 < state.tabs.len(),
                                        compact_menu_item("Move tab down"),
                                    )
                                    .clicked()
                                {
                                    move_tab = Some((idx, idx + 1));
                                    ui.close_menu();
                                }
                                let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                                if ui.add(compact_menu_item(mute_label)).clicked() {
                                    mute_tab = Some((idx, !tab.muted));
                                    ui.close_menu();
                                }
                                let dark_label = if tab.force_dark {
                                    "Disable force dark"
                                } else {
                                    "Enable force dark"
                                };
                                if ui.add(compact_menu_item(dark_label)).clicked() {
                                    force_dark_tab = Some((idx, !tab.force_dark));
                                    ui.close_menu();
                                }
                                let reader_label = if tab.reader_mode {
                                    "Disable reader mode"
                                } else {
                                    "Enable reader mode"
                                };
                                if ui.add(compact_menu_item(reader_label)).clicked() {
                                    reader_tab = Some((idx, !tab.reader_mode));
                                    ui.close_menu();
                                }
                                let scripts_label = if tab.user_scripts {
                                    "Disable userscripts"
                                } else {
                                    "Enable userscripts"
                                };
                                if ui.add(compact_menu_item(scripts_label)).clicked() {
                                    user_scripts_tab = Some((idx, !tab.user_scripts));
                                    ui.close_menu();
                                }
                                ui.separator();
                                for container in ContainerProfile::ALL {
                                    if ui
                                        .add_enabled(
                                            tab.container != container,
                                            compact_menu_item(container.label()),
                                        )
                                        .clicked()
                                    {
                                        container_tab = Some((idx, container));
                                        ui.close_menu();
                                    }
                                }
                                ui.separator();
                                for display_target in DisplayTarget::ALL {
                                    if ui
                                        .add_enabled(
                                            tab.display_target != display_target,
                                            compact_menu_item(display_target.label()),
                                        )
                                        .clicked()
                                    {
                                        display_tab = Some((idx, display_target));
                                        ui.close_menu();
                                    }
                                }
                                if ui.add(compact_menu_item("Close tab")).clicked() {
                                    close = Some(idx);
                                    ui.close_menu();
                                }
                            });
                            if inline_close_button(ui).clicked() {
                                close = Some(idx);
                            }
                        });
                    }
                    engine_new_tab_buttons(ui, state, true);
                });
        });
    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, enabled)) = force_dark_tab {
        state.select_tab(idx);
        state.set_active_tab_force_dark(enabled);
    } else if let Some((idx, enabled)) = reader_tab {
        state.select_tab(idx);
        state.set_active_tab_reader_mode(enabled);
    } else if let Some((idx, enabled)) = user_scripts_tab {
        state.select_tab(idx);
        state.set_active_tab_user_scripts(enabled);
    } else if let Some((idx, container)) = container_tab {
        state.select_tab(idx);
        state.set_active_tab_container(container);
    } else if let Some((idx, display_target)) = display_tab {
        state.select_tab(idx);
        state.set_active_tab_display_target(display_target);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

fn engine_new_tab_buttons(ui: &mut egui::Ui, state: &mut WebState, vertical: bool) {
    let mut button = |ui: &mut egui::Ui, engine: BrowserEngine| {
        let label = format!("+{}", engine.label());
        let mut widget =
            egui::Button::new(RichText::new(label).size(CHROME_FONT).color(Style::TEXT))
                .fill(Style::SURFACE)
                .min_size(egui::vec2(CHROME_NEW_TAB_W, CHROME_TAB_H));
        if vertical {
            widget = widget.min_size(egui::vec2(ui.available_width(), CHROME_TAB_H));
        }
        if ui
            .add(widget)
            .on_hover_text(format!("New {} tab", engine.label()))
            .clicked()
        {
            state.request_new_tab(engine);
        }
    };
    if vertical {
        button(ui, BrowserEngine::Servo);
        button(ui, BrowserEngine::Cef);
    } else {
        button(ui, BrowserEngine::Servo);
        button(ui, BrowserEngine::Cef);
    }
}

fn tab_pill(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    tab_pill_sized(ui, label, active, CHROME_TAB_W)
}

fn tab_pill_sized(ui: &mut egui::Ui, label: &str, active: bool, width: f32) -> egui::Response {
    let color = if active { Style::TEXT } else { Style::TEXT_DIM };
    let fill = if active {
        Style::SURFACE_HI
    } else {
        Style::SURFACE
    };
    ui.add(
        egui::Button::new(RichText::new(label).size(CHROME_FONT).color(color))
            .fill(fill)
            .min_size(egui::vec2(width, CHROME_TAB_H)),
    )
}

fn inline_close_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new("\u{00D7}")
                .size(CHROME_FONT)
                .color(Style::TEXT_DIM),
        )
        .fill(Style::SURFACE)
        .min_size(egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H)),
    )
    .on_hover_text("Close tab")
}

fn compact_menu_item(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).size(CHROME_FONT).color(Style::TEXT))
        .min_size(egui::vec2(124.0, CHROME_TAB_H))
}

fn tab_label(tab: &Tab) -> String {
    let title = tab.session.title().trim();
    let url = tab.session.nav().url.trim();
    let base = if !title.is_empty() {
        title
    } else if !url.is_empty() {
        url
    } else {
        "New tab"
    };
    let state = if tab.idle_suspended {
        "\u{25D2}"
    } else {
        match tab.session.state() {
            SessionState::Loading => "\u{25CC}",
            SessionState::Live => "\u{25CF}",
            SessionState::Crashed { .. } => "!",
        }
    };
    let container = tab.container.marker();
    let display = tab.display_target.marker();
    let muted = if tab.muted { "M " } else { "" };
    let force_dark = if tab.force_dark { "D " } else { "" };
    let reader = if tab.reader_mode { "R " } else { "" };
    let user_scripts = if tab.user_scripts { "S " } else { "" };
    let user_agent = tab.user_agent.marker();
    let device_profile = tab.device_profile.marker();
    format!(
        "{state} {container}{display}{muted}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}{}",
        ellipsize(base, 24)
    )
}

fn tab_hover(tab: &Tab) -> String {
    let url = tab.session.nav().url.trim();
    let state = if tab.idle_suspended {
        "Idle suspended"
    } else {
        match tab.session.state() {
            SessionState::Loading => "Loading",
            SessionState::Live => "Live",
            SessionState::Crashed { .. } => "Crashed",
        }
    };
    let container = match tab.container {
        ContainerProfile::None => String::new(),
        profile => format!(" - Container: {}", profile.label()),
    };
    let display = match tab.display_target {
        DisplayTarget::Current => String::new(),
        target => format!(" - Display target: {}", target.label()),
    };
    let audio = if tab.muted { " - Muted" } else { "" };
    let force_dark = if tab.force_dark { " - Force dark" } else { "" };
    let reader = if tab.reader_mode { " - Reader" } else { "" };
    let user_scripts = if tab.user_scripts {
        " - Curated userscripts"
    } else {
        ""
    };
    let user_agent = match tab.user_agent {
        UserAgentOverride::Default => String::new(),
        user_agent => format!(" - User agent: {}", user_agent.label()),
    };
    let device_profile = match tab.device_profile {
        DeviceProfile::Default => String::new(),
        profile => format!(" - Device: {}", profile.label()),
    };
    if url.is_empty() {
        format!(
            "{state}{container}{display}{audio}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    } else {
        format!(
            "{state} - {url}{container}{display}{audio}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    }
}

fn ellipsize(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i + 1 >= max_chars {
            out.push('\u{2026}');
            return out;
        }
        out.push(ch);
    }
    out
}

/// The crash reason string for a session, or empty if it has not crashed.
fn crash_reason(session: &WebSession) -> String {
    match session.state() {
        SessionState::Crashed { reason } => reason.clone(),
        _ => String::new(),
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
/// and delivers the live tab to a mesh node or a paired phone; Browser only owns
/// the tab metadata and stable user action.
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

/// Browser follow-me session snapshot. The sync owner drains this stream into the
/// Nebula+Syncthing session store and later drives startup restore; Browser only
/// publishes the state it already owns.
const ACTION_BROWSER_SESSION_SYNC: &str = "action/browser/session-sync";

/// Daemon-owned Browser session-sync snapshot subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SESSION_SYNC_SUBDIR` without creating
/// a desktop-shell dependency on the daemon crate.
const SESSION_SYNC_SUBDIR: &str = "browser-session-sync";

/// Daemon-owned latest snapshot filename. The file body is the Browser snapshot
/// JSON itself, so startup restore can feed it straight into the parser.
const SESSION_SYNC_LATEST_FILE: &str = "latest.json";

/// Daemon-owned send-tab outbox subdirectory. Must match
/// `mackesd::workers::browser_session_sync::SEND_TAB_OUTBOX_SUBDIR`.
const SEND_TAB_OUTBOX_SUBDIR: &str = "browser-send-tab";

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
        format!("TTS {}", self.state)
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
        match self.last_mode.as_deref() {
            Some("dictation") => format!("Dictation {}", self.state),
            _ => format!("Voice {}", self.state),
        }
    }
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
    fn ceremony_is_visible(&self) -> bool {
        self.state != "idle" || self.accepted > 0 || self.rejected > 0
    }

    fn hardware_is_visible(&self) -> bool {
        self.hardware_state != "unknown"
    }

    fn ctaphid_is_visible(&self) -> bool {
        self.hardware_ctaphid_state == "init_request_ready"
            && self.hardware_ctaphid_init_frame_count > 0
    }

    fn tone(&self) -> ChipTone {
        match self.state.as_str() {
            "pending" => ChipTone::Info,
            "created" | "asserted" => ChipTone::Ok,
            "error" => ChipTone::Warn,
            _ => ChipTone::Neutral,
        }
    }

    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "pending" => "Passkey pending".to_owned(),
            "created" => "Passkey created".to_owned(),
            "asserted" => "Passkey asserted".to_owned(),
            "error" => "Passkey error".to_owned(),
            other => format!("Passkey {other}"),
        }
    }

    fn hardware_tone(&self) -> ChipTone {
        match self.hardware_state.as_str() {
            "ready" => ChipTone::Ok,
            "present_permission_denied" => ChipTone::Warn,
            "unavailable" => ChipTone::Neutral,
            _ => ChipTone::Neutral,
        }
    }

    fn hardware_chip_label(&self) -> String {
        match self.hardware_state.as_str() {
            "ready" => "Security key ready".to_owned(),
            "present_permission_denied" => "Security key blocked".to_owned(),
            "unavailable" => "Security key unavailable".to_owned(),
            other => format!("Security key {other}"),
        }
    }

    fn ctaphid_tone(&self) -> ChipTone {
        match self.hardware_ctaphid_state.as_str() {
            "init_request_ready" => ChipTone::Info,
            _ => ChipTone::Neutral,
        }
    }

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

    fn chip_label(&self) -> String {
        match self.state.as_str() {
            "current" => "CEF current".to_owned(),
            "missing" => "CEF missing".to_owned(),
            "mismatch" => "CEF mismatch".to_owned(),
            "manifest_missing" => "CEF manifest".to_owned(),
            other => format!("CEF {other}"),
        }
    }
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
        Err(err) if err.is_empty() => "Spelling unavailable".to_owned(),
        Err(err) => format!("Spelling unavailable: {err}"),
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
}

impl SuggestionState {
    fn clear(&mut self) {
        self.draft.clear();
        self.items.clear();
        self.notice = None;
        self.in_flight = None;
        self.rx = None;
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

fn browser_send_tab_open_intent(body: &str, host: &str) -> Result<(BrowserEngine, String), String> {
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
    Ok((engine, url.to_owned()))
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

/// Build the `action/bookmarks/add` body for the live page. Pure — the wire shape
/// is asserted headless. `source` is omitted, so the worker mints the default
/// `Source::Manual` (a page the user bookmarked in-app).
fn bookmark_add_body(url: &str, title: &str) -> String {
    serde_json::json!({ "url": url, "title": title }).to_string()
}

fn adfilter_domain_body(domain: &str) -> String {
    serde_json::json!({ "domain": domain.trim() }).to_string()
}

fn browser_display_target_body(
    tab_index: usize,
    tab: &Tab,
    display_target: DisplayTarget,
) -> String {
    serde_json::json!({
        "op": "browser_display_target",
        "tab_index": tab_index,
        "engine": tab.engine.wire(),
        "target": display_target.wire(),
        "url": tab.session.nav().url.as_str(),
        "title": tab.session.title(),
    })
    .to_string()
}

/// Build the `action/chat/send` body sharing the live page into Chat. A link is
/// carried as the NOTIFY-CHAT [`MessageKind::Clipboard`] kind — its `preview`
/// (the title, falling back to the URL) shows in the timeline and its `full` (the
/// exact URL) is what a one-click re-copy puts back. Pure: the `kind` is a real
/// `mde_chat::MessageKind`, so it round-trips straight into what the worker
/// accepts (the same shape Files' `chat_bridge` writes).
fn chat_share_body(to: &str, url: &str, title: &str) -> String {
    let preview = if title.trim().is_empty() { url } else { title };
    let kind = MessageKind::Clipboard {
        preview: preview.to_string(),
        full: url.to_string(),
    };
    let kind_val = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
    serde_json::json!({ "scope": "peer", "to": to, "kind": kind_val }).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserShareTarget {
    Peer,
    Phone,
    Email,
    Qr,
}

impl BrowserShareTarget {
    const fn wire(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Phone => "phone",
            Self::Email => "email",
            Self::Qr => "qr",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Peer => "Peer",
            Self::Phone => "Phone",
            Self::Email => "Email",
            Self::Qr => "QR",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "peer" => Some(Self::Peer),
            "phone" => Some(Self::Phone),
            "email" => Some(Self::Email),
            "qr" => Some(Self::Qr),
            _ => None,
        }
    }

    fn destination(self) -> Option<(String, String)> {
        match self {
            Self::Phone => browser_phone_target_destination(),
            Self::Peer | Self::Email | Self::Qr => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserSendTabTarget {
    Node,
    Phone,
}

impl BrowserSendTabTarget {
    const fn wire(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Phone => "phone",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Node => "Node",
            Self::Phone => "Phone",
        }
    }

    fn destination(self) -> Option<(String, String)> {
        match self {
            Self::Node => {
                let host = local_hostname();
                Some((host.clone(), host))
            }
            Self::Phone => browser_phone_target_destination(),
        }
    }
}

fn browser_phone_target_destination() -> Option<(String, String)> {
    std::env::var("MDE_BROWSER_SEND_PHONE_TARGET")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .map(|id| {
            let label = std::env::var("MDE_BROWSER_SEND_PHONE_LABEL")
                .ok()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| id.clone());
            (id, label)
        })
}

/// Build the browser-owned platform share handoff. The receiving surfaces are
/// intentionally outside Browser ownership, so this publishes a stable typed verb
/// instead of pretending to complete peer/email/QR delivery in-process.
fn browser_share_body(target: BrowserShareTarget, url: &str, title: &str) -> String {
    let title = title.trim();
    let preview = if title.is_empty() { url } else { title };
    let mut body = serde_json::json!({
        "op": "browser_share",
        "target": target.wire(),
        "url": url,
        "title": title,
        "preview": preview,
        "source": "browser",
        "host": local_hostname(),
    });
    if let Some((id, label)) = target.destination() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("target_id".to_owned(), serde_json::json!(id));
            obj.insert("target_label".to_owned(), serde_json::json!(label));
        }
    }
    body.to_string()
}

fn publish_browser_share(root: Option<&Path>, target: BrowserShareTarget, url: &str, title: &str) {
    let body = browser_share_body(target, url, title);
    if root.is_some() {
        publish_to_bus(root, ACTION_BROWSER_SHARE, &body);
    } else {
        publish(ACTION_BROWSER_SHARE, &body);
    }
}

/// Build the browser-owned send-tab handoff for BROWSER-DD-7. Target selection and
/// delivery live in the session-sync / phone owners, so the Browser publishes the
/// current tab's URL/title/engine metadata and lets those owners route it.
fn browser_send_tab_body(
    target: BrowserSendTabTarget,
    engine: BrowserEngine,
    url: &str,
    title: &str,
) -> String {
    let title = title.trim();
    let preview = if title.is_empty() { url } else { title };
    let mut body = serde_json::json!({
        "op": "browser_send_tab",
        "target": target.wire(),
        "engine": engine.wire(),
        "url": url,
        "title": title,
        "preview": preview,
        "source": "browser",
        "host": local_hostname(),
    });
    if let Some((id, label)) = target.destination() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("target_id".to_owned(), serde_json::json!(id));
            obj.insert("target_label".to_owned(), serde_json::json!(label));
        }
    }
    body.to_string()
}

fn publish_browser_send_tab(
    root: Option<&Path>,
    target: BrowserSendTabTarget,
    engine: BrowserEngine,
    url: &str,
    title: &str,
) {
    let body = browser_send_tab_body(target, engine, url, title);
    if root.is_some() {
        publish_to_bus(root, ACTION_BROWSER_SEND_TAB, &body);
    } else {
        publish(ACTION_BROWSER_SEND_TAB, &body);
    }
}

fn browser_permission_prompt_body(
    kind: DevicePermissionKind,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    site: &str,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_permission_prompt",
        "permission": kind.wire(),
        "decision": "deny",
        "enforcement": "helper_default_deny",
        "engine": engine.wire(),
        "url": url,
        "title": title.trim(),
        "site": site,
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

fn browser_passkey_body(engine: BrowserEngine, helper_body: &str) -> Result<String, String> {
    let helper: serde_json::Value =
        serde_json::from_str(helper_body).map_err(|err| format!("invalid helper JSON: {err}"))?;
    let ceremony = status_required_str(&helper, "ceremony", "passkey helper event")?;
    if !matches!(ceremony.as_str(), "create" | "get") {
        return Err("unsupported ceremony".to_owned());
    }
    let origin = status_required_str(&helper, "origin", "passkey helper event")?;
    let rp_id = status_required_str(&helper, "rp_id", "passkey helper event")?;
    let challenge_b64url =
        status_required_str(&helper, "challenge_b64url", "passkey helper event")?;

    let mut body = serde_json::json!({
        "op": "browser_passkey",
        "source": "browser",
        "host": local_hostname(),
        "engine": engine.wire(),
        "ceremony": ceremony,
        "origin": origin,
        "rp_id": rp_id,
        "challenge_b64url": challenge_b64url,
    });
    let Some(obj) = body.as_object_mut() else {
        return Err("could not build passkey body".to_owned());
    };
    for key in ["user_handle_b64url", "user_name"] {
        if let Some(value) = optional_trimmed_str(&helper, key) {
            obj.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    if let Some(value) = optional_trimmed_str(&helper, "client_request_id") {
        obj.insert("client_request_id".to_owned(), serde_json::json!(value));
    }
    // security-2: forward the shim's user-presence (user-gesture) signal so the
    // daemon sets the WebAuthn User Present bit only when a human interaction was
    // actually observed, rather than hardcoding it. Absent => not present.
    let user_present = helper
        .get("user_present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    obj.insert("user_present".to_owned(), serde_json::json!(user_present));
    if let Some(timeout_ms) = helper.get("timeout_ms").and_then(serde_json::Value::as_u64) {
        obj.insert("timeout_ms".to_owned(), serde_json::json!(timeout_ms));
    }
    if let Some(credentials) = helper
        .get("allow_credentials")
        .and_then(serde_json::Value::as_array)
    {
        let credentials = credentials
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|credential| !credential.is_empty())
            .take(64)
            .collect::<Vec<_>>();
        obj.insert(
            "allow_credentials".to_owned(),
            serde_json::json!(credentials),
        );
    }
    Ok(body.to_string())
}

fn passkey_client_request_id(helper_body: &str) -> Option<String> {
    let helper: serde_json::Value = serde_json::from_str(helper_body).ok()?;
    optional_trimmed_str(&helper, "client_request_id")
}

const READ_ALOUD_TEXT_MAX_CHARS: usize = 20_000;
const TRANSLATE_TEXT_MAX_CHARS: usize = 20_000;
const TRANSLATION_RESULT_MAX_CHARS: usize = 40_000;
const OFFLINE_CACHE_TEXT_MAX_CHARS: usize = 64_000;
const OFFLINE_CACHE_VIEWPORT_MAX_BYTES: usize = 2 * 1024 * 1024;
const OFFLINE_CACHE_MHTML_MAX_BYTES: usize = 4 * 1024 * 1024;
const OFFLINE_CACHE_PDF_MAX_BYTES: usize = 8 * 1024 * 1024;
const OFFLINE_CACHE_RESOURCE_MAX_COUNT: usize = 128;
const OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS: usize = 2_048;
const MEDIA_SNIFFER_MAX_COUNT: usize = 128;
const MEDIA_SNIFFER_URL_MAX_CHARS: usize = 2_048;
const SCRAPE_CRAWL_SEED_MAX_COUNT: usize = 64;
const SCRAPE_CRAWL_MANIFEST_MAX_COUNT: usize = 128;
const SCRAPE_EXTRACT_TEXT_MAX_CHARS: usize = 64_000;
const SCRAPE_ARTICLE_TEXT_MAX_CHARS: usize = 16_000;
const SCRAPE_DOM_LINK_MAX_COUNT: usize = 64;
const SCRAPE_DOM_HEADING_MAX_COUNT: usize = 32;
const SCRAPE_DOM_TEXT_MAX_CHARS: usize = 240;

fn browser_read_aloud_body(request: &ReadAloudRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, READ_ALOUD_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    serde_json::json!({
        "op": "browser_read_aloud",
        "source": "browser",
        "host": local_hostname(),
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    })
    .to_string()
}

fn browser_translate_target_lang() -> String {
    let raw =
        std::env::var("MDE_BROWSER_TRANSLATE_TARGET_LANG").unwrap_or_else(|_| "en".to_owned());
    let lang = raw.trim();
    if lang.is_empty() {
        "en".to_owned()
    } else {
        clamp_chars(lang, 32)
    }
}

fn browser_translate_body(request: &TranslateRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, TRANSLATE_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    serde_json::json!({
        "op": "browser_translate",
        "source": "browser",
        "host": local_hostname(),
        "privacy": "offline_or_mesh_only",
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "source_lang": request.source_lang.trim(),
        "target_lang": request.target_lang.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    })
    .to_string()
}

fn browser_offline_cache_body(request: &OfflineCacheRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, OFFLINE_CACHE_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    let mut body = serde_json::json!({
        "op": "browser_offline_cache",
        "source": "browser",
        "host": local_hostname(),
        "privacy": "offline_or_mesh_only",
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    });
    if let Some(viewport) = &request.viewport {
        body["viewport_image"] = serde_json::json!({
            "mime": &viewport.mime,
            "width": viewport.width,
            "height": viewport.height,
            "data": &viewport.data_base64,
        });
    }
    if !request.resources.is_empty() {
        body["resource_manifest"] = serde_json::Value::Array(
            request
                .resources
                .iter()
                .map(|resource| {
                    serde_json::json!({
                        "url": &resource.url,
                        "resource": &resource.resource,
                        "allowed": resource.allowed,
                    })
                })
                .collect(),
        );
    }
    if let Some(archive) = offline_cache_mhtml_archive(request, &text, unix_ms()) {
        body["archive_mhtml"] = serde_json::json!({
            "mime": &archive.mime,
            "filename": &archive.filename,
            "bytes": archive.bytes,
            "data": &archive.data_base64,
        });
    }
    if let Some(pdf) = &request.pdf_snapshot {
        body["pdf_snapshot"] = serde_json::json!({
            "mime": &pdf.mime,
            "filename": &pdf.filename,
            "bytes": pdf.bytes,
            "data": &pdf.data_base64,
        });
    }
    body.to_string()
}

fn offline_cache_mhtml_archive(
    request: &OfflineCacheRequest,
    text: &str,
    unix_ms: u64,
) -> Option<OfflineCacheArchive> {
    let viewport_png = request
        .viewport
        .as_ref()
        .and_then(|viewport| {
            base64::engine::general_purpose::STANDARD
                .decode(viewport.data_base64.as_str())
                .ok()
        })
        .filter(|bytes| bytes.len() <= OFFLINE_CACHE_VIEWPORT_MAX_BYTES);
    let bytes = offline_cache_mhtml_document(
        &request.url,
        &request.title,
        unix_ms,
        text,
        viewport_png.as_deref(),
    );
    if bytes.is_empty() || bytes.len() > OFFLINE_CACHE_MHTML_MAX_BYTES {
        return None;
    }
    Some(OfflineCacheArchive {
        mime: "multipart/related".to_owned(),
        filename: capture_mhtml_filename_for(&request.url, &request.title, unix_ms),
        bytes: bytes.len(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn offline_cache_viewport_image(frame: &egui::ColorImage) -> Option<OfflineCacheViewportImage> {
    let [width, height] = frame.size;
    let png = encode_color_image_png(frame).ok()?;
    if png.len() > OFFLINE_CACHE_VIEWPORT_MAX_BYTES {
        return None;
    }
    Some(OfflineCacheViewportImage {
        mime: "image/png".to_owned(),
        width,
        height,
        data_base64: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

fn offline_cache_resource_manifest(
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<OfflineCacheResource> {
    recent
        .iter()
        .rev()
        .take(OFFLINE_CACHE_RESOURCE_MAX_COUNT)
        .filter_map(|resource| {
            let url = resource.url.trim();
            if url.is_empty() {
                return None;
            }
            Some(OfflineCacheResource {
                url: clamp_chars(url, OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS),
                resource: offline_cache_resource_type_name(resource.resource).to_owned(),
                allowed: resource.allowed,
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn offline_cache_resource_type_name(resource: u8) -> &'static str {
    match mde_web_preview_client::resource_from_wire(resource) {
        mde_web_preview_client::ResourceType::Document => "document",
        mde_web_preview_client::ResourceType::Subdocument => "subdocument",
        mde_web_preview_client::ResourceType::Stylesheet => "stylesheet",
        mde_web_preview_client::ResourceType::Script => "script",
        mde_web_preview_client::ResourceType::Image => "image",
        mde_web_preview_client::ResourceType::Font => "font",
        mde_web_preview_client::ResourceType::Media => "media",
        mde_web_preview_client::ResourceType::Object => "object",
        mde_web_preview_client::ResourceType::XmlHttpRequest => "xhr",
        mde_web_preview_client::ResourceType::Ping => "ping",
        mde_web_preview_client::ResourceType::WebSocket => "websocket",
        mde_web_preview_client::ResourceType::Other => "other",
    }
}

fn offline_cache_pdf_snapshot(saved: &SavedPdf) -> Option<OfflineCachePdf> {
    let bytes = std::fs::read(&saved.path).ok()?;
    if validate_offline_cache_pdf_bytes(&bytes, bytes.len()).is_err() {
        return None;
    }
    let filename = saved
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| valid_offline_pdf_filename(name))
        .map(str::to_owned)
        .unwrap_or_else(|| pdf_filename_for(&saved.url, &saved.title, unix_ms()));
    Some(OfflineCachePdf {
        mime: "application/pdf".to_owned(),
        filename,
        bytes: bytes.len(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn browser_voice_command_body(
    mode: VoiceCommandMode,
    tab_index: usize,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    address: &str,
    page_focused: bool,
) -> String {
    serde_json::json!({
        "op": "browser_voice_command",
        "source": "browser",
        "host": local_hostname(),
        "mode": mode.wire(),
        "tab_index": tab_index,
        "engine": engine.wire(),
        "url": url,
        "title": title.trim(),
        "address": address.trim(),
        "focus": if page_focused { "page" } else { "chrome" },
        "max_transcript_chars": 4096,
    })
    .to_string()
}

fn browser_voice_command_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_VOICE_COMMAND_PREFIX}{host}")
}

fn browser_read_aloud_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_READ_ALOUD_PREFIX}{host}")
}

fn browser_voice_command_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_VOICE_COMMAND_PREFIX}{host}")
}

fn browser_passkey_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_PASSKEYS_PREFIX}{host}")
}

fn browser_passkey_event_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_PASSKEYS_PREFIX}{host}")
}

fn browser_translation_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_TRANSLATE_PREFIX}{host}")
}

fn browser_share_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_SHARE_PREFIX}{host}")
}

fn browser_offline_cache_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_OFFLINE_CACHE_PREFIX}{host}")
}

fn browser_security_update_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_SECURITY_UPDATE_PREFIX}{host}")
}

fn cache_url_keys(url: &str) -> Vec<String> {
    let url = url.trim();
    if url.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![url.to_owned()];
    if let Some(canonical) = canonical_http_cache_url(url) {
        if !keys.iter().any(|key| key == &canonical) {
            keys.push(canonical);
        }
    }
    keys
}

fn canonical_http_cache_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (before_query, query) = rest
        .split_once('?')
        .map_or((rest, None), |(before, query)| (before, Some(query)));
    let (authority, path) = before_query
        .split_once('/')
        .map_or((before_query, ""), |(authority, path)| (authority, path));
    let authority = canonical_http_authority(&scheme, authority)?;
    let query = canonical_query(query);
    Some(match query {
        Some(query) => format!("{scheme}://{authority}/{path}?{query}"),
        None => format!("{scheme}://{authority}/{path}"),
    })
}

fn canonical_http_authority(scheme: &str, authority: &str) -> Option<String> {
    let authority = authority.trim();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after_host) = rest.split_once(']')?;
        let host = host.to_ascii_lowercase();
        let port = after_host.strip_prefix(':');
        return match port {
            Some(port) if is_default_http_port(scheme, port) => Some(format!("[{host}]")),
            Some(port) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                Some(format!("[{host}]:{port}"))
            }
            Some(_) => None,
            None if after_host.is_empty() => Some(format!("[{host}]")),
            None => None,
        };
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                (host, Some(port))
            } else {
                (authority, None)
            }
        });
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    match port {
        Some(port) if is_default_http_port(scheme, port) => Some(host),
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host),
    }
}

fn is_default_http_port(scheme: &str, port: &str) -> bool {
    matches!((scheme, port), ("http", "80") | ("https", "443"))
}

fn canonical_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    if query.is_empty() {
        return None;
    }
    let mut pairs = query
        .split('&')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_unstable();
    Some(pairs.join("&"))
}

fn parse_translation_result(body: &str) -> Result<BrowserTranslationResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("translation result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_translation") {
        return Err("translation result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_translate") {
        return Err("translation result has the wrong source".to_owned());
    }
    let host = result_required_str(&v, "host")?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "translation result is missing tab_index".to_owned())?;
    let engine_wire = result_required_str(&v, "engine")?;
    let engine = BrowserEngine::from_wire(&engine_wire)
        .ok_or_else(|| "translation result has an unsupported engine".to_owned())?;
    let translation = clamp_chars(
        &result_required_str(&v, "translation")?,
        TRANSLATION_RESULT_MAX_CHARS,
    );
    Ok(BrowserTranslationResult {
        host,
        tab_index,
        engine,
        url: result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        source_lang: result_required_str(&v, "source_lang")?,
        target_lang: result_required_str(&v, "target_lang")?,
        translation,
    })
}

fn parse_share_route_result(body: &str) -> Result<BrowserShareRouteResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("share result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_share_routed") {
        return Err("share result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_share") {
        return Err("share result has the wrong source".to_owned());
    }
    let target_wire = result_required_str(&v, "target")?;
    let target = BrowserShareTarget::from_wire(&target_wire)
        .ok_or_else(|| "share result has an unsupported target".to_owned())?;
    Ok(BrowserShareRouteResult {
        host: result_required_str(&v, "host")?,
        target,
        url: result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        preview: result_required_str(&v, "preview")?,
        request_id: result_required_str(&v, "request_id")?,
    })
}

fn qr_share_result(route: BrowserShareRouteResult) -> Result<BrowserQrShareResult, String> {
    if route.target != BrowserShareTarget::Qr {
        return Err("share result is not a QR route".to_owned());
    }
    let modules = qr_modules(&route.url)?;
    Ok(BrowserQrShareResult {
        host: route.host,
        url: route.url,
        title: route.title,
        preview: route.preview,
        request_id: route.request_id,
        modules,
    })
}

fn qr_modules(url: &str) -> Result<Vec<Vec<bool>>, String> {
    let code = QrCode::new(url.as_bytes()).map_err(|err| format!("QR encode failed: {err}"))?;
    let width = code.width();
    let mut modules = Vec::with_capacity(width);
    for y in 0..width {
        let mut row = Vec::with_capacity(width);
        for x in 0..width {
            row.push(code[(x, y)] == qrcode::Color::Dark);
        }
        modules.push(row);
    }
    Ok(modules)
}

fn parse_offline_cache_result(body: &str) -> Result<BrowserOfflineCacheResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("offline-cache result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_offline_cache_record") {
        return Err("offline-cache result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_offline_cache") {
        return Err("offline-cache result has the wrong source".to_owned());
    }
    if v.get("privacy").and_then(serde_json::Value::as_str) != Some("offline_or_mesh_only") {
        return Err("offline-cache result is not private".to_owned());
    }
    let host = cache_result_required_str(&v, "host")?;
    let cache_id = cache_result_required_str(&v, "cache_id")?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "offline-cache result is missing tab_index".to_owned())?;
    let engine_wire = cache_result_required_str(&v, "engine")?;
    let engine = BrowserEngine::from_wire(&engine_wire)
        .ok_or_else(|| "offline-cache result has an unsupported engine".to_owned())?;
    let text = clamp_chars(
        &cache_result_required_str(&v, "text")?,
        OFFLINE_CACHE_TEXT_MAX_CHARS,
    );
    let viewport = v
        .get("viewport_image")
        .map(parse_offline_cache_viewport_image)
        .transpose()?;
    let resources = v
        .get("resource_manifest")
        .map(parse_offline_cache_resource_manifest)
        .transpose()?
        .unwrap_or_default();
    let archive_mhtml = v
        .get("archive_mhtml")
        .map(parse_offline_cache_mhtml_archive)
        .transpose()?;
    let pdf_snapshot = v
        .get("pdf_snapshot")
        .map(parse_offline_cache_pdf_snapshot)
        .transpose()?;
    Ok(BrowserOfflineCacheResult {
        host,
        cache_id,
        tab_index,
        engine,
        url: cache_result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        text,
        viewport,
        resources,
        archive_mhtml,
        pdf_snapshot,
        cached_ms: v.get("cached_ms").and_then(serde_json::Value::as_u64),
    })
}

fn parse_offline_cache_viewport_image(
    v: &serde_json::Value,
) -> Result<OfflineCacheViewportImage, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "image/png")
        .ok_or_else(|| "offline-cache viewport image must be image/png".to_owned())?;
    let width = v
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "offline-cache viewport image is missing width".to_owned())?;
    let height = v
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "offline-cache viewport image is missing height".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache viewport image is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache viewport image base64: {err}"))?;
    if bytes.len() > OFFLINE_CACHE_VIEWPORT_MAX_BYTES {
        return Err("offline-cache viewport image is too large".to_owned());
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("offline-cache viewport image is not a PNG".to_owned());
    }
    Ok(OfflineCacheViewportImage {
        mime: mime.to_owned(),
        width,
        height,
        data_base64: data_base64.to_owned(),
    })
}

fn parse_offline_cache_mhtml_archive(v: &serde_json::Value) -> Result<OfflineCacheArchive, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "multipart/related")
        .ok_or_else(|| "offline-cache archive must be multipart/related".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_offline_archive_filename(name))
        .ok_or_else(|| "offline-cache archive filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= OFFLINE_CACHE_MHTML_MAX_BYTES)
        .ok_or_else(|| "offline-cache archive has invalid byte count".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache archive is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache archive base64: {err}"))?;
    validate_offline_cache_archive_bytes(&bytes, declared_bytes)?;
    Ok(OfflineCacheArchive {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data_base64: data_base64.to_owned(),
    })
}

fn parse_offline_cache_resource_manifest(
    v: &serde_json::Value,
) -> Result<Vec<OfflineCacheResource>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| "offline-cache resource manifest must be an array".to_owned())?;
    if items.len() > OFFLINE_CACHE_RESOURCE_MAX_COUNT {
        return Err("offline-cache resource manifest has too many entries".to_owned());
    }
    items
        .iter()
        .map(parse_offline_cache_resource)
        .collect::<Result<Vec<_>, _>>()
}

fn parse_offline_cache_resource(v: &serde_json::Value) -> Result<OfflineCacheResource, String> {
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| {
            !url.is_empty() && url.chars().count() <= OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS
        })
        .ok_or_else(|| "offline-cache resource URL is invalid".to_owned())?;
    let resource = v
        .get("resource")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|resource| valid_offline_resource_type(resource))
        .ok_or_else(|| "offline-cache resource type is invalid".to_owned())?;
    let allowed = v
        .get("allowed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "offline-cache resource allowed flag is missing".to_owned())?;
    Ok(OfflineCacheResource {
        url: url.to_owned(),
        resource: resource.to_owned(),
        allowed,
    })
}

fn offline_cache_archive_bytes(archive: &OfflineCacheArchive) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(archive.data_base64.as_str())
        .map_err(|err| format!("offline-cache archive base64: {err}"))?;
    validate_offline_cache_archive_bytes(&bytes, archive.bytes)?;
    Ok(bytes)
}

fn parse_offline_cache_pdf_snapshot(v: &serde_json::Value) -> Result<OfflineCachePdf, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "application/pdf")
        .ok_or_else(|| "offline-cache PDF must be application/pdf".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_offline_pdf_filename(name))
        .ok_or_else(|| "offline-cache PDF filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= OFFLINE_CACHE_PDF_MAX_BYTES)
        .ok_or_else(|| "offline-cache PDF has invalid byte count".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache PDF is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache PDF base64: {err}"))?;
    validate_offline_cache_pdf_bytes(&bytes, declared_bytes)?;
    Ok(OfflineCachePdf {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data_base64: data_base64.to_owned(),
    })
}

fn offline_cache_pdf_bytes(pdf: &OfflineCachePdf) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pdf.data_base64.as_str())
        .map_err(|err| format!("offline-cache PDF base64: {err}"))?;
    validate_offline_cache_pdf_bytes(&bytes, pdf.bytes)?;
    Ok(bytes)
}

fn validate_offline_cache_pdf_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("offline-cache PDF byte count mismatch".to_owned());
    }
    if bytes.len() > OFFLINE_CACHE_PDF_MAX_BYTES {
        return Err("offline-cache PDF is too large".to_owned());
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err("offline-cache PDF is not a PDF".to_owned());
    }
    Ok(())
}

fn validate_offline_cache_archive_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("offline-cache archive byte count mismatch".to_owned());
    }
    if bytes.len() > OFFLINE_CACHE_MHTML_MAX_BYTES {
        return Err("offline-cache archive is too large".to_owned());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "offline-cache archive is not UTF-8 MHTML".to_owned())?;
    if !text.starts_with("MIME-Version: 1.0\r\n") || !text.contains("multipart/related") {
        return Err("offline-cache archive is not MHTML".to_owned());
    }
    Ok(())
}

fn valid_offline_archive_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".mhtml")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_offline_pdf_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".pdf")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_offline_resource_type(resource: &str) -> bool {
    matches!(
        resource,
        "document"
            | "subdocument"
            | "stylesheet"
            | "script"
            | "image"
            | "font"
            | "media"
            | "object"
            | "xhr"
            | "ping"
            | "websocket"
            | "other"
    )
}

fn parse_read_aloud_status(body: &str) -> Result<BrowserReadAloudStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("read-aloud status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "read-aloud status")?;
    if !matches!(
        state.as_str(),
        "idle" | "speaking" | "spoken" | "unavailable" | "error"
    ) {
        return Err("read-aloud status has an unsupported state".to_owned());
    }
    Ok(BrowserReadAloudStatus {
        node: status_required_str(&v, "node", "read-aloud status")?,
        last_title: optional_trimmed_str(&v, "last_title"),
        last_url: optional_trimmed_str(&v, "last_url"),
        state,
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        spoken: v
            .get("spoken")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_request_ms: v.get("last_request_ms").and_then(serde_json::Value::as_u64),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn parse_voice_command_status(body: &str) -> Result<BrowserVoiceCommandStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("voice status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "voice status")?;
    if !matches!(
        state.as_str(),
        "idle" | "listening" | "transcribed" | "unavailable" | "error"
    ) {
        return Err("voice status has an unsupported state".to_owned());
    }
    if let Some(mode) = optional_trimmed_str(&v, "last_mode") {
        if !matches!(mode.as_str(), "command" | "dictation") {
            return Err("voice status has an unsupported mode".to_owned());
        }
    }
    Ok(BrowserVoiceCommandStatus {
        node: status_required_str(&v, "node", "voice status")?,
        last_url: optional_trimmed_str(&v, "last_url"),
        last_mode: optional_trimmed_str(&v, "last_mode"),
        state,
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        transcribed: v
            .get("transcribed")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_transcript_chars: v
            .get("last_transcript_chars")
            .and_then(serde_json::Value::as_u64),
        last_request_ms: v.get("last_request_ms").and_then(serde_json::Value::as_u64),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn parse_passkey_status(body: &str) -> Result<BrowserPasskeyStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("passkey status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "passkey status")?;
    if !matches!(
        state.as_str(),
        "idle" | "pending" | "created" | "asserted" | "error"
    ) {
        return Err("passkey status has an unsupported state".to_owned());
    }
    if let Some(ceremony) = optional_trimmed_str(&v, "last_ceremony") {
        if !matches!(ceremony.as_str(), "create" | "get") {
            return Err("passkey status has an unsupported ceremony".to_owned());
        }
    }
    let hardware_state =
        optional_trimmed_str(&v, "hardware_state").unwrap_or_else(|| "unknown".to_owned());
    if !matches!(
        hardware_state.as_str(),
        "unknown" | "unavailable" | "present_permission_denied" | "ready"
    ) {
        return Err("passkey status has an unsupported hardware state".to_owned());
    }
    let hardware_ctaphid_state =
        optional_trimmed_str(&v, "hardware_ctaphid_state").unwrap_or_else(|| "unknown".to_owned());
    if !matches!(
        hardware_ctaphid_state.as_str(),
        "unknown" | "unavailable" | "init_request_ready"
    ) {
        return Err("passkey status has an unsupported CTAP HID state".to_owned());
    }
    let hardware_ctaphid_init_frame_count = v
        .get("hardware_ctaphid_init_frame_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if hardware_ctaphid_state == "init_request_ready" && hardware_ctaphid_init_frame_count == 0 {
        return Err("passkey status CTAP HID INIT diagnostic has no frames".to_owned());
    }
    if hardware_ctaphid_state != "init_request_ready" && hardware_ctaphid_init_frame_count > 0 {
        return Err("passkey status CTAP HID frame count contradicts the CTAP state".to_owned());
    }
    Ok(BrowserPasskeyStatus {
        node: status_required_str(&v, "node", "passkey status")?,
        last_request_id: optional_trimmed_str(&v, "last_request_id"),
        last_host: optional_trimmed_str(&v, "last_host"),
        last_ceremony: optional_trimmed_str(&v, "last_ceremony"),
        last_rp_id: optional_trimmed_str(&v, "last_rp_id"),
        state,
        mirrored: v
            .get("mirrored")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_default(),
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_pending_ms: v.get("last_pending_ms").and_then(serde_json::Value::as_u64),
        hardware_state,
        hardware_key_count: v
            .get("hardware_key_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        hardware_readable_count: v
            .get("hardware_readable_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        hardware_ctaphid_state,
        hardware_ctaphid_init_frame_count,
        hardware_probe_ms: v
            .get("hardware_probe_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn parse_passkey_completion(body: &str) -> Result<BrowserPasskeyCompletion, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("passkey event JSON: {err}"))?;
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_passkeys") {
        return Err("passkey event has an unsupported source".to_owned());
    }
    let op = status_required_str(&v, "op", "passkey event")?;
    if !matches!(
        op.as_str(),
        "browser_passkey_created" | "browser_passkey_assertion"
    ) {
        return Err("passkey event is not a completion".to_owned());
    }
    let client_request_id = status_required_str(&v, "client_request_id", "passkey event")?;
    if client_request_id.len() > 128 {
        return Err("passkey event client_request_id is too long".to_owned());
    }
    let body = serde_json::to_string(&v).map_err(|err| format!("passkey event encode: {err}"))?;
    Ok(BrowserPasskeyCompletion {
        client_request_id,
        body,
    })
}

fn parse_security_update_status(body: &str) -> Result<BrowserSecurityUpdateStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("security update JSON: {err}"))?;
    let state = security_status_required_str(&v, "state")?;
    if !matches!(
        state.as_str(),
        "current" | "missing" | "mismatch" | "manifest_missing"
    ) {
        return Err("security update has an unsupported state".to_owned());
    }
    let updater_state = v
        .get("updater_state")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("idle")
        .to_owned();
    Ok(BrowserSecurityUpdateStatus {
        node: security_status_required_str(&v, "node")?,
        state,
        expected_cef_version: optional_trimmed_str(&v, "expected_cef_version"),
        expected_chromium_version: optional_trimmed_str(&v, "expected_chromium_version"),
        expected_channel: optional_trimmed_str(&v, "expected_channel"),
        active_runtime: optional_trimmed_str(&v, "active_runtime"),
        installed_version: optional_trimmed_str(&v, "installed_version"),
        installed_chromium: optional_trimmed_str(&v, "installed_chromium"),
        libcef_present: v
            .get("libcef_present")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        updater_state,
        last_update_ms: v.get("last_update_ms").and_then(serde_json::Value::as_u64),
        last_update_exit_code: v
            .get("last_update_exit_code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
        last_update_error: optional_trimmed_str(&v, "last_update_error"),
        last_error: optional_trimmed_str(&v, "last_error"),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn security_status_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("security update is missing {key}"))
}

fn optional_trimmed_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn status_required_str(v: &serde_json::Value, key: &str, context: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{context} is missing {key}"))
}

fn result_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("translation result is missing {key}"))
}

fn cache_result_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("offline-cache result is missing {key}"))
}

fn parse_voice_transcript_result(body: &str) -> Result<VoiceTranscriptResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("voice result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_voice_transcript") {
        return Err("voice result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_voice_command") {
        return Err("voice result has the wrong source".to_owned());
    }
    let host = v
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "voice result is missing host".to_owned())?;
    let mode = v
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .and_then(VoiceCommandMode::from_wire)
        .ok_or_else(|| "voice result has an unsupported mode".to_owned())?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "voice result is missing tab_index".to_owned())?;
    let focus = v
        .get("focus")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|focus| matches!(*focus, "page" | "chrome"))
        .ok_or_else(|| "voice result has an unsupported focus".to_owned())?;
    let transcript = v
        .get("transcript")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|transcript| !transcript.is_empty())
        .ok_or_else(|| "voice result is missing transcript".to_owned())?;
    Ok(VoiceTranscriptResult {
        host: host.to_owned(),
        mode,
        tab_index,
        focus: focus.to_owned(),
        transcript: clamp_chars(transcript, 4096),
    })
}

fn voice_command_action(transcript: &str) -> Option<BrowserVoiceAction> {
    let command = normalize_voice_command(transcript);
    match command.as_str() {
        "new tab" | "open new tab" | "open a new tab" => Some(BrowserVoiceAction::NewTab),
        "close tab" | "close current tab" => Some(BrowserVoiceAction::CloseTab),
        "back" | "go back" => Some(BrowserVoiceAction::Back),
        "forward" | "go forward" => Some(BrowserVoiceAction::Forward),
        "reload" | "refresh" | "reload page" | "refresh page" => Some(BrowserVoiceAction::Reload),
        "read aloud" | "read page aloud" | "read this page aloud" => {
            Some(BrowserVoiceAction::ReadAloud)
        }
        _ => voice_find_query(&command).map(BrowserVoiceAction::Find),
    }
}

fn voice_find_query(command: &str) -> Option<String> {
    for prefix in [
        "find in page ",
        "find on page ",
        "search page for ",
        "search this page for ",
        "search for ",
        "find ",
    ] {
        if let Some(query) = command.strip_prefix(prefix).map(str::trim) {
            if !query.is_empty() {
                return Some(query.to_owned());
            }
        }
    }
    None
}

fn normalize_voice_command(transcript: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = true;
    for ch in transcript.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_owned()
}

fn clamp_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn session_state_wire(state: &SessionState) -> &'static str {
    match state {
        SessionState::Loading => "loading",
        SessionState::Live => "live",
        SessionState::Crashed { .. } => "crashed",
    }
}

fn browser_session_sync_body(state: &WebState) -> String {
    let tabs = state
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let nav = tab.session.nav();
            serde_json::json!({
                "index": index,
                "engine": tab.engine.wire(),
                "container": tab.container.wire(),
                "display_target": tab.display_target.wire(),
                "url": nav.url.as_str(),
                "title": tab.session.title(),
                "state": session_state_wire(&tab.session.state()),
                "loading": nav.loading,
                "can_back": nav.can_back,
                "can_forward": nav.can_forward,
                "muted": tab.muted,
                "force_dark": tab.force_dark,
                "reader_mode": tab.reader_mode,
                "user_scripts": tab.user_scripts,
                "user_agent": tab.user_agent.wire(),
                "device_profile": tab.device_profile.wire(),
                "idle_suspended": tab.idle_suspended,
            })
        })
        .collect::<Vec<_>>();
    let downloads = state
        .download_jobs
        .iter()
        .map(|job| {
            serde_json::json!({
                "id": job.id.as_str(),
                "source": job.source.as_str(),
                "dest": job.dest.as_str(),
                "method": job.method,
                "state": job.state,
                "progress": job.progress,
                "updated_ms": job.updated_ms,
            })
        })
        .collect::<Vec<_>>();
    let speed_dial = state
        .speed_dial
        .iter()
        .map(|entry| {
            serde_json::json!({
                "label": entry.label.as_str(),
                "url": entry.url.as_str(),
                "hint": entry.hint.as_str(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "op": "browser_session_sync",
        "source": "browser",
        "host": local_hostname(),
        "active_index": if state.tabs.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!(state.active.min(state.tabs.len().saturating_sub(1)))
        },
        "settings": {
            "future_engine": state.engine.wire(),
            "vertical_tabs": state.vertical_tabs,
            "page_zoom_percent": state.page_zoom_percent,
            "find_open": state.find_open,
            "downloads_open": state.downloads_open,
            "power_mode": state.power_mode,
            "speed_dial": speed_dial,
        },
        "tabs": tabs,
        "downloads": downloads,
    })
    .to_string()
}

fn browser_tab_suspend_body(
    tab_index: usize,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    idle_after: Duration,
) -> String {
    let idle_after_ms = u64::try_from(idle_after.as_millis()).unwrap_or(u64::MAX);
    serde_json::json!({
        "op": "browser_tab_suspend",
        "tab_index": tab_index,
        "engine": engine.wire(),
        "url": url,
        "title": title,
        "idle_after_ms": idle_after_ms,
        "source": "browser",
        "host": local_hostname(),
    })
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalProtocol {
    Mailto,
    Tel,
    Magnet,
}

impl ExternalProtocol {
    fn from_url(url: &str) -> Option<Self> {
        let (scheme, _) = url.split_once(':')?;
        match scheme.to_ascii_lowercase().as_str() {
            "mailto" => Some(Self::Mailto),
            "tel" => Some(Self::Tel),
            "magnet" => Some(Self::Magnet),
            _ => None,
        }
    }

    const fn scheme(self) -> &'static str {
        match self {
            Self::Mailto => "mailto",
            Self::Tel => "tel",
            Self::Magnet => "magnet",
        }
    }

    const fn target(self) -> &'static str {
        match self {
            Self::Mailto => "email",
            Self::Tel => "voice",
            Self::Magnet => "transfers",
        }
    }

    const fn target_label(self) -> &'static str {
        match self {
            Self::Mailto => "Email",
            Self::Tel => "Voice",
            Self::Magnet => "Transfers",
        }
    }
}

fn browser_protocol_handoff_body(protocol: ExternalProtocol, url: &str) -> String {
    serde_json::json!({
        "op": "browser_protocol_handoff",
        "source": "browser",
        "host": local_hostname(),
        "scheme": protocol.scheme(),
        "target": protocol.target(),
        "url": url,
    })
    .to_string()
}

fn voice_dial_body(url: &str) -> String {
    let number = url.split_once(':').map_or(url, |(_, rest)| rest).trim();
    serde_json::json!({
        "peer": number,
        "host": local_hostname(),
        "source": "browser",
        "url": url,
    })
    .to_string()
}

fn browser_notify_body(severity: Severity, summary: &str, detail: Option<&str>) -> String {
    let mut body = serde_json::json!({
        "severity": severity.tag(),
        "host": local_hostname(),
        "source": "browser",
        "summary": summary,
        "action": "action/shell/goto/browser",
    });
    if let Some(detail) = detail.filter(|s| !s.trim().is_empty()) {
        body["detail"] = serde_json::Value::String(detail.to_owned());
    }
    body.to_string()
}

/// Resolve an address-bar draft into the URL sent to the helper.
///
/// BROWSER-DD-2 asks for a real omnibox, not a strict URL field: explicit schemes
/// pass through, likely hostnames become HTTPS URLs, and free text searches the
/// mesh-hosted SearXNG instance. Empty/whitespace drafts stay inert.
fn omnibox_target(draft: &str) -> Option<String> {
    let draft = draft.trim();
    if draft.is_empty() {
        return None;
    }
    if has_url_scheme(draft) {
        return Some(draft.to_owned());
    }
    if looks_like_host(draft) {
        return Some(format!("https://{draft}"));
    }
    Some(format!(
        "{DEFAULT_SEARCH_URL}?q={}",
        percent_encode_query(draft)
    ))
}

fn should_fetch_suggestions(draft: &str) -> bool {
    let draft = draft.trim();
    !draft.is_empty() && !has_url_scheme(draft) && !looks_like_host(draft)
}

fn suggestions_url(query: &str) -> String {
    format!(
        "{DEFAULT_SUGGEST_URL}?q={}",
        percent_encode_query(query.trim())
    )
}

fn fetch_suggestions(query: &str) -> Result<Vec<String>, String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(SUGGEST_TIMEOUT)
        .build()
        .map_err(|e| format!("Suggestions unavailable: {e}"))?
        .get(suggestions_url(query))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| format!("Suggestions unavailable: {e}"))?
        .text()
        .map_err(|e| format!("Suggestions unavailable: {e}"))?;
    parse_suggestions_json(query, &body)
}

fn chromium_devtools_frontend_for_active_url(active_url: &str) -> Result<Option<String>, String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(CEF_DEVTOOLS_TIMEOUT)
        .build()
        .map_err(|e| format!("target discovery unavailable: {e}"))?
        .get(CEF_DEVTOOLS_LIST_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| format!("target discovery unavailable: {e}"))?
        .text()
        .map_err(|e| format!("target discovery unavailable: {e}"))?;
    chromium_devtools_frontend_from_list(active_url, &body)
}

fn chromium_devtools_frontend_from_list(
    active_url: &str,
    body: &str,
) -> Result<Option<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid DevTools target JSON: {e}"))?;
    let Some(targets) = value.as_array() else {
        return Err("DevTools target JSON is not an array".to_owned());
    };
    let active_url = active_url.trim();
    let mut fallback = None;
    for target in targets {
        let target_url = target
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let target_type = target
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("page");
        if target_type != "page" || target_url.starts_with("devtools://") {
            continue;
        }
        let Some(frontend) = chromium_devtools_frontend_url(target) else {
            continue;
        };
        if target_url == active_url {
            return Ok(Some(frontend));
        }
        fallback.get_or_insert(frontend);
    }
    Ok(fallback)
}

fn chromium_devtools_frontend_url(target: &serde_json::Value) -> Option<String> {
    if let Some(frontend) = target
        .get("devtoolsFrontendUrl")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        if frontend.starts_with("http://127.0.0.1:9222/") {
            return Some(frontend.to_owned());
        }
        if frontend.starts_with('/') {
            return Some(format!("http://127.0.0.1:9222{frontend}"));
        }
    }
    let ws = target
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())?;
    let ws = ws
        .strip_prefix("ws://")
        .or_else(|| ws.strip_prefix("wss://"))
        .unwrap_or(ws);
    Some(format!(
        "http://127.0.0.1:9222/devtools/inspector.html?ws={ws}"
    ))
}

fn parse_suggestions_json(query: &str, body: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("Invalid suggestions JSON: {e}"))?;
    let mut out = Vec::new();
    collect_suggestion_values(query.trim(), &value, &mut out);
    out.truncate(8);
    Ok(out)
}

fn collect_suggestion_values(query: &str, value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => push_suggestion(query, s, out),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_suggestion_values(query, item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for key in [
                "suggestions",
                "results",
                "completions",
                "value",
                "phrase",
                "text",
            ] {
                if let Some(v) = map.get(key) {
                    collect_suggestion_values(query, v, out);
                }
            }
        }
        _ => {}
    }
}

fn push_suggestion(query: &str, value: &str, out: &mut Vec<String>) {
    let value = value.trim();
    if value.is_empty() || value == query || out.iter().any(|s| s == value) {
        return;
    }
    out.push(value.to_owned());
}

fn has_url_scheme(s: &str) -> bool {
    if let Some((scheme, _rest)) = s.split_once("://") {
        return valid_scheme(scheme);
    }
    let Some((scheme, _rest)) = s.split_once(':') else {
        return false;
    };
    matches!(
        scheme,
        "about" | "data" | "file" | "mailto" | "tel" | "magnet" | "view-source"
    )
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn looks_like_host(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    let host = s.split('/').next().unwrap_or(s);
    host == "localhost"
        || host.contains('.')
        || host.contains(':')
        || host.chars().all(|c| c.is_ascii_digit() || c == '.')
}

fn percent_encode_query(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn is_plain_http(url: &str) -> bool {
    url.trim_start().starts_with("http://")
}

fn is_new_tab_url(url: &str) -> bool {
    matches!(url.trim(), "" | "about:blank")
}

fn https_upgrade(url: &str) -> String {
    let trimmed = url.trim();
    trimmed
        .strip_prefix("http://")
        .map_or_else(|| trimmed.to_owned(), |rest| format!("https://{rest}"))
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

fn browser_capture_dir() -> PathBuf {
    std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Magic Mesh Browser Captures")
}

fn browser_pdf_dir() -> PathBuf {
    std::env::var_os("XDG_DOCUMENTS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Documents")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("Magic Mesh Browser PDFs")
}

fn browser_print_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-cups")
}

fn browser_scrape_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-scrapes")
}

fn browser_media_spool_dir() -> PathBuf {
    std::env::temp_dir().join("mde-browser-media")
}

fn capture_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "png", url, title, unix_ms)
}

fn capture_full_page_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-full-page", "png", url, title, unix_ms)
}

fn capture_mhtml_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "mhtml", url, title, unix_ms)
}

fn capture_annotated_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-annotated", "png", url, title, unix_ms)
}

fn capture_callout_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-callout", "png", url, title, unix_ms)
}

fn capture_freehand_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-freehand", "png", url, title, unix_ms)
}

fn scrape_export_filename_for(url: &str, title: &str, unix_ms: u64, ext: &str) -> String {
    output_filename_for("mde-browser-scrape", ext, url, title, unix_ms)
}

fn media_manifest_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-media-manifest", "json", url, title, unix_ms)
}

fn media_asset_request_filename_for(
    page_url: &str,
    title: &str,
    asset_url: &str,
    unix_ms: u64,
    index: usize,
) -> String {
    let base = output_filename_for(
        "mde-browser-media-download",
        "json",
        page_url,
        title,
        unix_ms,
    );
    let stem = base.strip_suffix(".json").unwrap_or(&base);
    let hint = sanitize_filename_component(&media_filename_hint(asset_url), 48);
    format!("{stem}-{index:03}-{hint}.download.json")
}

fn capture_region_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-region", "png", url, title, unix_ms)
}

fn pdf_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser", "pdf", url, title, unix_ms)
}

fn print_pdf_filename_for(url: &str, title: &str, unix_ms: u64) -> String {
    output_filename_for("mde-browser-print", "pdf", url, title, unix_ms)
}

fn pdf_file_looks_readable(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    use std::io::Read;
    file.read_exact(&mut magic).is_ok() && magic == *b"%PDF"
}

fn file_url_for_path(path: &Path) -> Result<String, String> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|err| format!("could not resolve current directory: {err}"))?
            .join(path)
    };
    let text = path.to_string_lossy();
    let mut out = String::from("file://");
    for byte in text.as_bytes() {
        match *byte {
            b'/' => out.push('/'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte));
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    Ok(out)
}

fn output_filename_for(prefix: &str, ext: &str, url: &str, title: &str, unix_ms: u64) -> String {
    let seed = host_of(url)
        .or_else(|| {
            let title = title.trim();
            (!title.is_empty()).then(|| title.to_owned())
        })
        .unwrap_or_else(|| "page".to_owned());
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in seed.chars() {
        let out = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            Some(ch.to_ascii_lowercase())
        } else if !last_dash {
            last_dash = true;
            Some('-')
        } else {
            None
        };
        if let Some(ch) = out {
            slug.push(ch);
        }
        if slug.len() >= 48 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "page" } else { slug };
    format!("{prefix}-{unix_ms}-{slug}.{ext}")
}

fn active_page_scrape_documents(
    url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
    page_text: Option<&str>,
    page_scrape_body: Option<&str>,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    let label = if title.trim().is_empty() {
        host_of(url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let crawl_seed = active_page_scrape_crawl_seed(url, recent);
    let dom_extract = scrape_dom_extract(url, page_scrape_body)?;
    let crawl_manifest = active_page_scrape_crawl_manifest(url, &crawl_seed, &dom_extract.links);
    let text_extract = if let Some(text) = dom_extract.text.as_deref() {
        scrape_text_extract_with_truncated(text, dom_extract.text_truncated)
    } else {
        scrape_text_extract(page_text)
    };
    let mut json_value = serde_json::json!({
        "op": "browser_active_page_scrape",
        "scope": "active_page_metadata_with_crawl_seed_text_and_dom",
        "url": url,
        "title": label,
        "engine": engine.wire(),
        "captured_ms": unix_ms,
        "formats": ["json", "csv", "md"],
        "crawl_seed_count": crawl_seed.len(),
        "crawl_manifest_status": if crawl_manifest.is_empty() { "empty" } else { "ready" },
        "crawl_execution_status": "not_started",
        "crawl_manifest_max_depth": 1,
        "crawl_manifest_count": crawl_manifest.len(),
        "crawl_seed": crawl_seed
            .iter()
            .map(|seed| {
                serde_json::json!({
                    "url": seed.url,
                    "resource": seed.resource,
                    "allowed": seed.allowed,
                    "same_origin": true,
                })
            })
            .collect::<Vec<_>>(),
        "crawl_manifest": crawl_manifest
            .iter()
            .map(|target| {
                serde_json::json!({
                    "url": target.url,
                    "source": target.source,
                    "resource": target.resource,
                    "allowed": target.allowed,
                    "same_origin": true,
                    "depth": target.depth,
                })
            })
            .collect::<Vec<_>>(),
        "extracted_text_status": text_extract.status,
        "extracted_text_chars": text_extract.original_chars,
        "extracted_text_truncated": text_extract.truncated,
        "dom_extract_status": dom_extract.status,
        "article_extract_status": dom_extract.article_status,
        "article_text_chars": dom_extract.article_text_chars,
        "article_text_truncated": dom_extract.article_text_truncated,
        "article_selector": dom_extract.article_selector,
        "canonical_url": dom_extract.canonical_url,
        "meta_description": dom_extract.meta_description,
        "document_lang": dom_extract.document_lang,
        "dom_link_count": dom_extract.links.len(),
        "dom_heading_count": dom_extract.headings.len(),
        "dom_links": dom_extract.links
            .iter()
            .map(|link| {
                serde_json::json!({
                    "url": link.url,
                    "text": link.text,
                    "rel": link.rel,
                    "target": link.target,
                    "same_origin": link.same_origin,
                })
            })
            .collect::<Vec<_>>(),
        "dom_headings": dom_extract.headings
            .iter()
            .map(|heading| {
                serde_json::json!({
                    "level": heading.level,
                    "text": heading.text,
                })
            })
            .collect::<Vec<_>>(),
    });
    if let Some(text) = &text_extract.text {
        json_value["extracted_text"] = serde_json::Value::String(text.clone());
    }
    if let Some(text) = &dom_extract.article_text {
        json_value["article_text"] = serde_json::Value::String(text.clone());
    }
    let json = serde_json::to_vec_pretty(&json_value)
        .map_err(|err| format!("encode scrape JSON: {err}"))?;
    let mut csv = format!(
        "captured_ms,engine,title,url,scope,seed_url,seed_resource,seed_allowed,text_status,text_chars,text_truncated,text,dom_kind,dom_url,dom_text,dom_level,dom_same_origin,dom_rel,dom_target\n{},{},{},{},active_page_metadata_with_crawl_seed_text_and_dom,,,,{},{},{},{},,,,,,,\n",
        unix_ms,
        csv_cell(engine.wire()),
        csv_cell(&label),
        csv_cell(url),
        csv_cell(text_extract.status),
        text_extract.original_chars,
        text_extract.truncated,
        csv_cell(text_extract.text.as_deref().unwrap_or(""))
    );
    for seed in &crawl_seed {
        csv.push_str(&format!(
            "{},{},{},{},crawl_seed,{},{},{},,,,,,,,,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&seed.url),
            csv_cell(seed.resource),
            seed.allowed
        ));
    }
    for target in &crawl_manifest {
        csv.push_str(&format!(
            "{},{},{},{},crawl_manifest,{},{},{},,,,crawl_target,{},{},{},true,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&target.url),
            csv_cell(target.source),
            target.allowed,
            csv_cell(&target.url),
            csv_cell(target.resource),
            target.depth
        ));
    }
    for link in &dom_extract.links {
        csv.push_str(&format!(
            "{},{},{},{},dom_link,,,,,,,,link,{},{},,{},{},{}\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&link.url),
            csv_cell(&link.text),
            link.same_origin,
            csv_cell(&link.rel),
            csv_cell(&link.target)
        ));
    }
    for heading in &dom_extract.headings {
        csv.push_str(&format!(
            "{},{},{},{},dom_heading,,,,,,,,heading,,{},{},,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&heading.text),
            heading.level
        ));
    }
    if let Some(article_text) = &dom_extract.article_text {
        csv.push_str(&format!(
            "{},{},{},{},dom_article,,,,,,,,article,,{},,{},{},{}\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(article_text),
            false,
            csv_cell(&dom_extract.article_selector),
            csv_cell(dom_extract.article_status)
        ));
    }
    if !dom_extract.canonical_url.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_canonical,,,,,,,,canonical,{},canonical,,{},,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.canonical_url),
            scrape_url_same_origin(url, &dom_extract.canonical_url)
        ));
    }
    if !dom_extract.meta_description.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_meta_description,,,,,,,,meta_description,,{},,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.meta_description)
        ));
    }
    if !dom_extract.document_lang.is_empty() {
        csv.push_str(&format!(
            "{},{},{},{},dom_document_lang,,,,,,,,document_lang,,{},,,,\n",
            unix_ms,
            csv_cell(engine.wire()),
            csv_cell(&label),
            csv_cell(url),
            csv_cell(&dom_extract.document_lang)
        ));
    }
    let csv = csv.into_bytes();
    let seed_md = if crawl_seed.is_empty() {
        "No same-origin crawl seed URLs were observed in helper resource telemetry.".to_owned()
    } else {
        crawl_seed
            .iter()
            .map(|seed| {
                format!(
                    "- `{}` ({}, allowed={})",
                    seed.url.replace('`', "\\`"),
                    seed.resource,
                    seed.allowed
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text_md = match &text_extract.text {
        Some(text) if !text.is_empty() => {
            format!("```text\n{}\n```", text.replace("```", "`\\`\\`"))
        }
        Some(_) => "No visible page text was returned by the helper.".to_owned(),
        None => "Visible page text was not requested for this export path.".to_owned(),
    };
    let crawl_manifest_md = if crawl_manifest.is_empty() {
        "No same-origin crawl targets were available for the handoff manifest.".to_owned()
    } else {
        crawl_manifest
            .iter()
            .map(|target| {
                format!(
                    "- `{}` (source={}, resource={}, depth={}, allowed={})",
                    target.url.replace('`', "\\`"),
                    target.source,
                    target.resource,
                    target.depth,
                    target.allowed
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let links_md = if dom_extract.links.is_empty() {
        match dom_extract.status {
            "not_requested" => "DOM links were not requested for this export path.".to_owned(),
            _ => "No DOM links were returned by the helper.".to_owned(),
        }
    } else {
        dom_extract
            .links
            .iter()
            .map(|link| {
                format!(
                    "- [{}]({}) (same_origin={}, rel=`{}`, target=`{}`)",
                    markdown_inline_text(&link.text),
                    link.url,
                    link.same_origin,
                    link.rel.replace('`', "\\`"),
                    link.target.replace('`', "\\`")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let headings_md = if dom_extract.headings.is_empty() {
        match dom_extract.status {
            "not_requested" => "DOM headings were not requested for this export path.".to_owned(),
            _ => "No DOM headings were returned by the helper.".to_owned(),
        }
    } else {
        dom_extract
            .headings
            .iter()
            .map(|heading| {
                format!(
                    "- h{} {}",
                    heading.level,
                    markdown_inline_text(&heading.text)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let article_md = match &dom_extract.article_text {
        Some(text) if !text.is_empty() => {
            let mut lines = vec![format!(
                "- Status: `{}`, selector `{}`, chars `{}`, truncated `{}`",
                dom_extract.article_status,
                dom_extract.article_selector.replace('`', "\\`"),
                dom_extract.article_text_chars,
                dom_extract.article_text_truncated
            )];
            if !dom_extract.canonical_url.is_empty() {
                lines.push(format!(
                    "- Canonical: `{}`",
                    dom_extract.canonical_url.replace('`', "\\`")
                ));
            }
            if !dom_extract.meta_description.is_empty() {
                lines.push(format!(
                    "- Description: {}",
                    markdown_inline_text(&dom_extract.meta_description)
                ));
            }
            if !dom_extract.document_lang.is_empty() {
                lines.push(format!(
                    "- Language: `{}`",
                    dom_extract.document_lang.replace('`', "\\`")
                ));
            }
            lines.push(String::new());
            lines.push("```text".to_owned());
            lines.push(text.replace("```", "`\\`\\`"));
            lines.push("```".to_owned());
            lines.join("\n")
        }
        Some(_) => "No article/main-body text was returned by the helper.".to_owned(),
        None => match dom_extract.status {
            "not_requested" => {
                "Article/main-body extraction was not requested for this export path.".to_owned()
            }
            _ => "No article/main-body text was returned by the helper.".to_owned(),
        },
    };
    let md = format!(
        "# {}\n\n- URL: `{}`\n- Engine: `{}`\n- Captured: `{}`\n- Scope: active page metadata with bounded crawl seed, extracted text, DOM links/headings/article metadata, and crawl manifest handoff\n- Crawl seed URLs: `{}`\n- Crawl manifest URLs: `{}` depth-1 handoff targets, execution `not_started`\n- Extracted text: `{}` chars, status `{}`, truncated `{}`\n- DOM extract: status `{}`, links `{}`, headings `{}`\n- Article extract: status `{}`, chars `{}`, truncated `{}`\n\n## Extracted Text\n\n{}\n\n## Article Extract\n\n{}\n\n## DOM Links\n\n{}\n\n## DOM Headings\n\n{}\n\n## Crawl Manifest\n\n{}\n\n## Crawl Seed\n\n{}\n\nRecursive network fetching remains a follow-up scraper hook.\n",
        markdown_heading_text(&label),
        url.replace('`', "\\`"),
        engine.label(),
        unix_ms,
        crawl_seed.len(),
        crawl_manifest.len(),
        text_extract.original_chars,
        text_extract.status,
        text_extract.truncated,
        dom_extract.status,
        dom_extract.links.len(),
        dom_extract.headings.len(),
        dom_extract.article_status,
        dom_extract.article_text_chars,
        dom_extract.article_text_truncated,
        text_md,
        article_md,
        links_md,
        headings_md,
        crawl_manifest_md,
        seed_md
    )
    .into_bytes();
    Ok(vec![("json", json), ("csv", csv), ("md", md)])
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeCrawlSeed {
    url: String,
    resource: &'static str,
    allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeCrawlTarget {
    url: String,
    source: &'static str,
    resource: &'static str,
    allowed: bool,
    depth: u8,
}

fn active_page_scrape_crawl_seed(
    page_url: &str,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<ScrapeCrawlSeed> {
    let Ok(page) = reqwest::Url::parse(page_url) else {
        return Vec::new();
    };
    let Some(origin_host) = page.host_str().map(str::to_ascii_lowercase) else {
        return Vec::new();
    };
    let origin_scheme = page.scheme().to_ascii_lowercase();
    let origin_port = page.port_or_known_default();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for resource in recent.iter().rev() {
        if out.len() >= SCRAPE_CRAWL_SEED_MAX_COUNT {
            break;
        }
        let url = resource.url.trim();
        if url.is_empty() {
            continue;
        }
        let Ok(parsed) = reqwest::Url::parse(url) else {
            continue;
        };
        if parsed.scheme().to_ascii_lowercase() != origin_scheme
            || parsed.host_str().map(str::to_ascii_lowercase) != Some(origin_host.clone())
            || parsed.port_or_known_default() != origin_port
        {
            continue;
        }
        let normalized = parsed.to_string();
        if !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(ScrapeCrawlSeed {
            url: clamp_chars(&normalized, MEDIA_SNIFFER_URL_MAX_CHARS),
            resource: offline_cache_resource_type_name(resource.resource),
            allowed: resource.allowed,
        });
    }
    out.reverse();
    out
}

fn active_page_scrape_crawl_manifest(
    page_url: &str,
    crawl_seed: &[ScrapeCrawlSeed],
    dom_links: &[ScrapeDomLink],
) -> Vec<ScrapeCrawlTarget> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for seed in crawl_seed {
        if out.len() >= SCRAPE_CRAWL_MANIFEST_MAX_COUNT {
            break;
        }
        if !seen.insert(seed.url.clone()) {
            continue;
        }
        out.push(ScrapeCrawlTarget {
            url: seed.url.clone(),
            source: "telemetry",
            resource: seed.resource,
            allowed: seed.allowed,
            depth: 1,
        });
    }
    for link in dom_links {
        if out.len() >= SCRAPE_CRAWL_MANIFEST_MAX_COUNT {
            break;
        }
        if !link.same_origin || !scrape_url_same_origin(page_url, &link.url) {
            continue;
        }
        if !seen.insert(link.url.clone()) {
            continue;
        }
        out.push(ScrapeCrawlTarget {
            url: link.url.clone(),
            source: "dom_link",
            resource: "document",
            allowed: true,
            depth: 1,
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeTextExtract {
    status: &'static str,
    text: Option<String>,
    original_chars: usize,
    truncated: bool,
}

fn scrape_text_extract(page_text: Option<&str>) -> ScrapeTextExtract {
    if let Some(text) = page_text {
        scrape_text_extract_with_truncated(text, false)
    } else {
        ScrapeTextExtract {
            status: "not_requested",
            text: None,
            original_chars: 0,
            truncated: false,
        }
    }
}

fn scrape_text_extract_with_truncated(text: &str, helper_truncated: bool) -> ScrapeTextExtract {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, SCRAPE_EXTRACT_TEXT_MAX_CHARS);
    ScrapeTextExtract {
        status: if text.is_empty() {
            "no_text"
        } else {
            "captured"
        },
        text: Some(text),
        original_chars,
        truncated: helper_truncated || original_chars > SCRAPE_EXTRACT_TEXT_MAX_CHARS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomExtract {
    status: &'static str,
    text: Option<String>,
    text_truncated: bool,
    article_status: &'static str,
    article_text: Option<String>,
    article_text_chars: usize,
    article_text_truncated: bool,
    article_selector: String,
    canonical_url: String,
    meta_description: String,
    document_lang: String,
    links: Vec<ScrapeDomLink>,
    headings: Vec<ScrapeDomHeading>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomLink {
    url: String,
    text: String,
    rel: String,
    target: String,
    same_origin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScrapeDomHeading {
    level: u8,
    text: String,
}

fn scrape_dom_extract(page_url: &str, body: Option<&str>) -> Result<ScrapeDomExtract, String> {
    let Some(body) = body else {
        return Ok(ScrapeDomExtract {
            status: "not_requested",
            text: None,
            text_truncated: false,
            article_status: "not_requested",
            article_text: None,
            article_text_chars: 0,
            article_text_truncated: false,
            article_selector: String::new(),
            canonical_url: String::new(),
            meta_description: String::new(),
            document_lang: String::new(),
            links: Vec::new(),
            headings: Vec::new(),
        });
    };
    if body.trim().is_empty() {
        return Ok(ScrapeDomExtract {
            status: "empty",
            text: Some(String::new()),
            text_truncated: false,
            article_status: "empty",
            article_text: Some(String::new()),
            article_text_chars: 0,
            article_text_truncated: false,
            article_selector: String::new(),
            canonical_url: String::new(),
            meta_description: String::new(),
            document_lang: String::new(),
            links: Vec::new(),
            headings: Vec::new(),
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("decode scrape DOM JSON: {err}"))?;
    let text = value
        .get("text")
        .and_then(serde_json::Value::as_str)
        .map(|text| clamp_chars(text.trim(), SCRAPE_EXTRACT_TEXT_MAX_CHARS))
        .unwrap_or_default();
    let text_truncated = value
        .get("text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let article_text = value
        .get("article_text")
        .and_then(serde_json::Value::as_str)
        .map(|text| clamp_chars(text.trim(), SCRAPE_ARTICLE_TEXT_MAX_CHARS));
    let article_text_chars = article_text
        .as_deref()
        .map(|text| text.chars().count())
        .unwrap_or(0);
    let article_text_truncated = value
        .get("article_text_truncated")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let article_status = match article_text.as_deref() {
        Some(text) if !text.is_empty() => "captured",
        Some(_) => "no_article",
        None => "not_returned",
    };
    let article_selector = clamp_chars(
        value
            .get("article_selector")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        80,
    );
    let canonical_url = clamp_chars(
        value
            .get("canonical_url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        MEDIA_SNIFFER_URL_MAX_CHARS,
    );
    let meta_description = clamp_chars(
        value
            .get("meta_description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        512,
    );
    let document_lang = clamp_chars(
        value
            .get("document_lang")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim(),
        64,
    );
    let mut links = Vec::new();
    if let Some(items) = value.get("links").and_then(serde_json::Value::as_array) {
        for item in items.iter().take(SCRAPE_DOM_LINK_MAX_COUNT) {
            let Some(raw_url) = item.get("url").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let url = clamp_chars(raw_url.trim(), MEDIA_SNIFFER_URL_MAX_CHARS);
            if url.is_empty() {
                continue;
            }
            links.push(ScrapeDomLink {
                same_origin: scrape_url_same_origin(page_url, &url),
                url,
                text: clamp_chars(
                    item.get("text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    SCRAPE_DOM_TEXT_MAX_CHARS,
                ),
                rel: clamp_chars(
                    item.get("rel")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    80,
                ),
                target: clamp_chars(
                    item.get("target")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .trim(),
                    40,
                ),
            });
        }
    }
    let mut headings = Vec::new();
    if let Some(items) = value.get("headings").and_then(serde_json::Value::as_array) {
        for item in items.iter().take(SCRAPE_DOM_HEADING_MAX_COUNT) {
            let text = clamp_chars(
                item.get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim(),
                SCRAPE_DOM_TEXT_MAX_CHARS,
            );
            if text.is_empty() {
                continue;
            }
            let level = item
                .get("level")
                .and_then(serde_json::Value::as_u64)
                .and_then(|level| u8::try_from(level).ok())
                .filter(|level| (1..=6).contains(level))
                .unwrap_or(0);
            headings.push(ScrapeDomHeading { level, text });
        }
    }
    let status = if links.is_empty() && headings.is_empty() {
        "no_dom"
    } else {
        "captured"
    };
    Ok(ScrapeDomExtract {
        status,
        text: Some(text),
        text_truncated,
        article_status,
        article_text,
        article_text_chars,
        article_text_truncated,
        article_selector,
        canonical_url,
        meta_description,
        document_lang,
        links,
        headings,
    })
}

fn scrape_url_same_origin(page_url: &str, candidate_url: &str) -> bool {
    let (Ok(page), Ok(candidate)) = (
        reqwest::Url::parse(page_url),
        reqwest::Url::parse(candidate_url),
    ) else {
        return false;
    };
    page.scheme().eq_ignore_ascii_case(candidate.scheme())
        && page.host_str().map(str::to_ascii_lowercase)
            == candidate.host_str().map(str::to_ascii_lowercase)
        && page.port_or_known_default() == candidate.port_or_known_default()
}

fn csv_cell(text: &str) -> String {
    let escaped = text.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn markdown_heading_text(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\r' | '\n' => ' ',
            _ => ch,
        })
        .collect::<String>()
}

fn markdown_inline_text(text: &str) -> String {
    text.replace('[', "\\[")
        .replace(']', "\\]")
        .replace('`', "\\`")
}

fn active_page_media_manifest(
    url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Result<Vec<u8>, String> {
    let label = if title.trim().is_empty() {
        host_of(url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let items = media_manifest_items(recent);
    serde_json::to_vec_pretty(&serde_json::json!({
        "op": "browser_media_manifest",
        "scope": "active_page_media_sniffer",
        "url": url,
        "title": label,
        "engine": engine.wire(),
        "captured_ms": unix_ms,
        "item_count": items.len(),
        "items": items,
    }))
    .map_err(|err| format!("encode media manifest JSON: {err}"))
}

fn media_manifest_items(
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<serde_json::Value> {
    recent
        .iter()
        .rev()
        .filter_map(|resource| {
            let url = resource.url.trim();
            let kind = media_candidate_kind(resource.resource, url)?;
            Some(serde_json::json!({
                "url": clamp_chars(url, MEDIA_SNIFFER_URL_MAX_CHARS),
                "resource": offline_cache_resource_type_name(resource.resource),
                "kind": kind,
                "allowed": resource.allowed,
                "filename_hint": media_filename_hint(url),
            }))
        })
        .take(MEDIA_SNIFFER_MAX_COUNT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn active_page_media_asset_requests(
    page_url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Result<Vec<Vec<u8>>, String> {
    active_page_media_asset_requests_with_selection(
        page_url,
        title,
        engine,
        unix_ms,
        recent,
        MediaAssetSelection::All,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaAssetSelection {
    All,
    Images,
}

impl MediaAssetSelection {
    fn accepts(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Images => matches!(kind, "image"),
        }
    }

    const fn empty_error(self) -> &'static str {
        match self {
            Self::All => "no observed media/image assets to queue",
            Self::Images => "no observed image assets to queue",
        }
    }
}

fn active_page_media_asset_requests_with_selection(
    page_url: &str,
    title: &str,
    engine: BrowserEngine,
    unix_ms: u64,
    recent: &[mde_web_preview_client::ResourceRequestStatus],
    selection: MediaAssetSelection,
) -> Result<Vec<Vec<u8>>, String> {
    let label = if title.trim().is_empty() {
        host_of(page_url).unwrap_or_else(|| "Untitled page".to_owned())
    } else {
        title.trim().to_owned()
    };
    let mut seen = BTreeSet::new();
    let mut requests = Vec::new();
    for resource in recent.iter().rev() {
        if requests.len() >= MEDIA_SNIFFER_MAX_COUNT {
            break;
        }
        let asset_url = resource.url.trim();
        if asset_url.is_empty() || !seen.insert(asset_url.to_owned()) {
            continue;
        }
        let Some(kind) = media_candidate_kind(resource.resource, asset_url) else {
            continue;
        };
        if !selection.accepts(kind) {
            continue;
        }
        let filename_hint = media_filename_hint(asset_url);
        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "op": "browser_media_download_request",
            "scope": "observed_media_asset",
            "source": "browser_power_mode",
            "page_url": page_url,
            "page_title": label,
            "engine": engine.wire(),
            "captured_ms": unix_ms,
            "asset_url": clamp_chars(asset_url, MEDIA_SNIFFER_URL_MAX_CHARS),
            "resource": offline_cache_resource_type_name(resource.resource),
            "kind": kind,
            "allowed_by_page_filter": resource.allowed,
            "ignore_blocking": !resource.allowed,
            "suggested_filename": filename_hint,
            "rename_strategy": "auto_rename_by_url_hint",
            "retrieval": "native_media_downloader_request",
        }))
        .map_err(|err| format!("encode media download request: {err}"))?;
        requests.push(body);
    }
    requests.reverse();
    Ok(requests)
}

fn media_candidate_kind(resource: u8, url: &str) -> Option<&'static str> {
    let lower = url.to_ascii_lowercase();
    if lower.contains(".m3u8") {
        return Some("hls");
    }
    if lower.contains(".mpd") {
        return Some("dash");
    }
    if media_url_has_any_suffix(&lower, &[".mp4", ".m4v", ".webm", ".mov", ".m4s", ".ts"]) {
        return Some("video");
    }
    if media_url_has_any_suffix(&lower, &[".mp3", ".m4a", ".aac", ".ogg", ".opus", ".flac"]) {
        return Some("audio");
    }
    if media_url_has_any_suffix(
        &lower,
        &[
            ".png", ".jpg", ".jpeg", ".webp", ".gif", ".avif", ".svg", ".bmp",
        ],
    ) {
        return Some("image");
    }
    match mde_web_preview_client::resource_from_wire(resource) {
        mde_web_preview_client::ResourceType::Media => Some("media"),
        mde_web_preview_client::ResourceType::Image => Some("image"),
        _ => None,
    }
}

fn sanitize_filename_component(text: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in text.chars() {
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
        if out.len() >= max_len {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "media".to_owned()
    } else {
        out.to_owned()
    }
}

fn media_url_has_any_suffix(lower_url: &str, suffixes: &[&str]) -> bool {
    let path = lower_url.split(['?', '#']).next().unwrap_or(lower_url);
    suffixes.iter().any(|suffix| path.ends_with(suffix))
}

fn media_filename_hint(url: &str) -> String {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    let leaf = path.rsplit('/').next().unwrap_or("media");
    let decoded = leaf.replace("%20", " ");
    let mut out = String::new();
    let mut last_dash = false;
    for ch in decoded.chars() {
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
        if out.len() >= 96 {
            break;
        }
    }
    let out = out.trim_matches('-');
    if out.is_empty() {
        "media".to_owned()
    } else {
        out.to_owned()
    }
}

fn capture_annotation_caption(url: &str, title: &str, unix_ms: u64) -> String {
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "page".to_owned())
    } else {
        title.to_owned()
    };
    format!("{label} | {url} | {unix_ms}")
}

fn mhtml_capture_document(url: &str, title: &str, unix_ms: u64, png: &[u8]) -> Vec<u8> {
    const BOUNDARY: &str = "----=_MagicMeshBrowserCapture";
    const IMAGE_LOCATION: &str = "mde-browser-capture.png";
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "Browser Capture".to_owned())
    } else {
        title.to_owned()
    };
    let html = format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>{title}</title></head><body>",
            "<h1>{title}</h1>",
            "<p>Captured from <a href=\"{url}\">{url}</a></p>",
            "<p>Capture time: {unix_ms}</p>",
            "<img src=\"{image_location}\" alt=\"Browser capture\">",
            "</body></html>"
        ),
        title = html_escape(&label),
        url = html_escape(url),
        unix_ms = unix_ms,
        image_location = IMAGE_LOCATION
    );
    let encoded_png = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = String::new();
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/related; type=\"text/html\"; boundary=\"{BOUNDARY}\"\r\n"
    ));
    out.push_str(&format!(
        "Subject: Magic Mesh Browser Capture - {}\r\n\r\n",
        mhtml_header_value(&html_escape(&label))
    ));
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str(&format!(
        "Content-Location: {}\r\n\r\n",
        if url.trim().is_empty() {
            "about:blank"
        } else {
            url.trim()
        }
    ));
    out.push_str(&html);
    out.push_str("\r\n");
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: image/png\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n");
    out.push_str(&format!("Content-Location: {IMAGE_LOCATION}\r\n\r\n"));
    for chunk in encoded_png.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    out.into_bytes()
}

fn offline_cache_mhtml_document(
    url: &str,
    title: &str,
    unix_ms: u64,
    text: &str,
    viewport_png: Option<&[u8]>,
) -> Vec<u8> {
    const BOUNDARY: &str = "----=_MagicMeshBrowserOfflineCache";
    const IMAGE_LOCATION: &str = "mde-browser-offline-viewport.png";
    let title = title.trim();
    let label = if title.is_empty() {
        host_of(url).unwrap_or_else(|| "Browser Offline Copy".to_owned())
    } else {
        title.to_owned()
    };
    let image_markup = viewport_png
        .map(|_| "<img src=\"mde-browser-offline-viewport.png\" alt=\"Cached viewport\">")
        .unwrap_or("");
    let html = format!(
        concat!(
            "<!doctype html><html><head><meta charset=\"utf-8\">",
            "<title>{title}</title></head><body>",
            "<h1>{title}</h1>",
            "<p>Offline copy from <a href=\"{url}\">{url}</a></p>",
            "<p>Capture time: {unix_ms}</p>",
            "{image_markup}",
            "<pre>{text}</pre>",
            "</body></html>"
        ),
        title = html_escape(&label),
        url = html_escape(url),
        unix_ms = unix_ms,
        image_markup = image_markup,
        text = html_escape(text)
    );
    let mut out = String::new();
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/related; type=\"text/html\"; boundary=\"{BOUNDARY}\"\r\n"
    ));
    out.push_str(&format!(
        "Subject: Magic Mesh Browser Offline Copy - {}\r\n\r\n",
        mhtml_header_value(&html_escape(&label))
    ));
    out.push_str(&format!("--{BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
    out.push_str("Content-Transfer-Encoding: 8bit\r\n");
    out.push_str(&format!(
        "Content-Location: {}\r\n\r\n",
        if url.trim().is_empty() {
            "about:blank"
        } else {
            url.trim()
        }
    ));
    out.push_str(&html);
    out.push_str("\r\n");
    if let Some(png) = viewport_png {
        let encoded_png = base64::engine::general_purpose::STANDARD.encode(png);
        out.push_str(&format!("--{BOUNDARY}\r\n"));
        out.push_str("Content-Type: image/png\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n");
        out.push_str(&format!("Content-Location: {IMAGE_LOCATION}\r\n\r\n"));
        for chunk in encoded_png.as_bytes().chunks(76) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push_str("\r\n");
        }
    }
    out.push_str(&format!("--{BOUNDARY}--\r\n"));
    out.into_bytes()
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn mhtml_header_value(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect()
}

fn process_error(command: &str, output: &ProcessOutput) -> String {
    let err = output.stderr.trim();
    if err.is_empty() {
        format!("{command} failed without an error message")
    } else {
        err.to_owned()
    }
}

fn run_process_with_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<ProcessOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("{program} failed to start: {err}"))?;
    let started = Instant::now();
    while started.elapsed() < timeout {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|err| format!("{program} output failed: {err}"))?;
                return Ok(ProcessOutput {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return Err(format!("{program} status failed: {err}")),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(format!("{program} timed out after {}s", timeout.as_secs()))
}

fn encode_color_image_png(img: &egui::ColorImage) -> Result<Vec<u8>, String> {
    let [w, h] = img.size;
    if w == 0 || h == 0 {
        return Err("empty frame".to_owned());
    }
    let expected = w
        .checked_mul(h)
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    if img.pixels.len() != expected {
        return Err(format!(
            "frame has {} pixels but expected {expected}",
            img.pixels.len()
        ));
    }
    let mut rgba = Vec::with_capacity(expected * 4);
    for pixel in &img.pixels {
        rgba.extend_from_slice(&[pixel.r(), pixel.g(), pixel.b(), pixel.a()]);
    }
    let mut bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut bytes, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|err| format!("could not write PNG header: {err}"))?;
        writer
            .write_image_data(&rgba)
            .map_err(|err| format!("could not write PNG pixels: {err}"))?;
    }
    Ok(bytes)
}

const ANNOTATION_BAR_HEIGHT: usize = 24;
const ANNOTATION_TEXT_SCALE: usize = 2;

fn annotate_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    if w == 0 || h == 0 {
        return Err("empty frame".to_owned());
    }
    let out_h = h
        .checked_add(ANNOTATION_BAR_HEIGHT)
        .ok_or_else(|| "annotated frame dimensions overflow".to_owned())?;
    let expected = w
        .checked_mul(h)
        .ok_or_else(|| "frame dimensions overflow".to_owned())?;
    if img.pixels.len() != expected {
        return Err(format!(
            "frame has {} pixels but expected {expected}",
            img.pixels.len()
        ));
    }
    let mut out = egui::ColorImage::new([w, out_h], Style::BG);
    out.pixels[..expected].copy_from_slice(&img.pixels);
    for y in h..out_h {
        for x in 0..w {
            out.pixels[y * w + x] = if y == h {
                Style::ACCENT
            } else {
                Style::SURFACE
            };
        }
    }
    draw_tiny_text(
        &mut out,
        6,
        h + 6,
        &caption.to_ascii_uppercase(),
        Style::TEXT,
    );
    Ok(out)
}

fn annotate_callout_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    let mut out = annotate_capture_image(img, caption)?;
    if w < 16 || h < 12 {
        draw_tiny_text(&mut out, 6, h + 6, "CALLOUT", Style::TEXT_STRONG);
        return Ok(out);
    }

    let box_w = (w / 3).clamp(12, 180);
    let box_h = (h / 3).clamp(8, 96);
    let x = (w.saturating_sub(box_w)) / 2;
    let y = (h.saturating_sub(box_h)) / 2;
    let accent = Style::ACCENT;
    draw_rect_outline(&mut out, x, y, box_w, box_h, accent);

    let leader_start_x = x.saturating_add(box_w);
    let leader_start_y = y;
    let leader_end_x = w.saturating_sub(3);
    let leader_end_y = 3;
    draw_diagonal_line(
        &mut out,
        leader_start_x,
        leader_start_y,
        leader_end_x,
        leader_end_y,
        accent,
    );
    draw_rect_outline(
        &mut out,
        leader_end_x.saturating_sub(10),
        leader_end_y,
        10,
        8,
        accent,
    );
    draw_tiny_text(
        &mut out,
        leader_end_x.saturating_sub(8),
        leader_end_y.saturating_add(1),
        "1",
        Style::TEXT_STRONG,
    );
    draw_tiny_text(&mut out, 6, h + 6, "CALLOUT", Style::TEXT_STRONG);
    Ok(out)
}

fn annotate_freehand_capture_image(
    img: &egui::ColorImage,
    caption: &str,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    let mut out = annotate_capture_image(img, caption)?;
    let stroke = Style::TEXT_STRONG;
    if w < 16 || h < 12 {
        draw_tiny_text(&mut out, 6, h + 6, "FREEHAND", stroke);
        return Ok(out);
    }

    let left = w / 5;
    let right = w.saturating_sub(left.max(1));
    let top = h / 4;
    let mid = h / 2;
    let bottom = h.saturating_sub(top.max(1));
    let points = [
        (left, mid),
        (left.saturating_add(w / 10), top),
        (left.saturating_add(w / 4), bottom),
        (left.saturating_add(w / 2), top.saturating_add(h / 8)),
        (right, mid),
    ];
    for segment in points.windows(2) {
        let [(x0, y0), (x1, y1)] = segment else {
            continue;
        };
        draw_thick_line(&mut out, *x0, *y0, *x1, *y1, stroke);
    }
    draw_tiny_text(&mut out, 6, h + 6, "FREEHAND", stroke);
    Ok(out)
}

fn draw_thick_line(
    img: &mut egui::ColorImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: egui::Color32,
) {
    for (dx, dy) in [(0isize, 0isize), (1, 0), (-1, 0), (0, 1), (0, -1)] {
        let sx0 = offset_coord(x0, dx);
        let sy0 = offset_coord(y0, dy);
        let sx1 = offset_coord(x1, dx);
        let sy1 = offset_coord(y1, dy);
        draw_diagonal_line(img, sx0, sy0, sx1, sy1, color);
    }
}

fn offset_coord(value: usize, delta: isize) -> usize {
    if delta.is_negative() {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta as usize)
    }
}

fn draw_rect_outline(
    img: &mut egui::ColorImage,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: egui::Color32,
) {
    if width == 0 || height == 0 {
        return;
    }
    let right = x.saturating_add(width.saturating_sub(1));
    let bottom = y.saturating_add(height.saturating_sub(1));
    for px in x..=right {
        set_pixel(img, px, y, color);
        set_pixel(img, px, bottom, color);
    }
    for py in y..=bottom {
        set_pixel(img, x, py, color);
        set_pixel(img, right, py, color);
    }
}

fn draw_diagonal_line(
    img: &mut egui::ColorImage,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
    color: egui::Color32,
) {
    let mut x0 = isize::try_from(x0).unwrap_or(isize::MAX);
    let mut y0 = isize::try_from(y0).unwrap_or(isize::MAX);
    let x1 = isize::try_from(x1).unwrap_or(isize::MAX);
    let y1 = isize::try_from(y1).unwrap_or(isize::MAX);
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        if let (Ok(x), Ok(y)) = (usize::try_from(x0), usize::try_from(y0)) {
            set_pixel(img, x, y, color);
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = err.saturating_mul(2);
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn set_pixel(img: &mut egui::ColorImage, x: usize, y: usize, color: egui::Color32) {
    let [w, h] = img.size;
    if x < w && y < h {
        img.pixels[y * w + x] = color;
    }
}

fn draw_tiny_text(
    img: &mut egui::ColorImage,
    mut x: usize,
    y: usize,
    text: &str,
    color: egui::Color32,
) {
    for ch in text.chars() {
        let glyph = tiny_glyph(ch);
        draw_tiny_glyph(img, x, y, glyph, color);
        x = x.saturating_add(6 * ANNOTATION_TEXT_SCALE);
        if x + 5 * ANNOTATION_TEXT_SCALE >= img.size[0] {
            break;
        }
    }
}

fn draw_tiny_glyph(
    img: &mut egui::ColorImage,
    x: usize,
    y: usize,
    glyph: [&'static str; 7],
    color: egui::Color32,
) {
    let [w, h] = img.size;
    for (gy, row) in glyph.iter().enumerate() {
        for (gx, bit) in row.as_bytes().iter().enumerate() {
            if *bit != b'1' {
                continue;
            }
            for sy in 0..ANNOTATION_TEXT_SCALE {
                for sx in 0..ANNOTATION_TEXT_SCALE {
                    let px = x + gx * ANNOTATION_TEXT_SCALE + sx;
                    let py = y + gy * ANNOTATION_TEXT_SCALE + sy;
                    if px < w && py < h {
                        img.pixels[py * w + px] = color;
                    }
                }
            }
        }
    }
}

fn tiny_glyph(ch: char) -> [&'static str; 7] {
    match ch {
        'A' => [
            "01110", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'B' => [
            "11110", "10001", "10001", "11110", "10001", "10001", "11110",
        ],
        'C' => [
            "01111", "10000", "10000", "10000", "10000", "10000", "01111",
        ],
        'D' => [
            "11110", "10001", "10001", "10001", "10001", "10001", "11110",
        ],
        'E' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
        'F' => [
            "11111", "10000", "10000", "11110", "10000", "10000", "10000",
        ],
        'G' => [
            "01111", "10000", "10000", "10111", "10001", "10001", "01111",
        ],
        'H' => [
            "10001", "10001", "10001", "11111", "10001", "10001", "10001",
        ],
        'I' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "11111",
        ],
        'J' => [
            "00111", "00010", "00010", "00010", "00010", "10010", "01100",
        ],
        'K' => [
            "10001", "10010", "10100", "11000", "10100", "10010", "10001",
        ],
        'L' => [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
        'M' => [
            "10001", "11011", "10101", "10101", "10001", "10001", "10001",
        ],
        'N' => [
            "10001", "11001", "10101", "10011", "10001", "10001", "10001",
        ],
        'O' => [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'P' => [
            "11110", "10001", "10001", "11110", "10000", "10000", "10000",
        ],
        'Q' => [
            "01110", "10001", "10001", "10001", "10101", "10010", "01101",
        ],
        'R' => [
            "11110", "10001", "10001", "11110", "10100", "10010", "10001",
        ],
        'S' => [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
        'T' => [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        'U' => [
            "10001", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
        'V' => [
            "10001", "10001", "10001", "10001", "10001", "01010", "00100",
        ],
        'W' => [
            "10001", "10001", "10001", "10101", "10101", "10101", "01010",
        ],
        'X' => [
            "10001", "10001", "01010", "00100", "01010", "10001", "10001",
        ],
        'Y' => [
            "10001", "10001", "01010", "00100", "00100", "00100", "00100",
        ],
        'Z' => [
            "11111", "00001", "00010", "00100", "01000", "10000", "11111",
        ],
        '0' => [
            "01110", "10001", "10011", "10101", "11001", "10001", "01110",
        ],
        '1' => [
            "00100", "01100", "00100", "00100", "00100", "00100", "01110",
        ],
        '2' => [
            "01110", "10001", "00001", "00010", "00100", "01000", "11111",
        ],
        '3' => [
            "11110", "00001", "00001", "01110", "00001", "00001", "11110",
        ],
        '4' => [
            "00010", "00110", "01010", "10010", "11111", "00010", "00010",
        ],
        '5' => [
            "11111", "10000", "10000", "11110", "00001", "00001", "11110",
        ],
        '6' => [
            "01110", "10000", "10000", "11110", "10001", "10001", "01110",
        ],
        '7' => [
            "11111", "00001", "00010", "00100", "01000", "01000", "01000",
        ],
        '8' => [
            "01110", "10001", "10001", "01110", "10001", "10001", "01110",
        ],
        '9' => [
            "01110", "10001", "10001", "01111", "00001", "00001", "01110",
        ],
        ':' => [
            "00000", "00100", "00100", "00000", "00100", "00100", "00000",
        ],
        '/' => [
            "00001", "00010", "00010", "00100", "01000", "01000", "10000",
        ],
        '.' => [
            "00000", "00000", "00000", "00000", "00000", "01100", "01100",
        ],
        '-' => [
            "00000", "00000", "00000", "11111", "00000", "00000", "00000",
        ],
        '_' => [
            "00000", "00000", "00000", "00000", "00000", "00000", "11111",
        ],
        '|' => [
            "00100", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
        ' ' => [
            "00000", "00000", "00000", "00000", "00000", "00000", "00000",
        ],
        _ => [
            "00000", "00000", "00000", "01110", "00000", "00000", "00000",
        ],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRegion {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl PixelRegion {
    fn from_points(a: egui::Pos2, b: egui::Pos2, frame_size: [usize; 2]) -> Option<Self> {
        let [frame_w, frame_h] = frame_size;
        if frame_w == 0 || frame_h == 0 {
            return None;
        }
        let min_x = a.x.min(b.x).floor().clamp(0.0, frame_w as f32) as usize;
        let min_y = a.y.min(b.y).floor().clamp(0.0, frame_h as f32) as usize;
        let max_x = a.x.max(b.x).ceil().clamp(0.0, frame_w as f32) as usize;
        let max_y = a.y.max(b.y).ceil().clamp(0.0, frame_h as f32) as usize;
        let width = max_x.saturating_sub(min_x);
        let height = max_y.saturating_sub(min_y);
        (width > 1 && height > 1).then_some(Self {
            x: min_x,
            y: min_y,
            width,
            height,
        })
    }

    fn rect_on_image(self, image_rect: egui::Rect, frame_size: [usize; 2]) -> egui::Rect {
        let [frame_w, frame_h] = frame_size;
        let sx = image_rect.width() / frame_w.max(1) as f32;
        let sy = image_rect.height() / frame_h.max(1) as f32;
        egui::Rect::from_min_size(
            image_rect.min + egui::vec2(self.x as f32 * sx, self.y as f32 * sy),
            egui::vec2(self.width as f32 * sx, self.height as f32 * sy),
        )
    }
}

fn crop_color_image(
    img: &egui::ColorImage,
    region: PixelRegion,
) -> Result<egui::ColorImage, String> {
    let [w, h] = img.size;
    if region.x >= w
        || region.y >= h
        || region.width == 0
        || region.height == 0
        || region.x + region.width > w
        || region.y + region.height > h
    {
        return Err("capture region is outside the active frame".to_owned());
    }
    let mut pixels = Vec::with_capacity(region.width * region.height);
    for row in region.y..region.y + region.height {
        let start = row * w + region.x;
        let end = start + region.width;
        pixels.extend_from_slice(&img.pixels[start..end]);
    }
    let mut out = egui::ColorImage::new([region.width, region.height], egui::Color32::TRANSPARENT);
    out.pixels = pixels;
    Ok(out)
}

/// The Browser page-actions menu (BOOKMARKS-10): the three mesh-integration verbs
/// on the current page. Rendered by BOTH the toolbar menu button and the address
/// bar's right-click context menu (one body, two entry points). Each item greys
/// out with no live URL to act on. §4 Carbon tokens on the chrome.
fn page_actions_menu(
    ui: &mut egui::Ui,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    url: &str,
    title: &str,
) {
    let has_page = !url.trim().is_empty();
    // Add-from-page → the mesh-synced bookmarks store (via the worker's add verb).
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{2606}  Add bookmark").color(Style::TEXT)),
        )
        .clicked()
    {
        publish(ACTION_BOOKMARKS_ADD, &bookmark_add_body(url, title));
        ui.close_menu();
    }
    // Copy-URL → the shell clipboard (egui's output command).
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{29C9}  Copy URL").color(Style::TEXT)),
        )
        .clicked()
    {
        ui.ctx().copy_text(url.to_string());
        ui.close_menu();
    }
    // Send-in-Chat → the NOTIFY-CHAT send verb.
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(RichText::new("\u{1F4AC}  Send in Chat").color(Style::TEXT)),
        )
        .clicked()
    {
        publish(
            ACTION_CHAT_SEND,
            &chat_share_body(&local_hostname(), url, title),
        );
        ui.close_menu();
    }
    for target in [
        BrowserShareTarget::Peer,
        BrowserShareTarget::Phone,
        BrowserShareTarget::Email,
        BrowserShareTarget::Qr,
    ] {
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    RichText::new(format!("{}  Share to {}", "\u{21AA}", target.label()))
                        .color(Style::TEXT),
                ),
            )
            .clicked()
        {
            publish_browser_share(bus_root, target, url, title);
            ui.close_menu();
        }
    }
    for target in [BrowserSendTabTarget::Node, BrowserSendTabTarget::Phone] {
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    RichText::new(format!("{}  Send tab to {}", "\u{21E5}", target.label()))
                        .color(Style::TEXT),
                ),
            )
            .clicked()
        {
            if let Some(engine) = engine {
                publish_browser_send_tab(bus_root, target, engine, url, title);
            }
            ui.close_menu();
        }
    }
}

/// The toolbar star that opens the BOOKMARKS-10 [`page_actions_menu`]; the glyph
/// dims with no live page (the menu items disable themselves too). Split out of
/// [`nav_chrome`] to keep that toolbar within its line budget.
fn page_actions_button(
    ui: &mut egui::Ui,
    has_page: bool,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    url: &str,
    title: &str,
) {
    let color = if has_page {
        Style::TEXT
    } else {
        Style::TEXT_DIM
    };
    ui.menu_button(
        RichText::new("\u{2606}").size(CHROME_FONT).color(color),
        |ui| {
            page_actions_menu(ui, bus_root, engine, url, title);
        },
    )
    .response
    .on_hover_text("Page actions \u{2014} bookmark, copy URL, share");
}

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

/// The navigation chrome bar — a §4-token toolbar. Back / forward / reload act on
/// the active session; the address bar loads on submit. On a crashed tab, Reload
/// becomes a respawn request. The page-actions menu (BOOKMARKS-10) hangs off both
/// the toolbar star button and the address bar's right-click.
fn nav_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    let crashed = state
        .tabs
        .get(state.active)
        .is_some_and(|t| t.session.is_crashed());
    let nav = state
        .tabs
        .get(state.active)
        .map(|t| t.session.nav().clone())
        .unwrap_or_default();
    let active_engine = state.tabs.get(state.active).map(|t| t.engine);
    let has_tab = !state.tabs.is_empty();
    // BOOKMARKS-7 — the per-page ad-filter blocked count the active session tracks.
    let blocked = state
        .tabs
        .get(state.active)
        .map_or(0, |t| t.session.blocked_count());
    // BOOKMARKS-10 — the live page's URL + title, owned clones so the page-actions
    // closures never borrow `state`; empty when there is no live tab to act on.
    let page_url = state
        .tabs
        .get(state.active)
        .map(|t| t.session.nav().url.clone())
        .unwrap_or_default();
    let page_title = state
        .tabs
        .get(state.active)
        .map(|t| t.session.title().to_string())
        .unwrap_or_default();
    let has_page = has_tab && !crashed && !page_url.trim().is_empty();

    let mut accepted_suggestion: Option<String> = None;
    ui.horizontal(|ui| {
        // Back / forward — enabled only when the live session offers the history.
        if nav_button(ui, "\u{2039}", "Back", has_tab && !crashed && nav.can_back) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_back();
            }
        }
        if nav_button(
            ui,
            "\u{203A}",
            "Forward",
            has_tab && !crashed && nav.can_forward,
        ) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_forward();
            }
        }
        // Stop while CEF is loading; otherwise Reload respawns crashed tabs or
        // reloads the page. Servo currently has no real cancel-load hook (DD-2,
        // investigated 2026-07-10 — see `can_show_stop_control`), so its compact
        // chrome keeps the honest Reload control while loading.
        let can_stop = can_show_stop_control(has_tab, crashed, nav.loading, active_engine);
        let (nav_label, nav_tip) = if can_stop {
            ("\u{00D7}", "Stop loading")
        } else if crashed {
            ("\u{21BB}", "Reload (restart page)")
        } else {
            ("\u{21BB}", "Reload")
        };
        if nav_button(ui, nav_label, nav_tip, has_tab) {
            if crashed {
                state.respawn_requested = true;
            } else if can_stop {
                if let Some(tab) = state.active_tab() {
                    tab.session.stop();
                }
            } else if let Some(tab) = state.active_tab() {
                tab.session.reload();
            }
        }

        // BOOKMARKS-10 — the page-actions menu (bookmark this page / copy its URL /
        // send it in Chat). The SAME three verbs also hang off the address bar's
        // right-click (below), so both the toolbar and the context menu reach them.
        page_actions_button(
            ui,
            has_page,
            state.bus_root.as_deref(),
            active_engine,
            &page_url,
            &page_title,
        );

        if nav_button(
            ui,
            "\u{25A3}",
            if state.capture_region_mode {
                "Select capture region"
            } else {
                "Capture viewport"
            },
            state.active_tab_has_frame(),
        ) {
            if state.capture_region_mode {
                state.cancel_region_capture();
            } else {
                state.capture_active_viewport();
            }
        }

        let (active_downloads, total_downloads) = state.download_counts();
        let downloads_label = if active_downloads > 0 {
            format!("\u{2193} {active_downloads}")
        } else {
            "\u{2193}".to_owned()
        };
        let downloads_tip = if total_downloads == 0 {
            "Downloads"
        } else {
            "Downloads from the shared Transfers ledger"
        };
        if ui
            .button(RichText::new(downloads_label).size(CHROME_FONT).color(
                if state.downloads_open {
                    Style::ACCENT
                } else {
                    Style::TEXT
                },
            ))
            .on_hover_text(downloads_tip)
            .clicked()
        {
            state.downloads_open = !state.downloads_open;
            if state.downloads_open {
                state.refresh_downloads();
            }
        }

        // BOOKMARKS-7 — a compact "N blocked" shield when the ad-filter has dropped
        // requests on this page (honest 0 stays hidden). Reads the session's
        // per-page counter; the engine is compiled from the mackesd `adfilter` blob.
        if blocked > 0 {
            ui.add_space(CHROME_GAP);
            ui.label(
                RichText::new(format!("\u{2298} {blocked}"))
                    .size(CHROME_FONT)
                    .color(Style::TEXT_DIM),
            )
            .on_hover_text(format!(
                "Ad-filter blocked {blocked} request{} on this page",
                if blocked == 1 { "" } else { "s" }
            ));
        }

        ui.add_space(CHROME_GAP);

        // The address bar fills the rest of the row.
        let field = egui::TextEdit::singleline(&mut state.address)
            .desired_width(ui.available_width() - Style::SP_XL * 2.0)
            .hint_text("Enter an address")
            .text_color(Style::TEXT)
            .font(egui::TextStyle::Small)
            .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H));
        let resp = ui.add_enabled(has_tab && !crashed, field);
        if resp.changed() && has_tab && !crashed {
            state.update_suggestions_for_address();
        }
        // BOOKMARKS-10 — right-click the address bar for the same page actions
        // (bookmark / copy URL / Send-in-Chat) the toolbar star exposes.
        resp.context_menu(|ui| {
            page_actions_menu(
                ui,
                state.bus_root.as_deref(),
                active_engine,
                &page_url,
                &page_title,
            );
        });
        let submit = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && has_tab
            && !crashed;

        let go = ui
            .add_enabled(
                has_tab && !crashed && !state.address.trim().is_empty(),
                egui::Button::new(
                    RichText::new("\u{2192}")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                )
                .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
            )
            .on_hover_text("Go")
            .clicked();

        if submit || go {
            state.submit_address();
        }
    });
    if has_tab && !crashed {
        accepted_suggestion = suggestions_panel(ui, state);
    }
    if let Some(suggestion) = accepted_suggestion {
        state.accept_suggestion(suggestion);
    }
}

fn find_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.find_open {
        return;
    }
    let enabled = state.can_drive_page_tools();
    let mut submit_forward = false;
    let mut submit_backward = false;
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Find")
                        .size(CHROME_FONT)
                        .color(Style::TEXT_DIM),
                );
                let resp = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut state.find_query)
                        .desired_width(220.0)
                        .hint_text("Find in page")
                        .text_color(Style::TEXT)
                        .font(egui::TextStyle::Small)
                        .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H)),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && ui.input(|i| i.modifiers.shift) {
                    submit_backward = true;
                } else if enter {
                    submit_forward = true;
                }
                if nav_button(ui, "\u{2191}", "Previous match", enabled) {
                    submit_backward = true;
                }
                if nav_button(ui, "\u{2193}", "Next match", enabled) {
                    submit_forward = true;
                }
                if nav_button(ui, "\u{00D7}", "Close find", true) {
                    state.close_find_bar();
                }
            });
        });
    if submit_backward {
        state.submit_find(true);
    } else if submit_forward {
        state.submit_find(false);
    }
}

fn print_settings_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.print_settings_open {
        return;
    }

    let printers = state.cups_printers.clone();
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Print").size(CHROME_FONT).color(Style::TEXT));
                ui.label(
                    RichText::new("CUPS destination")
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close print settings")
                        .clicked()
                    {
                        state.print_settings_open = false;
                    }
                    if ui
                        .small_button("\u{21BB}")
                        .on_hover_text("Refresh CUPS destinations")
                        .clicked()
                    {
                        state.refresh_cups_printers();
                    }
                });
            });

            if let Some(notice) = &state.cups_notice {
                ui.colored_label(
                    Style::DANGER,
                    RichText::new(format!("! {notice}")).size(Style::SMALL),
                );
            }

            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Destination").size(Style::SMALL));
                egui::ComboBox::from_id_salt("browser-cups-destination")
                    .selected_text(
                        state
                            .cups_settings
                            .destination
                            .as_deref()
                            .unwrap_or("System default"),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut state.cups_settings.destination,
                            None,
                            "System default",
                        );
                        for printer in &printers {
                            let label = if printer.is_default {
                                format!("{} (default)", printer.name)
                            } else {
                                printer.name.clone()
                            };
                            ui.selectable_value(
                                &mut state.cups_settings.destination,
                                Some(printer.name.clone()),
                                label,
                            );
                        }
                    });

                ui.separator();
                ui.label(RichText::new("Copies").size(Style::SMALL));
                ui.add(
                    egui::DragValue::new(&mut state.cups_settings.copies)
                        .range(1..=99)
                        .speed(1),
                );
                ui.checkbox(&mut state.cups_settings.duplex, "Duplex");
                ui.checkbox(&mut state.cups_settings.grayscale, "Grayscale");
                if ui
                    .add_enabled(
                        state.can_drive_page_tools(),
                        egui::Button::new(RichText::new("Print").size(Style::SMALL)),
                    )
                    .on_hover_text("Queue this page PDF and submit it to CUPS")
                    .clicked()
                {
                    state.print_active_page();
                }
            });

            if printers.is_empty() {
                muted_note(
                    ui,
                    "No CUPS destinations discovered; system default is still usable",
                );
            }
        });
}

fn downloads_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.downloads_open {
        return;
    }

    let mut action: Option<TransferVerb> = None;
    let worker_present = state.transfers.worker_present();
    let jobs = state.download_jobs.clone();
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Downloads")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new("browser_download ledger")
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close downloads")
                        .clicked()
                    {
                        state.downloads_open = false;
                    }
                    if ui
                        .small_button("\u{21BB}")
                        .on_hover_text("Refresh downloads")
                        .clicked()
                    {
                        state.refresh_downloads();
                    }
                });
            });

            if let Some(notice) = &state.download_notice {
                ui.colored_label(
                    Style::DANGER,
                    RichText::new(format!("! {notice}")).size(Style::SMALL),
                );
            }

            if jobs.is_empty() {
                let message = if worker_present {
                    "No browser downloads yet"
                } else {
                    "Transfers worker ledger is not present on this node"
                };
                muted_note(ui, message);
                return;
            }

            for job in jobs.iter().take(6) {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(short_transfer_name(job))
                                    .size(Style::SMALL)
                                    .color(Style::TEXT),
                            );
                            ui.label(
                                RichText::new(job.state.label())
                                    .size(Style::SMALL)
                                    .color(download_state_color(job.state)),
                            );
                            if job.policy.verify {
                                ui.label(
                                    RichText::new("verify")
                                        .size(Style::SMALL)
                                        .color(Style::TEXT_DIM),
                                );
                            }
                        });
                        ui.label(
                            RichText::new(job.route())
                                .size(Style::SMALL)
                                .color(Style::TEXT_DIM),
                        );
                        if let Some(progress) = job.progress {
                            ui.add(
                                egui::ProgressBar::new(f32::from(progress) / 100.0)
                                    .desired_width((ui.available_width() * 0.55).max(120.0))
                                    .text(format!("{progress}%")),
                            );
                        }
                        if let Some(err) = &job.error {
                            ui.colored_label(
                                Style::DANGER,
                                RichText::new(format!("! {err}")).size(Style::SMALL),
                            );
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !job.state.is_terminal()
                            && ui.small_button("Cancel").on_hover_text("Cancel").clicked()
                        {
                            action = Some(TransferVerb::Cancel(job.id.clone()));
                        }
                        if job.state.can_resume() && ui.small_button("Resume").clicked() {
                            action = Some(TransferVerb::Resume(job.id.clone()));
                        }
                        if job.state.can_pause() && ui.small_button("Pause").clicked() {
                            action = Some(TransferVerb::Pause(job.id.clone()));
                        }
                    });
                });
            }

            let hidden = jobs.len().saturating_sub(6);
            if hidden > 0 {
                muted_note(
                    ui,
                    &format!("{hidden} older browser download{} hidden", plural(hidden)),
                );
            }
        });

    if let Some(verb) = action {
        state.dispatch_download_verb(verb);
    }
}

fn short_transfer_name(job: &TransferJob) -> String {
    job.source
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .map_or_else(|| job.id.clone(), ToOwned::to_owned)
}

const fn download_state_color(state: TransferState) -> egui::Color32 {
    match state {
        TransferState::Done => Style::OK,
        TransferState::Failed => Style::DANGER,
        TransferState::Paused => Style::WARN,
        TransferState::Queued | TransferState::Running => Style::TEXT_DIM,
    }
}

fn suggestions_panel(ui: &mut egui::Ui, state: &WebState) -> Option<String> {
    if state.suggestions.items.is_empty() && state.suggestions.notice.is_none() {
        return None;
    }
    let mut accepted = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_space(Style::SP_XL * 4.0);
        for suggestion in &state.suggestions.items {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new(ellipsize(suggestion, 36))
                            .size(CHROME_FONT)
                            .color(Style::TEXT),
                    )
                    .fill(Style::SURFACE)
                    .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                )
                .on_hover_text(format!("Search for {suggestion}"))
                .clicked()
            {
                accepted = Some(suggestion.clone());
            }
        }
        if state.suggestions.items.is_empty() {
            if let Some(notice) = state.suggestions.notice.as_deref() {
                muted_note(ui, notice);
            }
        }
    });
    accepted
}

fn insecure_prompt(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(url) = state.insecure_prompt.clone() else {
        return;
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new("HTTP connection")
                .size(CHROME_FONT)
                .color(Style::WARN),
        );
        ui.label(RichText::new(ellipsize(&url, 64)).color(Style::TEXT_DIM));
        if ui
            .add(egui::Button::new(
                RichText::new("Use HTTPS")
                    .size(CHROME_FONT)
                    .color(Style::TEXT),
            ))
            .on_hover_text("Upgrade this navigation to HTTPS")
            .clicked()
        {
            state.upgrade_insecure_load();
        }
        if ui
            .add(egui::Button::new(
                RichText::new("Continue HTTP")
                    .size(CHROME_FONT)
                    .color(Style::WARN),
            ))
            .on_hover_text("Continue with the insecure HTTP URL")
            .clicked()
        {
            state.continue_insecure_load();
        }
        if ui
            .add(egui::Button::new(
                RichText::new("Cancel")
                    .size(CHROME_FONT)
                    .color(Style::TEXT_DIM),
            ))
            .clicked()
        {
            state.cancel_insecure_load();
        }
    });
}

fn new_tab_dashboard(ui: &mut egui::Ui, state: &mut WebState) {
    let mut submit_search = false;
    let mut open_service: Option<String> = None;
    centered(ui, |ui| {
        ui.label(
            RichText::new("Quasar Browser")
                .size(Style::HEADING)
                .color(Style::TEXT),
        );
        ui.add_space(Style::SP_M);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.dashboard_query)
                    .desired_width(420.0)
                    .hint_text("Search the mesh")
                    .text_color(Style::TEXT),
            );
            submit_search = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add(egui::Button::new(
                    RichText::new("Search").color(Style::TEXT),
                ))
                .clicked()
            {
                submit_search = true;
            }
        });
        ui.add_space(Style::SP_M);
        ui.horizontal_wrapped(|ui| {
            for service in &state.speed_dial {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(service.label.as_str())
                                .size(Style::BODY)
                                .color(Style::TEXT),
                        )
                        .min_size(egui::vec2(112.0, Style::SP_XL)),
                    )
                    .on_hover_text(service.hint.as_str())
                    .clicked()
                {
                    open_service = Some(service.url.clone());
                }
            }
        });
    });
    if submit_search {
        state.submit_dashboard_search();
    }
    if let Some(url) = open_service {
        state.open_mesh_service(url);
    }
}

/// A compact chrome button in the §4 palette, returning whether it was clicked.
fn nav_button(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> bool {
    let color = if enabled {
        Style::TEXT
    } else {
        Style::TEXT_DIM
    };
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(glyph).size(CHROME_FONT).color(color))
            .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
    )
    .on_hover_text(tip)
    .clicked()
}

/// Map a pointer position from egui panel space into the helper frame's **device
/// pixels** — the ONE transform both the live-input forward and the region-capture
/// drag flow through (browser-1).
///
/// The decoded frame is painted to *fill* `image_rect`, which sits below the tab
/// strip + nav chrome (so its origin is non-zero) and whose size — reported in egui
/// points — differs from the frame's device-pixel size on any non-1:1 seat (`HiDPI`,
/// maximized, 4K, a non-frame aspect). A pointer at fraction `f` across `image_rect`
/// maps to the same fraction across the `frame_size` device grid, so the transform
///
/// 1. clamps the pointer into `image_rect`,
/// 2. subtracts the rect origin and divides by the rect size for a `0..1` fraction
///    (both pointer and rect are egui points, so `pixels_per_point` cancels — the
///    mapping is DPI-independent), then
/// 3. multiplies by `frame_size` to land in frame device pixels, bounded to
///    `[0, frame_w] × [0, frame_h]`.
///
/// The old live path instead multiplied `pos - image_rect.min` by `pixels_per_point`
/// against a *fixed* 1280×800 frame, so clicks landed at the wrong page coordinate
/// on every seat whose displayed rect wasn't exactly 1280×800 device px.
fn map_pointer_to_frame(
    pos: egui::Pos2,
    image_rect: egui::Rect,
    frame_size: [usize; 2],
) -> egui::Pos2 {
    let clamped = pos.clamp(image_rect.min, image_rect.max);
    let rel = clamped - image_rect.min;
    egui::pos2(
        rel.x * frame_size[0] as f32 / image_rect.width().max(1.0),
        rel.y * frame_size[1] as f32 / image_rect.height().max(1.0),
    )
}

/// The device-pixel size the helper's frame should track — the browser panel `rect`
/// (egui points) scaled by `pixels_per_point`, clamped to [`MAX_CHANNEL_DIM`] so a
/// resize can never exceed the pre-sized channel. Rounded to whole pixels and at
/// least 1×1 (browser-1, item 2).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "device extent is scaled, rounded, then clamped into [1, MAX_CHANNEL_DIM]"
)]
fn frame_target_device_px(rect: egui::Rect, ppp: f32) -> (u32, u32) {
    let dim = |v: f32| -> u32 {
        if v.is_finite() {
            (v * ppp).round().clamp(1.0, MAX_CHANNEL_DIM as f32) as u32
        } else {
            1
        }
    };
    (dim(rect.width()), dim(rect.height()))
}

/// Paint the active tab's decoded frame to fill the body and forward this frame's
/// egui input to the session. Pointer geometry is mapped into frame device pixels
/// via [`map_pointer_to_frame`], and the helper's viewport is re-sized (debounced)
/// to track the real panel (browser-1).
fn paint_body(ui: &mut egui::Ui, state: &mut WebState, active: usize) {
    let Some((tex_id, texture_size, frame_size)) = state.tabs.get(active).and_then(|tab| {
        let texture = tab.texture.as_ref()?;
        Some((
            texture.id(),
            texture.size_vec2(),
            tab.last_frame.as_ref().map_or([0, 0], |frame| frame.size),
        ))
    }) else {
        return;
    };
    let size = ui.available_size();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let image_rect = fit_rect_preserving_aspect(rect, texture_size);
    ui.painter().rect_filled(rect, 0.0, Style::SURFACE);
    egui::Image::new(egui::load::SizedTexture::new(tex_id, image_rect.size()))
        .paint_at(ui, image_rect);

    // Drive the helper's CSS viewport to the real panel size (device px), debounced
    // so a drag-resize sends ONE settled resize instead of one per frame — this
    // makes the page track the panel instead of a fixed 1280×800 breakpoint
    // (browser-1, item 2). Runs every frame, in capture mode too, so the tracked
    // size never drifts. Repaint while settling so the debounce fires without input.
    let ppp = ui.ctx().pixels_per_point();
    let target = frame_target_device_px(rect, ppp);
    if let Some(tab) = state.tabs.get_mut(active) {
        if let Some((w, h)) = tab.resizer.observe(target, Instant::now(), RESIZE_DEBOUNCE) {
            tab.session.resize(w, h);
        }
        if tab.resizer.is_settling() {
            ui.ctx().request_repaint_after(RESIZE_DEBOUNCE);
        }
    }

    if state.capture_region_mode {
        handle_region_capture_drag(ui, state, &resp, image_rect, frame_size);
    }
    if resp.clicked() {
        resp.request_focus();
    }

    if state.capture_region_mode {
        return;
    }
    // Forward only page-owned input. Pointer geometry is mapped into frame device
    // pixels (via `map_pointer_to_frame` inside `browser_input_event`); keyboard/text
    // belongs to the page only after the image has focus, so address-bar/chrome
    // typing does not leak into the helper.
    let mut page_focused = state.tabs.get(active).is_some_and(|tab| tab.page_focused)
        || resp.has_focus()
        || resp.clicked()
        || resp.dragged();
    for event in ui.input(|i| i.events.clone()) {
        if let egui::Event::PointerButton { pos, pressed, .. } = &event {
            if *pressed {
                if image_rect.contains(*pos) {
                    page_focused = true;
                    resp.request_focus();
                } else if !rect.contains(*pos) {
                    page_focused = false;
                }
            }
        }
        if let Some(event) = browser_input_event(&event, image_rect, frame_size, page_focused) {
            if let Some(tab) = state.tabs.get_mut(active) {
                tab.last_activity = Instant::now();
                tab.idle_suspended = false;
                tab.session.send_input(&event, ppp);
            }
        }
    }
    if let Some(tab) = state.tabs.get_mut(active) {
        tab.page_focused = page_focused;
    }
    if let Some(tab) = state.tabs.get(active) {
        install_browser_page_accessibility(ui.ctx(), image_rect, tab, page_focused);
    }
}

fn handle_region_capture_drag(
    ui: &mut egui::Ui,
    state: &mut WebState,
    resp: &egui::Response,
    image_rect: egui::Rect,
    frame_size: [usize; 2],
) {
    if frame_size[0] == 0 || frame_size[1] == 0 {
        state.cancel_region_capture();
        state.capture_notice = Some("Capture failed: no painted page".to_owned());
        return;
    }
    // The SAME transform the live-input path uses (browser-1 dedup).
    let pointer_to_frame = |pos: egui::Pos2| map_pointer_to_frame(pos, image_rect, frame_size);
    if resp.drag_started() {
        if let Some(pos) = resp.interact_pointer_pos() {
            let pos = pointer_to_frame(pos);
            state.capture_region_start = Some(pos);
            state.capture_region_current = Some(pos);
        }
    } else if resp.dragged() {
        if let Some(pos) = resp.interact_pointer_pos() {
            state.capture_region_current = Some(pointer_to_frame(pos));
        }
    }

    if let (Some(start), Some(current)) = (state.capture_region_start, state.capture_region_current)
    {
        if let Some(region) = PixelRegion::from_points(start, current, frame_size) {
            let overlay = region.rect_on_image(image_rect, frame_size);
            ui.painter()
                .rect_filled(overlay, 0.0, Style::selection_wash());
            ui.painter().rect_stroke(
                overlay,
                0.0,
                egui::Stroke::new(1.0, Style::ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    }

    if resp.drag_stopped() {
        let result = state
            .capture_region_start
            .zip(state.capture_region_current)
            .and_then(|(start, current)| PixelRegion::from_points(start, current, frame_size))
            .ok_or_else(|| "selection is too small".to_owned())
            .and_then(|region| state.capture_active_region_to_dir(browser_capture_dir(), region));
        match result {
            Ok(path) => state.record_capture_success("Captured region", &path),
            Err(err) => state.capture_notice = Some(format!("Capture failed: {err}")),
        }
        state.cancel_region_capture();
    }
}

fn capture_notice(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(notice) = state.capture_notice.clone() else {
        return;
    };
    let tone = if notice.starts_with("Capture failed:")
        || notice.starts_with("PDF failed")
        || notice.starts_with("PDF viewer failed:")
        || notice.starts_with("Print failed:")
    {
        Style::DANGER
    } else {
        Style::ACCENT
    };
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(tone, RichText::new(notice).size(Style::SMALL));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Dismiss capture notice")
                        .clicked()
                    {
                        state.capture_notice = None;
                    }
                });
            });
        });
}

fn qr_share_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_qr_share.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("QR share")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(result.request_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close QR share")
                        .clicked()
                    {
                        state.latest_qr_share = None;
                    }
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy QR share URL")
                        .clicked()
                    {
                        ui.ctx().copy_text(result.url.clone());
                        state.capture_notice = Some("QR share URL copied".to_owned());
                    }
                });
            });
            let page = if result.title.trim().is_empty() {
                result.preview.as_str()
            } else {
                result.title.as_str()
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(page)
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} modules from {}",
                        result.modules.len(),
                        result.host
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
            });
            ui.add_space(Style::SP_XS);
            paint_qr_matrix(ui, &result.modules);
        });
}

fn paint_qr_matrix(ui: &mut egui::Ui, modules: &[Vec<bool>]) {
    let width = modules.len();
    if width == 0 {
        return;
    }
    let side = 168.0_f32.min(ui.available_width().max(96.0));
    let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, egui::Color32::WHITE);
    let quiet_zone = 4_usize;
    let total = width + quiet_zone * 2;
    let cell = rect.width() / total as f32;
    for (y, row) in modules.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if !*dark {
                continue;
            }
            let min = egui::pos2(
                rect.left() + (x + quiet_zone) as f32 * cell,
                rect.top() + (y + quiet_zone) as f32 * cell,
            );
            painter.rect_filled(
                egui::Rect::from_min_size(min, egui::vec2(cell.ceil(), cell.ceil())),
                0.0,
                egui::Color32::BLACK,
            );
        }
    }
}

fn translation_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_translation.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Translation")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(format!(
                        "{} \u{2192} {}",
                        result.source_lang, result.target_lang
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close translation")
                        .clicked()
                    {
                        state.latest_translation = None;
                    }
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy translated text")
                        .clicked()
                    {
                        ui.ctx().copy_text(result.translation.clone());
                        state.capture_notice = Some("Translation copied".to_owned());
                    }
                });
            });

            let page = if result.title.trim().is_empty() {
                result.url.as_str()
            } else {
                result.title.as_str()
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(page)
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from tab {} / {}",
                        result.translation.chars().count(),
                        result.tab_index,
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
            });
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.translation.as_str())
                            .size(Style::SMALL)
                            .color(Style::TEXT),
                    );
                });
        });
}

fn spellcheck_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_spellcheck.clone() else {
        return;
    };
    if !result.is_visible() {
        return;
    }
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Spelling")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(RichText::new(result.summary()).size(Style::SMALL).color(
                    if result.error.is_some() {
                        Style::WARN
                    } else {
                        Style::TEXT_DIM
                    },
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close spelling results")
                        .clicked()
                    {
                        state.latest_spellcheck = None;
                    }
                    if !result.misses.is_empty()
                        && ui
                            .small_button("Copy")
                            .on_hover_text("Copy spelling results")
                            .clicked()
                    {
                        ui.ctx().copy_text(spellcheck_results_text(&result.misses));
                        state.capture_notice = Some("Spelling results copied".to_owned());
                    }
                });
            });

            if let Some(error) = result.error.as_deref() {
                ui.label(RichText::new(error).size(Style::SMALL).color(Style::WARN));
                return;
            }

            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    for (row_index, miss) in result.misses.iter().take(24).enumerate() {
                        let occurrence = spellcheck_occurrence_index(&result.misses, row_index);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(miss.word.as_str())
                                    .size(Style::SMALL)
                                    .color(Style::WARN),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "chars {}..{}",
                                    miss.chars.start, miss.chars.end
                                ))
                                .size(Style::SMALL)
                                .color(Style::TEXT_DIM),
                            );
                            if miss.suggestions.is_empty() {
                                ui.label(
                                    RichText::new("no suggestions")
                                        .size(Style::SMALL)
                                        .color(Style::TEXT_DIM),
                                );
                            } else {
                                ui.label(
                                    RichText::new("suggest:")
                                        .size(Style::SMALL)
                                        .color(Style::TEXT_DIM),
                                );
                                for suggestion in miss.suggestions.iter().take(4) {
                                    if ui
                                        .small_button(suggestion.as_str())
                                        .on_hover_text(
                                            "Apply spelling suggestion to this occurrence",
                                        )
                                        .clicked()
                                    {
                                        state.apply_spellcheck_correction_at(
                                            result.tab_index,
                                            &miss.word,
                                            suggestion,
                                            occurrence,
                                        );
                                    }
                                    if ui
                                        .small_button("all")
                                        .on_hover_text(
                                            "Apply this suggestion to all visible matches",
                                        )
                                        .clicked()
                                    {
                                        state.apply_spellcheck_correction_all(
                                            result.tab_index,
                                            &miss.word,
                                            suggestion,
                                        );
                                    }
                                }
                            }
                        });
                    }
                    if result.misses.len() > 24 {
                        ui.label(
                            RichText::new(format!("{} more", result.misses.len() - 24))
                                .size(Style::SMALL)
                                .color(Style::TEXT_DIM),
                        );
                    }
                });
        });
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

fn offline_cache_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_offline_cache.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Offline copy")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(result.cache_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close offline copy")
                        .clicked()
                    {
                        state.latest_offline_cache = None;
                    }
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy cached page text")
                        .clicked()
                    {
                        ui.ctx().copy_text(result.text.clone());
                        state.capture_notice = Some("Offline copy text copied".to_owned());
                    }
                    if result.archive_mhtml.is_some()
                        && ui
                            .small_button("MHTML")
                            .on_hover_text("Save cached offline MHTML archive")
                            .clicked()
                    {
                        state.save_latest_offline_cache_archive();
                    }
                    if result.pdf_snapshot.is_some()
                        && ui
                            .small_button("PDF")
                            .on_hover_text("Open cached PDF snapshot")
                            .clicked()
                    {
                        state.open_latest_offline_cache_pdf();
                    }
                });
            });

            let page = if result.title.trim().is_empty() {
                result.url.as_str()
            } else {
                result.title.as_str()
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(page)
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from tab {} / {}",
                        result.text.chars().count(),
                        result.tab_index,
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
                if let Some(cached_ms) = result.cached_ms {
                    ui.label(
                        RichText::new(format!("cached {cached_ms}"))
                            .size(Style::SMALL)
                            .color(Style::TEXT_DIM),
                    );
                }
                if let Some(viewport) = &result.viewport {
                    ui.label(
                        RichText::new(format!(
                            "viewport PNG {}x{}",
                            viewport.width, viewport.height
                        ))
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                    );
                }
                if let Some(archive) = &result.archive_mhtml {
                    ui.label(
                        RichText::new(format!("MHTML {} bytes", archive.bytes))
                            .size(Style::SMALL)
                            .color(Style::TEXT_DIM),
                    );
                }
                if !result.resources.is_empty() {
                    let blocked = result
                        .resources
                        .iter()
                        .filter(|resource| !resource.allowed)
                        .count();
                    ui.label(
                        RichText::new(format!(
                            "resources {} / {} blocked",
                            result.resources.len(),
                            blocked
                        ))
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                    );
                }
            });
            if let Some(viewport) = &result.viewport {
                if let Some(texture) =
                    offline_cache_viewport_texture(ui.ctx(), &result.cache_id, viewport)
                {
                    let size = offline_cache_viewport_display_size(ui, viewport);
                    ui.add(
                        egui::Image::new(egui::load::SizedTexture::new(texture.id(), size))
                            .sense(Sense::hover()),
                    )
                    .on_hover_text("Cached viewport image");
                }
            }
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.text.as_str())
                            .size(Style::SMALL)
                            .color(Style::TEXT),
                    );
                });
        });
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

fn security_update_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(status) = state.latest_security_update.clone() else {
        return;
    };
    if !status.is_actionable() {
        return;
    }
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Browser engine update")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(status.state.as_str())
                        .size(Style::SMALL)
                        .color(match status.tone() {
                            ChipTone::Ok => Style::OK,
                            ChipTone::Warn | ChipTone::Danger => Style::WARN,
                            ChipTone::Info => Style::ACCENT,
                            ChipTone::Neutral => Style::TEXT_DIM,
                        }),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Hide browser engine update status")
                        .clicked()
                    {
                        state.latest_security_update = None;
                    }
                });
            });

            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("updater {}", status.updater_state))
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                if let Some(chromium) = &status.expected_chromium_version {
                    ui.label(
                        RichText::new(format!("Chromium {chromium}"))
                            .size(Style::SMALL)
                            .color(Style::TEXT_DIM),
                    );
                }
                if let Some(runtime) = &status.active_runtime {
                    ui.label(
                        RichText::new(runtime)
                            .size(Style::SMALL)
                            .color(Style::TEXT_DIM),
                    );
                }
            });

            for detail in [
                status.last_update_error.as_deref(),
                status.last_error.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                ui.label(RichText::new(detail).size(Style::SMALL).color(Style::WARN));
            }
        });
}

fn speech_status_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let read_aloud = state
        .latest_read_aloud_status
        .clone()
        .filter(BrowserReadAloudStatus::is_actionable);
    let voice = state
        .latest_voice_command_status
        .clone()
        .filter(BrowserVoiceCommandStatus::is_actionable);
    if read_aloud.is_none() && voice.is_none() {
        return;
    }
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Browser speech")
                        .size(CHROME_FONT)
                        .color(Style::TEXT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Hide browser speech status")
                        .clicked()
                    {
                        state.latest_read_aloud_status = None;
                        state.latest_voice_command_status = None;
                    }
                });
            });

            if let Some(status) = read_aloud {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(status.chip_label())
                            .size(Style::SMALL)
                            .color(speech_status_color(status.tone())),
                    );
                    if let Some(title) = status.last_title.as_deref() {
                        ui.label(
                            RichText::new(title)
                                .size(Style::SMALL)
                                .color(Style::TEXT_DIM),
                        );
                    } else if let Some(url) = status.last_url.as_deref() {
                        ui.label(RichText::new(url).size(Style::SMALL).color(Style::TEXT_DIM));
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} accepted / {} spoken / {} rejected",
                            status.accepted, status.spoken, status.rejected
                        ))
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                    );
                });
                if let Some(error) = status.last_error.as_deref() {
                    ui.label(RichText::new(error).size(Style::SMALL).color(Style::WARN));
                }
            }

            if let Some(status) = voice {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(status.chip_label())
                            .size(Style::SMALL)
                            .color(speech_status_color(status.tone())),
                    );
                    if let Some(url) = status.last_url.as_deref() {
                        ui.label(RichText::new(url).size(Style::SMALL).color(Style::TEXT_DIM));
                    }
                    if let Some(chars) = status.last_transcript_chars {
                        ui.label(
                            RichText::new(format!("{chars} transcript chars"))
                                .size(Style::SMALL)
                                .color(Style::TEXT_DIM),
                        );
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} accepted / {} transcribed / {} rejected",
                            status.accepted, status.transcribed, status.rejected
                        ))
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                    );
                });
                if let Some(error) = status.last_error.as_deref() {
                    ui.label(RichText::new(error).size(Style::SMALL).color(Style::WARN));
                }
            }
        });
}

fn speech_status_color(tone: ChipTone) -> egui::Color32 {
    match tone {
        ChipTone::Ok => Style::OK,
        ChipTone::Warn | ChipTone::Danger => Style::WARN,
        ChipTone::Info => Style::ACCENT,
        ChipTone::Neutral => Style::TEXT_DIM,
    }
}

fn fit_rect_preserving_aspect(outer: egui::Rect, content_size: egui::Vec2) -> egui::Rect {
    if content_size.x <= 0.0
        || content_size.y <= 0.0
        || outer.width() <= 0.0
        || outer.height() <= 0.0
    {
        return outer;
    }
    let scale = (outer.width() / content_size.x).min(outer.height() / content_size.y);
    let size = content_size * scale;
    egui::Rect::from_center_size(outer.center(), size)
}

fn accesskit_rect(rect: egui::Rect) -> egui::accesskit::Rect {
    egui::accesskit::Rect {
        x0: rect.min.x.into(),
        y0: rect.min.y.into(),
        x1: rect.max.x.into(),
        y1: rect.max.y.into(),
    }
}

fn browser_accessibility_id() -> egui::Id {
    egui::Id::new("browser-accessibility-status")
}

fn browser_page_accessibility_id() -> egui::Id {
    egui::Id::new("browser-accessibility-page")
}

fn tab_accessibility_state(tab: &Tab) -> String {
    if tab.idle_suspended {
        return "idle suspended".to_owned();
    }
    match tab.session.state() {
        SessionState::Loading => "loading".to_owned(),
        SessionState::Live => {
            if tab.texture.is_some() {
                "live".to_owned()
            } else {
                "live, waiting for first painted frame".to_owned()
            }
        }
        SessionState::Crashed { reason } => format!("crashed: {reason}"),
    }
}

fn tab_accessibility_tools(tab: &Tab) -> String {
    let mut tools = Vec::new();
    if tab.muted {
        tools.push("muted");
    }
    if tab.force_dark {
        tools.push("force dark");
    }
    if tab.reader_mode {
        tools.push("reader mode");
    }
    if tab.user_scripts {
        tools.push("userscripts");
    }
    if tab.page_focused {
        tools.push("page keyboard focus");
    }
    if tools.is_empty() {
        "no page tools enabled".to_owned()
    } else {
        tools.join(", ")
    }
}

fn tab_accessibility_summary(tab: &Tab) -> String {
    let nav = tab.session.nav();
    let title = tab.session.title().trim();
    let title = if title.is_empty() { "Untitled" } else { title };
    let url = nav.url.trim();
    let url = if url.is_empty() {
        "no committed URL"
    } else {
        url
    };
    let security = if url.starts_with("https://") {
        "secure"
    } else if url.starts_with("http://") {
        "not secure"
    } else {
        "local or internal"
    };
    format!(
        "{} page, {title}, {url}, {}, {}, container {}, display target {}, {}",
        tab.engine.label(),
        tab_accessibility_state(tab),
        security,
        tab.container.label(),
        tab.display_target.label(),
        tab_accessibility_tools(tab)
    )
}

fn browser_gate_notice(state: &WebState) -> &str {
    const DEFAULT_NOTICE: &str = "No live browser helper session is attached on this build or seat";
    #[cfg(feature = "live-helper")]
    {
        state.gate_notice.as_deref().unwrap_or(DEFAULT_NOTICE)
    }
    #[cfg(not(feature = "live-helper"))]
    {
        let _ = state;
        DEFAULT_NOTICE
    }
}

fn browser_accessibility_summary(state: &WebState) -> String {
    match state.tabs.get(state.active) {
        Some(tab) => format!(
            "Browser. Active tab {} of {}. {}",
            state.active + 1,
            state.tabs.len(),
            tab_accessibility_summary(tab)
        ),
        None => {
            let notice = browser_gate_notice(state);
            format!("Browser. No active tab. {notice}")
        }
    }
}

fn install_browser_accessibility(ctx: &egui::Context, rect: egui::Rect, state: &WebState) {
    let summary = browser_accessibility_summary(state);
    let _ = ctx.accesskit_node_builder(browser_accessibility_id(), |node| {
        node.set_role(egui::accesskit::Role::Status);
        node.set_live(egui::accesskit::Live::Polite);
        node.set_label("Browser status");
        node.set_value(summary);
        node.set_bounds(accesskit_rect(rect));
    });
}

fn install_browser_page_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    tab: &Tab,
    page_focused: bool,
) {
    let mut value = tab_accessibility_summary(tab);
    if page_focused {
        value.push_str(". Keyboard input is focused into the page canvas.");
    } else {
        value.push_str(". Click the page canvas to focus keyboard input.");
    }
    let _ = ctx.accesskit_node_builder(browser_page_accessibility_id(), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label("Browser page");
        node.set_value(value);
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
    });
}

/// Translate one egui event into the page-local event forwarded to the helper.
///
/// Pointer positions are mapped from panel space into frame device pixels via the
/// shared [`map_pointer_to_frame`] (`rect` = the displayed image rect, `frame_size`
/// = the current decoded frame). Only pointer positions are rewritten; wheel, keys,
/// and text pass through unchanged (gated on page focus). A pointer that leaves the
/// image while the page is focused reports `PointerGone` so the page's hover clears.
fn browser_input_event(
    event: &egui::Event,
    rect: egui::Rect,
    frame_size: [usize; 2],
    browser_focused: bool,
) -> Option<egui::Event> {
    match event {
        egui::Event::PointerMoved(pos) => {
            if rect.contains(*pos) {
                Some(egui::Event::PointerMoved(map_pointer_to_frame(
                    *pos, rect, frame_size,
                )))
            } else if browser_focused {
                Some(egui::Event::PointerGone)
            } else {
                None
            }
        }
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers,
        } => {
            if rect.contains(*pos) || browser_focused {
                Some(egui::Event::PointerButton {
                    pos: map_pointer_to_frame(*pos, rect, frame_size),
                    button: *button,
                    pressed: *pressed,
                    modifiers: *modifiers,
                })
            } else {
                None
            }
        }
        egui::Event::MouseWheel {
            unit,
            delta,
            modifiers,
        } => browser_focused.then_some(egui::Event::MouseWheel {
            unit: *unit,
            delta: *delta,
            modifiers: *modifiers,
        }),
        egui::Event::Key {
            key,
            physical_key,
            pressed,
            repeat,
            modifiers,
        } => browser_focused.then_some(egui::Event::Key {
            key: *key,
            physical_key: *physical_key,
            pressed: *pressed,
            repeat: *repeat,
            modifiers: *modifiers,
        }),
        egui::Event::Text(text) => browser_focused.then_some(egui::Event::Text(text.clone())),
        _ => None,
    }
}

/// The honest "page crashed" body: a danger caption naming the reason plus a
/// Reload that respawns the tab (setting `respawn_requested`). Never a fake page.
fn crashed_body(ui: &mut egui::Ui, reason: String, respawn_requested: &mut bool) {
    centered(ui, |ui| {
        ui.label(
            RichText::new("This page crashed")
                .size(Style::HEADING)
                .color(Style::DANGER),
        );
        ui.add_space(Style::SP_S);
        if !reason.is_empty() {
            muted_note(ui, reason);
        }
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("\u{21BB} Reload").color(Style::TEXT),
            ))
            .clicked()
        {
            *respawn_requested = true;
        }
    });
}

/// Render a daemon-owned private offline copy when the live page is unavailable.
fn cached_offline_body(
    ui: &mut egui::Ui,
    result: &BrowserOfflineCacheResult,
    unavailable_reason: Option<&str>,
) {
    egui::Frame::NONE
        .fill(Style::SURFACE)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Offline copy")
                        .size(Style::HEADING)
                        .color(Style::TEXT),
                );
                ui.label(
                    RichText::new(result.cache_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Copy")
                        .on_hover_text("Copy cached page text")
                        .clicked()
                    {
                        ui.ctx().copy_text(result.text.clone());
                    }
                });
            });
            if let Some(reason) = unavailable_reason
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
            {
                ui.add_space(Style::SP_XS);
                ui.label(
                    RichText::new(format!("Live page unavailable: {reason}"))
                        .size(Style::SMALL)
                        .color(Style::WARN),
                );
            }
            ui.add_space(Style::SP_XS);
            let page = if result.title.trim().is_empty() {
                result.url.as_str()
            } else {
                result.title.as_str()
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(page)
                        .size(Style::SMALL)
                        .color(Style::TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from {}",
                        result.text.chars().count(),
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(Style::TEXT_DIM),
                );
            });
            ui.add_space(Style::SP_S);
            egui::ScrollArea::vertical()
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.text.as_str())
                            .size(Style::SMALL)
                            .color(Style::TEXT),
                    );
                });
        });
}

/// The no-session `EmptyState` — an honest gated caption (or the NAMED live-path
/// notice when one is passed), never a placeholder page.
fn empty_body(ui: &mut egui::Ui, notice: Option<&str>) {
    centered(ui, |ui| {
        ui.label(
            RichText::new("Sandboxed browser")
                .size(Style::HEADING)
                .color(Style::TEXT),
        );
        ui.add_space(Style::SP_S);
        muted_note(
            ui,
            notice.unwrap_or(
                "The sandboxed Servo browser renders here in the shell. A live session \
                 attaches on a GPU seat (BOOKMARKS-5/6 live path is gated).",
            ),
        );
    });
}

/// Center `content` vertically + horizontally in the remaining body.
fn centered(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.5 - Style::SP_XL);
        content(ui);
    });
}

/// The Browser surface's shared **MENUBAR-ALL** top bar (design: `menubar-all.md`).
///
/// The UPPERCASE `BROWSER` title in the Terminals-group accent, the real
/// [`WebSession`] menus, and a live status cluster — the one shared
/// [`mde_egui::menubar::MenuBar`] every surface embeds. **Page** carries the
/// address-bar's open seam; Edit / View / History / Bookmarks bind to the session
/// + page-actions seams the toolbar chrome already drives (§6 glue, no new
/// behaviour). Engine choice lives in the tab strip as explicit `+ Servo` and
/// `+ CEF` buttons. A context-gated item renders **disabled** and an absent
/// capability is **omitted** (§7): no page-text Copy, no keyboard chord table —
/// and BROWSER-DD-8 **Power mode** is a real View toggle that reveals the
/// separate Power menu while keeping unfinished power tools honestly captioned. The
/// status cluster shows the active engine, committed URL, session lifecycle,
/// http/https security state, and ad-filter shield (BOOKMARKS-7).
mod menubar {
    use super::{
        bookmark_add_body, chat_share_body, local_hostname, publish, publish_browser_send_tab,
        publish_browser_share, BrowserEngine, BrowserPasskeyStatus, BrowserReadAloudStatus,
        BrowserSecurityUpdateStatus, BrowserSendTabTarget, BrowserShareTarget,
        BrowserVoiceCommandStatus, ContainerProfile, CupsPrintSettings, DevicePermissionKind,
        DeviceProfile, DisplayTarget, UserAgentOverride, WebState, ACTION_BOOKMARKS_ADD,
        ACTION_CHAT_SEND, CURATED_USERSCRIPT_COUNT, DEFAULT_DENIED_PERMISSIONS,
    };
    use mde_egui::egui;
    use mde_egui::menubar::{Entry, Item, Menu, MenuBar, MenuBarModel};
    use mde_egui::{ChipTone, StatusChip, Style};
    use mde_web_preview_client::SessionState;

    /// The lock glyph a secure (https) page wears in the security chip.
    const LOCK: &str = "\u{1F512}";
    /// The open-lock glyph a plain (http) page wears in the security chip.
    const UNLOCK: &str = "\u{1F513}";
    /// The ad-filter shield glyph (matches the toolbar "N blocked" readout).
    const SHIELD: &str = "\u{2298}";
    /// The committed-URL chip truncates to this many characters so a long address
    /// never crowds the status cluster.
    const URL_MAX: usize = 42;

    fn print_options_active(settings: &CupsPrintSettings) -> bool {
        settings.destination.is_some()
            || settings.copies > 1
            || settings.duplex
            || settings.grayscale
    }

    /// One Browser menu action — each maps to a real [`WebSession`]/page seam in
    /// [`apply`]. `Copy`, so the menu model stays a plain value tree.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum MenuAction {
        /// Navigate back (`WebSession::go_back`).
        Back,
        /// Navigate forward (`WebSession::go_forward`).
        Forward,
        /// Reload the page, or respawn a crashed tab (`WebSession::reload` /
        /// `respawn_requested` — the exact toolbar Reload behaviour).
        Reload,
        /// Load the address-bar draft on the active tab (`WebSession::load` —
        /// the toolbar Go button's exact seam, MENU-3).
        OpenAddress,
        /// Toggle the BROWSER-DD-2 vertical tab layout.
        ToggleVerticalTabs,
        /// Toggle the browser download manager drawer.
        ToggleDownloads,
        /// Toggle BROWSER-DD-8 power mode.
        TogglePowerMode,
        /// Cycle the active tab through the built-in container profiles.
        CycleContainer,
        /// Cycle the active tab through display-placement targets.
        CycleDisplayTarget,
        /// Increase page zoom.
        ZoomIn,
        /// Decrease page zoom.
        ZoomOut,
        /// Reset page zoom to 100%.
        ResetZoom,
        /// Open the compact find-in-page bar.
        OpenFind,
        /// Toggle the active tab's audio mute state.
        ToggleAudioMute,
        /// Toggle forced dark styling for the active tab.
        ToggleForceDark,
        /// Toggle reader-mode styling for the active tab.
        ToggleReaderMode,
        /// Toggle the shell-curated userscript bundle for the active tab.
        ToggleUserScripts,
        /// Run offline Hunspell over helper-extracted page text.
        CheckSpelling,
        /// Send helper-extracted page text to the platform TTS owner.
        ReadAloud,
        /// Send helper-extracted page text to the private translation owner.
        TranslatePage,
        /// Save helper-extracted page text to the private offline/mesh cache owner.
        SaveOfflineCopy,
        /// Ask the platform STT owner to capture and interpret a browser command.
        VoiceCommand,
        /// Ask the platform STT owner to capture dictation for the active page.
        Dictate,
        /// Capture the latest painted browser viewport to a PNG file.
        CaptureViewport,
        /// Capture the current full helper surface to a PNG file.
        CaptureFullPage,
        /// Capture the current page metadata plus rendered frame as MHTML.
        CaptureMhtml,
        /// Capture the latest viewport with a visible metadata caption band.
        CaptureAnnotatedViewport,
        /// Capture the latest viewport with a visible callout annotation.
        CaptureCalloutViewport,
        /// Capture the latest viewport with a visible freehand stroke.
        CaptureFreehandViewport,
        /// Arm a drag-to-select region capture over the latest painted viewport.
        CaptureRegion,
        /// Print the active page.
        PrintPage,
        /// Toggle the CUPS destination/options drawer.
        TogglePrintSettings,
        /// Save the active page as a PDF.
        SavePdf,
        /// Open the last saved PDF in a CEF tab using Chromium's built-in viewer.
        OpenLastPdf,
        /// Open the active page through the helper's `view-source:` navigation.
        OpenViewSource,
        /// Open the CEF helper's loopback Chromium DevTools portal.
        OpenChromiumDevtools,
        /// Export active-page scrape metadata files into the shared Transfers queue.
        ExportActivePageScrape,
        /// Export the active tab's observed media/image request manifest.
        ExportMediaManifest,
        /// Queue observed media/image asset download requests through Transfers.
        DownloadObservedMedia,
        /// Queue only observed image asset download requests through Transfers.
        DownloadObservedImages,
        /// Cycle the active tab's page-visible User-Agent override.
        CycleUserAgent,
        /// Cycle the active tab's page-visible device profile override.
        CycleDeviceProfile,
        /// Prompt and deny camera access for the active site.
        PromptCameraPermission,
        /// Prompt and deny microphone access for the active site.
        PromptMicrophonePermission,
        /// Prompt and deny location access for the active site.
        PromptLocationPermission,
        /// Prompt and deny notification access for the active site.
        PromptNotificationsPermission,
        /// Prompt and deny clipboard access for the active site.
        PromptClipboardPermission,
        /// Reset the active tab's transient browser state to the new-tab surface.
        ClearCurrentTabData,
        /// Toggle the current first-party site's ad/tracker blocking policy.
        ToggleSiteBlocking,
        /// Forget the current site's permission decisions while preserving default-deny.
        ForgetSitePermissions,
        /// Copy the committed URL to the shell clipboard (the page-actions seam).
        CopyUrl,
        /// Bookmark the live page (`action/bookmarks/add`, BOOKMARKS-10).
        AddBookmark,
        /// Open the full Bookmarks manager surface.
        OpenBookmarksManager,
        /// Share the live page into Chat (`action/chat/send`, BOOKMARKS-10).
        SendInChat,
        /// Hand the live page to the platform peer-share owner.
        ShareToPeer,
        /// Hand the live page to the platform phone-share owner.
        ShareToPhone,
        /// Hand the live page to the platform email owner.
        ShareToEmail,
        /// Hand the live page to the platform QR owner.
        ShareToQr,
        /// Hand the live tab to the session-sync owner for a target node.
        SendTabToNode,
        /// Hand the live tab to the phone bridge owner for a paired phone.
        SendTabToPhone,
    }

    /// A per-frame read-out of the active tab's live state — the single immutable
    /// borrow the menu model + status cluster are both built from, so the render is
    /// a pure function of it (unit-testable without a driven session).
    #[derive(Default)]
    #[allow(
        clippy::struct_excessive_bools,
        reason = "a flat read-out of the active tab's nav flags (can_back/can_forward/\
                  loading, mirroring NavState) plus has_tab/crashed and the address-bar \
                  draft flag — not a state machine"
    )]
    struct Snapshot {
        /// A tab is attached.
        has_tab: bool,
        /// Engine that owns the active tab, if any.
        active_engine: Option<BrowserEngine>,
        /// The active tab has crashed.
        crashed: bool,
        /// A back-history entry exists.
        can_back: bool,
        /// A forward-history entry exists.
        can_forward: bool,
        /// A load is in progress.
        loading: bool,
        /// The address bar holds a non-empty draft (gates Page → Open, MENU-3).
        typed_address: bool,
        /// Vertical tab chrome is enabled.
        vertical_tabs: bool,
        /// Active tab container identity.
        container: ContainerProfile,
        /// Active tab display-placement intent.
        display_target: DisplayTarget,
        /// Current page zoom percent.
        page_zoom_percent: u16,
        /// Find bar is open.
        find_open: bool,
        /// Download manager drawer is open.
        downloads_open: bool,
        /// Active browser-originated transfer count.
        active_downloads: usize,
        /// Total browser-originated transfer count.
        total_downloads: usize,
        /// BROWSER-DD-8 power mode is enabled.
        power_mode: bool,
        /// Active tab audio is muted.
        audio_muted: bool,
        /// Active tab force-dark styling is enabled.
        force_dark: bool,
        /// Active tab reader-mode styling is enabled.
        reader_mode: bool,
        /// Active tab has the shell-curated userscript bundle enabled.
        user_scripts: bool,
        /// Active tab page-visible User-Agent override.
        user_agent: UserAgentOverride,
        /// Active tab page-visible device profile override.
        device_profile: DeviceProfile,
        /// Active tab has a retained helper frame that can be captured.
        can_capture: bool,
        /// Drag-to-select region capture is armed.
        capture_region_mode: bool,
        /// CUPS print destination/options drawer is open.
        print_settings_open: bool,
        /// A non-default destination/options set is active.
        print_options_active: bool,
        /// A user save-as-PDF completed successfully and is still readable.
        has_saved_pdf: bool,
        /// The ad-filter blocked-request count for this page (BOOKMARKS-7).
        blocked: u32,
        /// The current first-party host, if the committed URL has one.
        current_site: Option<String>,
        /// Effective permission manager state for the current first-party host.
        current_site_permissions: Option<String>,
        /// Whether the native blocker is enabled for the current first-party host.
        site_blocking_enabled: bool,
        /// Safe-browsing mesh blocklist status.
        safe_browsing: String,
        /// Per-site data manager status.
        site_data: String,
        /// The committed URL.
        url: String,
        /// The session lifecycle, or `None` with no tab.
        state: Option<SessionState>,
        /// Daemon-owned read-aloud/TTS status for this node.
        read_aloud_status: Option<BrowserReadAloudStatus>,
        /// Daemon-owned voice-command/STT status for this node.
        voice_command_status: Option<BrowserVoiceCommandStatus>,
        /// Daemon-owned passkey/WebAuthn ceremony status for this node.
        passkey_status: Option<BrowserPasskeyStatus>,
        /// Daemon-owned CEF runtime updater status for this node.
        security_update: Option<BrowserSecurityUpdateStatus>,
    }

    impl Snapshot {
        /// Whether there is a live page (a non-crashed tab with a URL) to act on —
        /// the gate the page-family items (Copy URL / bookmark / share) share.
        fn has_page(&self) -> bool {
            self.has_tab && !self.crashed && !self.url.trim().is_empty()
        }
    }

    /// Read the active tab's live state into a [`Snapshot`] (one immutable borrow).
    fn snapshot(state: &WebState) -> Snapshot {
        let mut snap = state
            .tabs
            .get(state.active)
            .map_or_else(Snapshot::default, |tab| {
                let nav = tab.session.nav();
                let (active_downloads, total_downloads) = state.download_counts();
                Snapshot {
                    has_tab: true,
                    active_engine: Some(tab.engine),
                    crashed: tab.session.is_crashed(),
                    can_back: nav.can_back,
                    can_forward: nav.can_forward,
                    loading: nav.loading,
                    typed_address: false,
                    blocked: tab.session.blocked_count(),
                    current_site: state.active_first_party(),
                    current_site_permissions: state.active_site_permission_summary(),
                    site_blocking_enabled: state.active_site_blocking_enabled(),
                    safe_browsing: state.safe_browsing_summary(),
                    site_data: state.site_data_summary(),
                    url: nav.url.clone(),
                    state: Some(tab.session.state().clone()),
                    vertical_tabs: state.vertical_tabs,
                    container: tab.container,
                    display_target: tab.display_target,
                    page_zoom_percent: state.page_zoom_percent,
                    find_open: state.find_open,
                    downloads_open: state.downloads_open,
                    active_downloads,
                    total_downloads,
                    power_mode: state.power_mode,
                    audio_muted: tab.muted,
                    force_dark: tab.force_dark,
                    reader_mode: tab.reader_mode,
                    user_scripts: tab.user_scripts,
                    user_agent: tab.user_agent,
                    device_profile: tab.device_profile,
                    can_capture: tab.last_frame.is_some(),
                    capture_region_mode: state.capture_region_mode,
                    print_settings_open: state.print_settings_open,
                    print_options_active: print_options_active(&state.cups_settings),
                    has_saved_pdf: state.last_saved_pdf.is_some(),
                    read_aloud_status: state.latest_read_aloud_status.clone(),
                    voice_command_status: state.latest_voice_command_status.clone(),
                    passkey_status: state.latest_passkey_status.clone(),
                    security_update: state.latest_security_update.clone(),
                }
            });
        let (active_downloads, total_downloads) = state.download_counts();
        snap.typed_address = !state.address.trim().is_empty();
        snap.vertical_tabs = state.vertical_tabs;
        snap.page_zoom_percent = state.page_zoom_percent;
        snap.find_open = state.find_open;
        snap.downloads_open = state.downloads_open;
        snap.power_mode = state.power_mode;
        snap.capture_region_mode = state.capture_region_mode;
        snap.print_settings_open = state.print_settings_open;
        snap.print_options_active = print_options_active(&state.cups_settings);
        snap.has_saved_pdf = state.last_saved_pdf.is_some();
        snap.active_downloads = active_downloads;
        snap.total_downloads = total_downloads;
        snap.safe_browsing = state.safe_browsing_summary();
        snap.site_data = state.site_data_summary();
        snap.read_aloud_status = state.latest_read_aloud_status.clone();
        snap.voice_command_status = state.latest_voice_command_status.clone();
        snap.passkey_status = state.latest_passkey_status.clone();
        snap.security_update = state.latest_security_update.clone();
        snap
    }

    /// The Reload item's label — "Restart Page" on a crashed tab (it respawns),
    /// "Reload" otherwise (mirrors the toolbar tooltip).
    const fn reload_label(crashed: bool) -> &'static str {
        if crashed {
            "Restart Page"
        } else {
            "Reload"
        }
    }

    /// Build the Browser menus from live state (§6/§7): Page (the address-bar open
    /// seam), Edit (Copy URL), View (Reload, zoom, find, and the named BROWSER-DD-8
    /// Power-mode toggle), History (Back/Forward, gated on the live history),
    /// Privacy, and Bookmarks (add plus share). New-tab engine choice is handled by
    /// the tab strip's explicit `+ Servo` and `+ CEF` buttons.
    fn build_menus(s: &Snapshot) -> Vec<Menu<MenuAction>> {
        let has_page = s.has_page();
        let can_tools = s.has_tab && !s.crashed;
        let can_chromium_devtools = can_tools && s.active_engine == Some(BrowserEngine::Cef);
        let can_prompt_device_api = has_page && s.current_site.is_some();
        let mut menus = vec![
            Menu::new(
                "Page",
                vec![Entry::Item(
                    Item::new(MenuAction::OpenAddress, "Open Typed Address")
                        .shortcut("Enter")
                        .enabled(s.typed_address && s.has_tab && !s.crashed),
                )],
            ),
            Menu::new(
                "Edit",
                vec![Entry::Item(
                    Item::new(MenuAction::CopyUrl, "Copy URL").enabled(has_page),
                )],
            ),
            Menu::new(
                "View",
                vec![
                    Entry::Item(
                        Item::new(MenuAction::Reload, reload_label(s.crashed)).enabled(s.has_tab),
                    ),
                    Entry::Item(Item::new(
                        MenuAction::ToggleVerticalTabs,
                        if s.vertical_tabs {
                            "Horizontal Tabs"
                        } else {
                            "Vertical Tabs"
                        },
                    )),
                    Entry::Item(Item::new(
                        MenuAction::ToggleDownloads,
                        if s.downloads_open {
                            "Hide Downloads"
                        } else {
                            "Show Downloads"
                        },
                    )),
                    Entry::Item(
                        Item::new(
                            MenuAction::TogglePowerMode,
                            if s.power_mode {
                                "Disable Power Mode"
                            } else {
                                "Enable Power Mode"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::CycleContainer,
                            format!("Container: {}", s.container.label()),
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::CycleDisplayTarget,
                            format!("Display Target: {}", s.display_target.label()),
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Separator,
                    Entry::Item(
                        Item::new(MenuAction::ZoomIn, "Zoom In")
                            .shortcut("Ctrl++")
                            .enabled(can_tools && s.page_zoom_percent < super::PAGE_ZOOM_MAX),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ZoomOut, "Zoom Out")
                            .shortcut("Ctrl+-")
                            .enabled(can_tools && s.page_zoom_percent > super::PAGE_ZOOM_MIN),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ResetZoom, "Actual Size")
                            .shortcut("Ctrl+0")
                            .enabled(can_tools && s.page_zoom_percent != 100),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::OpenFind, "Find in Page")
                            .shortcut("Ctrl+F")
                            .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::ToggleAudioMute,
                            if s.audio_muted {
                                "Unmute Tab"
                            } else {
                                "Mute Tab"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::ToggleForceDark,
                            if s.force_dark {
                                "Disable Force Dark"
                            } else {
                                "Enable Force Dark"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::ToggleReaderMode,
                            if s.reader_mode {
                                "Disable Reader Mode"
                            } else {
                                "Enable Reader Mode"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::ToggleUserScripts,
                            if s.user_scripts {
                                "Disable Curated Userscripts"
                            } else {
                                "Enable Curated Userscripts"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Caption(format!(
                        "Userscript library: {CURATED_USERSCRIPT_COUNT} bundled site fixups"
                    )),
                    Entry::Item(
                        Item::new(MenuAction::CheckSpelling, "Check Spelling").enabled(can_tools),
                    ),
                    Entry::Item(Item::new(MenuAction::ReadAloud, "Read Aloud").enabled(can_tools)),
                    Entry::Item(
                        Item::new(MenuAction::TranslatePage, "Translate Page").enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::SaveOfflineCopy, "Save Offline Copy")
                            .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::VoiceCommand, "Voice Command").enabled(can_tools),
                    ),
                    Entry::Item(Item::new(MenuAction::Dictate, "Dictate").enabled(can_tools)),
                    Entry::Item(
                        Item::new(MenuAction::CaptureViewport, "Capture Viewport")
                            .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::CaptureFullPage, "Capture Full Page")
                            .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::CaptureMhtml, "Capture MHTML")
                            .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::CaptureAnnotatedViewport,
                            "Capture with Annotation",
                        )
                        .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::CaptureCalloutViewport, "Capture with Callout")
                            .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::CaptureFreehandViewport,
                            "Capture Freehand Markup",
                        )
                        .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(
                        Item::new(
                            MenuAction::CaptureRegion,
                            if s.capture_region_mode {
                                "Cancel Region Capture"
                            } else {
                                "Capture Region"
                            },
                        )
                        .enabled(can_tools && s.can_capture),
                    ),
                    Entry::Item(Item::new(MenuAction::PrintPage, "Print Page").enabled(can_tools)),
                    Entry::Item(
                        Item::new(
                            MenuAction::TogglePrintSettings,
                            if s.print_settings_open {
                                "Hide Print Settings"
                            } else {
                                "Print Settings"
                            },
                        )
                        .enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::SavePdf, "Save Page as PDF").enabled(can_tools),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::OpenLastPdf, "Open Last PDF")
                            .enabled(s.has_saved_pdf),
                    ),
                ],
            ),
            Menu::new(
                "History",
                vec![
                    Entry::Item(
                        Item::new(MenuAction::Back, "Back")
                            .enabled(s.has_tab && !s.crashed && s.can_back),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::Forward, "Forward")
                            .enabled(s.has_tab && !s.crashed && s.can_forward),
                    ),
                ],
            ),
            Menu::new("Privacy", {
                let mut entries = vec![
                    Entry::Caption("Cookies: blocked by the sandboxed engine".to_owned()),
                    Entry::Caption("Third-party cookies: blocked (no cookie store)".to_owned()),
                    Entry::Caption("Session data: cleared on tab close".to_owned()),
                    Entry::Caption(s.site_data.clone()),
                    Entry::Caption("Filter lists: bundled seed + synced/custom rules".to_owned()),
                    Entry::Caption(s.safe_browsing.clone()),
                    Entry::Caption(format!(
                        "Permissions: default deny ({DEFAULT_DENIED_PERMISSIONS})"
                    )),
                    Entry::Separator,
                    Entry::Item(
                        Item::new(
                            MenuAction::ToggleSiteBlocking,
                            if s.site_blocking_enabled {
                                "Disable Blocking for This Site"
                            } else {
                                "Enable Blocking for This Site"
                            },
                        )
                        .enabled(s.current_site.is_some() && s.has_tab && !s.crashed),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ForgetSitePermissions, "Forget Site Permissions")
                            .enabled(s.current_site.is_some() && s.has_tab && !s.crashed),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ClearCurrentTabData, "Clear Current Tab Data")
                            .enabled(s.has_tab && !s.crashed),
                    ),
                ];
                if let Some(site) = &s.current_site {
                    entries.insert(4, Entry::Caption(format!("This site: {site}")));
                }
                if let Some(summary) = &s.current_site_permissions {
                    entries.insert(5, Entry::Caption(format!("Site permissions: {summary}")));
                }
                entries
            }),
            Menu::new(
                "Bookmarks",
                vec![
                    Entry::Item(Item::new(
                        MenuAction::OpenBookmarksManager,
                        "Open Bookmarks Manager",
                    )),
                    Entry::Separator,
                    Entry::Item(
                        Item::new(MenuAction::AddBookmark, "Add Bookmark").enabled(has_page),
                    ),
                    Entry::Separator,
                    Entry::Item(
                        Item::new(MenuAction::SendInChat, "Send in Chat").enabled(has_page),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ShareToPeer, "Share to Peer").enabled(has_page),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ShareToPhone, "Share to Phone").enabled(has_page),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::ShareToEmail, "Share to Email").enabled(has_page),
                    ),
                    Entry::Item(Item::new(MenuAction::ShareToQr, "Share as QR").enabled(has_page)),
                    Entry::Separator,
                    Entry::Item(
                        Item::new(MenuAction::SendTabToNode, "Send Tab to Node").enabled(has_page),
                    ),
                    Entry::Item(
                        Item::new(MenuAction::SendTabToPhone, "Send Tab to Phone")
                            .enabled(has_page),
                    ),
                ],
            ),
        ];
        if s.power_mode {
            menus.insert(
                3,
                Menu::new(
                    "Power",
                    vec![
                        Entry::Item(
                            Item::new(MenuAction::OpenViewSource, "View Source").enabled(has_page),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::OpenChromiumDevtools, "Chromium DevTools")
                                .enabled(can_chromium_devtools),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::ExportActivePageScrape, "Export Page Scrape")
                                .enabled(has_page),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::ExportMediaManifest, "Export Media Manifest")
                                .enabled(has_page),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::DownloadObservedMedia, "Download Observed Media")
                                .enabled(has_page),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::DownloadObservedImages, "Download Observed Images")
                                .enabled(has_page),
                        ),
                        Entry::Item(
                            Item::new(
                                MenuAction::CycleUserAgent,
                                format!("User Agent: {}", s.user_agent.label()),
                            )
                            .enabled(can_tools),
                        ),
                        Entry::Item(
                            Item::new(
                                MenuAction::CycleDeviceProfile,
                                format!("Device Profile: {}", s.device_profile.label()),
                            )
                            .enabled(can_tools),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::PromptCameraPermission, "Prompt Camera Access")
                                .enabled(can_prompt_device_api),
                        ),
                        Entry::Item(
                            Item::new(
                                MenuAction::PromptMicrophonePermission,
                                "Prompt Microphone Access",
                            )
                            .enabled(can_prompt_device_api),
                        ),
                        Entry::Item(
                            Item::new(MenuAction::PromptLocationPermission, "Prompt Location")
                                .enabled(can_prompt_device_api),
                        ),
                        Entry::Item(
                            Item::new(
                                MenuAction::PromptNotificationsPermission,
                                "Prompt Notifications",
                            )
                            .enabled(can_prompt_device_api),
                        ),
                        Entry::Item(
                            Item::new(
                                MenuAction::PromptClipboardPermission,
                                "Prompt Clipboard Access",
                            )
                            .enabled(can_prompt_device_api),
                        ),
                        Entry::Separator,
                        Entry::Caption(
                            "UA/device overrides change page-visible navigator, screen, and \
                             viewport metadata; native request-header and compositor emulation \
                             remain follow-up hooks."
                                .to_owned(),
                        ),
                        Entry::Caption(
                            "Device API prompts record explicit per-site deny decisions for camera, \
                             microphone, location, notifications, and clipboard while helper \
                             enforcement remains default-deny."
                                .to_owned(),
                        ),
                        Entry::Caption(
                            "Chromium DevTools opens the CEF helper's loopback debugging portal; \
                             active CEF pages are selected from Chromium's target list when \
                             discovery is available. Servo DevTools remain a follow-up hook."
                                .to_owned(),
                        ),
                        Entry::Caption(
                            "Media Manifest exports observed image/media/HLS/DASH requests; \
                             Download Observed Media queues per-asset request files through \
                             Transfers, Download Observed Images narrows that batch to every \
                             observed image candidate, and blocked resources are marked for \
                             Power-mode ignore-blocking retrieval. Transfers now performs native \
                             direct/HLS/DASH fetches; native device emulation remains a follow-up \
                             tool."
                                .to_owned(),
                        ),
                        Entry::Caption(
                            "Export Page Scrape requests visible text plus DOM links/headings, writes \
                             bounded crawl seed/article/crawl-manifest JSON/CSV/Markdown artifacts, \
                             and submits the files through Transfers; the daemon executes bounded \
                             same-origin depth-1 crawl packages while deeper recursive discovery remains \
                             open."
                                .to_owned(),
                        ),
                    ],
                ),
            );
        }
        menus
    }

    /// The lifecycle status chip: Loading (a load in flight or the pre-first-frame
    /// state), Live (a painted, settled page), Crashed, or an idle "No session"
    /// with no tab.
    fn state_chip(s: &Snapshot) -> StatusChip {
        match &s.state {
            None => StatusChip::new("No session", ChipTone::Neutral),
            Some(SessionState::Crashed { .. }) => StatusChip::new("Crashed", ChipTone::Danger),
            Some(SessionState::Loading) => StatusChip::new("Loading", ChipTone::Info),
            Some(SessionState::Live) => {
                if s.loading {
                    StatusChip::new("Loading", ChipTone::Info)
                } else {
                    StatusChip::new("Live", ChipTone::Ok)
                }
            }
        }
    }

    /// The http/https security chip for the committed URL — a lock (Ok) for https,
    /// an open lock (Warn) for http, or `None` for a schemeless address
    /// (`about:blank`, empty) with no security state to report.
    fn security_chip(s: &Snapshot) -> Option<StatusChip> {
        if !s.has_tab {
            return None;
        }
        let url = s.url.trim();
        if url.starts_with("https://") {
            Some(StatusChip::with_icon(LOCK, "https", ChipTone::Ok))
        } else if url.starts_with("http://") {
            Some(StatusChip::with_icon(UNLOCK, "http", ChipTone::Warn))
        } else {
            None
        }
    }

    /// Truncate a URL to [`URL_MAX`] characters (an ellipsis tail) so the chip
    /// stays compact; a short URL is verbatim.
    fn truncate_url(url: &str) -> String {
        let url = url.trim();
        if url.chars().count() <= URL_MAX {
            return url.to_owned();
        }
        let head: String = url.chars().take(URL_MAX - 1).collect();
        format!("{head}\u{2026}")
    }

    /// Build the live status cluster: the active engine (MENU-3 — every live
    /// session is the sandboxed Servo helper today, so the chip appears only when
    /// a tab actually runs one, §7), the committed URL, the lifecycle state, the
    /// http/https security state, and the ad-filter shield (a `0` count stays
    /// hidden, §7).
    fn build_status(s: &Snapshot) -> Vec<StatusChip> {
        let mut chips = Vec::new();
        if let Some(engine) = s.active_engine {
            chips.push(StatusChip::new(engine.label(), ChipTone::Info));
        }
        if s.has_tab && !s.url.trim().is_empty() {
            chips.push(StatusChip::new(truncate_url(&s.url), ChipTone::Neutral));
        }
        if s.has_tab && s.page_zoom_percent != 100 {
            chips.push(StatusChip::new(
                format!("{}%", s.page_zoom_percent),
                ChipTone::Neutral,
            ));
        }
        if s.has_tab {
            let container = s.container.chip();
            if !container.is_empty() {
                chips.push(StatusChip::new(container, ChipTone::Info));
            }
            let display = s.display_target.chip();
            if !display.is_empty() {
                chips.push(StatusChip::new(display, ChipTone::Neutral));
            }
        }
        if s.has_tab && s.find_open {
            chips.push(StatusChip::new("Find", ChipTone::Info));
        }
        if s.active_downloads > 0 {
            chips.push(StatusChip::with_icon(
                "\u{2193}",
                s.active_downloads.to_string(),
                ChipTone::Info,
            ));
        } else if s.downloads_open && s.total_downloads > 0 {
            chips.push(StatusChip::with_icon(
                "\u{2193}",
                s.total_downloads.to_string(),
                ChipTone::Neutral,
            ));
        }
        if s.has_tab && s.audio_muted {
            chips.push(StatusChip::new("Muted", ChipTone::Warn));
        }
        if s.has_tab && s.force_dark {
            chips.push(StatusChip::new("Dark", ChipTone::Info));
        }
        if s.has_tab && s.reader_mode {
            chips.push(StatusChip::new("Reader", ChipTone::Info));
        }
        if s.has_tab && s.user_scripts {
            chips.push(StatusChip::new("Userscripts", ChipTone::Info));
        }
        if s.has_tab {
            let user_agent = s.user_agent.chip();
            if !user_agent.is_empty() {
                chips.push(StatusChip::new(user_agent, ChipTone::Warn));
            }
            let device_profile = s.device_profile.chip();
            if !device_profile.is_empty() {
                chips.push(StatusChip::new(device_profile, ChipTone::Warn));
            }
        }
        if s.power_mode {
            chips.push(StatusChip::new("Power", ChipTone::Warn));
        }
        if s.print_settings_open || s.print_options_active {
            chips.push(StatusChip::new("Print", ChipTone::Neutral));
        }
        if let Some(status) = &s.read_aloud_status {
            if status.is_visible() {
                chips.push(StatusChip::new(status.chip_label(), status.tone()));
            }
        }
        if let Some(status) = &s.voice_command_status {
            if status.is_visible() {
                chips.push(StatusChip::new(status.chip_label(), status.tone()));
            }
        }
        if let Some(status) = &s.passkey_status {
            if status.ceremony_is_visible() {
                chips.push(StatusChip::new(status.chip_label(), status.tone()));
            }
            if status.hardware_is_visible() {
                chips.push(StatusChip::new(
                    status.hardware_chip_label(),
                    status.hardware_tone(),
                ));
            }
            if status.ctaphid_is_visible() {
                chips.push(StatusChip::new(
                    status.ctaphid_chip_label(),
                    status.ctaphid_tone(),
                ));
            }
        }
        if let Some(status) = &s.security_update {
            chips.push(StatusChip::new(status.chip_label(), status.tone()));
        }
        chips.push(state_chip(s));
        if let Some(chip) = security_chip(s) {
            chips.push(chip);
        }
        if s.blocked > 0 {
            chips.push(StatusChip::with_icon(
                SHIELD,
                s.blocked.to_string(),
                ChipTone::Warn,
            ));
        }
        chips
    }

    /// Render the BROWSER bar and return the action the operator picked this frame.
    pub(super) fn show(state: &WebState, ui: &mut egui::Ui) -> Option<MenuAction> {
        let snap = snapshot(state);
        let menus = build_menus(&snap);
        let status = build_status(&snap);
        let model = MenuBarModel {
            // The dock groups Browser under **Terminals** (teal), so the title
            // wears that categorical accent (lock 2).
            title: "Browser",
            accent: Style::ACCENT_TERMINALS,
            menus: &menus,
            status: &status,
        };
        MenuBar::show(ui, &model)
    }

    /// The active tab's committed URL, or empty with no tab.
    fn page_url(state: &WebState) -> String {
        state
            .tabs
            .get(state.active)
            .map(|t| t.session.nav().url.clone())
            .unwrap_or_default()
    }

    /// The active tab's committed URL + title, or empties with no tab.
    fn page_url_title(state: &WebState) -> (String, String) {
        state.tabs.get(state.active).map_or_else(
            || (String::new(), String::new()),
            |t| (t.session.nav().url.clone(), t.session.title().to_owned()),
        )
    }

    /// Dispatch a picked action to its real seam (§6, no new behaviour) — the SAME
    /// seams the toolbar chrome + page-actions menu already drive.
    pub(super) fn apply(ctx: &egui::Context, state: &mut WebState, action: MenuAction) {
        match action {
            MenuAction::Back => {
                if let Some(tab) = state.active_tab() {
                    tab.session.go_back();
                }
            }
            MenuAction::Forward => {
                if let Some(tab) = state.active_tab() {
                    tab.session.go_forward();
                }
            }
            MenuAction::Reload => {
                let crashed = state
                    .tabs
                    .get(state.active)
                    .is_some_and(|t| t.session.is_crashed());
                if crashed {
                    state.respawn_requested = true;
                } else if let Some(tab) = state.active_tab() {
                    tab.session.reload();
                }
            }
            MenuAction::OpenAddress => {
                // The toolbar Go button's exact seam (MENU-3): load the address
                // draft on a live (non-crashed) active tab, including the
                // HTTPS-only prompt for explicit http:// targets.
                state.submit_address();
            }
            MenuAction::ToggleVerticalTabs => state.toggle_vertical_tabs(),
            MenuAction::ToggleDownloads => {
                state.downloads_open = !state.downloads_open;
                if state.downloads_open {
                    state.refresh_downloads();
                }
            }
            MenuAction::TogglePowerMode => state.toggle_power_mode(),
            MenuAction::CycleContainer => state.cycle_active_tab_container(),
            MenuAction::CycleDisplayTarget => state.cycle_active_tab_display_target(),
            MenuAction::ZoomIn => state.zoom_in(),
            MenuAction::ZoomOut => state.zoom_out(),
            MenuAction::ResetZoom => state.reset_zoom(),
            MenuAction::OpenFind => state.open_find_bar(),
            MenuAction::ToggleAudioMute => state.toggle_active_tab_mute(),
            MenuAction::ToggleForceDark => state.toggle_active_tab_force_dark(),
            MenuAction::ToggleReaderMode => state.toggle_active_tab_reader_mode(),
            MenuAction::ToggleUserScripts => state.toggle_active_tab_user_scripts(),
            MenuAction::CheckSpelling => state.request_active_spellcheck(),
            MenuAction::ReadAloud => state.request_active_read_aloud(),
            MenuAction::TranslatePage => state.request_active_translate_page(),
            MenuAction::SaveOfflineCopy => state.request_active_offline_cache(),
            MenuAction::VoiceCommand => {
                state.request_active_voice_command(super::VoiceCommandMode::Command)
            }
            MenuAction::Dictate => {
                state.request_active_voice_command(super::VoiceCommandMode::Dictation)
            }
            MenuAction::CaptureViewport => state.capture_active_viewport(),
            MenuAction::CaptureFullPage => state.capture_active_full_page(),
            MenuAction::CaptureMhtml => state.capture_active_mhtml(),
            MenuAction::CaptureAnnotatedViewport => state.capture_active_annotated_viewport(),
            MenuAction::CaptureCalloutViewport => state.capture_active_callout_viewport(),
            MenuAction::CaptureFreehandViewport => state.capture_active_freehand_viewport(),
            MenuAction::CaptureRegion => {
                if state.capture_region_mode {
                    state.cancel_region_capture();
                } else {
                    state.start_region_capture();
                }
            }
            MenuAction::PrintPage => state.print_active_page(),
            MenuAction::TogglePrintSettings => state.toggle_print_settings(),
            MenuAction::SavePdf => state.save_active_page_pdf(),
            MenuAction::OpenLastPdf => state.open_last_saved_pdf(),
            MenuAction::OpenViewSource => state.open_active_view_source(),
            MenuAction::OpenChromiumDevtools => state.open_chromium_devtools(),
            MenuAction::ExportActivePageScrape => state.export_active_page_metadata_scrape(),
            MenuAction::ExportMediaManifest => state.export_active_media_manifest(),
            MenuAction::DownloadObservedMedia => state.download_observed_media_assets(),
            MenuAction::DownloadObservedImages => state.download_observed_image_assets(),
            MenuAction::CycleUserAgent => state.cycle_active_tab_user_agent(),
            MenuAction::CycleDeviceProfile => state.cycle_active_tab_device_profile(),
            MenuAction::PromptCameraPermission => {
                state.prompt_active_device_permission(DevicePermissionKind::Camera)
            }
            MenuAction::PromptMicrophonePermission => {
                state.prompt_active_device_permission(DevicePermissionKind::Microphone)
            }
            MenuAction::PromptLocationPermission => {
                state.prompt_active_device_permission(DevicePermissionKind::Location)
            }
            MenuAction::PromptNotificationsPermission => {
                state.prompt_active_device_permission(DevicePermissionKind::Notifications)
            }
            MenuAction::PromptClipboardPermission => {
                state.prompt_active_device_permission(DevicePermissionKind::Clipboard)
            }
            MenuAction::ClearCurrentTabData => state.clear_active_session_data(),
            MenuAction::ToggleSiteBlocking => {
                let enabled = !state.active_site_blocking_enabled();
                state.set_active_site_blocking(enabled);
            }
            MenuAction::ForgetSitePermissions => state.forget_active_site_permissions(),
            MenuAction::CopyUrl => {
                let url = page_url(state);
                if !url.trim().is_empty() {
                    ctx.copy_text(url);
                }
            }
            MenuAction::AddBookmark => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish(ACTION_BOOKMARKS_ADD, &bookmark_add_body(&url, &title));
                }
            }
            MenuAction::OpenBookmarksManager => state.request_bookmarks_manager(),
            MenuAction::SendInChat => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish(
                        ACTION_CHAT_SEND,
                        &chat_share_body(&local_hostname(), &url, &title),
                    );
                }
            }
            MenuAction::ShareToPeer => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_share(
                        state.bus_root.as_deref(),
                        BrowserShareTarget::Peer,
                        &url,
                        &title,
                    );
                }
            }
            MenuAction::ShareToPhone => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_share(
                        state.bus_root.as_deref(),
                        BrowserShareTarget::Phone,
                        &url,
                        &title,
                    );
                }
            }
            MenuAction::ShareToEmail => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_share(
                        state.bus_root.as_deref(),
                        BrowserShareTarget::Email,
                        &url,
                        &title,
                    );
                }
            }
            MenuAction::ShareToQr => {
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_share(
                        state.bus_root.as_deref(),
                        BrowserShareTarget::Qr,
                        &url,
                        &title,
                    );
                }
            }
            MenuAction::SendTabToNode => {
                let Some(engine) = state.tabs.get(state.active).map(|tab| tab.engine) else {
                    return;
                };
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_send_tab(
                        state.bus_root.as_deref(),
                        BrowserSendTabTarget::Node,
                        engine,
                        &url,
                        &title,
                    );
                }
            }
            MenuAction::SendTabToPhone => {
                let Some(engine) = state.tabs.get(state.active).map(|tab| tab.engine) else {
                    return;
                };
                let (url, title) = page_url_title(state);
                if !url.trim().is_empty() {
                    publish_browser_send_tab(
                        state.bus_root.as_deref(),
                        BrowserSendTabTarget::Phone,
                        engine,
                        &url,
                        &title,
                    );
                }
            }
        }
    }

    #[cfg(test)]
    #[allow(
        clippy::panic,
        reason = "a let-else in a model test names the expected menu shape (house style, \
                  mirroring the shared menubar.rs tests)"
    )]
    mod tests {
        use super::{
            apply, build_menus, build_status, reload_label, security_chip, show, state_chip,
            truncate_url, BrowserEngine, BrowserPasskeyStatus, BrowserReadAloudStatus,
            BrowserVoiceCommandStatus, ContainerProfile, DeviceProfile, DisplayTarget, MenuAction,
            Snapshot, UserAgentOverride, WebState, URL_MAX,
        };
        use crate::web::BrowserSecurityUpdateStatus;
        use mde_egui::egui;
        use mde_egui::menubar::Entry;
        use mde_egui::{ChipTone, Style};
        use mde_web_preview_client::SessionState;

        /// A live, navigable https page (a non-crashed tab, one back entry, three
        /// blocked requests) — the model tests read their gating off it.
        fn https_page() -> Snapshot {
            Snapshot {
                has_tab: true,
                active_engine: Some(BrowserEngine::Servo),
                crashed: false,
                can_back: true,
                can_forward: false,
                loading: false,
                typed_address: false,
                vertical_tabs: false,
                container: ContainerProfile::None,
                display_target: DisplayTarget::Current,
                page_zoom_percent: 100,
                find_open: false,
                downloads_open: false,
                active_downloads: 0,
                total_downloads: 0,
                power_mode: false,
                audio_muted: false,
                force_dark: false,
                reader_mode: false,
                user_scripts: false,
                user_agent: UserAgentOverride::Default,
                device_profile: DeviceProfile::Default,
                can_capture: true,
                capture_region_mode: false,
                print_settings_open: false,
                print_options_active: false,
                has_saved_pdf: false,
                blocked: 3,
                current_site: Some("example.com".to_owned()),
                current_site_permissions: Some(
                    "example.com: all sensitive prompts denied by default".to_owned(),
                ),
                site_blocking_enabled: true,
                safe_browsing: "Safe browsing: 2 mesh-hosted unsafe hosts loaded".to_owned(),
                site_data: "Site data: 1 tracked site · 1 open tab · example.com cleared 0 times"
                    .to_owned(),
                url: "https://example.com/path".to_owned(),
                state: Some(SessionState::Live),
                read_aloud_status: None,
                voice_command_status: None,
                passkey_status: None,
                security_update: None,
            }
        }

        #[test]
        fn the_menus_cover_the_real_browser_seams() {
            let menus = build_menus(&https_page());
            let titles: Vec<&str> = menus.iter().map(|m| m.title.as_str()).collect();
            assert_eq!(
                titles,
                ["Page", "Edit", "View", "History", "Privacy", "Bookmarks"]
            );
            // Engine selection is not hidden in a menu; the tab strip exposes
            // explicit + Servo and + CEF new-tab buttons. File/Help are also
            // honestly omitted rather than present-but-dead menus (§7).
            assert!(!titles.contains(&"Engine"));
            assert!(!titles.contains(&"File"));
            assert!(!titles.contains(&"Help"));
        }

        #[test]
        fn open_typed_address_gates_on_a_draft_and_a_live_tab() {
            // A typed draft + a live tab → enabled; no draft, or a crashed tab,
            // or no tab → the honest disable (§7).
            let ready = Snapshot {
                typed_address: true,
                ..https_page()
            };
            let open = |menus: Vec<mde_egui::menubar::Menu<MenuAction>>| {
                menus
                    .into_iter()
                    .find(|m| m.title == "Page")
                    .and_then(|m| {
                        m.entries.into_iter().find_map(|e| match e {
                            Entry::Item(i) if i.id == MenuAction::OpenAddress => Some(i.enabled),
                            _ => None,
                        })
                    })
                    .expect("Page → Open Typed Address is present")
            };
            assert!(open(build_menus(&ready)), "draft + live tab enables");
            assert!(!open(build_menus(&https_page())), "no draft disables");
            let crashed = Snapshot {
                typed_address: true,
                crashed: true,
                ..https_page()
            };
            assert!(!open(build_menus(&crashed)), "a crashed tab disables");
            let no_tab = Snapshot {
                typed_address: true,
                ..Snapshot::default()
            };
            assert!(!open(build_menus(&no_tab)), "no tab disables");
        }

        #[test]
        fn the_view_menu_toggles_power_mode_without_showing_power_tools_by_default() {
            let view = build_menus(&https_page())
                .into_iter()
                .find(|m| m.title == "View")
                .expect("View menu present");
            let power = view
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::TogglePowerMode => Some(i),
                    _ => None,
                })
                .expect("Power mode toggle is in View");
            assert_eq!(power.label, "Enable Power Mode");
            assert!(
                build_menus(&https_page())
                    .iter()
                    .all(|menu| menu.title != "Power"),
                "the Power menu stays hidden until the operator enables Power mode"
            );
        }

        #[test]
        fn power_mode_adds_power_menu_and_status_chip() {
            let snap = Snapshot {
                power_mode: true,
                ..https_page()
            };
            let menus = build_menus(&snap);
            let titles: Vec<&str> = menus.iter().map(|m| m.title.as_str()).collect();
            assert_eq!(
                titles,
                [
                    "Page",
                    "Edit",
                    "View",
                    "Power",
                    "History",
                    "Privacy",
                    "Bookmarks"
                ]
            );
            let power = menus
                .iter()
                .find(|m| m.title == "Power")
                .expect("Power menu present");
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::OpenViewSource && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::OpenChromiumDevtools && !i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::ExportActivePageScrape && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::ExportMediaManifest && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::DownloadObservedMedia
                    && i.label == "Download Observed Media"
                    && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::DownloadObservedImages
                    && i.label == "Download Observed Images"
                    && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::CycleUserAgent
                    && i.label == "User Agent: Default User Agent"
                    && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::CycleDeviceProfile
                    && i.label == "Device Profile: Default Device"
                    && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::PromptCameraPermission
                    && i.label == "Prompt Camera Access"
                    && i.enabled
            )));
            assert!(power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::PromptClipboardPermission
                    && i.label == "Prompt Clipboard Access"
                    && i.enabled
            )));
            let chips = build_status(&snap);
            assert!(
                chips.iter().any(|chip| chip.text == "Power"),
                "Power mode is visible in the status cluster"
            );

            let cef_snap = Snapshot {
                power_mode: true,
                active_engine: Some(BrowserEngine::Cef),
                ..https_page()
            };
            let cef_power = build_menus(&cef_snap)
                .into_iter()
                .find(|m| m.title == "Power")
                .expect("CEF Power menu present");
            assert!(cef_power.entries.iter().any(|e| matches!(
                e,
                Entry::Item(i) if i.id == MenuAction::OpenChromiumDevtools && i.enabled
            )));

            let ua_snap = Snapshot {
                power_mode: true,
                user_agent: UserAgentOverride::AndroidChrome,
                device_profile: DeviceProfile::Phone,
                ..https_page()
            };
            let chips = build_status(&ua_snap);
            assert!(
                chips.iter().any(|chip| chip.text == "UA Mobile"),
                "UA override is visible in the status cluster"
            );
            assert!(
                chips.iter().any(|chip| chip.text == "Device Phone"),
                "device override is visible in the status cluster"
            );
        }

        #[test]
        fn the_view_menu_exposes_real_zoom_and_find_actions() {
            let view = build_menus(&https_page())
                .into_iter()
                .find(|m| m.title == "View")
                .expect("View menu present");
            let item = |id: MenuAction| {
                view.entries
                    .iter()
                    .find_map(|e| match e {
                        Entry::Item(i) if i.id == id => Some(i),
                        _ => None,
                    })
                    .expect("view item present")
            };
            assert!(item(MenuAction::ZoomIn).enabled);
            assert!(item(MenuAction::ZoomOut).enabled);
            assert!(
                !item(MenuAction::ResetZoom).enabled,
                "100% zoom has nothing to reset"
            );
            assert!(item(MenuAction::OpenFind).enabled);
            assert_eq!(item(MenuAction::ToggleDownloads).label, "Show Downloads");
            assert!(item(MenuAction::ToggleDownloads).enabled);
            assert_eq!(item(MenuAction::TogglePowerMode).label, "Enable Power Mode");
            assert!(item(MenuAction::TogglePowerMode).enabled);
            assert_eq!(item(MenuAction::ToggleAudioMute).label, "Mute Tab");
            assert!(item(MenuAction::ToggleAudioMute).enabled);
            assert_eq!(item(MenuAction::ToggleForceDark).label, "Enable Force Dark");
            assert!(item(MenuAction::ToggleForceDark).enabled);
            assert_eq!(
                item(MenuAction::ToggleReaderMode).label,
                "Enable Reader Mode"
            );
            assert!(item(MenuAction::ToggleReaderMode).enabled);
            assert_eq!(
                item(MenuAction::ToggleUserScripts).label,
                "Enable Curated Userscripts"
            );
            assert!(item(MenuAction::ToggleUserScripts).enabled);
            assert_eq!(item(MenuAction::CheckSpelling).label, "Check Spelling");
            assert!(item(MenuAction::CheckSpelling).enabled);
            assert_eq!(item(MenuAction::ReadAloud).label, "Read Aloud");
            assert!(item(MenuAction::ReadAloud).enabled);
            assert_eq!(item(MenuAction::TranslatePage).label, "Translate Page");
            assert!(item(MenuAction::TranslatePage).enabled);
            assert_eq!(item(MenuAction::VoiceCommand).label, "Voice Command");
            assert!(item(MenuAction::VoiceCommand).enabled);
            assert_eq!(item(MenuAction::Dictate).label, "Dictate");
            assert!(item(MenuAction::Dictate).enabled);
            assert!(item(MenuAction::CaptureViewport).enabled);
            assert!(item(MenuAction::CaptureFullPage).enabled);
            assert!(item(MenuAction::CaptureMhtml).enabled);
            assert!(item(MenuAction::CaptureAnnotatedViewport).enabled);
            assert!(item(MenuAction::CaptureCalloutViewport).enabled);
            assert!(item(MenuAction::CaptureFreehandViewport).enabled);
            assert!(item(MenuAction::CaptureRegion).enabled);
            assert!(item(MenuAction::PrintPage).enabled);
            assert_eq!(
                item(MenuAction::TogglePrintSettings).label,
                "Print Settings"
            );
            assert!(item(MenuAction::TogglePrintSettings).enabled);
            assert!(item(MenuAction::SavePdf).enabled);
            assert!(!item(MenuAction::OpenLastPdf).enabled);
            assert_eq!(
                item(MenuAction::CycleContainer).label,
                "Container: No Container"
            );
            assert!(item(MenuAction::CycleContainer).enabled);
            assert_eq!(
                item(MenuAction::CycleDisplayTarget).label,
                "Display Target: Current Display"
            );
            assert!(item(MenuAction::CycleDisplayTarget).enabled);

            let zoomed = Snapshot {
                container: ContainerProfile::Work,
                display_target: DisplayTarget::Secondary,
                page_zoom_percent: 150,
                find_open: true,
                downloads_open: true,
                active_downloads: 2,
                total_downloads: 3,
                audio_muted: true,
                force_dark: true,
                reader_mode: true,
                user_scripts: true,
                ..https_page()
            };
            let texts = build_status(&zoomed)
                .into_iter()
                .map(|c| c.text)
                .collect::<Vec<_>>();
            assert!(texts.contains(&"150%".to_owned()));
            assert!(texts.contains(&"Work".to_owned()));
            assert!(texts.contains(&"Display 2".to_owned()));
            assert!(texts.contains(&"Find".to_owned()));
            assert!(texts.contains(&"2".to_owned()));
            assert!(texts.contains(&"Muted".to_owned()));
            assert!(texts.contains(&"Dark".to_owned()));
            assert!(texts.contains(&"Reader".to_owned()));
            assert!(texts.contains(&"Userscripts".to_owned()));

            let muted = Snapshot {
                audio_muted: true,
                force_dark: true,
                reader_mode: true,
                user_scripts: true,
                ..https_page()
            };
            let view = build_menus(&muted)
                .into_iter()
                .find(|m| m.title == "View")
                .expect("View menu present");
            let unmute = view
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleAudioMute => Some(i),
                    _ => None,
                })
                .expect("mute item present");
            assert_eq!(unmute.label, "Unmute Tab");
            let disable_dark = view
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleForceDark => Some(i),
                    _ => None,
                })
                .expect("force-dark item present");
            assert_eq!(disable_dark.label, "Disable Force Dark");
            let disable_reader = view
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleReaderMode => Some(i),
                    _ => None,
                })
                .expect("reader item present");
            assert_eq!(disable_reader.label, "Disable Reader Mode");
            let disable_scripts = view
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleUserScripts => Some(i),
                    _ => None,
                })
                .expect("userscripts item present");
            assert_eq!(disable_scripts.label, "Disable Curated Userscripts");
        }

        #[test]
        fn the_engine_chip_reads_the_live_helper() {
            // A tab runs the sandboxed Servo helper → the engine chip shows; with
            // no session there is no engine to claim (§7).
            let chips = build_status(&https_page());
            assert_eq!(chips[0].text, "Servo", "the engine chip leads the cluster");
            assert_eq!(chips[0].tone, ChipTone::Info);
            assert!(
                !build_status(&Snapshot::default())
                    .iter()
                    .any(|c| c.text == "Servo"),
                "no tab ⇒ no engine chip"
            );
        }

        #[test]
        fn active_engine_chip_does_not_depend_on_future_tab_default() {
            let snap = Snapshot {
                active_engine: Some(BrowserEngine::Servo),
                ..https_page()
            };
            let chips = build_status(&snap);
            assert_eq!(
                chips[0].text, "Servo",
                "the status chip reads the actual active tab engine"
            );
            assert!(
                build_menus(&snap).iter().all(|m| m.title != "Engine"),
                "future-tab engine choice lives in the tab strip, not a menu"
            );
        }

        #[test]
        fn speech_owner_status_chips_reflect_retained_daemon_state() {
            let idle = Snapshot {
                read_aloud_status: Some(BrowserReadAloudStatus {
                    node: "node-a".to_owned(),
                    last_title: None,
                    last_url: None,
                    state: "idle".to_owned(),
                    last_error: None,
                    accepted: 0,
                    spoken: 0,
                    rejected: 0,
                    last_request_ms: None,
                    updated_ms: 1,
                }),
                ..https_page()
            };
            assert!(
                !build_status(&idle).iter().any(|c| c.text == "TTS idle"),
                "idle workers do not crowd the status cluster"
            );

            let active = Snapshot {
                read_aloud_status: Some(BrowserReadAloudStatus {
                    node: "node-a".to_owned(),
                    last_title: Some("Example".to_owned()),
                    last_url: Some("https://example.test/".to_owned()),
                    state: "speaking".to_owned(),
                    last_error: None,
                    accepted: 1,
                    spoken: 0,
                    rejected: 0,
                    last_request_ms: Some(2),
                    updated_ms: 3,
                }),
                voice_command_status: Some(BrowserVoiceCommandStatus {
                    node: "node-a".to_owned(),
                    last_url: Some("https://example.test/".to_owned()),
                    last_mode: Some("dictation".to_owned()),
                    state: "unavailable".to_owned(),
                    last_error: Some("STT runtime is not configured".to_owned()),
                    accepted: 1,
                    transcribed: 0,
                    rejected: 0,
                    last_transcript_chars: None,
                    last_request_ms: Some(4),
                    updated_ms: 5,
                }),
                ..https_page()
            };
            let chips = build_status(&active);
            assert!(chips
                .iter()
                .any(|c| { c.text == "TTS speaking" && c.tone == ChipTone::Info }));
            assert!(chips
                .iter()
                .any(|c| { c.text == "Dictation unavailable" && c.tone == ChipTone::Warn }));
        }

        #[test]
        fn passkey_owner_status_chip_reflects_retained_daemon_state() {
            let idle = Snapshot {
                passkey_status: Some(BrowserPasskeyStatus {
                    node: "node-a".to_owned(),
                    last_request_id: None,
                    last_host: None,
                    last_ceremony: None,
                    last_rp_id: None,
                    state: "idle".to_owned(),
                    mirrored: false,
                    last_error: None,
                    accepted: 0,
                    rejected: 0,
                    last_pending_ms: None,
                    hardware_state: "unknown".to_owned(),
                    hardware_key_count: 0,
                    hardware_readable_count: 0,
                    hardware_ctaphid_state: "unknown".to_owned(),
                    hardware_ctaphid_init_frame_count: 0,
                    hardware_probe_ms: 0,
                    updated_ms: 1,
                }),
                ..https_page()
            };
            assert!(
                !build_status(&idle).iter().any(|c| c.text == "Passkey idle"),
                "idle passkey worker state does not crowd the status cluster"
            );

            let hardware_ready = Snapshot {
                passkey_status: Some(BrowserPasskeyStatus {
                    node: "node-a".to_owned(),
                    last_request_id: None,
                    last_host: None,
                    last_ceremony: None,
                    last_rp_id: None,
                    state: "idle".to_owned(),
                    mirrored: false,
                    last_error: None,
                    accepted: 0,
                    rejected: 0,
                    last_pending_ms: None,
                    hardware_state: "ready".to_owned(),
                    hardware_key_count: 1,
                    hardware_readable_count: 1,
                    hardware_ctaphid_state: "init_request_ready".to_owned(),
                    hardware_ctaphid_init_frame_count: 1,
                    hardware_probe_ms: 2,
                    updated_ms: 3,
                }),
                ..https_page()
            };
            assert!(build_status(&hardware_ready)
                .iter()
                .any(|c| { c.text == "Security key ready" && c.tone == ChipTone::Ok }));
            assert!(build_status(&hardware_ready)
                .iter()
                .any(|c| { c.text == "CTAP INIT framed" && c.tone == ChipTone::Info }));

            let pending = Snapshot {
                passkey_status: Some(BrowserPasskeyStatus {
                    node: "node-a".to_owned(),
                    last_request_id: Some("01HPASSKEY".to_owned()),
                    last_host: Some("node-a".to_owned()),
                    last_ceremony: Some("create".to_owned()),
                    last_rp_id: Some("example.test".to_owned()),
                    state: "pending".to_owned(),
                    mirrored: true,
                    last_error: None,
                    accepted: 1,
                    rejected: 0,
                    last_pending_ms: Some(2),
                    hardware_state: "present_permission_denied".to_owned(),
                    hardware_key_count: 1,
                    hardware_readable_count: 0,
                    hardware_ctaphid_state: "unavailable".to_owned(),
                    hardware_ctaphid_init_frame_count: 0,
                    hardware_probe_ms: 2,
                    updated_ms: 3,
                }),
                ..https_page()
            };
            assert!(build_status(&pending)
                .iter()
                .any(|c| { c.text == "Passkey pending" && c.tone == ChipTone::Info }));
            assert!(build_status(&pending)
                .iter()
                .any(|c| { c.text == "Security key blocked" && c.tone == ChipTone::Warn }));

            let created = Snapshot {
                passkey_status: Some(BrowserPasskeyStatus {
                    node: "node-a".to_owned(),
                    last_request_id: Some("01HPASSKEY2".to_owned()),
                    last_host: Some("node-a".to_owned()),
                    last_ceremony: Some("create".to_owned()),
                    last_rp_id: Some("example.test".to_owned()),
                    state: "created".to_owned(),
                    mirrored: true,
                    last_error: None,
                    accepted: 2,
                    rejected: 0,
                    last_pending_ms: Some(4),
                    hardware_state: "unavailable".to_owned(),
                    hardware_key_count: 0,
                    hardware_readable_count: 0,
                    hardware_ctaphid_state: "unavailable".to_owned(),
                    hardware_ctaphid_init_frame_count: 0,
                    hardware_probe_ms: 4,
                    updated_ms: 5,
                }),
                ..https_page()
            };
            assert!(build_status(&created)
                .iter()
                .any(|c| { c.text == "Passkey created" && c.tone == ChipTone::Ok }));
            assert!(build_status(&created)
                .iter()
                .any(|c| { c.text == "Security key unavailable" && c.tone == ChipTone::Neutral }));
        }

        #[test]
        fn security_update_status_chip_reflects_retained_daemon_state() {
            let snap = Snapshot {
                security_update: Some(BrowserSecurityUpdateStatus {
                    node: "node-a".to_owned(),
                    state: "mismatch".to_owned(),
                    expected_cef_version: Some("149.0.6".to_owned()),
                    expected_chromium_version: Some("149.0.7827.201".to_owned()),
                    expected_channel: Some("stable".to_owned()),
                    active_runtime: Some("/opt/mde/cef".to_owned()),
                    installed_version: Some("old".to_owned()),
                    installed_chromium: Some("old".to_owned()),
                    libcef_present: true,
                    updater_state: "failed".to_owned(),
                    last_update_ms: Some(123),
                    last_update_exit_code: Some(69),
                    last_update_error: Some("installer unavailable".to_owned()),
                    last_error: Some(
                        "active CEF runtime does not match packaged manifest".to_owned(),
                    ),
                    updated_ms: 124,
                }),
                ..https_page()
            };

            let chips = build_status(&snap);

            assert!(chips
                .iter()
                .any(|c| { c.text == "CEF mismatch" && c.tone == ChipTone::Warn }));
        }

        #[test]
        fn back_and_forward_gate_on_the_live_history() {
            // can_back = true, can_forward = false → Back enabled, Forward greyed —
            // the §7 disable, never an omission.
            let history = build_menus(&https_page())
                .into_iter()
                .find(|m| m.title == "History")
                .expect("History menu present");
            let items: Vec<(String, bool)> = history
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Item(i) => Some((i.label.clone(), i.enabled)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                items,
                [("Back".to_owned(), true), ("Forward".to_owned(), false)]
            );
        }

        #[test]
        fn the_page_family_items_disable_without_a_live_page() {
            // No tab → page/session items grey (Copy URL / Reload / Back / Forward /
            // Add Bookmark / Send in Chat / Share), while the pure chrome layout
            // toggle remains usable.
            let menus = build_menus(&Snapshot::default());
            for menu in &menus {
                for entry in &menu.entries {
                    if let Entry::Item(item) = entry {
                        assert_eq!(
                            item.enabled,
                            matches!(
                                item.label.as_str(),
                                "Vertical Tabs" | "Show Downloads" | "Open Bookmarks Manager"
                            ),
                            "{} has the expected no-page gate",
                            item.label
                        );
                    }
                }
            }
        }

        #[test]
        fn the_bookmarks_menu_exposes_platform_share_handoffs() {
            let bookmarks = build_menus(&https_page())
                .into_iter()
                .find(|m| m.title == "Bookmarks")
                .expect("Bookmarks menu present");
            let items: Vec<(&str, bool)> = bookmarks
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Item(i) => Some((i.label.as_str(), i.enabled)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                items,
                [
                    ("Open Bookmarks Manager", true),
                    ("Add Bookmark", true),
                    ("Send in Chat", true),
                    ("Share to Peer", true),
                    ("Share to Phone", true),
                    ("Share to Email", true),
                    ("Share as QR", true),
                    ("Send Tab to Node", true),
                    ("Send Tab to Phone", true),
                ]
            );
        }

        #[test]
        fn the_privacy_menu_exposes_the_enforced_cookie_policy() {
            let menus = build_menus(&https_page());
            let privacy = menus
                .into_iter()
                .find(|m| m.title == "Privacy")
                .expect("Privacy menu present");
            let captions: Vec<&str> = privacy
                .entries
                .iter()
                .filter_map(|e| match e {
                    Entry::Caption(c) => Some(c.as_str()),
                    _ => None,
                })
                .collect();
            assert!(
                captions.iter().any(|c| c.contains("Cookies: blocked")),
                "cookie store policy is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Third-party cookies: blocked")),
                "third-party cookie policy is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Session data: cleared on tab close")),
                "clear-on-close policy is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Site data: 1 tracked site")),
                "the per-site data manager summary is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Filter lists: bundled seed + synced/custom rules")),
                "the filter-list policy source is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Safe browsing: 2 mesh-hosted unsafe hosts loaded")),
                "the safe-browsing mesh blocklist status is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Permissions: default deny")),
                "the default-deny permission manager policy is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("This site: example.com")),
                "the current first-party site is visible"
            );
            assert!(
                captions
                    .iter()
                    .any(|c| c.contains("Site permissions: example.com")),
                "the current site's effective permission policy is visible"
            );
            let site_toggle = privacy
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleSiteBlocking => Some(i),
                    _ => None,
                })
                .expect("site-blocking item present");
            assert!(
                site_toggle.enabled,
                "live pages can toggle per-site blocking"
            );
            assert_eq!(site_toggle.label, "Disable Blocking for This Site");
            let forget = privacy
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ForgetSitePermissions => Some(i),
                    _ => None,
                })
                .expect("permission manager item present");
            assert!(
                forget.enabled,
                "live pages can forget current-site permission decisions"
            );
            let clear = privacy
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ClearCurrentTabData => Some(i),
                    _ => None,
                })
                .expect("clear item present");
            assert!(clear.enabled, "live non-crashed tabs can be cleared");
        }

        #[test]
        fn the_privacy_menu_reenables_blocking_for_an_allowlisted_site() {
            let snap = Snapshot {
                site_blocking_enabled: false,
                ..https_page()
            };
            let privacy = build_menus(&snap)
                .into_iter()
                .find(|m| m.title == "Privacy")
                .expect("Privacy menu present");
            let toggle = privacy
                .entries
                .iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ToggleSiteBlocking => Some(i),
                    _ => None,
                })
                .expect("site toggle present");
            assert_eq!(toggle.label, "Enable Blocking for This Site");
            assert!(toggle.enabled);
        }

        #[test]
        fn reload_relabels_to_restart_and_stays_enabled_on_a_crashed_tab() {
            assert_eq!(reload_label(false), "Reload");
            assert_eq!(reload_label(true), "Restart Page");
            let crashed = Snapshot {
                has_tab: true,
                crashed: true,
                ..Snapshot::default()
            };
            let view = build_menus(&crashed)
                .into_iter()
                .find(|m| m.title == "View")
                .expect("View menu present");
            let Entry::Item(reload) = &view.entries[0] else {
                panic!("View[0] is Reload");
            };
            assert_eq!(reload.label, "Restart Page");
            assert!(
                reload.enabled,
                "Reload stays enabled on a crashed tab (it respawns)"
            );
        }

        #[test]
        fn the_status_cluster_reflects_the_live_page() {
            let chips = build_status(&https_page());
            let texts: Vec<&str> = chips.iter().map(|c| c.text.as_str()).collect();
            // Engine · URL · Live · https · 3 blocked, left→right (MENU-3 leads
            // with the active engine).
            assert_eq!(texts[0], "Servo");
            assert_eq!(texts[1], "https://example.com/path");
            assert!(texts.contains(&"Live"), "the lifecycle chip is present");
            assert!(texts.contains(&"https"), "the security chip is present");
            assert!(texts.contains(&"3"), "the ad-filter shield shows the count");
        }

        #[test]
        fn the_state_chip_tracks_the_session_lifecycle() {
            let live = state_chip(&https_page());
            assert_eq!(live.text, "Live");
            assert_eq!(live.tone, ChipTone::Ok);
            let loading = Snapshot {
                has_tab: true,
                loading: true,
                state: Some(SessionState::Live),
                ..Snapshot::default()
            };
            assert_eq!(state_chip(&loading).text, "Loading");
            let crashed = Snapshot {
                has_tab: true,
                state: Some(SessionState::Crashed {
                    reason: "boom".to_owned(),
                }),
                ..Snapshot::default()
            };
            assert_eq!(state_chip(&crashed).tone, ChipTone::Danger);
            // No tab → an honest neutral idle chip, never a fake "Live".
            assert_eq!(state_chip(&Snapshot::default()).tone, ChipTone::Neutral);
        }

        #[test]
        fn the_security_chip_reads_the_url_scheme() {
            assert_eq!(
                security_chip(&https_page()).expect("https chip").tone,
                ChipTone::Ok
            );
            let http = Snapshot {
                has_tab: true,
                url: "http://plain.example/".to_owned(),
                state: Some(SessionState::Live),
                ..Snapshot::default()
            };
            assert_eq!(
                security_chip(&http).expect("http chip").tone,
                ChipTone::Warn
            );
            // A schemeless address (about:blank) and no-tab both omit the chip.
            let blank = Snapshot {
                has_tab: true,
                url: "about:blank".to_owned(),
                state: Some(SessionState::Live),
                ..Snapshot::default()
            };
            assert!(security_chip(&blank).is_none());
            assert!(security_chip(&Snapshot::default()).is_none());
        }

        #[test]
        fn a_long_url_truncates_but_a_short_one_is_verbatim() {
            assert_eq!(truncate_url("https://a.co/"), "https://a.co/");
            let long = "https://example.com/a/very/long/path/that/keeps/going/and/going/on";
            let out = truncate_url(long);
            assert!(out.chars().count() <= URL_MAX, "truncated within the cap");
            assert!(
                out.ends_with('\u{2026}'),
                "a truncated URL wears an ellipsis"
            );
        }

        #[test]
        fn apply_on_an_empty_state_is_a_safe_noop() {
            let ctx = egui::Context::default();
            let mut state = WebState {
                address: "https://example.com/".to_owned(),
                ..WebState::default()
            };
            for action in [
                MenuAction::Back,
                MenuAction::Forward,
                MenuAction::Reload,
                MenuAction::OpenAddress,
                MenuAction::TogglePowerMode,
                MenuAction::CycleContainer,
                MenuAction::CycleDisplayTarget,
                MenuAction::ZoomIn,
                MenuAction::ZoomOut,
                MenuAction::ResetZoom,
                MenuAction::OpenFind,
                MenuAction::ToggleAudioMute,
                MenuAction::ToggleForceDark,
                MenuAction::ToggleReaderMode,
                MenuAction::ToggleUserScripts,
                MenuAction::CheckSpelling,
                MenuAction::ReadAloud,
                MenuAction::TranslatePage,
                MenuAction::VoiceCommand,
                MenuAction::Dictate,
                MenuAction::CaptureViewport,
                MenuAction::CaptureFullPage,
                MenuAction::CaptureMhtml,
                MenuAction::CaptureAnnotatedViewport,
                MenuAction::CaptureCalloutViewport,
                MenuAction::CaptureFreehandViewport,
                MenuAction::CaptureRegion,
                MenuAction::PrintPage,
                MenuAction::SavePdf,
                MenuAction::OpenViewSource,
                MenuAction::OpenChromiumDevtools,
                MenuAction::ExportActivePageScrape,
                MenuAction::ExportMediaManifest,
                MenuAction::DownloadObservedMedia,
                MenuAction::DownloadObservedImages,
                MenuAction::CycleUserAgent,
                MenuAction::CycleDeviceProfile,
                MenuAction::PromptCameraPermission,
                MenuAction::PromptMicrophonePermission,
                MenuAction::PromptLocationPermission,
                MenuAction::PromptNotificationsPermission,
                MenuAction::PromptClipboardPermission,
                MenuAction::ClearCurrentTabData,
                MenuAction::ToggleSiteBlocking,
                MenuAction::ForgetSitePermissions,
                MenuAction::CopyUrl,
                MenuAction::AddBookmark,
                MenuAction::OpenBookmarksManager,
                MenuAction::SendInChat,
                MenuAction::ShareToPeer,
                MenuAction::ShareToPhone,
                MenuAction::ShareToEmail,
                MenuAction::ShareToQr,
                MenuAction::SendTabToNode,
                MenuAction::SendTabToPhone,
            ] {
                apply(&ctx, &mut state, action);
            }
            assert!(!state.respawn_requested, "no tab → Reload is a no-op");
            assert!(state.tabs.is_empty(), "no action attaches or drops a tab");
            assert_eq!(state.page_zoom_percent, 100, "no tab → zoom is unchanged");
            assert!(!state.find_open, "no tab → find remains closed");
        }

        #[test]
        fn the_view_menu_toggles_vertical_tabs() {
            let ctx = egui::Context::default();
            let mut state = WebState::default();
            assert!(!state.vertical_tabs);
            apply(&ctx, &mut state, MenuAction::ToggleVerticalTabs);
            assert!(state.vertical_tabs);
            apply(&ctx, &mut state, MenuAction::ToggleVerticalTabs);
            assert!(!state.vertical_tabs);
        }

        #[test]
        fn the_view_menu_toggles_downloads() {
            let ctx = egui::Context::default();
            let mut state = WebState::default();
            assert!(!state.downloads_open);
            apply(&ctx, &mut state, MenuAction::ToggleDownloads);
            assert!(state.downloads_open);
            apply(&ctx, &mut state, MenuAction::ToggleDownloads);
            assert!(!state.downloads_open);
        }

        #[test]
        fn the_bookmarks_menu_requests_the_manager_surface() {
            let ctx = egui::Context::default();
            let mut state = WebState::default();
            assert!(!state.take_bookmarks_manager_request());
            apply(&ctx, &mut state, MenuAction::OpenBookmarksManager);
            assert!(state.take_bookmarks_manager_request());
            assert!(!state.take_bookmarks_manager_request());
        }

        #[test]
        fn the_browser_bar_renders_headless() {
            use egui::{pos2, vec2, Rect};
            let ctx = egui::Context::default();
            Style::install(&ctx);
            let state = WebState::default();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1024.0, 640.0))),
                ..Default::default()
            };
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let _ = show(&state, ui);
                });
            });
            let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
            assert!(!prims.is_empty(), "the Browser bar produced no primitives");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mde_egui::egui::{pos2, vec2, Rect};
    use mde_web_preview_client::{scm, testkit, wire};
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// A headless 960×640 shell body, mirroring the VDI + shell render tests.
    fn body_input() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(960.0, 640.0))),
            ..Default::default()
        }
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

    fn run_panel_output(
        ctx: &egui::Context,
        state: &mut WebState,
        input: egui::RawInput,
    ) -> egui::FullOutput {
        ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| web_panel(ui, state));
        })
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

    #[test]
    fn curated_userscript_bundle_contains_the_first_site_fixups() {
        let bundle = curated_userscript_bundle();
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
    fn browser_page_exports_accesskit_status_and_clickable_page_region() {
        let (session, _helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
        state.tabs[state.active].force_dark = true;
        state.tabs[state.active].reader_mode = true;
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

        let page = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Browser page"))
            .expect("browser page accesskit node");
        assert_eq!(page.role(), egui::accesskit::Role::Button);
        let page_value = page.value().expect("browser page value");
        assert!(
            !page_value.contains("CEF"),
            "test session defaults to Servo"
        );
        assert!(page_value.contains("Click the page canvas to focus keyboard input"));
    }

    fn write_helper_event(stream: &UnixStream, msg: &mde_web_preview_client::EventMsg) {
        let mut stream = stream;
        stream
            .write_all(&wire::frame(&msg.encode()))
            .expect("write helper event");
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
            Some("PDF saved /tmp/mde-page.pdf")
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
            Some("PDF failed /tmp/mde-page.pdf")
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
            format!("PDF saved {path_text}")
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
            Some("Opening PDF in CEF viewer")
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
        assert!(
            state
                .capture_notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("PDF viewer failed:")),
            "viewer should explain the refused file: {:?}",
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
            browser_input_event(&key, rect, frame, false),
            None,
            "address-bar/chrome keystrokes must not leak into the page"
        );
        assert_eq!(
            browser_input_event(&key, rect, frame, true),
            Some(key),
            "click-focused page canvas receives keyboard events"
        );
        assert_eq!(
            browser_input_event(&egui::Event::Text("mesh".to_owned()), rect, frame, true),
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
        match browser_input_event(&ev, rect, frame, true).expect("focused click forwards") {
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
                true
            ),
            Some(egui::Event::PointerGone)
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

        let page_point = pos2(480.0, 420.0);
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
    fn page_zoom_and_find_actions_send_helper_controls() {
        let (session, helper, _writer) = live_page_session();
        let mut state = WebState::default();
        state.push_session(session);
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
            "Spelling unavailable: hunspell not installed"
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
        assert_eq!(
            unavailable.summary(),
            "Spellcheck unavailable: hunspell not installed"
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

        let job = submit_pdf_to_cups_with_runner(
            &path,
            "Magic Mesh Browser - Example",
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
            vec![
                "-t".to_owned(),
                "Magic Mesh Browser - Example".to_owned(),
                path.to_string_lossy().into_owned(),
            ]
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
    fn cups_print_submission_applies_destination_and_options() {
        let dir = tempfile::tempdir().expect("cups pdf dir");
        let path = dir.path().join("page.pdf");
        std::fs::write(&path, b"%PDF-1.7\n").expect("pdf fixture");
        let settings = CupsPrintSettings {
            destination: Some("Office".to_owned()),
            copies: 3,
            duplex: true,
            grayscale: true,
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
            tab_label(&state.tabs[1]).contains("W "),
            "the tab pill carries the Work marker"
        );
        assert!(
            tab_hover(&state.tabs[1]).contains("Container: Work"),
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
            tab_label(&state.tabs[1]).contains("D2 "),
            "the tab pill carries the Display 2 marker"
        );
        assert!(
            tab_hover(&state.tabs[1]).contains("Display target: Secondary Display"),
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
            tab_label(&state.tabs[0]).contains('\u{25D2}'),
            "suspended tabs wear the idle marker"
        );
        assert!(tab_hover(&state.tabs[0]).contains("Idle suspended"));
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
        let mut state = WebState::default();
        assert!(run_panel(&mut state), "the gated EmptyState drew nothing");
        assert!(state.tabs.is_empty());
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
    fn new_tab_dashboard_renders_for_about_blank() {
        let (session, _helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        assert!(run_until_texture(&mut state));
        assert_eq!(state.tabs[0].session.nav().url, "about:blank");
        assert!(run_panel(&mut state), "new-tab dashboard draws");
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
            session,
            engine: BrowserEngine::Servo,
            container: ContainerProfile::None,
            display_target: DisplayTarget::Current,
            muted: false,
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
            resizer: ViewportResizer::default(),
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
    }

    #[test]
    fn custom_filter_rules_compile_into_open_tabs() {
        use mde_web_preview_client::{ControlMsg, EventMsg, ResourceType};

        let (shell, helper) = UnixStream::pair().expect("socketpair");
        helper.set_nonblocking(true).expect("helper nonblocking");
        let mut state = WebState::default();
        state.push_session(WebSession::from_stream(shell, None).expect("session"));
        state.add_custom_filter_rules("TestCustom", "||ads.custom.test^");

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
    }

    #[test]
    fn reload_on_a_crashed_tab_respawns_it() {
        let (session, helper) = testkit::connect().expect("connect");
        let mut state = WebState::default();
        state.push_session(session);
        run_until_texture(&mut state);
        helper.crash();
        run_panel(&mut state);
        assert!(state.tabs[0].session.is_crashed());

        // The Reload button on a crashed tab requests a respawn; the shell swaps in
        // a fresh session (here a new fake helper) and the new page flows again.
        state.respawn_requested = true;
        assert!(state.take_respawn_request());
        let (fresh, _helper2) = testkit::connect().expect("respawn connect");
        state.respawn_active_with(fresh);
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
            wait_for_fresh_frame(&mut state),
            "clear action loaded about:blank into the helper"
        );
        assert!(
            run_panel(&mut state),
            "new-tab dashboard renders after clear"
        );
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
    fn adfilter_domain_body_matches_the_worker_allow_block_shape() {
        assert_eq!(ACTION_ADFILTER_ALLOW, "action/adfilter/allow");
        assert_eq!(ACTION_ADFILTER_BLOCK, "action/adfilter/block");
        let body = adfilter_domain_body(" news.example.com ");
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["domain"], "news.example.com");
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
    fn browser_send_tab_preview_falls_back_to_the_url_when_the_title_is_blank() {
        let body = browser_send_tab_body(
            BrowserSendTabTarget::Node,
            BrowserEngine::Servo,
            "https://example.com/",
            "   ",
        );
        let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(v["target"], "node");
        assert_eq!(v["target_id"], local_hostname());
        assert_eq!(v["target_label"], local_hostname());
        assert_eq!(v["engine"], "servo");
        assert_eq!(v["title"], "");
        assert_eq!(v["preview"], "https://example.com/");
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
                .is_some_and(|summary| summary
                    .contains("example.test: camera denied; helper default deny remains active")),
            "prompt history should be reflected in the active-site permission summary"
        );
        assert_eq!(
            state.capture_notice.as_deref(),
            Some("Camera prompt denied for example.test; helper default deny remains active")
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
            Some("Chromium DevTools requires a live CEF tab")
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
        assert!(local_dir.join("phone.json").exists());
        assert!(local_dir.join("other.json").exists());
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
                    assert_eq!(v["target_id"], local_hostname());
                    assert_eq!(v["target_label"], local_hostname());
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
            Some("Read aloud: sent page text to TTS")
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
        assert_eq!(
            resources[1]["url"],
            "https://www.google-analytics.com/collect"
        );
        assert_eq!(resources[1]["resource"], "script");
        assert_eq!(resources[1]["allowed"], false);
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
        assert_eq!(read_status.chip_label(), "TTS unavailable");
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
        assert_eq!(read_status.chip_label(), "TTS speaking");
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
    fn browser_passkey_helper_event_publishes_daemon_handoff() {
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
            Some("Passkey: sent ceremony to daemon")
        );
        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
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
        assert_eq!(
            state.pending_passkey_requests.get("mde-pk-test-2"),
            Some(&0usize)
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
        state.gate_notice = Some("helper unavailable".to_owned());
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
        state.address = "https://example.test/current".to_owned();
        run_until_texture(&mut state);
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
            Some("Dictation: sent STT request")
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

        state.set_vertical_tabs(true);
        state.publish_session_snapshot();
        let msgs = persist
            .list_since(ACTION_BROWSER_SESSION_SYNC, None)
            .expect("list browser session sync after change");
        assert_eq!(msgs.len(), 2, "a changed setting emits a new snapshot");
        let latest: serde_json::Value =
            serde_json::from_str(msgs[1].body.as_deref().expect("sync body")).expect("valid JSON");
        assert_eq!(latest["settings"]["vertical_tabs"], true);
    }

    #[test]
    fn browser_capture_success_publishes_notify_feed_event() {
        let bus = tempfile::tempdir().expect("temp bus");
        let mut state = WebState::default().with_bus_root(Some(bus.path().to_path_buf()));
        let path = PathBuf::from("/tmp/mde-browser-capture.png");

        state.record_capture_success("Captured", &path);

        let persist = Persist::open(bus.path().to_path_buf()).expect("open bus");
        let msgs = persist
            .list_since(EVENT_NOTIFY_BROWSER, None)
            .expect("list browser notify events");
        assert_eq!(msgs.len(), 1);
        let body = msgs[0].body.as_deref().expect("notify body");
        let v: serde_json::Value = serde_json::from_str(body).expect("valid JSON");
        assert_eq!(v["severity"], "info");
        assert_eq!(v["source"], "browser");
        assert_eq!(v["summary"], "Captured /tmp/mde-browser-capture.png");
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
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://video.example.test/master.m3u8".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::XmlHttpRequest,
                ),
                allowed: false,
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
        assert_eq!(v["ignore_blocking"], true);
        assert_eq!(v["suggested_filename"], "master.m3u8");
        assert_eq!(v["rename_strategy"], "auto_rename_by_url_hint");
    }

    #[test]
    fn media_asset_request_selection_batches_only_observed_images() {
        let recent = vec![
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/app.js".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Script,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/hero.png".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: true,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://cdn.example.test/photo".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Image,
                ),
                allowed: false,
            },
            mde_web_preview_client::ResourceRequestStatus {
                url: "https://video.example.test/clip.mp4".to_owned(),
                resource: mde_web_preview_client::resource_to_wire(
                    mde_web_preview_client::ResourceType::Media,
                ),
                allowed: true,
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
            notice.contains("not installed"),
            "the absent-helper gate names it honestly: {notice}"
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
            notice.contains("failed to start") && notice.contains("exec denied"),
            "a spawn failure surfaces its reason: {notice}"
        );
        assert!(run_panel(&mut state), "the honest failure notice draws");
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn helper_bin_path_defaults_and_honors_engine_env_overrides() {
        use std::path::PathBuf;
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
            notice.contains("Chromium/CEF runtime") && notice.contains(CEF_LIB_NAME),
            "the CEF runtime gate names the missing library: {notice}"
        );
    }

    #[cfg(feature = "live-helper")]
    #[test]
    fn cef_live_open_uses_the_browser_ui_spawn_path_and_pumps_a_frame() {
        use std::cell::RefCell;
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
    fn cef_live_browser_ui_renders_a_real_site_when_farm_smoke_is_enabled() {
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
            run_until_texture(&mut state),
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
    fn cef_runtime_gate_accepts_the_upstream_bundle_layout() {
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
                let body = b"<!doctype html><html><body><h1>CEF Browser UI live smoke</h1><p>real HTTP page</p></body></html>";
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
}
