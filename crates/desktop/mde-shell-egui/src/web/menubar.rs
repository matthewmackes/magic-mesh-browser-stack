//! The Browser surface's shared **MENUBAR-ALL** top bar (design: `menubar-all.md`).
//!
//! The UPPERCASE `BROWSER` title in the Terminals-group accent, the real
//! `WebSession` menus, and a live status cluster — the one shared
//! `mde_egui::menubar::MenuBar` every surface embeds. **Page** carries the
//! address-bar's open seam; Edit / View / History / Bookmarks bind to the session
//! + page-actions seams the toolbar chrome already drives (§6 glue, no new
//! behaviour). Engine choice lives in the tab strip as explicit `+ Servo` and
//! `+ CEF` buttons. A context-gated item renders **disabled** and an absent
//! capability is **omitted** (§7): no page-text Copy, no keyboard chord table —
//! and BROWSER-DD-8 **Power mode** is a real View toggle that reveals the
//! separate Power menu while keeping unfinished power tools honestly captioned. The
//! status cluster shows the active engine, committed URL, session lifecycle,
//! http/https security state, and ad-filter shield (BOOKMARKS-7). `use super::{…}`
//! pulls in the parent's body builders, engine/state/target types, and action
//! topics. A pure relocation from the `web` god-module.

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
    settings.destination.is_some() || settings.copies > 1 || settings.duplex || settings.grayscale
}

/// One Browser menu action — each maps to a real [`WebSession`]/page seam in
/// [`apply`]. `Copy`, so the menu model stays a plain value tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MenuAction {
    /// Navigate back (`WebSession::go_back`).
    Back,
    /// Navigate forward (`WebSession::go_forward`).
    Forward,
    /// Reopen the most recently closed tab from the session-only reopen stack
    /// (`WebState::restore_closed_tab` — the Ctrl+Shift+T seam).
    ReopenClosedTab,
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
    /// Toggle the BOOKMARKS-BAR horizontal bookmarks bar below the nav chrome.
    ToggleBookmarksBar,
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
    /// The BOOKMARKS-BAR bookmarks bar is shown.
    bookmarks_bar_visible: bool,
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
    /// The session-only reopen stack holds at least one closed tab.
    can_reopen_closed: bool,
    /// Title (or URL) of the most recently closed tab, naming the History →
    /// reopen item.
    last_closed: Option<String>,
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
                bookmarks_bar_visible: state.bookmarks_bar_visible,
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
                // Overwritten below with the rest of the tab-independent state.
                can_reopen_closed: false,
                last_closed: None,
            }
        });
    let (active_downloads, total_downloads) = state.download_counts();
    snap.typed_address = !state.address.trim().is_empty();
    snap.vertical_tabs = state.vertical_tabs;
    snap.page_zoom_percent = state.page_zoom_percent;
    snap.find_open = state.find_open;
    snap.downloads_open = state.downloads_open;
    snap.bookmarks_bar_visible = state.bookmarks_bar_visible;
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
    snap.can_reopen_closed = !state.closed_tabs.is_empty();
    snap.last_closed = state.closed_tabs.last().map(|closed| {
        if closed.title.is_empty() {
            closed.url.clone()
        } else {
            closed.title.clone()
        }
    });
    snap
}

/// The History → reopen item names the tab it would restore ("Reopen
/// “<title>”") when one is retained — the desktop-browser convention — and
/// falls back to the plain verb with an empty stack (where it renders
/// disabled).
fn reopen_closed_label(s: &Snapshot) -> String {
    s.last_closed.as_deref().map_or_else(
        || "Reopen Closed Tab".to_owned(),
        |last| format!("Reopen \u{201C}{}\u{201D}", super::ellipsize(last, 24)),
    )
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
                Entry::Item(Item::new(
                    MenuAction::ToggleBookmarksBar,
                    if s.bookmarks_bar_visible {
                        "Hide Bookmarks Bar"
                    } else {
                        "Show Bookmarks Bar"
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
                    Item::new(MenuAction::SaveOfflineCopy, "Save Offline Copy").enabled(can_tools),
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
                Entry::Item(Item::new(MenuAction::SavePdf, "Save Page as PDF").enabled(can_tools)),
                Entry::Item(
                    Item::new(MenuAction::OpenLastPdf, "Open Last PDF").enabled(s.has_saved_pdf),
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
                Entry::Separator,
                Entry::Item(
                    Item::new(MenuAction::ReopenClosedTab, reopen_closed_label(s))
                        .shortcut("Ctrl+Shift+T")
                        .enabled(s.can_reopen_closed),
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
                Entry::Item(Item::new(MenuAction::AddBookmark, "Add Bookmark").enabled(has_page)),
                Entry::Separator,
                Entry::Item(Item::new(MenuAction::SendInChat, "Send in Chat").enabled(has_page)),
                Entry::Item(Item::new(MenuAction::ShareToPeer, "Share to Peer").enabled(has_page)),
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
                    Item::new(MenuAction::SendTabToPhone, "Send Tab to Phone").enabled(has_page),
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
        MenuAction::ReopenClosedTab => state.restore_closed_tab(),
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
        MenuAction::ToggleBookmarksBar => state.toggle_bookmarks_bar(),
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
            can_reopen_closed: false,
            last_closed: None,
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
                last_error: Some("active CEF runtime does not match packaged manifest".to_owned()),
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
            [
                ("Back".to_owned(), true),
                ("Forward".to_owned(), false),
                ("Reopen Closed Tab".to_owned(), false),
            ]
        );
    }

    #[test]
    fn history_menu_gates_reopen_closed_tab_on_the_session_stack() {
        let reopen = |snap: &Snapshot| {
            build_menus(snap)
                .into_iter()
                .find(|m| m.title == "History")
                .expect("History menu present")
                .entries
                .into_iter()
                .find_map(|e| match e {
                    Entry::Item(i) if i.id == MenuAction::ReopenClosedTab => Some(i),
                    _ => None,
                })
                .expect("Reopen Closed Tab item present")
        };
        // Empty stack → the honest §7 disable with the plain verb.
        let item = reopen(&https_page());
        assert!(!item.enabled, "an empty reopen stack disables the item");
        assert_eq!(item.label, "Reopen Closed Tab");
        assert_eq!(item.shortcut.as_deref(), Some("Ctrl+Shift+T"));

        // A retained closed tab enables the item and names its title.
        let with_stack = Snapshot {
            can_reopen_closed: true,
            last_closed: Some("Example".to_owned()),
            ..https_page()
        };
        let item = reopen(&with_stack);
        assert!(item.enabled, "a retained closed tab enables the reopen");
        assert_eq!(item.label, "Reopen \u{201C}Example\u{201D}");
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
            MenuAction::ToggleBookmarksBar,
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
