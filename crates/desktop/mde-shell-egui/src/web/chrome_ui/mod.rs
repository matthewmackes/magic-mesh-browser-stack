//! Browser-local Chrome-style visual scope.
//!
//! This module is the first slice of the BROWSER-CHROME `web/chrome_ui/`
//! extraction: the Browser gets a local light chrome treatment without changing
//! the helper/session control path. Page pixels still come from the active engine;
//! this scope only affects shell-owned tabs, toolbar, menus, drawers, and the new
//! tab dashboard.

use std::{
    collections::{hash_map::DefaultHasher, BTreeSet},
    hash::{Hash, Hasher},
    ops::Range,
    path::Path,
    time::{Duration, Instant},
};

use mde_egui::egui::{
    self, Color32, FontFamily, FontId, RichText, TextStyle, TextureHandle, TextureOptions,
};
use mde_egui::menubar::Entry;
use mde_egui::{AnimatedScalar, ChipTone, Motion, MotionMode, MotionPreset, Style};
use mde_theme::brand::icons::{icon_image, IconId};
use mde_web_preview_client::{
    confusable_reason, host_of, BeforeUnloadDialog, CertError, ConfusableReason, CursorKind,
    EditCommand, JsDialog, MediaTransportAction, SessionState,
};

mod accessibility;
mod body;
mod drawers;
#[cfg(test)]
use super::BrowserSecurityUpdateStatus;
use super::{
    browser_capture_dir, ellipsize, is_new_tab_url, media_metadata_chip_label, BrowserEngine,
    BrowserOfflineCacheResult, BrowserReadAloudStatus, BrowserVoiceCommandStatus, ContainerProfile,
    DeviceProfile, DisplayTarget, FaviconCache, ManagedPolicyBlock, PendingPasskeyConsent,
    PixelRegion, Tab, UserAgentOverride, WebState, CHROME_BUTTON, CHROME_FONT, CHROME_GAP,
    CHROME_NEW_TAB_W, CHROME_OMNIBOX_H, CHROME_TAB_CLOSE, CHROME_TAB_H, CHROME_TAB_MIN_W,
    CHROME_TAB_PINNED_W, CHROME_TAB_RAIL_W, CHROME_TAB_W, MAX_CHANNEL_DIM, PRIVATE_MODE_EXPLAINER,
    RESIZE_DEBOUNCE,
};
use accessibility::install_browser_page_accessibility;
use drawers::{
    downloads_drawer, history_drawer, notifications_drawer, offline_cache_drawer,
    print_settings_drawer, qr_share_drawer, recommendations_drawer, security_update_drawer,
    site_styles_drawer, speech_status_drawer, spellcheck_drawer, translation_drawer,
};

pub(super) fn install_browser_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    state: &WebState,
) {
    accessibility::install_browser_accessibility(ctx, rect, state);
}

pub(super) fn centered(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    accessibility::centered(ui, content);
}

pub(super) fn active_body(ui: &mut egui::Ui, state: &mut WebState) {
    scope(ui, |ui| body::active_body(ui, state));
}

pub(super) const BROWSER_NO_LIVE_PAGE_NOTICE: &str =
    "No live browser page is available on this device";

/// The browser-reserved tab accelerators (Chrome's tab-strip keyboard UX),
/// live only while the Browser surface is painted — this runs from
/// [`super::web_panel`].
///
/// Every match CONSUMES the key from this frame's input, so a reserved
/// shortcut never leaks into chrome widgets or the page-canvas forwarding at
/// the bottom of the frame (`paint_body` clones `i.events` after this ran) —
/// the same reservation Chrome makes for Ctrl+T/W. Page-canvas keyboard focus
/// deliberately does NOT pause these: tab management stays reachable while
/// page typing is forwarded.
///
/// Tab-opening/closing accelerators (Ctrl+T / Ctrl+W / Ctrl+Shift+T) pause
/// while a chrome text field (omnibox / find bar / dashboard search) owned
/// keyboard focus on the last painted frame — closing the tab out of an
/// in-progress edit would surprise, and egui's own TextEdit binds Ctrl+W as
/// delete-previous-word, which the pause preserves. Tab CYCLING
/// (Ctrl+Tab / Ctrl+digit) stays
/// live during edits, exactly like desktop browsers — and deliberately so:
/// egui's own focus traversal walks widget focus on Tab presses, so a cycling
/// shortcut gated on text focus could dead-end itself once that walk reaches
/// the omnibox.
pub(super) fn handle_tab_keyboard(ctx: &egui::Context, state: &mut WebState) {
    const CTRL: egui::Modifiers = egui::Modifiers::CTRL;
    const CTRL_SHIFT: egui::Modifiers = egui::Modifiers::CTRL.plus(egui::Modifiers::SHIFT);

    // F11 toggles immersive/fullscreen mode; Esc leaves it. Handled before the
    // edit-focus gate so the immersive view is always escapable, even mid-typing.
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F11)) {
        state.fullscreen = !state.fullscreen;
    }
    if state.fullscreen
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        state.fullscreen = false;
    }

    // ORDER MATTERS: `consume_key` matches modifiers logically (an EXTRA
    // Shift is ignored — egui's documented behaviour), so the Ctrl+Shift
    // variants must be consumed before their plain-Ctrl counterparts or
    // Ctrl+Shift+T would trigger "new tab" instead of "reopen".
    if !state.chrome_edit_focus {
        if ctx.input_mut(|i| i.consume_key(CTRL_SHIFT, egui::Key::T)) {
            state.restore_closed_tab();
        }
        if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::T)) {
            state.request_new_tab(state.engine);
        }
        if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::W)) {
            state.close_tab(state.active);
        }
    }

    if ctx.input_mut(|i| i.consume_key(CTRL_SHIFT, egui::Key::Tab)) {
        state.select_prev_tab();
    }
    if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::Tab)) {
        state.select_next_tab();
    }

    // Ctrl+1..Ctrl+8 activate the Nth tab (out-of-range is ignored by
    // `select_tab`); Ctrl+9 activates the LAST tab — the Chrome convention.
    const DIGITS: [egui::Key; 8] = [
        egui::Key::Num1,
        egui::Key::Num2,
        egui::Key::Num3,
        egui::Key::Num4,
        egui::Key::Num5,
        egui::Key::Num6,
        egui::Key::Num7,
        egui::Key::Num8,
    ];
    for (nth, key) in DIGITS.into_iter().enumerate() {
        if ctx.input_mut(|i| i.consume_key(CTRL, key)) {
            state.select_tab(nth);
        }
    }
    if ctx.input_mut(|i| i.consume_key(CTRL, egui::Key::Num9)) {
        if let Some(last) = state.tabs.len().checked_sub(1) {
            state.select_tab(last);
        }
    }
}

/// Browser chrome uses the same Inter proportional UI face as the rest of
/// Construct. Chrome-inspired colour and layout stay local to this module; the font
/// is no longer a Roboto-only exception.
pub(super) fn chrome_font_family() -> FontFamily {
    FontFamily::Proportional
}

// Chromium/Chrome Refresh light roles, mirrored as local egui tokens so every
// Browser surface can stay on the stock Chrome palette instead of inheriting
// the darker shell chrome.
pub(super) const CHROME_TOOLBAR: Color32 = Color32::from_rgb(255, 255, 255);
pub(super) const CHROME_SURFACE: Color32 = Color32::from_rgb(255, 255, 255);
pub(super) const CHROME_SURFACE_CONTAINER: Color32 = Color32::from_rgb(237, 242, 252);
pub(super) const CHROME_SURFACE_CONTAINER_HIGH: Color32 = Color32::from_rgb(232, 234, 242);
pub(super) const CHROME_PRIMARY: Color32 = Color32::from_rgb(11, 87, 208);
pub(super) const CHROME_PRIMARY_CONTAINER: Color32 = Color32::from_rgb(211, 227, 253);
pub(super) const CHROME_ON_PRIMARY_CONTAINER: Color32 = Color32::from_rgb(4, 30, 73);
pub(super) const CHROME_SUCCESS_CONTAINER: Color32 = Color32::from_rgb(196, 238, 208);
pub(super) const CHROME_ON_SUCCESS_CONTAINER: Color32 = Color32::from_rgb(8, 65, 30);
pub(super) const CHROME_OUTLINE: Color32 = Color32::from_rgb(218, 220, 224);
pub(super) const CHROME_TEXT: Color32 = Color32::from_rgb(31, 31, 31);
pub(super) const CHROME_ICON: Color32 = Color32::from_rgb(95, 99, 104);
pub(super) const CHROME_TEXT_DIM: Color32 = CHROME_ICON;
pub(super) const CHROME_SUCCESS: Color32 = Color32::from_rgb(20, 108, 46);
pub(super) const CHROME_WARN: Color32 = Color32::from_rgb(177, 91, 0);
pub(super) const CHROME_ERROR: Color32 = Color32::from_rgb(179, 38, 30);
pub(super) const CHROME_GROUP_BLUE: Color32 = CHROME_PRIMARY;
pub(super) const CHROME_GROUP_GREEN: Color32 = Color32::from_rgb(24, 128, 56);
pub(super) const CHROME_GROUP_AMBER: Color32 = Color32::from_rgb(245, 124, 0);
pub(super) const CHROME_GROUP_RED: Color32 = CHROME_ERROR;
pub(super) const CHROME_GROUP_PURPLE: Color32 = Color32::from_rgb(132, 48, 206);

const STATE_HOVER_ALPHA: u8 = 20;
const STATE_FOCUS_ALPHA: u8 = 26;
const STATE_PRESSED_ALPHA: u8 = 26;
const CHROME_TAB_RADIUS: f32 = 8.0;
const TAB_FAVICON_SIZE: f32 = 16.0;
const TAB_ENGINE_BADGE_H: f32 = 16.0;
const TAB_STATUS_CHIP: f32 = 14.0;
const TAB_STATUS_CHIP_GAP: f32 = 2.0;
const ACTION_BUTTON_RADIUS: f32 = 8.0;
const ICON_BUTTON_RADIUS: f32 = 8.0;
const ENGINE_TOOLBAR_CHIP_W: f32 = 42.0;
const ENGINE_TOOLBAR_BADGE: f32 = 14.0;
const NEW_TAB_TYPE_BUTTON_W: f32 = 50.0;
const DASHBOARD_PANEL_W: f32 = 680.0;
const DASHBOARD_PANEL_MIN_W: f32 = 220.0;
const DASHBOARD_SEARCH_W: f32 = 600.0;
const DASHBOARD_SEARCH_MIN_W: f32 = 180.0;
const DASHBOARD_SEARCH_H: f32 = 54.0;
const DASHBOARD_SEARCH_SUBMIT: f32 = 36.0;
const DASHBOARD_TILE_H: f32 = 78.0;
const DASHBOARD_TILE_MIN_W: f32 = 132.0;
const DASHBOARD_TILE_MAX_W: f32 = 156.0;
const DASHBOARD_TILE_ICON: f32 = 32.0;
const CHROME_TOOLTIP_W: f32 = 260.0;
const CHROME_POPUP_INNER_MARGIN_X: f32 = 6.0;
const CHROME_OPTIONS_CARD_MARGIN_X: i8 = 6;
const CHROME_OPTIONS_CARD_MARGIN_Y: i8 = 4;
const DASHBOARD_SEARCH_MARGIN_X: i8 = 12;
const DASHBOARD_SEARCH_MARGIN_Y: i8 = 4;
const CHROME_PROMPT_MARGIN_X: i8 = 6;
const CHROME_PROMPT_MARGIN_Y: i8 = 4;
const PAGE_ACTIONS_MENU_W: f32 = 260.0;
const BOOKMARK_OVERFLOW_MENU_W: f32 = 240.0;
const PASSWORD_MENU_W: f32 = 280.0;
const SITE_INFO_POPUP_W: f32 = 300.0;
const PASSWORD_FIELD_MIN_W: f32 = 160.0;
const OMNIBOX_FONT: f32 = 15.5;
const OMNIBOX_SECURITY_SLOT_W: f32 = 34.0;
const OMNIBOX_TEXT_TINY_MIN: f32 = 36.0;
const TOOLBAR_COUNT_BADGE_W: f32 = 28.0;
const TOOLBAR_COUNT_BADGE_H: f32 = 16.0;
const CHROME_SEPARATOR_H: f32 = 9.0;
const OPTION_ROW_H: f32 = 32.0;
const OPTION_ICON_SIZE: f32 = 19.0;
const OPTION_ROW_MAX_W: f32 = 760.0;
const NAV_FULL_OMNIBOX_FLOOR: f32 = 360.0;
const NAV_FULL_OMNIBOX_MIN: f32 = 220.0;
const NAV_COMPACT_OMNIBOX_MIN: f32 = 180.0;
const NAV_OMNIBOX_TINY_MIN: f32 = 96.0;
const OPTIONS_RAIL_W: f32 = 154.0;
const OPTIONS_WIDE_GAP: f32 = 10.0;
const OPTIONS_CONTENT_MIN_W: f32 = 420.0;
const OPTIONS_CATEGORY_CHIP_H: f32 = 26.0;
const OPTIONS_CATEGORY_CHIP_MIN_W: f32 = 88.0;
const OPTIONS_CATEGORY_CHIP_MAX_W: f32 = 172.0;
const OPTIONS_COMPACT_BREAKPOINT: f32 =
    OPTIONS_RAIL_W + OPTIONS_WIDE_GAP + OPTIONS_CONTENT_MIN_W + 36.0;
const TAB_SEARCH_PANEL_W: f32 = 320.0;
const TAB_SEARCH_PANEL_MIN_W: f32 = 220.0;
const TAB_SEARCH_EDIT_MIN_W: f32 = 72.0;
const SUGGESTIONS_LEADING_INSET: f32 = CHROME_BUTTON + CHROME_GAP;
const MEDIA_CLUSTER_LABEL_W: f32 = 132.0;
const MEDIA_CLUSTER_COMPACT_LABEL_W: f32 = 84.0;
const MEDIA_PIP_W: f32 = 272.0;
const MEDIA_PIP_VIDEO_H: f32 = 150.0;
const MEDIA_PIP_MARGIN: f32 = 14.0;
const TAB_GROUP_PALETTE: [Color32; 5] = [
    CHROME_GROUP_BLUE,
    CHROME_GROUP_GREEN,
    CHROME_GROUP_AMBER,
    CHROME_GROUP_RED,
    CHROME_GROUP_PURPLE,
];

/// The fixed slot width of one bookmark button on the single-row bar. Fixed so the
/// overflow split ([`bookmark_bar_visible_count`]) is exact — no font measuring.
pub(super) const BOOKMARK_BTN_W: f32 = 132.0;
/// The width reserved for the overflow menu button when the row can't hold
/// every bookmark.
pub(super) const BOOKMARK_OVERFLOW_W: f32 = 26.0;
/// The elision budget for a bookmark button's title (fits inside [`BOOKMARK_BTN_W`]
/// at [`CHROME_FONT`]); the full title rides the hover tooltip.
const BOOKMARK_TITLE_CHARS: usize = 18;
const TAB_DRAG_SETTLE_TRAVEL: f32 = 7.0;

pub(super) const fn button_text(enabled: bool) -> Color32 {
    if enabled {
        CHROME_TEXT
    } else {
        CHROME_TEXT_DIM
    }
}

pub(super) const fn tab_text(active: bool) -> Color32 {
    if active {
        CHROME_TEXT
    } else {
        CHROME_TEXT_DIM
    }
}

pub(super) const fn selected_text(selected: bool) -> Color32 {
    if selected {
        CHROME_ON_PRIMARY_CONTAINER
    } else {
        CHROME_TEXT
    }
}

pub(super) const fn page_action_icon_color(has_page: bool, is_bookmarked: bool) -> Color32 {
    match (has_page, is_bookmarked) {
        (false, _) => CHROME_TEXT_DIM,
        (true, true) => CHROME_PRIMARY,
        (true, false) => CHROME_ICON,
    }
}

pub(super) const fn tab_stroke(active: bool) -> Color32 {
    if active {
        CHROME_OUTLINE
    } else {
        CHROME_SURFACE_CONTAINER_HIGH
    }
}

pub(super) const fn row_fill(selected: bool) -> Color32 {
    if selected {
        CHROME_PRIMARY_CONTAINER
    } else {
        CHROME_TOOLBAR
    }
}

pub(super) const fn menu_item_fill(selected: bool) -> Color32 {
    if selected {
        CHROME_PRIMARY_CONTAINER
    } else {
        CHROME_TOOLBAR
    }
}

pub(super) const fn prompt_fill() -> Color32 {
    CHROME_PRIMARY_CONTAINER
}

pub(super) const fn tone_color(tone: ChipTone) -> Color32 {
    match tone {
        ChipTone::Ok => CHROME_SUCCESS,
        ChipTone::Warn | ChipTone::Danger => CHROME_WARN,
        ChipTone::Info => CHROME_PRIMARY,
        ChipTone::Neutral => CHROME_TEXT_DIM,
    }
}

pub(super) const fn engine_display_name(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "Chromium",
        BrowserEngine::Servo => "Lightweight",
    }
}

#[cfg(test)]
pub(super) const fn engine_marker(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "CEF",
        BrowserEngine::Servo => "Servo",
    }
}

pub(super) const fn engine_glyph(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "C",
        BrowserEngine::Servo => "S",
    }
}

pub(super) const fn engine_accent(engine: BrowserEngine) -> Color32 {
    match engine {
        BrowserEngine::Cef => CHROME_PRIMARY,
        BrowserEngine::Servo => CHROME_SUCCESS,
    }
}

pub(super) const fn engine_container(engine: BrowserEngine) -> Color32 {
    match engine {
        BrowserEngine::Cef => CHROME_PRIMARY_CONTAINER,
        BrowserEngine::Servo => CHROME_SUCCESS_CONTAINER,
    }
}

pub(super) const fn engine_on_container(engine: BrowserEngine) -> Color32 {
    match engine {
        BrowserEngine::Cef => CHROME_ON_PRIMARY_CONTAINER,
        BrowserEngine::Servo => CHROME_ON_SUCCESS_CONTAINER,
    }
}

pub(super) const fn download_state_color(
    state: mde_files_egui::transfers::TransferState,
) -> Color32 {
    match state {
        mde_files_egui::transfers::TransferState::Done => CHROME_SUCCESS,
        mde_files_egui::transfers::TransferState::Failed => CHROME_ERROR,
        mde_files_egui::transfers::TransferState::Paused => CHROME_WARN,
        mde_files_egui::transfers::TransferState::Queued
        | mde_files_egui::transfers::TransferState::Running => CHROME_TEXT_DIM,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserActionRole {
    Primary,
    Secondary,
    Warning,
    Quiet,
}

pub(super) const fn action_button_fill(role: BrowserActionRole) -> Color32 {
    match role {
        BrowserActionRole::Primary => CHROME_PRIMARY,
        BrowserActionRole::Secondary | BrowserActionRole::Quiet => CHROME_TOOLBAR,
        BrowserActionRole::Warning => CHROME_WARN,
    }
}

pub(super) const fn action_button_text(role: BrowserActionRole) -> Color32 {
    match role {
        BrowserActionRole::Primary | BrowserActionRole::Warning => CHROME_TOOLBAR,
        BrowserActionRole::Secondary => CHROME_TEXT,
        BrowserActionRole::Quiet => CHROME_TEXT_DIM,
    }
}

pub(super) const fn action_button_stroke(role: BrowserActionRole) -> Color32 {
    match role {
        BrowserActionRole::Primary => CHROME_PRIMARY,
        BrowserActionRole::Secondary | BrowserActionRole::Quiet => CHROME_OUTLINE,
        BrowserActionRole::Warning => CHROME_WARN,
    }
}

pub(super) const fn tab_group_color(index: usize) -> Color32 {
    TAB_GROUP_PALETTE[index % TAB_GROUP_PALETTE.len()]
}

pub(super) const fn page_backdrop_fill() -> Color32 {
    CHROME_SURFACE_CONTAINER
}

pub(super) fn browser_muted_note(ui: &mut egui::Ui, msg: &str) {
    ui.label(RichText::new(msg).size(Style::SMALL).color(CHROME_TEXT_DIM));
}

fn browser_status_note(ui: &mut egui::Ui, icon: ChromeIcon, msg: &str, color: Color32) {
    ui.horizontal_wrapped(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
        paint_chrome_icon(ui.painter(), rect, icon, color);
        ui.label(RichText::new(msg).size(Style::SMALL).color(color));
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChromeIcon {
    Back,
    Forward,
    Reload,
    Stop,
    Options,
    Downloads,
    Capture,
    Bookmark,
    Security,
    Warning,
    Search,
    Close,
    ZoomIn,
    ZoomOut,
    Print,
    Privacy,
    History,
    Tabs,
    Engine,
    NewTab,
    Up,
    Down,
    Check,
    Page,
    Edit,
    View,
    Power,
    Share,
    Find,
    Audio,
    Play,
    Pause,
    MediaStop,
    Previous,
    Next,
    Minus,
    Plus,
    VolumeDown,
    VolumeOff,
    VolumeUp,
    PictureInPicture,
    DarkMode,
    Lock,
    Notifications,
    Recommend,
}

#[cfg(test)]
pub(super) const REQUIRED_BROWSER_ICONS: &[ChromeIcon] = &[
    ChromeIcon::Back,
    ChromeIcon::Forward,
    ChromeIcon::Reload,
    ChromeIcon::Stop,
    ChromeIcon::Options,
    ChromeIcon::Downloads,
    ChromeIcon::Capture,
    ChromeIcon::Bookmark,
    ChromeIcon::Security,
    ChromeIcon::Warning,
    ChromeIcon::Search,
    ChromeIcon::Close,
    ChromeIcon::ZoomIn,
    ChromeIcon::ZoomOut,
    ChromeIcon::Print,
    ChromeIcon::Privacy,
    ChromeIcon::History,
    ChromeIcon::Tabs,
    ChromeIcon::Engine,
    ChromeIcon::Play,
    ChromeIcon::Pause,
    ChromeIcon::MediaStop,
    ChromeIcon::Previous,
    ChromeIcon::Next,
    ChromeIcon::Minus,
    ChromeIcon::Plus,
    ChromeIcon::VolumeDown,
    ChromeIcon::VolumeOff,
    ChromeIcon::VolumeUp,
    ChromeIcon::PictureInPicture,
];

#[cfg(test)]
const ALL_BROWSER_ICONS: &[ChromeIcon] = &[
    ChromeIcon::Back,
    ChromeIcon::Forward,
    ChromeIcon::Reload,
    ChromeIcon::Stop,
    ChromeIcon::Options,
    ChromeIcon::Downloads,
    ChromeIcon::Capture,
    ChromeIcon::Bookmark,
    ChromeIcon::Security,
    ChromeIcon::Warning,
    ChromeIcon::Search,
    ChromeIcon::Close,
    ChromeIcon::ZoomIn,
    ChromeIcon::ZoomOut,
    ChromeIcon::Print,
    ChromeIcon::Privacy,
    ChromeIcon::History,
    ChromeIcon::Tabs,
    ChromeIcon::Engine,
    ChromeIcon::NewTab,
    ChromeIcon::Up,
    ChromeIcon::Down,
    ChromeIcon::Check,
    ChromeIcon::Page,
    ChromeIcon::Edit,
    ChromeIcon::View,
    ChromeIcon::Power,
    ChromeIcon::Share,
    ChromeIcon::Find,
    ChromeIcon::Audio,
    ChromeIcon::Play,
    ChromeIcon::Pause,
    ChromeIcon::MediaStop,
    ChromeIcon::Previous,
    ChromeIcon::Next,
    ChromeIcon::Minus,
    ChromeIcon::Plus,
    ChromeIcon::VolumeDown,
    ChromeIcon::VolumeOff,
    ChromeIcon::VolumeUp,
    ChromeIcon::PictureInPicture,
    ChromeIcon::DarkMode,
    ChromeIcon::Lock,
    ChromeIcon::Notifications,
    ChromeIcon::Recommend,
];

#[cfg(test)]
pub(super) const LOADING_GLOBE_SHAPE_COUNT: usize = 8;

#[cfg(test)]
pub(super) const fn chrome_icon_painted_shape_count(icon: ChromeIcon) -> usize {
    match icon {
        ChromeIcon::Back
        | ChromeIcon::Forward
        | ChromeIcon::Check
        | ChromeIcon::Close
        | ChromeIcon::NewTab
        | ChromeIcon::Up
        | ChromeIcon::Down
        | ChromeIcon::ZoomIn
        | ChromeIcon::ZoomOut
        | ChromeIcon::Plus
        | ChromeIcon::Pause
        | ChromeIcon::Previous
        | ChromeIcon::Next => 2,
        ChromeIcon::Stop
        | ChromeIcon::Options
        | ChromeIcon::Search
        | ChromeIcon::Warning
        | ChromeIcon::Security
        | ChromeIcon::Bookmark
        | ChromeIcon::Page
        | ChromeIcon::Edit
        | ChromeIcon::View
        | ChromeIcon::Power
        | ChromeIcon::Share
        | ChromeIcon::Find
        | ChromeIcon::Audio
        | ChromeIcon::Play
        | ChromeIcon::VolumeOff
        | ChromeIcon::VolumeDown
        | ChromeIcon::VolumeUp
        | ChromeIcon::PictureInPicture
        | ChromeIcon::Notifications
        | ChromeIcon::Recommend
        | ChromeIcon::DarkMode => 3,
        ChromeIcon::Minus => 1,
        ChromeIcon::Lock | ChromeIcon::MediaStop => 4,
        ChromeIcon::Reload
        | ChromeIcon::Downloads
        | ChromeIcon::Capture
        | ChromeIcon::Print
        | ChromeIcon::Privacy
        | ChromeIcon::History
        | ChromeIcon::Tabs
        | ChromeIcon::Engine => 4,
    }
}

const fn chrome_icon_yamis_id(icon: ChromeIcon) -> Option<IconId> {
    match icon {
        ChromeIcon::Back => Some(IconId::ArrowLeft),
        ChromeIcon::Forward => Some(IconId::ArrowRight),
        ChromeIcon::Options => Some(IconId::Menu),
        ChromeIcon::Downloads => Some(IconId::Downloads),
        ChromeIcon::Capture => Some(IconId::Capture),
        ChromeIcon::Bookmark => Some(IconId::Bookmarks),
        ChromeIcon::Security | ChromeIcon::Privacy => Some(IconId::Security),
        ChromeIcon::Warning => Some(IconId::Warning),
        ChromeIcon::Search | ChromeIcon::Find => Some(IconId::Search),
        ChromeIcon::Close => Some(IconId::Close),
        ChromeIcon::Reload => Some(IconId::Reload),
        ChromeIcon::Stop => Some(IconId::Cancel),
        ChromeIcon::Print => Some(IconId::Print),
        ChromeIcon::History => Some(IconId::History),
        ChromeIcon::Tabs => Some(IconId::Tabs),
        ChromeIcon::Engine => Some(IconId::Internet),
        ChromeIcon::NewTab => Some(IconId::NewTab),
        ChromeIcon::Plus => Some(IconId::Add),
        ChromeIcon::Up => Some(IconId::ChevronUp),
        ChromeIcon::Down => Some(IconId::ArrowDown),
        ChromeIcon::Check => Some(IconId::Check),
        ChromeIcon::Page => Some(IconId::Page),
        ChromeIcon::Edit => Some(IconId::TextEdit),
        ChromeIcon::View => Some(IconId::View),
        ChromeIcon::Power => Some(IconId::Power),
        ChromeIcon::Share => Some(IconId::Share),
        ChromeIcon::Audio => Some(IconId::Audio),
        ChromeIcon::Play => Some(IconId::Play),
        ChromeIcon::Pause => Some(IconId::Pause),
        ChromeIcon::MediaStop => Some(IconId::MediaStop),
        ChromeIcon::Previous => Some(IconId::Previous),
        ChromeIcon::Next => Some(IconId::Next),
        ChromeIcon::Minus => Some(IconId::Remove),
        ChromeIcon::ZoomIn => Some(IconId::ZoomIn),
        ChromeIcon::ZoomOut => Some(IconId::ZoomOut),
        ChromeIcon::VolumeDown => Some(IconId::VolumeLow),
        ChromeIcon::VolumeOff => Some(IconId::VolumeMuted),
        ChromeIcon::VolumeUp => Some(IconId::Volume),
        ChromeIcon::PictureInPicture => Some(IconId::PictureInPicture),
        ChromeIcon::DarkMode => Some(IconId::DarkMode),
        ChromeIcon::Lock => Some(IconId::Lock),
        ChromeIcon::Notifications => Some(IconId::Notifications),
        // No YAMIS "recommend" glyph — paints the local hand-vector star fallback.
        ChromeIcon::Recommend => None,
    }
}

/// The **Mackes-Carbon** glyph name each browser chrome icon renders through —
/// the canonical platform icon set (freedesktop Icon-Naming-Spec names, served
/// by the shared `mde_egui::carbon` loader). This is the mapping the icon-set
/// foundation proves out: every one of the 45 [`ChromeIcon`] variants resolves
/// to an embedded Carbon glyph, so `paint_chrome_icon` renders a real tinted SVG
/// rather than a hand-rolled procedural draw.
const fn chrome_icon_carbon_name(icon: ChromeIcon) -> &'static str {
    match icon {
        ChromeIcon::Back => "go-previous",
        ChromeIcon::Forward => "go-next",
        ChromeIcon::Reload => "view-refresh",
        ChromeIcon::Stop => "process-stop",
        ChromeIcon::Options => "open-menu",
        ChromeIcon::Downloads => "download",
        ChromeIcon::Capture => "camera-photo",
        ChromeIcon::Bookmark => "bookmark-new",
        ChromeIcon::Security => "security-high",
        ChromeIcon::Warning => "dialog-warning",
        ChromeIcon::Search => "system-search",
        ChromeIcon::Close => "window-close",
        ChromeIcon::ZoomIn => "zoom-in",
        ChromeIcon::ZoomOut => "zoom-out",
        ChromeIcon::Print => "document-print",
        // Privacy reads as a "prevent/blocked" padlock, distinct from the
        // Security shield (`security-high`) and the Lock screen glyph.
        ChromeIcon::Privacy => "changes-prevent",
        ChromeIcon::History => "document-open-recent",
        ChromeIcon::Tabs => "view-grid",
        ChromeIcon::Engine => "globe",
        ChromeIcon::NewTab => "new-tab",
        ChromeIcon::Up => "go-up",
        ChromeIcon::Down => "go-down",
        ChromeIcon::Check => "emblem-ok",
        ChromeIcon::Page => "text-x-generic",
        ChromeIcon::Edit => "document-edit",
        ChromeIcon::View => "view",
        ChromeIcon::Power => "system-shutdown",
        ChromeIcon::Share => "share",
        ChromeIcon::Find => "edit-find",
        ChromeIcon::Audio => "audio-volume-high",
        ChromeIcon::Play => "media-playback-start",
        ChromeIcon::Pause => "media-playback-pause",
        ChromeIcon::MediaStop => "media-playback-stop",
        ChromeIcon::Previous => "media-skip-backward",
        ChromeIcon::Next => "media-skip-forward",
        ChromeIcon::Minus => "list-remove",
        ChromeIcon::Plus => "list-add",
        ChromeIcon::VolumeDown => "audio-volume-low",
        ChromeIcon::VolumeOff => "audio-volume-muted",
        ChromeIcon::VolumeUp => "audio-volume-high",
        // carbon-map: closest Carbon glyph. The set has no dedicated
        // picture-in-picture mark; `overlay` (two overlapping frames) reads as a
        // video floating over the page, the PiP gesture.
        ChromeIcon::PictureInPicture => "overlay",
        ChromeIcon::DarkMode => "weather-clear-night",
        ChromeIcon::Lock => "system-lock-screen",
        ChromeIcon::Notifications => "notification",
        ChromeIcon::Recommend => "star",
    }
}

fn action_button(label: impl Into<String>, role: BrowserActionRole) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(label.into())
            .size(CHROME_FONT)
            .color(action_button_text(role)),
    )
    .fill(action_button_fill(role))
    .stroke(egui::Stroke::new(1.0, action_button_stroke(role)))
    .corner_radius(ACTION_BUTTON_RADIUS)
    .min_size(egui::vec2(72.0, CHROME_BUTTON))
}

fn chrome_tooltip_frame<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    apply_visuals(ui);
    egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(8.0)
        .inner_margin(Style::tooltip_margin())
        .show(ui, |ui| {
            ui.set_max_width(CHROME_TOOLTIP_W);
            add_contents(ui)
        })
        .inner
}

pub(super) fn chrome_tooltip(ui: &mut egui::Ui, text: &str) {
    chrome_tooltip_frame(ui, |ui| {
        ui.add(egui::Label::new(RichText::new(text).size(Style::SMALL).color(CHROME_TEXT)).wrap());
    });
}

pub(super) fn chrome_hover_text(
    response: egui::Response,
    text: impl Into<String>,
) -> egui::Response {
    let text = text.into();
    response.on_hover_ui(move |ui| chrome_tooltip(ui, text.as_str()))
}

fn chrome_context_menu(response: &egui::Response, add_contents: impl FnOnce(&mut egui::Ui)) {
    let previous_style = response.ctx.style();
    let mut chrome_style = (*previous_style).clone();
    apply_chrome_style(&mut chrome_style);
    response.ctx.set_style(chrome_style);
    let _ = response.context_menu(|ui| {
        apply_visuals(ui);
        add_contents(ui);
    });
    response.ctx.set_style(previous_style);
}

fn chrome_popup_frame<R>(
    ui: &mut egui::Ui,
    width: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    apply_visuals(ui);
    let width = chrome_popup_width(ui, width);
    egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(
            CHROME_POPUP_INNER_MARGIN_X as i8,
            6,
        ))
        .show(ui, |ui| {
            ui.set_min_width(width);
            ui.set_width(width);
            add_contents(ui)
        })
        .inner
}

fn chrome_options_card_margin() -> egui::Margin {
    egui::Margin::symmetric(CHROME_OPTIONS_CARD_MARGIN_X, CHROME_OPTIONS_CARD_MARGIN_Y)
}

fn dashboard_search_margin() -> egui::Margin {
    egui::Margin::symmetric(DASHBOARD_SEARCH_MARGIN_X, DASHBOARD_SEARCH_MARGIN_Y)
}

fn chrome_prompt_margin() -> egui::Margin {
    egui::Margin::symmetric(CHROME_PROMPT_MARGIN_X, CHROME_PROMPT_MARGIN_Y)
}

fn chrome_popup_width(ui: &egui::Ui, preferred_width: f32) -> f32 {
    chrome_popup_width_from_bounds(
        bounded_available_width(ui),
        ui.clip_rect().width(),
        preferred_width,
    )
}

fn reserve_toolbar_popup_width(ui: &mut egui::Ui, preferred_width: f32) {
    let width = chrome_popup_width_from_bounds(0.0, ui.clip_rect().width(), preferred_width);
    ui.set_min_width(width);
    ui.set_width(width);
}

fn chrome_popup_width_from_bounds(
    bounded_available_width: f32,
    clip_width: f32,
    preferred_width: f32,
) -> f32 {
    let preferred_width = preferred_width.max(1.0);
    let bounded_available_width = bounded_available_width.max(0.0);
    let envelope_width = if bounded_available_width > 1.0 {
        bounded_available_width
    } else {
        clip_width.max(0.0)
    };
    if envelope_width > 1.0 {
        preferred_width.min(envelope_width).max(1.0)
    } else {
        preferred_width
    }
}

fn downloads_toolbar_tip(active: usize, total: usize) -> String {
    if total == 0 {
        "Downloads".to_owned()
    } else if active > 0 && active < total {
        format!("Downloads: {active} active / {total} total")
    } else if active > 0 {
        format!("Downloads: {active} active")
    } else if total == 1 {
        "Downloads: 1 complete".to_owned()
    } else {
        format!("Downloads: {total} complete")
    }
}

fn toolbar_count_badge_text(count: u64) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_owned()),
    }
}

fn toolbar_count_badge_width(count: u64) -> f32 {
    if count == 0 {
        0.0
    } else {
        TOOLBAR_COUNT_BADGE_W
    }
}

fn download_count_badge_reserve(active: usize) -> f32 {
    let badge_w = toolbar_count_badge_width(active as u64);
    if badge_w == 0.0 {
        0.0
    } else {
        badge_w + CHROME_GAP
    }
}

fn download_count_badge(ui: &mut egui::Ui, active: usize, tip: &str) {
    toolbar_count_badge(ui, active as u64, tip);
}

fn toolbar_count_badge(ui: &mut egui::Ui, count: u64, tip: &str) {
    let Some(text) = toolbar_count_badge_text(count) else {
        return;
    };
    let (slot, response) = ui.allocate_exact_size(
        egui::vec2(TOOLBAR_COUNT_BADGE_W, CHROME_BUTTON),
        egui::Sense::hover(),
    );
    let rect = egui::Rect::from_center_size(
        slot.center(),
        egui::vec2(TOOLBAR_COUNT_BADGE_W, TOOLBAR_COUNT_BADGE_H),
    );
    ui.painter()
        .rect_filled(rect, TOOLBAR_COUNT_BADGE_H * 0.5, CHROME_PRIMARY_CONTAINER);
    ui.painter().rect_stroke(
        rect,
        TOOLBAR_COUNT_BADGE_H * 0.5,
        egui::Stroke::new(1.0, CHROME_PRIMARY.gamma_multiply(0.35)),
        egui::StrokeKind::Inside,
    );
    let galley =
        ui.fonts(|fonts| fonts.layout_no_wrap(text, font_id(10.0), CHROME_ON_PRIMARY_CONTAINER));
    ui.painter().galley(
        rect.center() - galley.size() * 0.5,
        galley,
        CHROME_ON_PRIMARY_CONTAINER,
    );
    let _ = chrome_hover_text(response, tip);
}

fn action_icon_button(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    role: BrowserActionRole,
    tip: &str,
    min_size: egui::Vec2,
) -> egui::Response {
    let enabled = ui.is_enabled();
    let (rect, response) = allocate_browser_icon_button(ui, enabled, min_size, tip);
    let fill = animated_response_fill(
        ui,
        &response,
        action_button_fill(role),
        action_button_text(role),
        enabled,
    );
    ui.painter().rect(
        rect,
        ACTION_BUTTON_RADIUS,
        fill,
        egui::Stroke::new(1.0, action_button_stroke(role)),
        egui::StrokeKind::Inside,
    );
    let icon_color = if enabled {
        action_button_text(role)
    } else {
        CHROME_TEXT_DIM
    };
    paint_chrome_icon(ui.painter(), rect, icon, icon_color);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn allocate_browser_icon_button(
    ui: &mut egui::Ui,
    enabled: bool,
    size: egui::Vec2,
    tip: &str,
) -> (egui::Rect, egui::Response) {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    (rect, response)
}

fn allocate_browser_icon_button_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    enabled: bool,
    tip: &str,
) -> egui::Response {
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let response = ui.allocate_rect(rect, sense);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    response
}

fn paint_transparent_icon_button_state(
    ui: &egui::Ui,
    response: &egui::Response,
    rect: egui::Rect,
    radius: f32,
    layer: Color32,
    enabled: bool,
) {
    let fill = animated_response_fill(ui, response, CHROME_TOOLBAR, layer, enabled);
    if fill != CHROME_TOOLBAR {
        ui.painter().rect_filled(rect, radius, fill);
    }
}

fn chrome_text_field(
    ui: &mut egui::Ui,
    enabled: bool,
    text: &mut String,
    hint: &str,
    desired_width: f32,
    min_width: f32,
    password: bool,
    tip: &str,
    id: Option<egui::Id>,
) -> egui::Response {
    let mut edit = egui::TextEdit::singleline(text)
        .desired_width(desired_width)
        .hint_text(
            RichText::new(hint)
                .size(Style::SMALL)
                .color(CHROME_TEXT_DIM),
        )
        .text_color(CHROME_TEXT)
        .font(font_id(Style::SMALL))
        .background_color(CHROME_SURFACE)
        .margin(egui::Margin::symmetric(0, 0))
        .frame(false)
        .min_size(egui::vec2(min_width, CHROME_OMNIBOX_H - 6.0));
    if password {
        edit = edit.password(true);
    }
    if let Some(id) = id {
        edit = edit.id(id);
    }

    let inner = egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(ICON_BUTTON_RADIUS)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| ui.add_enabled(enabled, edit));
    mde_egui::focus::paint_focus_ring(ui.painter(), inner.response.rect, inner.inner.has_focus());
    chrome_hover_text(inner.inner, tip)
}

fn chrome_omnibox_field(
    ui: &mut egui::Ui,
    enabled: bool,
    text: &mut String,
    hint: &str,
    desired_width: f32,
    min_width: f32,
    tip: &str,
    id: Option<egui::Id>,
    page_url: &str,
    recent_resources: impl FnOnce() -> Vec<mde_web_preview_client::ResourceRequestStatus>,
    permissions: Option<&SiteInfoPermissionSummary>,
) -> egui::Response {
    let text_desired = (desired_width - OMNIBOX_SECURITY_SLOT_W).max(OMNIBOX_TEXT_TINY_MIN);
    let text_min = (min_width - OMNIBOX_SECURITY_SLOT_W).max(OMNIBOX_TEXT_TINY_MIN);
    let mut edit = egui::TextEdit::singleline(text)
        .desired_width(text_desired)
        .hint_text(
            RichText::new(hint)
                .size(OMNIBOX_FONT)
                .color(CHROME_TEXT_DIM),
        )
        .text_color(CHROME_TEXT)
        .font(font_id(OMNIBOX_FONT))
        .background_color(CHROME_SURFACE)
        .margin(egui::Margin::symmetric(0, 0))
        .frame(false)
        .min_size(egui::vec2(text_min, CHROME_OMNIBOX_H - 6.0));
    if let Some(id) = id {
        edit = edit.id(id);
    }

    let inner = egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(ICON_BUTTON_RADIUS)
        .inner_margin(egui::Margin::symmetric(3, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                omnibox_security_button(ui, page_url, recent_resources, permissions);
                ui.add_enabled(enabled, edit)
            })
            .inner
        });
    mde_egui::focus::paint_focus_ring(ui.painter(), inner.response.rect, inner.inner.has_focus());
    chrome_hover_text(inner.inner, tip)
}

fn ad_filter_domain_row(ui: &mut egui::Ui, domain: &str, count: u64) {
    ui.horizontal(|ui| {
        egui::Frame::NONE
            .fill(CHROME_PRIMARY_CONTAINER)
            .inner_margin(egui::Margin::symmetric(6, 1))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(count.to_string())
                        .size(Style::SMALL)
                        .strong()
                        .color(CHROME_PRIMARY),
                );
            });
        ui.label(
            RichText::new(domain)
                .size(Style::SMALL)
                .color(CHROME_TEXT_DIM),
        );
    });
}

fn ad_filter_chip_reserve(blocked: u64) -> f32 {
    let badge_w = toolbar_count_badge_width(blocked);
    if badge_w == 0.0 {
        0.0
    } else {
        CHROME_GAP + CHROME_BUTTON + CHROME_GAP + badge_w
    }
}

fn ad_filter_chip(ui: &mut egui::Ui, blocked: u64, top_blocked: &[(String, u64)]) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
            egui::Sense::hover(),
        );
        paint_chrome_icon(ui.painter(), rect, ChromeIcon::Privacy, CHROME_TEXT_DIM);
        toolbar_count_badge(ui, blocked, "Ad-filter blocked requests");
    })
    .response
    .on_hover_ui(|ui| ad_filter_hover_card(ui, blocked, top_blocked));
}

fn ad_filter_hover_card(ui: &mut egui::Ui, blocked: u64, top_blocked: &[(String, u64)]) {
    chrome_tooltip_frame(ui, |ui| {
        ui.label(
            RichText::new(format!(
                "Ad-filter blocked {blocked} request{} on this page",
                if blocked == 1 { "" } else { "s" }
            ))
            .size(Style::SMALL)
            .color(CHROME_TEXT),
        );
        if !top_blocked.is_empty() {
            chrome_separator(ui);
            for (domain, count) in top_blocked {
                ad_filter_domain_row(ui, domain, *count);
            }
        }
    });
}

fn chrome_separator(ui: &mut egui::Ui) {
    chrome_separator_with_inset(ui, 0.0);
}

fn chrome_separator_with_inset(ui: &mut egui::Ui, left_inset: f32) {
    let width = bounded_available_width(ui).max(1.0);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(width, CHROME_SEPARATOR_H), egui::Sense::hover());
    let left = (rect.left() + left_inset).min(rect.right() - 1.0);
    let right = rect.right().max(left + 1.0);
    let y = rect.center().y;
    ui.painter().line_segment(
        [egui::pos2(left, y), egui::pos2(right, y)],
        egui::Stroke::new(1.0, CHROME_OUTLINE),
    );
}

fn selection_wash() -> Color32 {
    CHROME_PRIMARY.gamma_multiply(0.16)
}

pub(super) fn font_id(size: f32) -> FontId {
    FontId::new(size, chrome_font_family())
}

pub(super) fn omnibox_dim_format(font_id: FontId) -> egui::TextFormat {
    egui::TextFormat {
        font_id,
        color: CHROME_TEXT_DIM,
        ..Default::default()
    }
}

pub(super) fn omnibox_strong_format(font_id: FontId) -> egui::TextFormat {
    egui::TextFormat {
        font_id,
        color: CHROME_TEXT,
        ..Default::default()
    }
}

fn state_layer(base: Color32, layer: Color32, alpha: u8) -> Color32 {
    fn blend_channel(base: u8, layer: u8, alpha: u8) -> u8 {
        let alpha = u16::from(alpha);
        let inv = 255u16.saturating_sub(alpha);
        let mixed = u16::from(base) * inv + u16::from(layer) * alpha + 127;
        (mixed / 255) as u8
    }

    Color32::from_rgb(
        blend_channel(base.r(), layer.r(), alpha),
        blend_channel(base.g(), layer.g(), alpha),
        blend_channel(base.b(), layer.b(), alpha),
    )
}

fn tab_shadow_fill(active: bool) -> Color32 {
    state_layer(
        CHROME_SURFACE_CONTAINER,
        CHROME_OUTLINE,
        if active { 108 } else { 42 },
    )
}

fn tab_badge_depth_fill() -> Color32 {
    state_layer(CHROME_TOOLBAR, CHROME_OUTLINE, 84)
}

fn control_state_target_alpha(enabled: bool, response: &egui::Response) -> u8 {
    if !enabled {
        return 0;
    }
    if response.is_pointer_button_down_on() {
        STATE_PRESSED_ALPHA
    } else if response.has_focus() {
        STATE_FOCUS_ALPHA
    } else if response.hovered() {
        STATE_HOVER_ALPHA
    } else {
        0
    }
}

fn animate_control_state_alpha_with_mode(
    ctx: &egui::Context,
    id: impl Hash,
    target_alpha: u8,
    mode: MotionMode,
) -> f32 {
    Motion::animate_typed_with_mode(
        ctx,
        ("browser-chrome-control-state", id),
        f32::from(target_alpha),
        MotionPreset::Control,
        mode,
    )
    .value()
    .clamp(0.0, 255.0)
}

fn animate_control_state_alpha(ctx: &egui::Context, id: impl Hash, target_alpha: u8) -> u8 {
    animate_control_state_alpha_with_mode(ctx, id, target_alpha, Motion::mode()).round() as u8
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BrowserPopoverMotion {
    opacity: f32,
    scale: f32,
    anchor_offset: f32,
    active: bool,
}

fn popover_motion_with_mode(
    ctx: &egui::Context,
    id: impl Hash,
    visible: bool,
    mode: MotionMode,
) -> BrowserPopoverMotion {
    let progress = Motion::animate_typed_with_mode(
        ctx,
        ("browser-chrome-popover", id),
        if visible { 1.0 } else { 0.0 },
        MotionPreset::Popover,
        mode,
    );
    let opacity = progress.value().clamp(0.0, 1.0);
    let transform_progress = if matches!(mode, MotionMode::Normal) {
        opacity
    } else {
        1.0
    };

    BrowserPopoverMotion {
        opacity,
        scale: 0.98 + 0.02 * transform_progress,
        anchor_offset: 4.0 * (1.0 - transform_progress),
        active: !progress.is_settled(),
    }
}

fn popover_motion(ctx: &egui::Context, id: impl Hash, visible: bool) -> BrowserPopoverMotion {
    popover_motion_with_mode(ctx, id, visible, Motion::mode())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BrowserDialogMotion {
    opacity: f32,
    scale: f32,
    y_offset: f32,
    active: bool,
}

fn dialog_prompt_motion_with_mode(
    ctx: &egui::Context,
    id: impl Hash,
    mode: MotionMode,
) -> BrowserDialogMotion {
    let id = egui::Id::new(("browser-chrome-dialog-prompt", id));
    let spec = Motion::spec(MotionPreset::Dialog);
    let dt = ctx.input(|i| i.stable_dt);
    let mut progress = ctx
        .data_mut(|data| data.get_temp::<AnimatedScalar>(id))
        .unwrap_or_else(|| AnimatedScalar::settled(0.0));
    progress.advance(1.0, spec, mode, dt);
    ctx.data_mut(|data| data.insert_temp(id, progress));
    if !progress.is_settled() {
        ctx.request_repaint();
    }

    let opacity = progress.value().clamp(0.0, 1.0);
    let transform_progress = if matches!(mode, MotionMode::Normal) {
        opacity
    } else {
        1.0
    };

    BrowserDialogMotion {
        opacity,
        scale: 0.97 + 0.03 * transform_progress,
        y_offset: 3.0 * (1.0 - transform_progress),
        active: !progress.is_settled(),
    }
}

fn dialog_prompt_motion(ctx: &egui::Context, id: impl Hash) -> BrowserDialogMotion {
    dialog_prompt_motion_with_mode(ctx, id, Motion::mode())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BrowserPanelMotion {
    opacity: f32,
    y_offset: f32,
    active: bool,
}

fn panel_motion_with_mode(
    ctx: &egui::Context,
    id: impl Hash,
    visible: bool,
    mode: MotionMode,
) -> BrowserPanelMotion {
    let id = egui::Id::new(("browser-chrome-panel", id));
    let spec = Motion::spec(MotionPreset::Panel);
    let dt = ctx.input(|i| i.stable_dt);
    let mut progress = ctx
        .data_mut(|data| data.get_temp::<AnimatedScalar>(id))
        .unwrap_or_else(|| AnimatedScalar::settled(0.0));
    progress.advance(if visible { 1.0 } else { 0.0 }, spec, mode, dt);
    ctx.data_mut(|data| data.insert_temp(id, progress));
    if !progress.is_settled() {
        ctx.request_repaint();
    }

    let opacity = progress.value().clamp(0.0, 1.0);
    let travel = match mode {
        MotionMode::Normal => 7.0,
        MotionMode::Reduced => 2.0,
        MotionMode::Disabled => 0.0,
    };
    BrowserPanelMotion {
        opacity,
        y_offset: travel * (1.0 - opacity),
        active: !progress.is_settled(),
    }
}

fn panel_motion(ctx: &egui::Context, id: impl Hash, visible: bool) -> BrowserPanelMotion {
    panel_motion_with_mode(ctx, id, visible, Motion::mode())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BrowserPageMotion {
    opacity: f32,
    active: bool,
}

fn page_motion_with_mode(
    ctx: &egui::Context,
    id: impl Hash,
    page_key: u64,
    mode: MotionMode,
) -> BrowserPageMotion {
    let key_id = egui::Id::new(("browser-chrome-page-key", &id));
    let progress_id = egui::Id::new(("browser-chrome-page", id));
    let spec = Motion::spec(MotionPreset::Page);
    let dt = ctx.input(|input| input.stable_dt);
    let mut progress = ctx.data_mut(|data| {
        let last_key = data.get_temp::<u64>(key_id);
        let changed = last_key.is_some_and(|last| last != page_key);
        data.insert_temp(key_id, page_key);
        if changed {
            data.insert_temp(progress_id, AnimatedScalar::settled(0.0));
        }
        data.get_temp::<AnimatedScalar>(progress_id)
            .unwrap_or_else(|| AnimatedScalar::settled(1.0))
    });
    progress.advance(1.0, spec, mode, dt);
    ctx.data_mut(|data| data.insert_temp(progress_id, progress));
    if !progress.is_settled() {
        ctx.request_repaint();
    }

    BrowserPageMotion {
        opacity: progress.value().clamp(0.0, 1.0),
        active: !progress.is_settled(),
    }
}

fn page_motion(ctx: &egui::Context, id: impl Hash, page_key: u64) -> BrowserPageMotion {
    page_motion_with_mode(ctx, id, page_key, Motion::mode())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BrowserDragSettleMotion {
    offset: egui::Vec2,
    accent_alpha: u8,
    active: bool,
}

impl BrowserDragSettleMotion {
    const fn settled() -> Self {
        Self {
            offset: egui::Vec2::ZERO,
            accent_alpha: 0,
            active: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TabDragSettleEvent {
    tab_id: u64,
    direction: f32,
}

fn tab_drag_settle_event_id(axis: TabAxis) -> egui::Id {
    egui::Id::new(("browser-tab-drag-settle-event", axis))
}

fn tab_drag_settle_progress_id(axis: TabAxis, tab_id: u64) -> egui::Id {
    egui::Id::new(("browser-tab-drag-settle-progress", axis, tab_id))
}

fn tab_drag_settle_direction(from: usize, to: usize) -> f32 {
    match to.cmp(&from) {
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Equal => 0.0,
    }
}

fn note_tab_drag_settle(ctx: &egui::Context, tab_id: u64, axis: TabAxis, direction: f32) {
    if direction.abs() <= f32::EPSILON {
        return;
    }
    let event = TabDragSettleEvent {
        tab_id,
        direction: direction.signum(),
    };
    let progress_id = tab_drag_settle_progress_id(axis, tab_id);
    ctx.data_mut(|data| {
        data.insert_temp(tab_drag_settle_event_id(axis), event);
        data.insert_temp(progress_id, AnimatedScalar::settled(0.0));
    });
    ctx.request_repaint();
}

fn tab_drag_settle_motion_with_mode(
    ctx: &egui::Context,
    tab_id: u64,
    axis: TabAxis,
    mode: MotionMode,
) -> BrowserDragSettleMotion {
    let event =
        ctx.data(|data| data.get_temp::<TabDragSettleEvent>(tab_drag_settle_event_id(axis)));
    let Some(event) = event.filter(|event| event.tab_id == tab_id) else {
        return BrowserDragSettleMotion::settled();
    };

    let progress_id = tab_drag_settle_progress_id(axis, tab_id);
    let spec = Motion::spec(MotionPreset::DragSettle);
    let dt = ctx.input(|input| input.stable_dt);
    let mut progress = ctx
        .data_mut(|data| data.get_temp::<AnimatedScalar>(progress_id))
        .unwrap_or_else(|| AnimatedScalar::settled(0.0));
    progress.advance(1.0, spec, mode, dt);
    ctx.data_mut(|data| data.insert_temp(progress_id, progress));
    if !progress.is_settled() {
        ctx.request_repaint();
    }

    let settle = progress.value().clamp(0.0, 1.0);
    let travel = if matches!(mode, MotionMode::Normal) {
        TAB_DRAG_SETTLE_TRAVEL
    } else {
        0.0
    };
    let distance = event.direction * travel * (1.0 - settle);
    let offset = match axis {
        TabAxis::Horizontal => egui::vec2(distance, 0.0),
        TabAxis::Vertical => egui::vec2(0.0, distance),
    };

    BrowserDragSettleMotion {
        offset,
        accent_alpha: (112.0 * (1.0 - settle)).round().clamp(0.0, 112.0) as u8,
        active: !progress.is_settled(),
    }
}

fn tab_drag_settle_motion(
    ctx: &egui::Context,
    tab_id: u64,
    axis: TabAxis,
) -> BrowserDragSettleMotion {
    tab_drag_settle_motion_with_mode(ctx, tab_id, axis, Motion::mode())
}

fn active_body_page_motion_key(state: &WebState) -> u64 {
    let mut hasher = DefaultHasher::new();
    "browser-active-body".hash(&mut hasher);
    state.active.hash(&mut hasher);
    if let Some(block) = &state.managed_policy_block {
        "managed-policy".hash(&mut hasher);
        block.url.hash(&mut hasher);
        block.rule.hash(&mut hasher);
        return hasher.finish();
    }
    let Some(tab) = state.tabs.get(state.active) else {
        "empty".hash(&mut hasher);
        return hasher.finish();
    };
    tab.id.hash(&mut hasher);
    if let Some(page) = tab.internal_page {
        "internal".hash(&mut hasher);
        page.url().hash(&mut hasher);
        return hasher.finish();
    }
    let nav_url = tab.session.nav().url.trim();
    if let Some(url) = tab.session.safe_browsing_block() {
        "safe-browsing".hash(&mut hasher);
        url.hash(&mut hasher);
        return hasher.finish();
    }
    if tab.session.is_crashed() {
        "crashed".hash(&mut hasher);
        nav_url.hash(&mut hasher);
        return hasher.finish();
    }
    if tab.session.cert_error().is_some() {
        "certificate-error".hash(&mut hasher);
        nav_url.hash(&mut hasher);
        return hasher.finish();
    }
    if is_new_tab_url(nav_url) {
        "new-tab".hash(&mut hasher);
    } else if tab.texture.is_some() {
        "page".hash(&mut hasher);
    } else {
        "loading".hash(&mut hasher);
    }
    nav_url.hash(&mut hasher);
    hasher.finish()
}

fn page_transition_frame(ui: &mut egui::Ui, key: u64, contents: impl FnOnce(&mut egui::Ui)) {
    let motion = page_motion(ui.ctx(), "active-body", key);
    if motion.active {
        ui.ctx().request_repaint();
    }
    if motion.opacity >= 0.999 {
        contents(ui);
        return;
    }
    ui.scope(|ui| {
        ui.multiply_opacity(motion.opacity.max(0.18));
        contents(ui);
    });
}

fn animated_state_layer(
    ctx: &egui::Context,
    id: impl Hash,
    base: Color32,
    layer: Color32,
    target_alpha: u8,
) -> Color32 {
    let alpha = animate_control_state_alpha(ctx, id, target_alpha);
    if alpha == 0 {
        base
    } else {
        state_layer(base, layer, alpha)
    }
}

fn animated_response_fill(
    ui: &egui::Ui,
    response: &egui::Response,
    base: Color32,
    layer: Color32,
    enabled: bool,
) -> Color32 {
    animated_state_layer(
        ui.ctx(),
        response.id,
        base,
        layer,
        control_state_target_alpha(enabled, response),
    )
}

fn icon_stroke(color: Color32) -> egui::Stroke {
    egui::Stroke::new(1.7, color)
}

fn loading_globe_accesskit_id(placement: &'static str) -> egui::Id {
    egui::Id::new(("browser-loading-globe", placement))
}

fn browser_chrome_reduce_motion(ui: &egui::Ui) -> bool {
    ui.style().animation_time <= 0.0 || Motion::reduce_motion()
}

fn loading_globe_phase(time: f64, reduce_motion: bool) -> (f32, bool) {
    if reduce_motion {
        return (0.0, false);
    }
    (
        (time as f32 * std::f32::consts::TAU * 0.8).rem_euclid(std::f32::consts::TAU),
        true,
    )
}

fn accesskit_rect(rect: egui::Rect) -> egui::accesskit::Rect {
    egui::accesskit::Rect {
        x0: rect.min.x.into(),
        y0: rect.min.y.into(),
        x1: rect.max.x.into(),
        y1: rect.max.y.into(),
    }
}

fn ellipse_points(center: egui::Pos2, rx: f32, ry: f32, phase: f32) -> Vec<egui::Pos2> {
    (0..=32)
        .map(|step| {
            let t = phase + std::f32::consts::TAU * step as f32 / 32.0;
            egui::pos2(center.x + rx * t.cos(), center.y + ry * t.sin())
        })
        .collect()
}

fn paint_loading_globe(painter: &egui::Painter, rect: egui::Rect, phase: f32, scale: f32) {
    let r = rect.shrink(2.0 * scale.max(0.5));
    let c = r.center();
    let radius = r.width().min(r.height()) * 0.44;
    let outline = egui::Stroke::new(1.5 * scale, CHROME_PRIMARY);
    let dim = egui::Stroke::new(1.0 * scale, CHROME_TEXT_DIM.gamma_multiply(0.62));
    let bright = egui::Stroke::new(1.8 * scale, CHROME_SUCCESS);
    let orbit = egui::Stroke::new(1.35 * scale, CHROME_WARN);

    painter.circle_filled(c, radius, CHROME_PRIMARY_CONTAINER.gamma_multiply(0.72));
    painter.circle_stroke(c, radius, outline);
    painter.add(egui::Shape::line(
        ellipse_points(c, radius * 0.82, radius * 0.26, 0.0),
        dim,
    ));
    painter.add(egui::Shape::line(
        ellipse_points(c, radius * 0.82, radius * 0.26, std::f32::consts::PI),
        dim,
    ));
    painter.line_segment(
        [
            egui::pos2(c.x - radius * 0.86, c.y),
            egui::pos2(c.x + radius * 0.86, c.y),
        ],
        dim,
    );
    let meridian_phase = phase.sin().abs();
    painter.add(egui::Shape::line(
        ellipse_points(c, radius * (0.18 + 0.46 * meridian_phase), radius, 0.0),
        dim,
    ));
    painter.add(egui::Shape::line(
        ellipse_points(
            c,
            radius * (0.18 + 0.46 * (phase + std::f32::consts::FRAC_PI_2).sin().abs()),
            radius,
            0.0,
        ),
        bright,
    ));
    painter.add(egui::Shape::line(
        ellipse_points(c, radius * 1.12, radius * 0.30, phase),
        orbit,
    ));
}

fn paint_chrome_icon(painter: &egui::Painter, rect: egui::Rect, icon: ChromeIcon, color: Color32) {
    // Mackes-Carbon is the canonical platform icon set: render the mapped glyph
    // through the shared `mde_egui::carbon` loader first (rasterized + tinted +
    // ctx-cached). The YAMIS texture path and the procedural draws below stay as
    // fallbacks for the (embedded-asset-only) case where a Carbon glyph cannot
    // rasterize or upload.
    if mde_egui::carbon::paint_carbon(painter, rect, chrome_icon_carbon_name(icon), color) {
        return;
    }
    if try_paint_yamis_chrome_icon(painter, rect, icon, color) {
        return;
    }
    let r = rect.shrink(4.0);
    let c = r.center();
    let stroke = icon_stroke(color);
    match icon {
        ChromeIcon::Back => {
            painter.line_segment([r.left_center(), r.right_center()], stroke);
            painter.line_segment([r.left_center(), egui::pos2(c.x, r.top())], stroke);
            painter.line_segment([r.left_center(), egui::pos2(c.x, r.bottom())], stroke);
        }
        ChromeIcon::Forward => {
            painter.line_segment([r.left_center(), r.right_center()], stroke);
            painter.line_segment([r.right_center(), egui::pos2(c.x, r.top())], stroke);
            painter.line_segment([r.right_center(), egui::pos2(c.x, r.bottom())], stroke);
        }
        ChromeIcon::Reload => {
            painter.circle_stroke(c, r.width().min(r.height()) * 0.36, stroke);
            painter.line_segment([egui::pos2(c.x + 3.0, r.top()), r.right_top()], stroke);
            painter.line_segment([r.right_top(), egui::pos2(r.right(), c.y - 5.0)], stroke);
        }
        ChromeIcon::Stop | ChromeIcon::Close => {
            painter.line_segment([r.left_top(), r.right_bottom()], stroke);
            painter.line_segment([r.right_top(), r.left_bottom()], stroke);
        }
        ChromeIcon::Options => {
            for y in [c.y - 5.0, c.y, c.y + 5.0] {
                painter.circle_filled(egui::pos2(c.x, y), 1.7, color);
            }
        }
        ChromeIcon::Downloads => {
            painter.line_segment(
                [egui::pos2(c.x, r.top()), egui::pos2(c.x, c.y + 3.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y + 3.0), egui::pos2(c.x - 4.5, c.y - 1.5)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y + 3.0), egui::pos2(c.x + 4.5, c.y - 1.5)],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(r.left() + 2.0, r.bottom()),
                    egui::pos2(r.right() - 2.0, r.bottom()),
                ],
                stroke,
            );
        }
        ChromeIcon::Capture => {
            painter.rect_stroke(r, 3.0, stroke, egui::StrokeKind::Inside);
            painter.circle_stroke(c, 4.0, stroke);
        }
        ChromeIcon::Bookmark => {
            let points = vec![
                egui::pos2(c.x, r.top()),
                egui::pos2(c.x + 3.0, c.y - 1.0),
                egui::pos2(r.right(), c.y - 1.0),
                egui::pos2(c.x + 4.0, c.y + 2.5),
                egui::pos2(c.x + 5.5, r.bottom()),
                egui::pos2(c.x, c.y + 4.5),
                egui::pos2(c.x - 5.5, r.bottom()),
                egui::pos2(c.x - 4.0, c.y + 2.5),
                egui::pos2(r.left(), c.y - 1.0),
                egui::pos2(c.x - 3.0, c.y - 1.0),
                egui::pos2(c.x, r.top()),
            ];
            painter.add(egui::Shape::line(points, stroke));
        }
        ChromeIcon::Security | ChromeIcon::Privacy => {
            let top = egui::pos2(c.x, r.top());
            let shield = vec![
                top,
                egui::pos2(r.right(), r.top() + 3.0),
                egui::pos2(r.right() - 1.0, c.y + 4.0),
                egui::pos2(c.x, r.bottom()),
                egui::pos2(r.left() + 1.0, c.y + 4.0),
                egui::pos2(r.left(), r.top() + 3.0),
                top,
            ];
            painter.add(egui::Shape::line(shield, stroke));
            if icon == ChromeIcon::Privacy {
                painter.line_segment(
                    [
                        egui::pos2(c.x, r.top() + 4.0),
                        egui::pos2(c.x, r.bottom() - 4.0),
                    ],
                    stroke,
                );
            }
        }
        ChromeIcon::Warning => {
            let triangle = vec![
                egui::pos2(c.x, r.top()),
                egui::pos2(r.right(), r.bottom()),
                egui::pos2(r.left(), r.bottom()),
                egui::pos2(c.x, r.top()),
            ];
            painter.add(egui::Shape::line(triangle, stroke));
            painter.line_segment(
                [egui::pos2(c.x, c.y - 3.0), egui::pos2(c.x, c.y + 2.0)],
                stroke,
            );
            painter.circle_filled(egui::pos2(c.x, r.bottom() - 2.5), 1.2, color);
        }
        ChromeIcon::Lock => {
            let body = egui::Rect::from_min_max(
                egui::pos2(r.left() + 2.0, c.y),
                egui::pos2(r.right() - 2.0, r.bottom()),
            );
            painter.rect_stroke(body, 2.0, stroke, egui::StrokeKind::Inside);
            painter.circle_stroke(egui::pos2(c.x, c.y - 1.0), 5.0, stroke);
            painter.line_segment(
                [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x - 5.0, c.y + 1.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x + 5.0, c.y), egui::pos2(c.x + 5.0, c.y + 1.0)],
                stroke,
            );
        }
        ChromeIcon::Search | ChromeIcon::Find => {
            painter.circle_stroke(egui::pos2(c.x - 2.0, c.y - 2.0), 5.0, stroke);
            painter.line_segment([egui::pos2(c.x + 2.0, c.y + 2.0), r.right_bottom()], stroke);
        }
        ChromeIcon::ZoomIn | ChromeIcon::ZoomOut => {
            painter.circle_stroke(egui::pos2(c.x - 2.0, c.y - 2.0), 5.0, stroke);
            painter.line_segment([egui::pos2(c.x + 2.0, c.y + 2.0), r.right_bottom()], stroke);
            painter.line_segment(
                [
                    egui::pos2(c.x - 5.0, c.y - 2.0),
                    egui::pos2(c.x + 1.0, c.y - 2.0),
                ],
                stroke,
            );
            if icon == ChromeIcon::ZoomIn {
                painter.line_segment(
                    [
                        egui::pos2(c.x - 2.0, c.y - 5.0),
                        egui::pos2(c.x - 2.0, c.y + 1.0),
                    ],
                    stroke,
                );
            }
        }
        ChromeIcon::Print => {
            painter.rect_stroke(
                egui::Rect::from_min_size(
                    egui::pos2(r.left() + 2.0, r.top()),
                    egui::vec2(r.width() - 4.0, 5.0),
                ),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                egui::Rect::from_min_size(
                    egui::pos2(r.left(), c.y - 3.0),
                    egui::vec2(r.width(), 8.0),
                ),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                egui::Rect::from_min_size(
                    egui::pos2(r.left() + 3.0, c.y + 3.0),
                    egui::vec2(r.width() - 6.0, 7.0),
                ),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ChromeIcon::History => {
            painter.circle_stroke(c, 7.0, stroke);
            painter.line_segment([c, egui::pos2(c.x, c.y - 5.0)], stroke);
            painter.line_segment([c, egui::pos2(c.x - 4.0, c.y)], stroke);
        }
        ChromeIcon::Tabs => {
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(r.left() + 2.0, r.top() + 4.0),
                    r.right_bottom(),
                ),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    r.left_top(),
                    egui::pos2(r.right() - 3.0, r.bottom() - 4.0),
                ),
                2.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ChromeIcon::Engine => {
            painter.circle_stroke(c, 7.0, stroke);
            painter.circle_filled(c, 2.0, color);
            painter.line_segment([r.left_center(), r.right_center()], stroke);
            painter.line_segment(
                [egui::pos2(c.x, r.top()), egui::pos2(c.x, r.bottom())],
                stroke,
            );
        }
        ChromeIcon::NewTab => {
            painter.line_segment(
                [egui::pos2(c.x, r.top()), egui::pos2(c.x, r.bottom())],
                stroke,
            );
            painter.line_segment([r.left_center(), r.right_center()], stroke);
        }
        ChromeIcon::Minus | ChromeIcon::Plus => {
            painter.line_segment([r.left_center(), r.right_center()], stroke);
            if icon == ChromeIcon::Plus {
                painter.line_segment(
                    [egui::pos2(c.x, r.top()), egui::pos2(c.x, r.bottom())],
                    stroke,
                );
            }
        }
        ChromeIcon::Up => {
            painter.line_segment(
                [egui::pos2(r.left(), c.y + 3.0), egui::pos2(c.x, c.y - 4.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y - 4.0), egui::pos2(r.right(), c.y + 3.0)],
                stroke,
            );
        }
        ChromeIcon::Down => {
            painter.line_segment(
                [egui::pos2(r.left(), c.y - 3.0), egui::pos2(c.x, c.y + 4.0)],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x, c.y + 4.0), egui::pos2(r.right(), c.y - 3.0)],
                stroke,
            );
        }
        ChromeIcon::Check => {
            painter.line_segment([r.left_center(), egui::pos2(c.x - 1.0, r.bottom())], stroke);
            painter.line_segment([egui::pos2(c.x - 1.0, r.bottom()), r.right_top()], stroke);
        }
        ChromeIcon::Page => {
            painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(r.left() + 3.0, c.y - 2.0),
                    egui::pos2(r.right() - 3.0, c.y - 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(r.left() + 3.0, c.y + 3.0),
                    egui::pos2(r.right() - 5.0, c.y + 3.0),
                ],
                stroke,
            );
        }
        ChromeIcon::Edit => {
            painter.line_segment(
                [
                    egui::pos2(r.left() + 3.0, r.bottom() - 2.0),
                    egui::pos2(r.right() - 2.0, r.top() + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(r.left() + 2.0, r.bottom()),
                    egui::pos2(r.left() + 7.0, r.bottom() - 1.0),
                ],
                stroke,
            );
        }
        ChromeIcon::View => {
            painter.circle_stroke(c, 6.0, stroke);
            painter.line_segment([r.left_center(), egui::pos2(c.x - 6.0, c.y)], stroke);
            painter.line_segment([egui::pos2(c.x + 6.0, c.y), r.right_center()], stroke);
        }
        ChromeIcon::Power => {
            painter.circle_stroke(c, 7.0, stroke);
            painter.line_segment([egui::pos2(c.x, r.top()), c], stroke);
        }
        ChromeIcon::Share => {
            painter.circle_stroke(egui::pos2(r.left() + 3.0, c.y), 2.5, stroke);
            painter.circle_stroke(egui::pos2(r.right() - 3.0, r.top() + 3.0), 2.5, stroke);
            painter.circle_stroke(egui::pos2(r.right() - 3.0, r.bottom() - 3.0), 2.5, stroke);
            painter.line_segment(
                [
                    egui::pos2(r.left() + 5.0, c.y - 1.0),
                    egui::pos2(r.right() - 5.0, r.top() + 4.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(r.left() + 5.0, c.y + 1.0),
                    egui::pos2(r.right() - 5.0, r.bottom() - 4.0),
                ],
                stroke,
            );
        }
        ChromeIcon::Audio => {
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(r.left(), c.y - 4.0),
                    egui::pos2(c.x - 1.0, c.y + 4.0),
                ),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - 1.0, c.y - 4.0),
                    egui::pos2(c.x + 4.0, r.top() + 2.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - 1.0, c.y + 4.0),
                    egui::pos2(c.x + 4.0, r.bottom() - 2.0),
                ],
                stroke,
            );
        }
        ChromeIcon::VolumeDown | ChromeIcon::VolumeOff | ChromeIcon::VolumeUp => {
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(r.left(), c.y - 4.0),
                    egui::pos2(c.x - 3.0, c.y + 4.0),
                ),
                1.0,
                stroke,
                egui::StrokeKind::Inside,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - 3.0, c.y - 4.0),
                    egui::pos2(c.x + 1.0, r.top() + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - 3.0, c.y + 4.0),
                    egui::pos2(c.x + 1.0, r.bottom() - 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [egui::pos2(c.x + 5.0, c.y), egui::pos2(r.right() - 1.0, c.y)],
                stroke,
            );
            if icon == ChromeIcon::VolumeOff {
                painter.line_segment(
                    [
                        egui::pos2(c.x + 4.0, r.bottom() - 1.0),
                        egui::pos2(r.right() - 1.0, r.top() + 1.0),
                    ],
                    stroke,
                );
            } else if icon == ChromeIcon::VolumeUp {
                painter.line_segment(
                    [
                        egui::pos2(r.right() - 3.0, c.y - 4.0),
                        egui::pos2(r.right() - 3.0, c.y + 4.0),
                    ],
                    stroke,
                );
            }
        }
        ChromeIcon::Play => {
            let points = vec![
                egui::pos2(r.left() + 3.0, r.top() + 2.0),
                egui::pos2(r.left() + 3.0, r.bottom() - 2.0),
                egui::pos2(r.right() - 2.0, c.y),
                egui::pos2(r.left() + 3.0, r.top() + 2.0),
            ];
            painter.add(egui::Shape::line(points, stroke));
        }
        ChromeIcon::Pause => {
            let left = egui::Rect::from_min_max(
                egui::pos2(c.x - 5.0, r.top() + 2.0),
                egui::pos2(c.x - 2.0, r.bottom() - 2.0),
            );
            let right = egui::Rect::from_min_max(
                egui::pos2(c.x + 2.0, r.top() + 2.0),
                egui::pos2(c.x + 5.0, r.bottom() - 2.0),
            );
            painter.rect_filled(left, 1.0, color);
            painter.rect_filled(right, 1.0, color);
        }
        ChromeIcon::MediaStop => {
            painter.rect_filled(
                egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0)),
                1.5,
                color,
            );
        }
        ChromeIcon::Previous => {
            painter.line_segment(
                [
                    egui::pos2(r.left() + 2.0, r.top() + 1.0),
                    egui::pos2(r.left() + 2.0, r.bottom() - 1.0),
                ],
                stroke,
            );
            let points = vec![
                egui::pos2(r.right() - 2.0, r.top() + 1.5),
                egui::pos2(r.left() + 4.0, c.y),
                egui::pos2(r.right() - 2.0, r.bottom() - 1.5),
                egui::pos2(r.right() - 2.0, r.top() + 1.5),
            ];
            painter.add(egui::Shape::line(points, stroke));
        }
        ChromeIcon::Next => {
            painter.line_segment(
                [
                    egui::pos2(r.right() - 2.0, r.top() + 1.0),
                    egui::pos2(r.right() - 2.0, r.bottom() - 1.0),
                ],
                stroke,
            );
            let points = vec![
                egui::pos2(r.left() + 2.0, r.top() + 1.5),
                egui::pos2(r.right() - 4.0, c.y),
                egui::pos2(r.left() + 2.0, r.bottom() - 1.5),
                egui::pos2(r.left() + 2.0, r.top() + 1.5),
            ];
            painter.add(egui::Shape::line(points, stroke));
        }
        ChromeIcon::PictureInPicture => {
            painter.rect_stroke(r, 2.0, stroke, egui::StrokeKind::Inside);
            painter.line_segment(
                [
                    egui::pos2(r.left() + 3.0, r.top() + 4.0),
                    egui::pos2(r.right() - 3.0, r.top() + 4.0),
                ],
                stroke,
            );
            painter.rect_stroke(
                egui::Rect::from_min_max(
                    egui::pos2(c.x + 1.0, c.y + 1.0),
                    egui::pos2(r.right() - 2.0, r.bottom() - 2.0),
                ),
                1.5,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        ChromeIcon::DarkMode => {
            painter.circle_stroke(c, 7.0, stroke);
            painter.circle_filled(egui::pos2(c.x + 3.0, c.y - 2.0), 7.0, CHROME_TOOLBAR);
        }
        ChromeIcon::Notifications => {
            // A bell: domed body, its open rim, and the clapper below.
            let bell = vec![
                egui::pos2(c.x - 5.0, c.y + 3.0),
                egui::pos2(c.x - 5.0, c.y),
                egui::pos2(c.x - 3.5, r.top() + 3.0),
                egui::pos2(c.x, r.top() + 1.0),
                egui::pos2(c.x + 3.5, r.top() + 3.0),
                egui::pos2(c.x + 5.0, c.y),
                egui::pos2(c.x + 5.0, c.y + 3.0),
            ];
            painter.add(egui::Shape::line(bell, stroke));
            painter.line_segment(
                [
                    egui::pos2(c.x - 6.0, c.y + 3.0),
                    egui::pos2(c.x + 6.0, c.y + 3.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(c.x - 1.6, r.bottom() - 2.0),
                    egui::pos2(c.x + 1.6, r.bottom() - 2.0),
                ],
                stroke,
            );
        }
        ChromeIcon::Recommend => {
            // A five-point star with two small flanking sparkles.
            let outer = r.width().min(r.height()) * 0.42;
            let inner = outer * 0.42;
            let mut pts = Vec::with_capacity(11);
            for i in 0..10 {
                let radius = if i % 2 == 0 { outer } else { inner };
                let angle = -std::f32::consts::FRAC_PI_2 + (i as f32) * std::f32::consts::PI / 5.0;
                pts.push(egui::pos2(
                    c.x + radius * angle.cos(),
                    c.y + radius * angle.sin(),
                ));
            }
            pts.push(pts[0]);
            painter.add(egui::Shape::line(pts, stroke));
            painter.line_segment(
                [
                    egui::pos2(r.right() - 2.0, r.top() + 2.0),
                    egui::pos2(r.right() - 2.0, r.top() + 5.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    egui::pos2(r.left() + 2.0, r.bottom() - 5.0),
                    egui::pos2(r.left() + 2.0, r.bottom() - 2.0),
                ],
                stroke,
            );
        }
    }
}

fn try_paint_yamis_chrome_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    icon: ChromeIcon,
    color: Color32,
) -> bool {
    let Some(id) = chrome_icon_yamis_id(icon) else {
        return false;
    };
    let draw_rect = rect.shrink(2.0);
    let Some(texture) = yamis_chrome_icon_texture(
        painter.ctx(),
        id,
        draw_rect.width().min(draw_rect.height()),
        color,
    ) else {
        return false;
    };
    let [width, height] = texture.size();
    let image_rect = fitted_icon_rect(draw_rect, width.max(1) as f32 / height.max(1) as f32);
    painter.image(
        texture.id(),
        image_rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        Color32::WHITE,
    );
    true
}

fn fitted_icon_rect(rect: egui::Rect, aspect: f32) -> egui::Rect {
    let aspect = aspect.max(0.01);
    let rect_aspect = rect.width().max(1.0) / rect.height().max(1.0);
    let size = if aspect > rect_aspect {
        egui::vec2(rect.width(), rect.width() / aspect)
    } else {
        egui::vec2(rect.height() * aspect, rect.height())
    };
    egui::Rect::from_center_size(rect.center(), size)
}

#[allow(
    clippy::cast_possible_truncation, // rounded, clamped-positive f32 -> u32
    clippy::cast_sign_loss            // size_px >= 1.0 by the .max(1.0) clamp
)]
fn yamis_chrome_icon_texture(
    ctx: &egui::Context,
    id: IconId,
    logical_px: f32,
    tint: Color32,
) -> Option<TextureHandle> {
    let size_px = (logical_px * ctx.pixels_per_point()).round().max(1.0) as u32;
    let tint = tint.to_array();
    let key = egui::Id::new(("browser-yamis-icon", id.name(), size_px, tint));
    if let Some(cached) = ctx.data_mut(|data| data.get_temp::<Option<TextureHandle>>(key)) {
        return cached;
    }
    let texture = icon_image(id, size_px, tint).ok().map(|img| {
        let color = egui::ColorImage::from_rgba_unmultiplied(img.size_usize(), &img.rgba);
        ctx.load_texture(
            format!("browser-{}", id.name()),
            color,
            TextureOptions::LINEAR,
        )
    });
    ctx.data_mut(|data| data.insert_temp(key, texture.clone()));
    texture
}

#[cfg(test)]
pub(super) const fn loading_globe_painted_shape_count() -> usize {
    LOADING_GLOBE_SHAPE_COUNT
}

pub(super) fn loading_globe(
    ui: &mut egui::Ui,
    size: f32,
    placement: &'static str,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let time = ui.input(|input| input.time);
    let (phase, animate) = loading_globe_phase(time, browser_chrome_reduce_motion(ui));
    if animate {
        ui.ctx().request_repaint_after(Duration::from_millis(33));
    }
    paint_loading_globe(ui.painter(), rect, phase, size / 44.0);
    let _ = ui
        .ctx()
        .accesskit_node_builder(loading_globe_accesskit_id(placement), |node| {
            node.set_role(egui::accesskit::Role::Image);
            node.set_label("Browser loading globe");
            node.set_value("Loading page");
            node.set_bounds(accesskit_rect(rect));
        });
    chrome_hover_text(response, "Loading")
}

pub(super) fn chrome_icon_button(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    tip: &str,
    enabled: bool,
    selected: bool,
) -> egui::Response {
    let enabled = ui.is_enabled() && enabled;
    let base_fill = if selected {
        CHROME_PRIMARY_CONTAINER
    } else {
        CHROME_TOOLBAR
    };
    let (rect, response) =
        allocate_browser_icon_button(ui, enabled, egui::vec2(CHROME_BUTTON, CHROME_BUTTON), tip);
    let state_fill = animated_response_fill(
        ui,
        &response,
        base_fill,
        if selected {
            CHROME_PRIMARY
        } else {
            CHROME_ICON
        },
        enabled,
    );
    if selected || state_fill != base_fill {
        ui.painter()
            .rect_filled(rect, ICON_BUTTON_RADIUS, state_fill);
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            ICON_BUTTON_RADIUS,
            egui::Stroke::new(1.0, CHROME_PRIMARY),
            egui::StrokeKind::Inside,
        );
    }
    let color = if !enabled {
        CHROME_TEXT_DIM
    } else if selected {
        CHROME_ON_PRIMARY_CONTAINER
    } else {
        CHROME_ICON
    };
    paint_chrome_icon(ui.painter(), rect, icon, color);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn toolbar_icon_menu_anchor(
    ui: &mut egui::Ui,
    popup_id: egui::Id,
    icon: ChromeIcon,
    icon_color: Color32,
    tip: &str,
) -> egui::Response {
    let enabled = ui.is_enabled();
    let selected = ui.memory(|mem| mem.is_popup_open(popup_id));
    let (rect, response) =
        allocate_browser_icon_button(ui, enabled, egui::vec2(CHROME_BUTTON, CHROME_BUTTON), tip);
    let base_fill = if selected {
        CHROME_PRIMARY_CONTAINER
    } else {
        CHROME_TOOLBAR
    };
    let resolved_icon_color = if !enabled {
        CHROME_TEXT_DIM
    } else if selected {
        CHROME_ON_PRIMARY_CONTAINER
    } else {
        icon_color
    };
    let fill = animated_response_fill(ui, &response, base_fill, resolved_icon_color, enabled);
    if selected || fill != base_fill {
        ui.painter().rect_filled(rect, ICON_BUTTON_RADIUS, fill);
    }
    if selected {
        ui.painter().rect_stroke(
            rect,
            ICON_BUTTON_RADIUS,
            egui::Stroke::new(1.0, CHROME_PRIMARY),
            egui::StrokeKind::Inside,
        );
    }
    paint_chrome_icon(ui.painter(), rect, icon, resolved_icon_color);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn menu_anchor_keyboard_toggle(ui: &egui::Ui, response: &egui::Response) -> bool {
    response.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space))
}

/// Run a Browser chrome/body subtree under a light Chrome-style egui scope.
///
/// `Ui::scope` clones style state, so the rest of the shell keeps its existing
/// platform visuals after this closure returns.
pub(super) fn scope<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        apply_visuals(ui);
        add(ui)
    })
    .inner
}

/// Render the secondary Browser drawer stack in the same order for horizontal
/// and vertical chrome layouts.
pub(super) fn drawer_stack(ui: &mut egui::Ui, state: &mut WebState) {
    let visible = drawer_stack_visible(state);
    let motion = panel_motion(ui.ctx(), "drawer-stack", visible);
    if !visible {
        return;
    }

    ui.scope(|ui| {
        apply_visuals(ui);
        if motion.active {
            ui.ctx().request_repaint();
        }
        ui.multiply_opacity(motion.opacity.max(0.2));
        if motion.y_offset > 0.0 {
            ui.add_space(motion.y_offset);
        }

        qr_share_drawer(ui, state);
        spellcheck_drawer(ui, state);
        speech_status_drawer(ui, state);
        security_update_drawer(ui, state);
        translation_drawer(ui, state);
        offline_cache_drawer(ui, state);
        print_settings_drawer(ui, state);
        site_styles_drawer(ui, state);
        downloads_drawer(ui, state);
        history_drawer(ui, state);
        notifications_drawer(ui, state);
        recommendations_drawer(ui, state);
    });
}

fn drawer_stack_visible(state: &WebState) -> bool {
    state.latest_qr_share.is_some()
        || state
            .latest_spellcheck
            .as_ref()
            .is_some_and(|result| result.is_visible())
        || state
            .latest_read_aloud_status
            .as_ref()
            .is_some_and(BrowserReadAloudStatus::is_actionable)
        || state
            .latest_voice_command_status
            .as_ref()
            .is_some_and(BrowserVoiceCommandStatus::is_actionable)
        || state
            .latest_security_update
            .as_ref()
            .is_some_and(|status| status.is_actionable())
        || state.latest_translation.is_some()
        || state.latest_offline_cache.is_some()
        || state.print_settings_open
        || state.site_styles_open
        || state.downloads_open
        || state.history_open
        || state.notifications_open
        || state.recommendations_open
}

fn apply_visuals(ui: &mut egui::Ui) {
    apply_chrome_style(ui.style_mut());
}

fn apply_chrome_style(style: &mut egui::Style) {
    style.spacing.item_spacing = egui::vec2(CHROME_GAP, CHROME_GAP);
    style
        .text_styles
        .insert(TextStyle::Small, FontId::new(12.0, chrome_font_family()));
    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(13.0, chrome_font_family()));

    let visuals = &mut style.visuals;
    visuals.dark_mode = false;
    visuals.override_text_color = Some(CHROME_TEXT);
    visuals.panel_fill = CHROME_SURFACE;
    visuals.window_fill = CHROME_TOOLBAR;
    visuals.extreme_bg_color = CHROME_TOOLBAR;
    visuals.faint_bg_color = CHROME_SURFACE;
    visuals.widgets.noninteractive.bg_fill = CHROME_SURFACE;
    visuals.widgets.noninteractive.fg_stroke.color = CHROME_TEXT_DIM;
    visuals.widgets.noninteractive.bg_stroke.color = CHROME_OUTLINE;
    visuals.widgets.inactive.bg_fill = CHROME_TOOLBAR;
    visuals.widgets.inactive.fg_stroke.color = CHROME_TEXT;
    visuals.widgets.inactive.bg_stroke.color = CHROME_OUTLINE;
    visuals.widgets.hovered.bg_fill = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA);
    visuals.widgets.hovered.fg_stroke.color = CHROME_TEXT;
    visuals.widgets.active.bg_fill = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_PRESSED_ALPHA);
    visuals.widgets.active.fg_stroke.color = CHROME_TEXT;
    visuals.widgets.open.weak_bg_fill = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA);
    visuals.widgets.open.bg_fill = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA);
    visuals.widgets.open.fg_stroke = egui::Stroke::new(1.0, CHROME_TEXT);
    visuals.widgets.open.bg_stroke = egui::Stroke::new(1.0, CHROME_OUTLINE);
    visuals.selection.bg_fill =
        state_layer(CHROME_PRIMARY_CONTAINER, CHROME_PRIMARY, STATE_FOCUS_ALPHA);
    visuals.selection.stroke.color = CHROME_ON_PRIMARY_CONTAINER;
}

/// The width of each pill in the single-row horizontal strip: full width when the
/// strip is roomy, shrinking toward [`CHROME_TAB_MIN_W`] as tabs multiply. Once at
/// the floor the strip scrolls horizontally rather than wrapping onto stacked rows.
pub(super) fn horizontal_tab_pill_width(available: f32, tab_count: usize) -> f32 {
    let tab_count = tab_count.max(1) as f32;
    let per_slot_overhead = CHROME_TAB_CLOSE + 2.0 * CHROME_GAP;
    let per_tab = available / tab_count - per_slot_overhead;
    per_tab.clamp(CHROME_TAB_MIN_W, CHROME_TAB_W)
}

pub(super) fn tab_pill_sized(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    width: f32,
    engine: BrowserEngine,
    favicon: Option<&TextureHandle>,
    status_chips: &[TabStatusChip],
) -> egui::Response {
    // `click_and_drag` keeps activation, middle-click close, and drag-reorder on
    // the same browser-tab affordance while egui handles the click/drag threshold.
    let enabled = ui.is_enabled();
    let (_, response) = ui.allocate_exact_size(
        egui::vec2(width, CHROME_TAB_H),
        egui::Sense::click_and_drag(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let accent = engine_accent(engine);
    let r = response.rect;
    let pressed = response.is_pointer_button_down_on();
    let active_fill = CHROME_TOOLBAR;
    let inactive_fill = state_layer(CHROME_SURFACE_CONTAINER, accent, 5);
    let state_layer_color = if active {
        accent
    } else if pressed {
        CHROME_TEXT
    } else {
        accent
    };
    let fill = animated_response_fill(
        ui,
        &response,
        if active { active_fill } else { inactive_fill },
        state_layer_color,
        true,
    );

    if active {
        ui.painter().rect_filled(
            r.expand2(egui::vec2(1.0, 1.5)),
            CHROME_TAB_RADIUS + 1.0,
            state_layer(CHROME_SURFACE, accent, 16),
        );
    }
    let shadow = egui::Rect::from_min_max(
        egui::pos2(r.left() + 4.0, r.bottom() - if active { 2.0 } else { 1.0 }),
        egui::pos2(r.right() - 4.0, r.bottom()),
    );
    ui.painter()
        .rect_filled(shadow, 1.0, tab_shadow_fill(active));
    ui.painter().rect(
        r,
        CHROME_TAB_RADIUS,
        fill,
        egui::Stroke::new(
            if active { 1.5 } else { 1.0 },
            if active || response.hovered() {
                state_layer(CHROME_OUTLINE, accent, if active { 150 } else { 96 })
            } else {
                tab_stroke(active)
            },
        ),
        egui::StrokeKind::Inside,
    );

    let indicator = egui::Rect::from_min_max(
        egui::pos2(
            r.left() + 7.0,
            if active {
                r.bottom() - 3.0
            } else {
                r.bottom() - 2.0
            },
        ),
        egui::pos2(
            if active {
                r.right() - 7.0
            } else {
                (r.left() + 24.0).min(r.right() - 7.0)
            },
            r.bottom(),
        ),
    );
    ui.painter().rect_filled(
        indicator,
        if active { 1.5 } else { 1.0 },
        if active {
            accent
        } else {
            accent.gamma_multiply(0.55)
        },
    );
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(r.left() + 12.0, r.center().y),
        egui::vec2(TAB_FAVICON_SIZE, TAB_FAVICON_SIZE),
    );
    paint_tab_identity_icon(ui, icon_rect, engine, favicon, active);

    if r.width() >= 72.0 {
        let badge_w = tab_engine_badge_width(engine);
        let badge = egui::Rect::from_center_size(
            egui::pos2(r.right() - 7.0 - badge_w / 2.0, r.center().y),
            egui::vec2(badge_w, TAB_ENGINE_BADGE_H),
        );
        let show_badge = r.width() >= 104.0;
        if show_badge {
            let badge_fill = if active {
                accent
            } else {
                state_layer(CHROME_TOOLBAR, accent, 38)
            };
            if active {
                ui.painter().rect_filled(
                    badge.expand2(egui::vec2(1.5, 1.0)),
                    TAB_ENGINE_BADGE_H / 2.0 + 1.0,
                    tab_badge_depth_fill(),
                );
            }
            ui.painter().rect(
                badge,
                TAB_ENGINE_BADGE_H / 2.0,
                badge_fill,
                egui::Stroke::new(1.0, accent.gamma_multiply(if active { 0.8 } else { 0.5 })),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                badge.center(),
                egui::Align2::CENTER_CENTER,
                engine_glyph(engine),
                font_id(CHROME_FONT - 3.0),
                if active { CHROME_TOOLBAR } else { accent },
            );
        }

        let text_left = icon_rect.right() + 6.0;
        let right_rail = if show_badge {
            badge.left() - 6.0
        } else {
            r.right() - 8.0
        };
        let chip_count = visible_tab_status_chip_count(r.width(), status_chips.len());
        let chip_slot_w = tab_status_chip_slot_width(chip_count);
        let text_right = if chip_count == 0 {
            right_rail
        } else {
            (right_rail - chip_slot_w - 5.0).max(text_left)
        };
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, r.top()),
            egui::pos2(text_right.max(text_left), r.bottom()),
        );
        let label_chars = (text_rect.width() / 6.0).floor() as usize;
        if label_chars > 1 && !label.is_empty() {
            let label = ellipsize(label, label_chars);
            ui.painter().with_clip_rect(text_rect).text(
                egui::pos2(text_left, r.center().y),
                egui::Align2::LEFT_CENTER,
                label,
                font_id(CHROME_FONT),
                tab_text(active),
            );
        }
        if chip_count > 0 {
            let chip_left = right_rail - chip_slot_w;
            paint_tab_status_chips(
                ui,
                egui::pos2(chip_left, r.center().y),
                &status_chips[..chip_count],
                active,
                accent,
            );
        }
    } else {
        ui.painter().circle_filled(
            egui::pos2(r.right() - 5.0, r.bottom() - 5.0),
            (r.width() * 0.10).clamp(1.8, 2.4),
            accent,
        );
    }

    mde_egui::focus::paint_focus_ring(ui.painter(), r, response.has_focus());
    response
}

fn paint_tab_drag_settle(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    engine: BrowserEngine,
    axis: TabAxis,
    motion: BrowserDragSettleMotion,
) {
    if !motion.active || motion.accent_alpha == 0 {
        return;
    }
    let accent = engine_accent(engine);
    let color =
        Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), motion.accent_alpha);
    let shifted = rect.translate(motion.offset);
    ui.painter().rect_stroke(
        shifted.shrink(1.0),
        CHROME_TAB_RADIUS,
        egui::Stroke::new(1.5, color),
        egui::StrokeKind::Inside,
    );
    let snap_edge = match axis {
        TabAxis::Horizontal => egui::Rect::from_min_max(
            egui::pos2(shifted.left() + 7.0, shifted.bottom() - 3.0),
            egui::pos2(shifted.right() - 7.0, shifted.bottom()),
        ),
        TabAxis::Vertical => egui::Rect::from_min_max(
            egui::pos2(shifted.left(), shifted.top() + 6.0),
            egui::pos2(shifted.left() + 3.0, shifted.bottom() - 6.0),
        ),
    };
    ui.painter().rect_filled(snap_edge, 1.5, color);
}

fn tab_engine_badge_width(engine: BrowserEngine) -> f32 {
    match engine {
        BrowserEngine::Cef | BrowserEngine::Servo => 22.0,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TabStatusChipTone {
    Dim,
    Accent,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TabStatusChip {
    pub(super) icon: ChromeIcon,
    pub(super) label: &'static str,
    pub(super) tone: TabStatusChipTone,
}

pub(super) fn tab_status_chips(tab: &Tab) -> Vec<TabStatusChip> {
    if tab.internal_page.is_some() {
        return Vec::new();
    }

    let mut chips = Vec::new();
    if tab.idle_suspended {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Pause,
            label: "Idle suspended",
            tone: TabStatusChipTone::Dim,
        });
    } else {
        match tab.session.state() {
            SessionState::Loading => chips.push(TabStatusChip {
                icon: ChromeIcon::Reload,
                label: "Loading",
                tone: TabStatusChipTone::Accent,
            }),
            SessionState::Live => {}
            SessionState::Crashed { .. } => chips.push(TabStatusChip {
                icon: ChromeIcon::Warning,
                label: "Crashed",
                tone: TabStatusChipTone::Error,
            }),
        }
    }
    if tab.container != ContainerProfile::None {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Privacy,
            label: tab.container.label(),
            tone: TabStatusChipTone::Accent,
        });
    }
    if tab.display_target != DisplayTarget::Current {
        chips.push(TabStatusChip {
            icon: ChromeIcon::View,
            label: tab.display_target.label(),
            tone: TabStatusChipTone::Accent,
        });
    }
    if tab.muted {
        chips.push(TabStatusChip {
            icon: ChromeIcon::VolumeOff,
            label: "Muted",
            tone: TabStatusChipTone::Dim,
        });
    }
    if !tab.autoplay_blocked {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Play,
            label: "Autoplay allowed",
            tone: TabStatusChipTone::Warn,
        });
    }
    if tab.force_dark {
        chips.push(TabStatusChip {
            icon: ChromeIcon::DarkMode,
            label: "Force dark",
            tone: TabStatusChipTone::Accent,
        });
    }
    if tab.reader_mode {
        chips.push(TabStatusChip {
            icon: ChromeIcon::View,
            label: "Reader mode",
            tone: TabStatusChipTone::Accent,
        });
    }
    if tab.user_scripts {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Edit,
            label: "Site fixups",
            tone: TabStatusChipTone::Accent,
        });
    }
    if tab.user_agent != UserAgentOverride::Default {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Engine,
            label: tab.user_agent.label(),
            tone: TabStatusChipTone::Dim,
        });
    }
    if tab.device_profile != DeviceProfile::Default {
        chips.push(TabStatusChip {
            icon: ChromeIcon::Page,
            label: tab.device_profile.label(),
            tone: TabStatusChipTone::Dim,
        });
    }
    chips
}

#[cfg(test)]
pub(super) fn tab_status_chip_labels(tab: &Tab) -> Vec<&'static str> {
    tab_status_chips(tab)
        .into_iter()
        .map(|chip| chip.label)
        .collect()
}

fn visible_tab_status_chip_count(width: f32, chip_count: usize) -> usize {
    if chip_count == 0 || width < 116.0 {
        0
    } else if width < 148.0 {
        chip_count.min(1)
    } else if width < 184.0 {
        chip_count.min(2)
    } else {
        chip_count.min(4)
    }
}

fn tab_status_chip_slot_width(chip_count: usize) -> f32 {
    if chip_count == 0 {
        0.0
    } else {
        chip_count as f32 * TAB_STATUS_CHIP
            + (chip_count.saturating_sub(1) as f32 * TAB_STATUS_CHIP_GAP)
    }
}

fn tab_status_chip_color(chip: TabStatusChip, accent: Color32) -> Color32 {
    match chip.tone {
        TabStatusChipTone::Dim => CHROME_TEXT_DIM,
        TabStatusChipTone::Accent => accent,
        TabStatusChipTone::Warn => CHROME_WARN,
        TabStatusChipTone::Error => CHROME_ERROR,
    }
}

fn paint_tab_status_chips(
    ui: &mut egui::Ui,
    left_center: egui::Pos2,
    chips: &[TabStatusChip],
    active: bool,
    accent: Color32,
) {
    for (idx, chip) in chips.iter().enumerate() {
        let x = left_center.x + idx as f32 * (TAB_STATUS_CHIP + TAB_STATUS_CHIP_GAP);
        let rect = egui::Rect::from_center_size(
            egui::pos2(x + TAB_STATUS_CHIP / 2.0, left_center.y),
            egui::vec2(TAB_STATUS_CHIP, TAB_STATUS_CHIP),
        );
        let color = tab_status_chip_color(*chip, accent);
        ui.painter().circle_filled(
            rect.center(),
            TAB_STATUS_CHIP / 2.0,
            state_layer(
                if active {
                    CHROME_TOOLBAR
                } else {
                    CHROME_SURFACE
                },
                color,
                20,
            ),
        );
        ui.painter().circle_stroke(
            rect.center(),
            TAB_STATUS_CHIP / 2.0,
            egui::Stroke::new(1.0, color.gamma_multiply(0.5)),
        );
        paint_chrome_icon(ui.painter(), rect.expand(2.0), chip.icon, color);
    }
}

fn paint_tab_identity_icon(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    engine: BrowserEngine,
    favicon: Option<&TextureHandle>,
    active: bool,
) {
    let accent = engine_accent(engine);
    ui.painter().circle_filled(
        rect.center(),
        TAB_FAVICON_SIZE / 2.0,
        if active {
            CHROME_TOOLBAR
        } else {
            state_layer(CHROME_TOOLBAR, accent, 28)
        },
    );
    ui.painter().circle_stroke(
        rect.center(),
        TAB_FAVICON_SIZE / 2.0,
        egui::Stroke::new(
            if active { 1.5 } else { 1.0 },
            accent.gamma_multiply(if active { 0.95 } else { 0.55 }),
        ),
    );
    if let Some(texture) = favicon {
        ui.painter().image(
            texture.id(),
            rect.shrink(1.5),
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            engine_glyph(engine),
            font_id(CHROME_FONT - 2.0),
            accent,
        );
    }
}

pub(super) fn inline_close_button(ui: &mut egui::Ui) -> egui::Response {
    let enabled = ui.is_enabled();
    let (rect, response) = allocate_browser_icon_button(
        ui,
        enabled,
        egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H),
        "Close tab",
    );
    paint_transparent_icon_button_state(
        ui,
        &response,
        rect,
        CHROME_TAB_CLOSE / 2.0,
        CHROME_TEXT_DIM,
        enabled,
    );
    paint_chrome_icon(ui.painter(), rect, ChromeIcon::Close, CHROME_TEXT_DIM);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, "Close tab")
}

/// Which audio icon (and hover label) a tab shows, if any.
pub(super) const fn audio_icon_for(
    audible: bool,
    muted: bool,
) -> Option<(ChromeIcon, &'static str)> {
    if muted {
        Some((ChromeIcon::VolumeOff, "Unmute tab"))
    } else if audible {
        Some((ChromeIcon::VolumeUp, "Mute tab"))
    } else {
        None
    }
}

pub(super) fn tab_audio_button(
    ui: &mut egui::Ui,
    audible: bool,
    muted: bool,
) -> Option<egui::Response> {
    let (icon, hover) = audio_icon_for(audible, muted)?;
    let enabled = ui.is_enabled();
    let (rect, response) = allocate_browser_icon_button(
        ui,
        enabled,
        egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H),
        hover,
    );
    paint_transparent_icon_button_state(
        ui,
        &response,
        rect,
        CHROME_TAB_CLOSE / 2.0,
        CHROME_TEXT_DIM,
        enabled,
    );
    paint_chrome_icon(ui.painter(), rect, icon, CHROME_TEXT_DIM);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    Some(chrome_hover_text(response, hover))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserMediaToolbarModel {
    pub(super) label: String,
    pub(super) paused: bool,
    pub(super) background: bool,
    pub(super) audible: bool,
    pub(super) muted: bool,
}

pub(super) fn browser_media_toolbar_model(state: &WebState) -> Option<BrowserMediaToolbarModel> {
    let tab_index = super::wire::browser_media_status_tab_index(state)?;
    let tab = state.tabs.get(tab_index)?;
    let metadata = tab.session.media_metadata()?;
    let label = media_metadata_chip_label(&metadata.body)?;
    let value: serde_json::Value = serde_json::from_str(&metadata.body).ok()?;
    let paused = value
        .get("paused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| !tab.session.audible());
    Some(BrowserMediaToolbarModel {
        label,
        paused,
        background: tab_index != state.active,
        audible: tab.session.audible(),
        muted: tab.muted,
    })
}

pub(super) const fn media_toolbar_play_action(
    paused: bool,
) -> (ChromeIcon, &'static str, MediaTransportAction) {
    if paused {
        (
            ChromeIcon::Play,
            "Play browser media",
            MediaTransportAction::Play,
        )
    } else {
        (
            ChromeIcon::Pause,
            "Pause browser media",
            MediaTransportAction::Pause,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MediaToolbarDensity {
    Full,
    Compact,
    IconOnly,
    Hidden,
}

fn media_toolbar_trailing_nav_min_width() -> f32 {
    CHROME_BUTTON + CHROME_GAP + 160.0 + (CHROME_BUTTON * 2.0) + Style::SP_XL
}

pub(super) fn media_toolbar_estimated_width(density: MediaToolbarDensity) -> f32 {
    match density {
        MediaToolbarDensity::Full => {
            CHROME_GAP + 6.0 + MEDIA_CLUSTER_LABEL_W + (CHROME_BUTTON * 6.0) + (CHROME_GAP * 6.0)
        }
        MediaToolbarDensity::Compact => {
            CHROME_GAP
                + 6.0
                + MEDIA_CLUSTER_COMPACT_LABEL_W
                + (CHROME_BUTTON * 3.0)
                + (CHROME_GAP * 3.0)
        }
        MediaToolbarDensity::IconOnly => CHROME_GAP + CHROME_BUTTON,
        MediaToolbarDensity::Hidden => 0.0,
    }
}

pub(super) fn media_toolbar_density(available_width: f32) -> MediaToolbarDensity {
    if available_width.is_infinite() && available_width.is_sign_positive() {
        return MediaToolbarDensity::Full;
    }
    if !available_width.is_finite() {
        return MediaToolbarDensity::Hidden;
    }
    let media_budget = available_width - media_toolbar_trailing_nav_min_width();
    if media_budget >= media_toolbar_estimated_width(MediaToolbarDensity::Full) {
        MediaToolbarDensity::Full
    } else if media_budget >= media_toolbar_estimated_width(MediaToolbarDensity::Compact) {
        MediaToolbarDensity::Compact
    } else if media_budget >= media_toolbar_estimated_width(MediaToolbarDensity::IconOnly) {
        MediaToolbarDensity::IconOnly
    } else {
        MediaToolbarDensity::Hidden
    }
}

fn media_toolbar_label_width(density: MediaToolbarDensity) -> f32 {
    match density {
        MediaToolbarDensity::Full => MEDIA_CLUSTER_LABEL_W,
        MediaToolbarDensity::Compact => MEDIA_CLUSTER_COMPACT_LABEL_W,
        MediaToolbarDensity::IconOnly | MediaToolbarDensity::Hidden => 0.0,
    }
}

fn media_toolbar_label_text(label: &str, density: MediaToolbarDensity) -> String {
    let limit = match density {
        MediaToolbarDensity::Full => 32,
        MediaToolbarDensity::Compact => 18,
        MediaToolbarDensity::IconOnly | MediaToolbarDensity::Hidden => 0,
    };
    format!("    {}", ellipsize(label, limit))
}

fn media_toolbar_icon_button(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    tip: &str,
    action: MediaTransportAction,
) -> Option<MediaTransportAction> {
    chrome_icon_button(ui, icon, tip, true, false)
        .clicked()
        .then_some(action)
}

fn pip_icon_button_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    icon: ChromeIcon,
    tip: &str,
) -> egui::Response {
    let enabled = ui.is_enabled();
    let response = allocate_browser_icon_button_at(ui, rect, enabled, tip);
    paint_transparent_icon_button_state(
        ui,
        &response,
        rect,
        ICON_BUTTON_RADIUS,
        CHROME_TEXT,
        enabled,
    );
    paint_chrome_icon(ui.painter(), rect, icon, CHROME_TEXT);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn browser_media_toolbar(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(model) = browser_media_toolbar_model(state) else {
        return;
    };
    let (play_icon, play_tip, play_action) = media_toolbar_play_action(model.paused);
    let density = media_toolbar_density(ui.available_width());
    if density == MediaToolbarDensity::Hidden {
        return;
    }
    let mut picked = None;
    ui.add_space(CHROME_GAP);
    if density == MediaToolbarDensity::IconOnly {
        if let Some(action) = media_toolbar_icon_button(ui, play_icon, play_tip, play_action) {
            picked = Some(action);
        }
        if let Some(action) = picked {
            state.selected_media_transport(action);
        }
        return;
    }
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(ICON_BUTTON_RADIUS)
        .inner_margin(egui::Margin::symmetric(3, 1))
        .show(ui, |ui| {
            ui.set_height(CHROME_BUTTON);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = CHROME_GAP;
                let tone = if model.background {
                    CHROME_PRIMARY
                } else if model.muted {
                    CHROME_TEXT_DIM
                } else {
                    CHROME_TEXT
                };
                let label_text = media_toolbar_label_text(&model.label, density);
                let label = chrome_hover_text(
                    ui.add_sized(
                        egui::vec2(media_toolbar_label_width(density), CHROME_BUTTON),
                        egui::Label::new(RichText::new(label_text).size(CHROME_FONT).color(tone)),
                    ),
                    if model.background {
                        "Background Browser media"
                    } else if model.audible {
                        "Active Browser media"
                    } else {
                        "Browser media"
                    },
                );
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(label.rect.left() + 11.0, label.rect.center().y),
                    egui::vec2(14.0, 14.0),
                );
                paint_chrome_icon(ui.painter(), icon_rect, ChromeIcon::Audio, tone);
                if let Some(action) = media_toolbar_icon_button(
                    ui,
                    ChromeIcon::Previous,
                    "Previous browser media",
                    MediaTransportAction::Previous,
                ) {
                    picked = Some(action);
                }
                if let Some(action) =
                    media_toolbar_icon_button(ui, play_icon, play_tip, play_action)
                {
                    picked = Some(action);
                }
                if density == MediaToolbarDensity::Full {
                    if let Some(action) = media_toolbar_icon_button(
                        ui,
                        ChromeIcon::MediaStop,
                        "Stop browser media",
                        MediaTransportAction::Stop,
                    ) {
                        picked = Some(action);
                    }
                    if let Some(action) = media_toolbar_icon_button(
                        ui,
                        ChromeIcon::VolumeDown,
                        "Lower browser media volume",
                        MediaTransportAction::VolumeDown,
                    ) {
                        picked = Some(action);
                    }
                    if let Some(action) = media_toolbar_icon_button(
                        ui,
                        ChromeIcon::VolumeUp,
                        "Raise browser media volume",
                        MediaTransportAction::VolumeUp,
                    ) {
                        picked = Some(action);
                    }
                }
                if let Some(action) = media_toolbar_icon_button(
                    ui,
                    ChromeIcon::Next,
                    "Next browser media",
                    MediaTransportAction::Next,
                ) {
                    picked = Some(action);
                }
            });
        });
    if let Some(action) = picked {
        state.selected_media_transport(action);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BrowserMediaPipModel {
    pub(super) tab_index: usize,
    pub(super) label: String,
    pub(super) paused: bool,
    pub(super) background: bool,
    pub(super) audible: bool,
    pub(super) muted: bool,
    pub(super) frame_size: [usize; 2],
}

pub(super) fn browser_media_pip_model(state: &WebState) -> Option<BrowserMediaPipModel> {
    if !state.media_pip_open {
        return None;
    }
    let tab_index = super::wire::browser_media_status_tab_index(state)?;
    let tab = state.tabs.get(tab_index)?;
    if tab.internal_page.is_some() || tab.session.is_crashed() {
        return None;
    }
    let frame_size = tab.last_frame.as_ref()?.size;
    let metadata = tab.session.media_metadata()?;
    let label = media_metadata_chip_label(&metadata.body)?;
    let value: serde_json::Value = serde_json::from_str(&metadata.body).ok()?;
    let paused = value
        .get("paused")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| !tab.session.audible());
    Some(BrowserMediaPipModel {
        tab_index,
        label,
        paused,
        background: tab_index != state.active,
        audible: tab.session.audible(),
        muted: tab.muted,
        frame_size,
    })
}

pub(super) fn media_pip_overlay(ui: &mut egui::Ui, state: &mut WebState) {
    scope(ui, |ui| media_pip_overlay_contents(ui, state));
}

fn media_pip_overlay_contents(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(model) = browser_media_pip_model(state) else {
        return;
    };
    let Some((texture_id, texture_size)) = state.tabs.get(model.tab_index).and_then(|tab| {
        tab.texture
            .as_ref()
            .map(|texture| (texture.id(), texture.size_vec2()))
    }) else {
        return;
    };
    let (play_icon, play_tip, play_action) = media_toolbar_play_action(model.paused);
    let mut picked = None;
    let mut close = false;
    let mut focus_media_tab = false;
    let outer = ui.clip_rect();
    let pip_size = egui::vec2(
        MEDIA_PIP_W + 12.0,
        MEDIA_PIP_VIDEO_H + CHROME_BUTTON.mul_add(3.0, CHROME_GAP + 12.0),
    );
    let min = egui::pos2(
        (outer.right() - MEDIA_PIP_MARGIN - pip_size.x).max(outer.left() + MEDIA_PIP_MARGIN),
        (outer.bottom() - MEDIA_PIP_MARGIN - pip_size.y).max(outer.top() + MEDIA_PIP_MARGIN),
    );
    let pip_rect = egui::Rect::from_min_size(min, pip_size);
    ui.painter()
        .rect_filled(pip_rect, ICON_BUTTON_RADIUS, CHROME_TOOLBAR);
    ui.painter().rect_stroke(
        pip_rect,
        ICON_BUTTON_RADIUS,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );

    let content = pip_rect.shrink(6.0);
    let icon_color = if model.background {
        CHROME_PRIMARY
    } else if model.muted {
        CHROME_TEXT_DIM
    } else {
        CHROME_TEXT
    };
    let icon_rect = egui::Rect::from_min_size(content.left_top(), egui::vec2(20.0, 20.0));
    paint_chrome_icon(
        ui.painter(),
        icon_rect,
        ChromeIcon::PictureInPicture,
        icon_color,
    );
    let close_rect = egui::Rect::from_min_size(
        egui::pos2(content.right() - CHROME_BUTTON, content.top()),
        egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
    );
    if pip_icon_button_at(ui, close_rect, ChromeIcon::Close, "Close PiP").clicked() {
        close = true;
    }
    let title_rect = egui::Rect::from_min_max(
        egui::pos2(icon_rect.right() + CHROME_GAP, content.top()),
        egui::pos2(
            close_rect.left() - CHROME_GAP,
            content.top() + CHROME_BUTTON,
        ),
    );
    ui.put(
        title_rect,
        egui::Label::new(
            RichText::new("Picture-in-Picture")
                .size(CHROME_FONT)
                .color(CHROME_TEXT),
        ),
    );
    let label_rect = egui::Rect::from_min_size(
        egui::pos2(content.left(), title_rect.bottom()),
        egui::vec2(content.width(), CHROME_BUTTON),
    );
    ui.put(
        label_rect,
        egui::Label::new(RichText::new(model.label.as_str()).size(CHROME_FONT).color(
            if model.audible && !model.muted {
                CHROME_TEXT
            } else {
                CHROME_TEXT_DIM
            },
        )),
    );
    let video_rect = egui::Rect::from_min_size(
        egui::pos2(content.left(), label_rect.bottom()),
        egui::vec2(content.width(), MEDIA_PIP_VIDEO_H),
    );
    let video_resp = ui.interact(
        video_rect,
        egui::Id::new(("browser-media-pip-video", model.tab_index)),
        egui::Sense::click(),
    );
    ui.painter()
        .rect_filled(video_rect, ICON_BUTTON_RADIUS, page_backdrop_fill());
    let image_rect = fit_rect_preserving_aspect(video_rect.shrink(2.0), texture_size);
    egui::Image::new(egui::load::SizedTexture::new(texture_id, image_rect.size()))
        .paint_at(ui, image_rect);
    ui.painter().rect_stroke(
        video_rect,
        ICON_BUTTON_RADIUS,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    if video_resp.clicked() {
        focus_media_tab = true;
    }
    let _ = chrome_hover_text(
        video_resp,
        if model.background {
            "Show media tab"
        } else {
            "Browser media"
        },
    );

    let controls_top = video_rect.bottom() + CHROME_GAP;
    let mut button_left = content.left();
    let mut button = |ui: &mut egui::Ui,
                      icon: ChromeIcon,
                      tip: &str,
                      action: MediaTransportAction,
                      picked: &mut Option<MediaTransportAction>| {
        let rect = egui::Rect::from_min_size(
            egui::pos2(button_left, controls_top),
            egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
        );
        button_left += CHROME_BUTTON + CHROME_GAP;
        if pip_icon_button_at(ui, rect, icon, tip).clicked() {
            *picked = Some(action);
        }
    };
    button(
        ui,
        ChromeIcon::Previous,
        "Previous browser media",
        MediaTransportAction::Previous,
        &mut picked,
    );
    button(ui, play_icon, play_tip, play_action, &mut picked);
    button(
        ui,
        ChromeIcon::MediaStop,
        "Stop browser media",
        MediaTransportAction::Stop,
        &mut picked,
    );
    button(
        ui,
        ChromeIcon::VolumeDown,
        "Lower browser media volume",
        MediaTransportAction::VolumeDown,
        &mut picked,
    );
    button(
        ui,
        ChromeIcon::VolumeUp,
        "Raise browser media volume",
        MediaTransportAction::VolumeUp,
        &mut picked,
    );
    button(
        ui,
        ChromeIcon::Next,
        "Next browser media",
        MediaTransportAction::Next,
        &mut picked,
    );

    if close {
        state.media_pip_open = false;
    } else if focus_media_tab {
        state.select_tab(model.tab_index);
    }
    if let Some(action) = picked {
        state.selected_media_transport(action);
    }
}

fn tab_context_menu_row(
    ui: &mut egui::Ui,
    label: &str,
    icon: ChromeIcon,
    enabled: bool,
) -> egui::Response {
    chrome_menu_row(ui, label, icon, enabled, "Unavailable for this tab")
}

fn menu_icon(title: &str) -> ChromeIcon {
    match title {
        "Page" => ChromeIcon::Page,
        "Engine" => ChromeIcon::Engine,
        "Edit" => ChromeIcon::Edit,
        "View" => ChromeIcon::View,
        "History" => ChromeIcon::History,
        "Privacy" => ChromeIcon::Privacy,
        "Bookmarks" => ChromeIcon::Bookmark,
        "Power" => ChromeIcon::Power,
        _ => ChromeIcon::Options,
    }
}

fn technical_menu_label(title: &str) -> &'static str {
    match title {
        "Page" => "Navigation",
        "Engine" => "Engines",
        "Edit" => "Input",
        "View" => "Rendering",
        "History" => "Session",
        "Privacy" => "Storage",
        "Bookmarks" => "Bookmarks",
        "Power" => "Instrumentation",
        _ => "Controls",
    }
}

fn action_icon(action: super::menubar::MenuAction) -> ChromeIcon {
    use super::menubar::MenuAction;
    match action {
        MenuAction::Back => ChromeIcon::Back,
        MenuAction::Forward => ChromeIcon::Forward,
        MenuAction::Reload => ChromeIcon::Reload,
        MenuAction::OpenAddress => ChromeIcon::Page,
        MenuAction::SelectEngine(_) => ChromeIcon::Engine,
        MenuAction::ToggleVerticalTabs => ChromeIcon::Tabs,
        MenuAction::ToggleDownloads => ChromeIcon::Downloads,
        MenuAction::ToggleNotifications => ChromeIcon::Notifications,
        MenuAction::ToggleRecommendations => ChromeIcon::Recommend,
        MenuAction::ToggleHistory | MenuAction::ReopenClosedTab => ChromeIcon::History,
        MenuAction::ToggleBookmarksBar
        | MenuAction::AddBookmark
        | MenuAction::OpenBookmarksManager => ChromeIcon::Bookmark,
        MenuAction::TogglePowerMode => ChromeIcon::Power,
        MenuAction::ZoomIn => ChromeIcon::ZoomIn,
        MenuAction::ZoomOut => ChromeIcon::ZoomOut,
        MenuAction::ResetZoom => ChromeIcon::Search,
        MenuAction::OpenFind => ChromeIcon::Find,
        MenuAction::ToggleAudioMute | MenuAction::ToggleMediaPlayback => ChromeIcon::Audio,
        MenuAction::TogglePictureInPicture => ChromeIcon::PictureInPicture,
        MenuAction::ToggleAutoplayBlock
        | MenuAction::ToggleForceDark
        | MenuAction::ToggleReaderMode
        | MenuAction::ToggleUserScripts
        | MenuAction::OpenSiteStyles => ChromeIcon::DarkMode,
        MenuAction::CaptureViewport
        | MenuAction::CaptureFullPage
        | MenuAction::CaptureMhtml
        | MenuAction::CaptureAnnotatedViewport
        | MenuAction::CaptureCalloutViewport
        | MenuAction::CaptureFreehandViewport
        | MenuAction::CaptureRegion => ChromeIcon::Capture,
        MenuAction::PrintPage
        | MenuAction::TogglePrintSettings
        | MenuAction::SavePdf
        | MenuAction::OpenLastPdf => ChromeIcon::Print,
        MenuAction::ToggleSiteBlocking
        | MenuAction::ForgetSitePermissions
        | MenuAction::ClearCurrentTabData
        | MenuAction::ClearAllBrowsingData => ChromeIcon::Privacy,
        MenuAction::CopyUrl
        | MenuAction::SendInChat
        | MenuAction::ShareToPeer
        | MenuAction::ShareToPhone
        | MenuAction::ShareToEmail
        | MenuAction::ShareToQr
        | MenuAction::SendTabToNode
        | MenuAction::SendTabToPhone => ChromeIcon::Share,
        MenuAction::OpenViewSource
        | MenuAction::OpenChromiumDevtools
        | MenuAction::ExportActivePageScrape
        | MenuAction::ExportMediaManifest
        | MenuAction::DownloadObservedMedia
        | MenuAction::DownloadObservedImages
        | MenuAction::CycleUserAgent
        | MenuAction::CycleDeviceProfile
        | MenuAction::PromptCameraPermission
        | MenuAction::PromptMicrophonePermission
        | MenuAction::PromptLocationPermission
        | MenuAction::PromptNotificationsPermission
        | MenuAction::PromptClipboardPermission
        | MenuAction::CheckSpelling
        | MenuAction::ReadAloud
        | MenuAction::TranslatePage
        | MenuAction::SaveOfflineCopy
        | MenuAction::VoiceCommand
        | MenuAction::Dictate
        | MenuAction::CycleContainer
        | MenuAction::CycleDisplayTarget => ChromeIcon::Options,
    }
}

fn option_row_width(available_width: f32) -> f32 {
    available_width.max(0.0).min(OPTION_ROW_MAX_W)
}

fn disabled_option_tip(action: super::menubar::MenuAction) -> &'static str {
    use super::menubar::MenuAction;
    match action {
        MenuAction::OpenAddress => "Type an address in a live tab first",
        MenuAction::Back => "No back-history entry is available",
        MenuAction::Forward => "No forward-history entry is available",
        MenuAction::ReopenClosedTab => "No closed Browser tab is available to reopen",
        MenuAction::Reload => "Open a Browser tab first",
        MenuAction::TogglePowerMode => "Power Mode requires an open Browser tab",
        MenuAction::CycleContainer
        | MenuAction::CycleDisplayTarget
        | MenuAction::ZoomIn
        | MenuAction::ZoomOut
        | MenuAction::ResetZoom
        | MenuAction::OpenFind
        | MenuAction::ToggleAudioMute
        | MenuAction::ToggleMediaPlayback
        | MenuAction::ToggleAutoplayBlock
        | MenuAction::ToggleForceDark
        | MenuAction::ToggleReaderMode
        | MenuAction::ToggleUserScripts
        | MenuAction::OpenSiteStyles
        | MenuAction::CheckSpelling
        | MenuAction::ReadAloud
        | MenuAction::TranslatePage
        | MenuAction::SaveOfflineCopy
        | MenuAction::VoiceCommand
        | MenuAction::Dictate
        | MenuAction::PrintPage
        | MenuAction::TogglePrintSettings
        | MenuAction::SavePdf
        | MenuAction::CycleUserAgent
        | MenuAction::CycleDeviceProfile => "Requires a live web page",
        MenuAction::CaptureViewport
        | MenuAction::CaptureFullPage
        | MenuAction::CaptureMhtml
        | MenuAction::CaptureAnnotatedViewport
        | MenuAction::CaptureCalloutViewport
        | MenuAction::CaptureFreehandViewport
        | MenuAction::CaptureRegion => "Requires a live page with a painted frame",
        MenuAction::TogglePictureInPicture => "Start video playback in a Browser tab first",
        MenuAction::OpenLastPdf => "Requires a readable PDF saved from Browser",
        MenuAction::OpenChromiumDevtools => "Requires a live Chromium page",
        MenuAction::OpenViewSource
        | MenuAction::ExportActivePageScrape
        | MenuAction::ExportMediaManifest
        | MenuAction::DownloadObservedMedia
        | MenuAction::DownloadObservedImages
        | MenuAction::CopyUrl
        | MenuAction::AddBookmark
        | MenuAction::SendInChat
        | MenuAction::ShareToPeer
        | MenuAction::ShareToPhone
        | MenuAction::ShareToEmail
        | MenuAction::ShareToQr
        | MenuAction::SendTabToNode
        | MenuAction::SendTabToPhone => "Requires a loaded page URL",
        MenuAction::PromptCameraPermission
        | MenuAction::PromptMicrophonePermission
        | MenuAction::PromptLocationPermission
        | MenuAction::PromptNotificationsPermission
        | MenuAction::PromptClipboardPermission
        | MenuAction::ToggleSiteBlocking
        | MenuAction::ForgetSitePermissions => "Requires a loaded first-party site",
        MenuAction::ClearCurrentTabData | MenuAction::ClearAllBrowsingData => {
            "Open a non-crashed Browser tab first"
        }
        MenuAction::SelectEngine(_)
        | MenuAction::ToggleVerticalTabs
        | MenuAction::ToggleDownloads
        | MenuAction::ToggleNotifications
        | MenuAction::ToggleRecommendations
        | MenuAction::ToggleHistory
        | MenuAction::ToggleBookmarksBar
        | MenuAction::OpenBookmarksManager => "Available from Browser Options",
    }
}

fn bounded_available_width(ui: &egui::Ui) -> f32 {
    let clip_remaining = (ui.clip_rect().right() - ui.next_widget_position().x).max(0.0);
    ui.available_width().max(0.0).min(clip_remaining)
}

fn options_compact_layout(available_width: f32) -> bool {
    available_width < OPTIONS_COMPACT_BREAKPOINT
}

fn option_row_accesskit_id(action: super::menubar::MenuAction) -> egui::Id {
    egui::Id::new(("browser-options-row", format!("{action:?}")))
}

fn option_row_accesskit_value(
    item: &mde_egui::menubar::Item<super::menubar::MenuAction>,
) -> String {
    let mut parts = Vec::new();
    if item.enabled {
        match item.checked {
            Some(true) => parts.push("On".to_owned()),
            Some(false) => parts.push("Off".to_owned()),
            None => parts.push("Available".to_owned()),
        }
    } else {
        parts.push(format!("Unavailable: {}", disabled_option_tip(item.id)));
    }
    if let Some(shortcut) = &item.shortcut {
        parts.push(format!("Shortcut {shortcut}"));
    }
    parts.join(", ")
}

fn install_option_row_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    item: &mde_egui::menubar::Item<super::menubar::MenuAction>,
) {
    let _ = ctx.accesskit_node_builder(option_row_accesskit_id(item.id), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(item.label.as_str());
        node.set_value(option_row_accesskit_value(item));
        node.set_bounds(accesskit_rect(rect));
        if item.enabled {
            node.add_action(egui::accesskit::Action::Click);
        }
        if item.checked == Some(true) {
            node.set_selected(true);
        }
    });
}

fn option_row(
    ui: &mut egui::Ui,
    item: &mde_egui::menubar::Item<super::menubar::MenuAction>,
) -> Option<super::menubar::MenuAction> {
    let width = option_row_width(bounded_available_width(ui));
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, OPTION_ROW_H),
        if item.enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let selected = item.checked == Some(true);
    let fill = animated_response_fill(
        ui,
        &response,
        menu_item_fill(selected),
        CHROME_TEXT,
        item.enabled,
    );
    ui.painter().rect(
        rect,
        8.0,
        fill,
        egui::Stroke::new(
            1.0,
            if selected {
                CHROME_PRIMARY
            } else {
                CHROME_OUTLINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    install_option_row_accessibility(ui.ctx(), rect, item);
    let text_color = button_text(item.enabled);
    let icon_color = if selected {
        CHROME_ON_PRIMARY_CONTAINER
    } else if item.enabled {
        CHROME_ICON
    } else {
        CHROME_TEXT_DIM
    };
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 17.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
    );
    paint_chrome_icon(ui.painter(), icon_rect, action_icon(item.id), icon_color);
    if selected {
        let check_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 15.0, rect.center().y),
            egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
        );
        paint_chrome_icon(ui.painter(), check_rect, ChromeIcon::Check, CHROME_PRIMARY);
    } else if !item.enabled {
        let lock_rect = egui::Rect::from_center_size(
            egui::pos2(rect.right() - 15.0, rect.center().y),
            egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
        );
        paint_chrome_icon(ui.painter(), lock_rect, ChromeIcon::Lock, CHROME_TEXT_DIM);
    }
    let trailing_reserved = if selected || !item.enabled {
        34.0
    } else {
        12.0
    };
    let label_left = rect.left() + 34.0;
    let label_right = (rect.right()
        - if item.shortcut.is_some() {
            116.0
        } else {
            trailing_reserved
        })
    .max(label_left);
    let label_clip = egui::Rect::from_min_max(
        egui::pos2(label_left, rect.top()),
        egui::pos2(label_right, rect.bottom()),
    )
    .intersect(rect);
    ui.painter().with_clip_rect(label_clip).text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        item.label.as_str(),
        font_id(CHROME_FONT + 1.0),
        text_color,
    );
    if let Some(shortcut) = &item.shortcut {
        let shortcut_right = rect.right() - trailing_reserved;
        let shortcut_left = (shortcut_right - 104.0).max(label_left + 8.0);
        let shortcut_clip = egui::Rect::from_min_max(
            egui::pos2(shortcut_left.min(shortcut_right), rect.top()),
            egui::pos2(shortcut_right, rect.bottom()),
        )
        .intersect(rect);
        ui.painter().with_clip_rect(shortcut_clip).text(
            egui::pos2(
                rect.right() - if selected { 34.0 } else { 12.0 },
                rect.center().y,
            ),
            egui::Align2::RIGHT_CENTER,
            shortcut.as_str(),
            font_id(CHROME_FONT),
            CHROME_TEXT_DIM,
        );
    }
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    let response = if item.enabled {
        response
    } else {
        chrome_hover_text(response, disabled_option_tip(item.id))
    };
    (response.clicked() && item.enabled).then_some(item.id)
}

fn render_options_entries(
    ui: &mut egui::Ui,
    entries: &[Entry<super::menubar::MenuAction>],
    picked: &mut Option<super::menubar::MenuAction>,
) {
    for entry in entries {
        match entry {
            Entry::Item(item) => {
                if let Some(action) = option_row(ui, item) {
                    *picked = Some(action);
                }
                ui.add_space(3.0);
            }
            Entry::Submenu { label, entries, .. } => {
                ui.label(
                    RichText::new(label.as_str())
                        .size(CHROME_FONT + 1.0)
                        .color(CHROME_TEXT),
                );
                render_options_entries(ui, entries, picked);
            }
            Entry::Separator => {
                chrome_separator(ui);
            }
            Entry::Caption(caption) => {
                ui.label(
                    RichText::new(caption.as_str())
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT_DIM),
                );
                ui.add_space(2.0);
            }
        }
    }
}

fn render_options_category_index(
    ui: &mut egui::Ui,
    menus: &[mde_egui::menubar::Menu<super::menubar::MenuAction>],
    compact: bool,
) {
    if compact {
        let width = bounded_available_width(ui);
        ui.set_width(width);
        ui.set_max_width(width);
    }
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(8.0)
        .inner_margin(chrome_options_card_margin())
        .show(ui, |ui| {
            if !compact {
                ui.set_width(OPTIONS_RAIL_W);
            }
            ui.label(
                RichText::new("Controls")
                    .size(CHROME_FONT + 3.0)
                    .color(CHROME_TEXT),
            );
            ui.label(
                RichText::new(super::BROWSER_OPTIONS_URL)
                    .size(CHROME_FONT)
                    .color(CHROME_TEXT_DIM),
            );
            ui.add_space(8.0);
            if compact {
                let chip_max_width = bounded_available_width(ui);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                    for menu in menus {
                        render_options_category_chip(
                            ui,
                            technical_menu_label(&menu.title),
                            menu_icon(&menu.title),
                            chip_max_width,
                        );
                    }
                });
            } else {
                for menu in menus {
                    ui.horizontal(|ui| {
                        let rect = egui::Rect::from_center_size(
                            ui.next_widget_position() + egui::vec2(8.0, 8.0),
                            egui::vec2(OPTION_ICON_SIZE - 2.0, OPTION_ICON_SIZE - 2.0),
                        );
                        ui.allocate_space(egui::vec2(20.0, 18.0));
                        paint_chrome_icon(
                            ui.painter(),
                            rect,
                            menu_icon(&menu.title),
                            CHROME_TEXT_DIM,
                        );
                        ui.label(
                            RichText::new(technical_menu_label(&menu.title))
                                .size(CHROME_FONT)
                                .color(CHROME_TEXT),
                        );
                    });
                    ui.add_space(3.0);
                }
            }
        });
}

fn options_category_chip_width(label: &str, max_width: f32) -> f32 {
    let max_width = max_width.max(0.0);
    if max_width == 0.0 {
        return 0.0;
    }
    let min_width = OPTIONS_CATEGORY_CHIP_MIN_W.min(max_width);
    let max_width = OPTIONS_CATEGORY_CHIP_MAX_W.min(max_width).max(min_width);
    let text_width = label.chars().count() as f32 * 7.0 + 34.0;
    text_width.clamp(min_width, max_width)
}

fn render_options_category_chip(ui: &mut egui::Ui, label: &str, icon: ChromeIcon, max_width: f32) {
    let width = options_category_chip_width(label, max_width);
    if width <= 0.0 {
        return;
    }
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, OPTIONS_CATEGORY_CHIP_H),
        egui::Sense::hover(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, label));
    ui.painter().rect(
        rect,
        7.0,
        CHROME_SURFACE,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE - 2.0, OPTION_ICON_SIZE - 2.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, icon, CHROME_TEXT_DIM);
    let text_right = rect.right() - 7.0;
    let text_left = icon_rect.right() + 6.0;
    if text_left < text_right {
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, rect.top()),
            egui::pos2(text_right, rect.bottom()),
        );
        ui.painter().with_clip_rect(text_rect).text(
            egui::pos2(text_rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            font_id(CHROME_FONT),
            CHROME_TEXT,
        );
    }
}

fn render_options_command_page(
    ui: &mut egui::Ui,
    menus: &[mde_egui::menubar::Menu<super::menubar::MenuAction>],
    picked: &mut Option<super::menubar::MenuAction>,
    compact: bool,
) {
    if compact {
        let width = bounded_available_width(ui);
        ui.set_width(width);
        ui.set_max_width(width);
    } else {
        ui.set_min_width(ui.available_width().max(0.0).min(OPTIONS_CONTENT_MIN_W));
    }
    ui.label(
        RichText::new("Browser Options")
            .size(CHROME_FONT + 8.0)
            .color(CHROME_TEXT),
    );
    ui.label(
        RichText::new("Browser commands and site controls")
            .size(CHROME_FONT)
            .color(CHROME_TEXT_DIM),
    );
    ui.add_space(10.0);
    for menu in menus {
        if compact {
            let width = bounded_available_width(ui);
            ui.set_width(width);
            ui.set_max_width(width);
        }
        egui::Frame::NONE
            .fill(CHROME_SURFACE_CONTAINER)
            .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
            .corner_radius(8.0)
            .inner_margin(chrome_options_card_margin())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let rect = egui::Rect::from_center_size(
                        ui.next_widget_position() + egui::vec2(9.0, 9.0),
                        egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
                    );
                    ui.allocate_space(egui::vec2(22.0, 20.0));
                    paint_chrome_icon(ui.painter(), rect, menu_icon(&menu.title), CHROME_PRIMARY);
                    ui.label(
                        RichText::new(technical_menu_label(&menu.title))
                            .size(CHROME_FONT + 4.0)
                            .color(CHROME_TEXT),
                    );
                });
                ui.add_space(5.0);
                render_options_entries(ui, &menu.entries, picked);
            });
        ui.add_space(10.0);
    }
}

pub(super) fn options_page(ui: &mut egui::Ui, state: &mut WebState) {
    let menus = super::menubar::chrome_menus(state);
    let mut picked = None;
    let page_rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    if !page_rect.is_positive() {
        return;
    }
    ui.allocate_rect(page_rect, egui::Sense::hover());
    ui.painter().rect_filled(page_rect, 0.0, CHROME_SURFACE);

    let inner_rect = page_rect.shrink2(egui::vec2(10.0, 8.0));
    if !inner_rect.is_positive() {
        return;
    }
    let mut page_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    page_ui.set_clip_rect(page_rect);

    let compact = options_compact_layout(bounded_available_width(&page_ui));
    if compact {
        let page_width = bounded_available_width(&page_ui);
        let page_height = page_ui.available_height().max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("browser-options-page")
            .max_width(page_width)
            .max_height(page_height)
            .auto_shrink([false, false])
            .show(&mut page_ui, |ui| {
                ui.set_width(page_width);
                ui.set_max_width(page_width);
                render_options_category_index(ui, &menus, true);
                ui.add_space(10.0);
                render_options_command_page(ui, &menus, &mut picked, true);
            });
    } else {
        let page_height = page_ui.available_height().max(0.0);
        page_ui.horizontal(|ui| {
            ui.set_height(page_height);
            ui.allocate_ui_with_layout(
                egui::vec2(OPTIONS_RAIL_W, page_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(OPTIONS_RAIL_W);
                    ui.set_max_width(OPTIONS_RAIL_W);
                    render_options_category_index(ui, &menus, false);
                },
            );
            ui.add_space(OPTIONS_WIDE_GAP);
            let page_width = bounded_available_width(ui);
            ui.allocate_ui_with_layout(
                egui::vec2(page_width, page_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(page_width);
                    ui.set_max_width(page_width);
                    egui::ScrollArea::vertical()
                        .id_salt("browser-options-page")
                        .max_width(page_width)
                        .max_height(page_height)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.set_width(page_width);
                            ui.set_max_width(page_width);
                            render_options_command_page(ui, &menus, &mut picked, false);
                        });
                },
            );
        });
    }
    if let Some(action) = picked {
        super::menubar::apply(ui.ctx(), state, action);
    }
}

pub(super) fn tab_label(tab: &Tab) -> String {
    if let Some(page) = tab.internal_page {
        return page.title().to_owned();
    }
    let title = tab.session.title().trim();
    let url = tab.session.nav().url.trim();
    let base = if !title.is_empty() {
        title
    } else if !url.is_empty() {
        url
    } else {
        "New tab"
    };
    ellipsize(base, 28)
}

pub(super) fn tab_hover(tab: &Tab) -> String {
    if let Some(page) = tab.internal_page {
        return format!("{} - {}", page.title(), page.url());
    }
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
    let engine = format!(" - Engine: {}", engine_display_name(tab.engine));
    let audio = if tab.muted { " - Muted" } else { "" };
    let now_playing = tab
        .session
        .media_metadata()
        .and_then(|metadata| media_metadata_chip_label(&metadata.body))
        .map_or_else(String::new, |label| format!(" - {label}"));
    let autoplay = if tab.autoplay_blocked {
        " - Autoplay blocked"
    } else {
        ""
    };
    let force_dark = if tab.force_dark { " - Force dark" } else { "" };
    let reader = if tab.reader_mode { " - Reader" } else { "" };
    let user_scripts = if tab.user_scripts {
        " - Site fixups"
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
            "{state}{engine}{container}{display}{audio}{now_playing}{autoplay}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    } else {
        format!(
            "{state} - {url}{engine}{container}{display}{audio}{now_playing}{autoplay}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    }
}

/// Fit a native page-frame size into a thumbnail no wider than `max_w`, preserving
/// aspect ratio; zero for a degenerate frame.
pub(super) fn thumbnail_size(native: egui::Vec2, max_w: f32) -> egui::Vec2 {
    if native.x <= 0.0 || native.y <= 0.0 {
        return egui::Vec2::ZERO;
    }
    let w = max_w.min(native.x);
    egui::vec2(w, w * native.y / native.x)
}

pub(super) fn tab_hover_card(ui: &mut egui::Ui, tab: &Tab) {
    chrome_tooltip_frame(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let (icon_rect, _) =
                ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
            paint_chrome_icon(ui.painter(), icon_rect, ChromeIcon::Tabs, CHROME_TEXT_DIM);
            ui.add(
                egui::Label::new(
                    RichText::new(tab_hover(tab))
                        .size(Style::SMALL)
                        .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        });
        if let Some(tex) = &tab.texture {
            let size = thumbnail_size(tex.size_vec2(), 240.0);
            if size.x > 0.0 {
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    tex.id(),
                    size,
                )));
            }
        }
    });
}

/// Case-insensitive match of `query` against each tab's title AND committed URL;
/// returns the matching tab indices in strip order. An empty/blank query matches
/// everything. Pure so the tab-search dropdown and tests share one rule.
pub(super) fn matching_tab_indices(tabs: &[Tab], query: &str) -> Vec<usize> {
    let q = query.trim().to_ascii_lowercase();
    tabs.iter()
        .enumerate()
        .filter(|(_, tab)| {
            let internal_match = tab.internal_page.is_some_and(|page| {
                page.title().to_ascii_lowercase().contains(&q)
                    || page.url().to_ascii_lowercase().contains(&q)
            });
            q.is_empty()
                || internal_match
                || tab.session.title().to_ascii_lowercase().contains(&q)
                || tab.session.nav().url.to_ascii_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// A one-line label for a tab-search result row: page title, URL, then "New tab".
fn tab_search_row_label(tab: &Tab) -> String {
    if let Some(page) = tab.internal_page {
        return page.title().to_owned();
    }
    let title = tab.session.title().trim();
    if !title.is_empty() {
        return ellipsize(title, 48);
    }
    let url = tab.session.nav().url.trim();
    if url.is_empty() {
        "New tab".to_owned()
    } else {
        ellipsize(url, 48)
    }
}

fn tab_search_separator(ui: &mut egui::Ui) {
    ui.add_space(5.0);
    let width = ui.available_width().max(1.0);
    let rect = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(width, 1.0));
    ui.painter()
        .hline(rect.x_range(), rect.center().y, (1.0, CHROME_OUTLINE));
    ui.allocate_space(egui::vec2(width, 1.0));
    ui.add_space(5.0);
}

fn tab_search_panel_width(available_width: f32) -> f32 {
    let available_width = available_width.max(0.0);
    if available_width == 0.0 {
        return 0.0;
    }
    let min_width = TAB_SEARCH_PANEL_MIN_W.min(available_width);
    available_width.min(TAB_SEARCH_PANEL_W).max(min_width)
}

fn tab_search_row_width(available_width: f32) -> f32 {
    available_width.max(0.0)
}

fn tab_search_clear_visible(query_non_empty: bool, available_width: f32) -> bool {
    query_non_empty && available_width >= CHROME_BUTTON + TAB_SEARCH_EDIT_MIN_W
}

fn tab_search_edit_width(available_width: f32, clear_visible: bool) -> f32 {
    let reserved = if clear_visible { CHROME_BUTTON } else { 0.0 };
    (available_width.max(0.0) - reserved).max(1.0)
}

fn tab_search_result_row(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let width = tab_search_row_width(bounded_available_width(ui)).max(1.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, CHROME_TAB_H), egui::Sense::click());
    let fill = animated_response_fill(ui, &response, row_fill(active), CHROME_TEXT, true);
    ui.painter().rect(
        rect,
        7.0,
        fill,
        egui::Stroke::new(1.0, tab_stroke(active)),
        egui::StrokeKind::Inside,
    );
    let text_color = selected_text(active);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE - 1.0, OPTION_ICON_SIZE - 1.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, ChromeIcon::Tabs, text_color);
    let text_left = icon_rect.right() + 6.0;
    let text_right = rect.right() - 8.0;
    if text_left < text_right {
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, rect.top()),
            egui::pos2(text_right, rect.bottom()),
        );
        ui.painter().with_clip_rect(text_rect).text(
            egui::pos2(text_rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            font_id(CHROME_FONT),
            text_color,
        );
    }
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    response
}

fn tab_search_result_accesskit_id(tab_id: u64) -> egui::Id {
    egui::Id::new(("browser-tab-search-result", tab_id))
}

fn install_tab_search_result_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    tab_id: u64,
    label: &str,
    active: bool,
    index: usize,
    total: usize,
) {
    let _ = ctx.accesskit_node_builder(tab_search_result_accesskit_id(tab_id), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(format!("Switch to tab {label}"));
        node.set_value(format!(
            "Tab {} of {}{}",
            index + 1,
            total,
            if active { ", active" } else { "" }
        ));
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
        if active {
            node.set_selected(true);
        }
    });
}

fn tab_search_results(ui: &mut egui::Ui, state: &WebState) -> Option<usize> {
    let mut select = None;
    tab_search_separator(ui);
    let matches = matching_tab_indices(&state.tabs, &state.tab_search_query);
    let total = state.tabs.len();
    egui::ScrollArea::vertical()
        .max_height(260.0)
        .show(ui, |ui| {
            if matches.is_empty() {
                browser_muted_note(ui, "No matching tabs");
            }
            for idx in matches {
                let active = idx == state.active;
                let label = tab_search_row_label(&state.tabs[idx]);
                let response = tab_search_result_row(ui, &label, active);
                install_tab_search_result_accessibility(
                    ui.ctx(),
                    response.rect,
                    state.tabs[idx].id,
                    &label,
                    active,
                    idx,
                    total,
                );
                if response.clicked() {
                    select = Some(idx);
                }
            }
        });
    select
}

fn tab_search_clear_button_id() -> egui::Id {
    egui::Id::new("mde_web_tab_search_clear")
}

fn tab_search_field(ui: &mut egui::Ui, query: &mut String) -> egui::Response {
    let outer = egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(ICON_BUTTON_RADIUS)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let icon_edge = OPTION_ICON_SIZE - 2.0;
                let (icon_slot, _) = ui.allocate_exact_size(
                    egui::vec2(icon_edge + 3.0, CHROME_OMNIBOX_H - 6.0),
                    egui::Sense::hover(),
                );
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(icon_slot.left() + icon_edge / 2.0, icon_slot.center().y),
                    egui::vec2(icon_edge, icon_edge),
                );
                paint_chrome_icon(ui.painter(), icon_rect, ChromeIcon::Search, CHROME_TEXT_DIM);

                let available = bounded_available_width(ui);
                let show_clear = tab_search_clear_visible(!query.is_empty(), available);
                let width = tab_search_edit_width(available, show_clear);
                let edit = egui::TextEdit::singleline(query)
                    .desired_width(width)
                    .hint_text(
                        RichText::new("Search tabs")
                            .size(Style::SMALL)
                            .color(CHROME_TEXT_DIM),
                    )
                    .text_color(CHROME_TEXT)
                    .font(font_id(Style::SMALL))
                    .background_color(CHROME_SURFACE)
                    .margin(egui::Margin::symmetric(0, 0))
                    .frame(false)
                    .min_size(egui::vec2(width, CHROME_OMNIBOX_H - 6.0));
                let response = chrome_hover_text(ui.add(edit), "Search tabs");

                if show_clear {
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
                        egui::Sense::hover(),
                    );
                    let clear_resp =
                        ui.interact(rect, tab_search_clear_button_id(), egui::Sense::click());
                    clear_resp.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            ui.is_enabled(),
                            "Clear tab search",
                        )
                    });
                    paint_transparent_icon_button_state(
                        ui,
                        &clear_resp,
                        rect,
                        ICON_BUTTON_RADIUS,
                        CHROME_TEXT,
                        true,
                    );
                    let tint = if clear_resp.hovered() || clear_resp.has_focus() {
                        CHROME_TEXT
                    } else {
                        CHROME_TEXT_DIM
                    };
                    paint_chrome_icon(ui.painter(), rect, ChromeIcon::Close, tint);
                    mde_egui::focus::paint_focus_ring(ui.painter(), rect, clear_resp.has_focus());
                    let clear_resp =
                        clear_resp.on_hover_ui(|ui| chrome_tooltip(ui, "Clear tab search"));
                    if clear_resp.clicked() || menu_anchor_keyboard_toggle(ui, &clear_resp) {
                        query.clear();
                        response.request_focus();
                    }
                }

                response
            })
            .inner
        });
    mde_egui::focus::paint_focus_ring(ui.painter(), outer.response.rect, outer.inner.has_focus());
    outer.inner
}

fn tab_search_menu_contents(ui: &mut egui::Ui, state: &mut WebState) -> Option<usize> {
    let frame_width = tab_search_popup_content_width(bounded_available_width(ui));
    chrome_popup_frame(ui, frame_width, |ui| tab_search_menu_inner(ui, state))
}

fn tab_search_popup_content_width(available_width: f32) -> f32 {
    let panel_width = tab_search_panel_width(available_width);
    if panel_width <= 0.0 {
        1.0
    } else {
        (panel_width - CHROME_POPUP_INNER_MARGIN_X * 2.0).max(1.0)
    }
}

fn tab_search_menu_inner(ui: &mut egui::Ui, state: &mut WebState) -> Option<usize> {
    let width = tab_search_panel_width(bounded_available_width(ui));
    if width > 0.0 {
        ui.set_width(width);
        ui.set_max_width(width);
    }
    let resp = tab_search_field(ui, &mut state.tab_search_query);
    state.chrome_edit_focus |= resp.has_focus();
    tab_search_results(ui, state)
}

fn tab_search_anchor_button(ui: &mut egui::Ui, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
        egui::Sense::click(),
    );
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), "Search tabs")
    });
    let base_fill = if selected {
        CHROME_PRIMARY_CONTAINER
    } else {
        CHROME_TOOLBAR
    };
    let icon_color = if selected {
        CHROME_ON_PRIMARY_CONTAINER
    } else {
        CHROME_TEXT
    };
    let fill = animated_response_fill(ui, &response, base_fill, icon_color, true);
    ui.painter().rect(
        rect,
        ICON_BUTTON_RADIUS,
        fill,
        egui::Stroke::new(
            1.0,
            if selected {
                CHROME_PRIMARY
            } else {
                CHROME_OUTLINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    paint_chrome_icon(ui.painter(), rect, ChromeIcon::Search, icon_color);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, "Search tabs")
}

/// Chrome's "Search tabs" dropdown: live-filtered, clickable tab chooser.
pub(super) fn tab_search_menu(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    let popup_id = tab_search_menu_popup_id();
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let response = tab_search_anchor_button(ui, popup_open);
    let keyboard_toggle = response.has_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space));
    if response.clicked() || keyboard_toggle {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    egui::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            apply_visuals(ui);
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            if let Some(idx) = tab_search_menu_contents(ui, state) {
                select = Some(idx);
                ui.memory_mut(|mem| mem.close_popup());
            }
        },
    );
    if let Some(idx) = select {
        state.select_tab(idx);
        state.tab_search_query.clear();
    }
}

pub(super) fn tab_search_menu_popup_id() -> egui::Id {
    egui::Id::new("mde_web_tab_search_menu_popup")
}

pub(super) fn tab_strip(ui: &mut egui::Ui, state: &mut WebState) {
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
    let mut group_tab: Option<usize> = None;
    let mut ungroup_tab_idx: Option<usize> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut autoplay_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    let mut pin_tab: Option<(usize, bool)> = None;
    let mut duplicate_tab_idx: Option<usize> = None;
    let mut close_others_idx: Option<usize> = None;
    let mut close_right_idx: Option<usize> = None;

    // Overflow (BROWSER tabstrip): pills shrink toward a floor as they multiply;
    // once at the floor the strip scrolls horizontally in ONE row instead of
    // wrapping onto stacked rows.
    let pill_width = horizontal_tab_pill_width(ui.available_width(), state.tabs.len());

    // Scroll the active pill into view only when the active tab actually CHANGED,
    // so the operator can still scroll the strip freely while a tab stays selected.
    let last_active_id = egui::Id::new("browser-horizontal-tabs-last-active");
    let active_changed =
        ui.ctx().data(|d| d.get_temp::<usize>(last_active_id)) != Some(state.active);

    // Drag-reorder bookkeeping: every pill's laid-out rect (in tab order), plus the
    // pill under a settled drag and where it was dropped.
    let mut pill_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let mut drag_from: Option<usize> = None;
    let mut drop_pointer: Option<egui::Pos2> = None;
    let mut drag_settle: Option<(u64, f32)> = None;

    egui::ScrollArea::horizontal()
        .id_salt("browser-horizontal-tabs")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for idx in 0..state.tabs.len() {
                    let favicon = tab_favicon_texture_at(ui.ctx(), &mut state.tabs, idx);
                    let tab = &state.tabs[idx];
                    let active = idx == state.active;
                    // Pinned tabs collapse to a compact favicon-only pill (no title).
                    let label = if tab.pinned {
                        String::new()
                    } else {
                        tab_label(tab)
                    };
                    let status_chips = tab_status_chips(tab);
                    let pill_w = if tab.pinned {
                        CHROME_TAB_PINNED_W
                    } else {
                        pill_width
                    };
                    let tab_response = tab_pill_sized(
                        ui,
                        &label,
                        active,
                        pill_w,
                        tab.engine,
                        favicon.as_ref(),
                        &status_chips,
                    );
                    let settle_motion =
                        tab_drag_settle_motion(ui.ctx(), tab.id, TabAxis::Horizontal);
                    pill_rects.push((idx, tab_response.rect));
                    if tab_response.clicked() {
                        select = Some(idx);
                    }
                    // Middle-click closes the tab under the pointer (the ubiquitous
                    // desktop-browser gesture) — same seam as the inline × button.
                    if tab_response.middle_clicked() {
                        close = Some(idx);
                    }
                    // A settled horizontal drag reorders the tab to where it was
                    // dropped; egui's own click/drag threshold keeps a plain click
                    // (activate) and a middle-click (close) intact.
                    if tab_response.drag_stopped() {
                        drag_from = Some(idx);
                        drop_pointer = tab_response.interact_pointer_pos();
                    }
                    if active && active_changed {
                        tab_response.scroll_to_me(Some(egui::Align::Center));
                    }
                    // Tab-group indicator: a colored strip along the pill's bottom edge
                    // (Chrome's grouped-tab color band), painted from the response rect
                    // so it never disturbs the click/drag interaction above.
                    if let Some(color) = tab
                        .group
                        .and_then(|g| state.tab_groups.get(g))
                        .map(|g| g.color)
                    {
                        let r = tab_response.rect;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_max(
                                egui::pos2(r.left() + 2.0, r.bottom() - 2.0),
                                egui::pos2(r.right() - 2.0, r.bottom()),
                            ),
                            0.0,
                            color,
                        );
                    }
                    paint_tab_drag_settle(
                        ui,
                        tab_response.rect,
                        tab.engine,
                        TabAxis::Horizontal,
                        settle_motion,
                    );
                    let tab_response = tab_response.on_hover_ui(|ui| tab_hover_card(ui, tab));
                    chrome_context_menu(&tab_response, |ui| {
                        if tab_context_menu_row(ui, "Move tab left", ChromeIcon::Back, idx > 0)
                            .clicked()
                        {
                            move_tab = Some((idx, idx - 1));
                            ui.close_menu();
                        }
                        if tab_context_menu_row(
                            ui,
                            "Move tab right",
                            ChromeIcon::Forward,
                            idx + 1 < state.tabs.len(),
                        )
                        .clicked()
                        {
                            move_tab = Some((idx, idx + 1));
                            ui.close_menu();
                        }
                        let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                        if tab_context_menu_row(ui, pin_label, ChromeIcon::Bookmark, true).clicked()
                        {
                            pin_tab = Some((idx, !tab.pinned));
                            ui.close_menu();
                        }
                        if tab_context_menu_row(ui, "Duplicate tab", ChromeIcon::Page, true)
                            .clicked()
                        {
                            duplicate_tab_idx = Some(idx);
                            ui.close_menu();
                        }
                        if tab_context_menu_row(
                            ui,
                            "Close other tabs",
                            ChromeIcon::Close,
                            state.tabs.len() > 1,
                        )
                        .clicked()
                        {
                            close_others_idx = Some(idx);
                            ui.close_menu();
                        }
                        if tab_context_menu_row(
                            ui,
                            "Close tabs to the right",
                            ChromeIcon::Forward,
                            idx + 1 < state.tabs.len(),
                        )
                        .clicked()
                        {
                            close_right_idx = Some(idx);
                            ui.close_menu();
                        }
                        if tab.group.is_none() {
                            if tab_context_menu_row(
                                ui,
                                "Add tab to new group",
                                ChromeIcon::Tabs,
                                true,
                            )
                            .clicked()
                            {
                                group_tab = Some(idx);
                                ui.close_menu();
                            }
                        } else if tab_context_menu_row(
                            ui,
                            "Remove from group",
                            ChromeIcon::Tabs,
                            true,
                        )
                        .clicked()
                        {
                            ungroup_tab_idx = Some(idx);
                            ui.close_menu();
                        }
                        let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                        if tab_context_menu_row(ui, mute_label, ChromeIcon::Audio, true).clicked() {
                            mute_tab = Some((idx, !tab.muted));
                            ui.close_menu();
                        }
                        let autoplay_label = if tab.autoplay_blocked {
                            "Allow autoplay"
                        } else {
                            "Block autoplay"
                        };
                        if tab_context_menu_row(ui, autoplay_label, ChromeIcon::Play, true)
                            .clicked()
                        {
                            autoplay_tab = Some((idx, !tab.autoplay_blocked));
                            ui.close_menu();
                        }
                        let dark_label = if tab.force_dark {
                            "Disable force dark"
                        } else {
                            "Enable force dark"
                        };
                        if tab_context_menu_row(ui, dark_label, ChromeIcon::DarkMode, true)
                            .clicked()
                        {
                            force_dark_tab = Some((idx, !tab.force_dark));
                            ui.close_menu();
                        }
                        let reader_label = if tab.reader_mode {
                            "Disable reader mode"
                        } else {
                            "Enable reader mode"
                        };
                        if tab_context_menu_row(ui, reader_label, ChromeIcon::View, true).clicked()
                        {
                            reader_tab = Some((idx, !tab.reader_mode));
                            ui.close_menu();
                        }
                        let scripts_label = if tab.user_scripts {
                            "Disable site fixups"
                        } else {
                            "Enable site fixups"
                        };
                        if tab_context_menu_row(ui, scripts_label, ChromeIcon::Edit, true).clicked()
                        {
                            user_scripts_tab = Some((idx, !tab.user_scripts));
                            ui.close_menu();
                        }
                        chrome_separator(ui);
                        for container in ContainerProfile::ALL {
                            if tab_context_menu_row(
                                ui,
                                container.label(),
                                ChromeIcon::Privacy,
                                tab.container != container,
                            )
                            .clicked()
                            {
                                container_tab = Some((idx, container));
                                ui.close_menu();
                            }
                        }
                        chrome_separator(ui);
                        for display_target in DisplayTarget::ALL {
                            if tab_context_menu_row(
                                ui,
                                display_target.label(),
                                ChromeIcon::View,
                                tab.display_target != display_target,
                            )
                            .clicked()
                            {
                                display_tab = Some((idx, display_target));
                                ui.close_menu();
                            }
                        }
                        if tab_context_menu_row(ui, "Close tab", ChromeIcon::Close, true).clicked()
                        {
                            close = Some(idx);
                            ui.close_menu();
                        }
                    });
                    // Speaker affordance for an audible/muted tab, click-to-mute.
                    if let Some(audio) = tab_audio_button(ui, tab.session.audible(), tab.muted) {
                        if audio.clicked() {
                            mute_tab = Some((idx, !tab.muted));
                        }
                    }
                    // Pinned tabs hide the inline × (Chrome's affordance); they
                    // still close via middle-click or the context menu.
                    if !tab.pinned && inline_close_button(ui).clicked() {
                        close = Some(idx);
                    }
                }
                tab_search_menu(ui, state);
            });
        });

    ui.ctx()
        .data_mut(|d| d.insert_temp(last_active_id, state.active));

    // Resolve a settled drag to a concrete reorder against the laid-out pills.
    if let (Some(from), Some(pointer)) = (drag_from, drop_pointer) {
        if let Some(to) = tab_drag_target_index(&pill_rects, pointer, TabAxis::Horizontal) {
            if to != from {
                move_tab = Some((from, to));
                drag_settle = state
                    .tabs
                    .get(from)
                    .map(|tab| (tab.id, tab_drag_settle_direction(from, to)));
            }
        }
    }

    #[cfg(test)]
    {
        let rects: Vec<egui::Rect> = pill_rects.iter().map(|(_, r)| *r).collect();
        ui.ctx()
            .data_mut(|d| d.insert_temp(tab_pill_rects_id(), rects));
    }

    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, blocked)) = autoplay_tab {
        state.select_tab(idx);
        state.set_active_tab_autoplay_blocked(blocked);
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
    } else if let Some((idx, pinned)) = pin_tab {
        state.set_tab_pinned(idx, pinned);
    } else if let Some(idx) = duplicate_tab_idx {
        state.duplicate_tab(idx);
    } else if let Some(idx) = close_others_idx {
        state.close_other_tabs(idx);
    } else if let Some(idx) = close_right_idx {
        state.close_tabs_to_the_right(idx);
    } else if let Some(idx) = group_tab {
        state.new_group_from_tab(idx);
    } else if let Some(idx) = ungroup_tab_idx {
        state.ungroup_tab(idx);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
        if let Some((tab_id, direction)) = drag_settle {
            note_tab_drag_settle(ui.ctx(), tab_id, TabAxis::Horizontal, direction);
        }
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
    let mut group_tab: Option<usize> = None;
    let mut ungroup_tab_idx: Option<usize> = None;
    let mut mute_tab: Option<(usize, bool)> = None;
    let mut autoplay_tab: Option<(usize, bool)> = None;
    let mut force_dark_tab: Option<(usize, bool)> = None;
    let mut reader_tab: Option<(usize, bool)> = None;
    let mut user_scripts_tab: Option<(usize, bool)> = None;
    let mut container_tab: Option<(usize, ContainerProfile)> = None;
    let mut display_tab: Option<(usize, DisplayTarget)> = None;
    let mut pin_tab: Option<(usize, bool)> = None;
    let mut duplicate_tab_idx: Option<usize> = None;
    let mut close_others_idx: Option<usize> = None;
    let mut close_right_idx: Option<usize> = None;

    // Drag-reorder bookkeeping mirrors the horizontal strip, but the drop point is
    // matched along Y — a vertical drag reorders the stacked pills.
    let mut pill_rects: Vec<(usize, egui::Rect)> = Vec::new();
    let mut drag_from: Option<usize> = None;
    let mut drop_pointer: Option<egui::Pos2> = None;
    let mut drag_settle: Option<(u64, f32)> = None;

    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width((CHROME_TAB_RAIL_W - 8.0).max(CHROME_NEW_TAB_W));
            egui::ScrollArea::vertical()
                .id_salt("browser-vertical-tabs")
                .max_height(ui.available_height().max(CHROME_TAB_H * 3.0))
                .show(ui, |ui| {
                    for idx in 0..state.tabs.len() {
                        let favicon = tab_favicon_texture_at(ui.ctx(), &mut state.tabs, idx);
                        let tab = &state.tabs[idx];
                        let active = idx == state.active;
                        // Pinned tabs collapse to a compact favicon-only pill.
                        let label = if tab.pinned {
                            String::new()
                        } else {
                            tab_label(tab)
                        };
                        let status_chips = tab_status_chips(tab);
                        ui.horizontal(|ui| {
                            let width = if tab.pinned {
                                CHROME_TAB_PINNED_W
                            } else {
                                (ui.available_width() - CHROME_TAB_CLOSE - CHROME_GAP)
                                    .max(CHROME_NEW_TAB_W)
                            };
                            let resp = tab_pill_sized(
                                ui,
                                &label,
                                active,
                                width,
                                tab.engine,
                                favicon.as_ref(),
                                &status_chips,
                            );
                            let settle_motion =
                                tab_drag_settle_motion(ui.ctx(), tab.id, TabAxis::Vertical);
                            pill_rects.push((idx, resp.rect));
                            if resp.clicked() {
                                select = Some(idx);
                            }
                            // Middle-click closes this tab — same gesture as the
                            // horizontal strip.
                            if resp.middle_clicked() {
                                close = Some(idx);
                            }
                            // A settled vertical drag reorders this pill to where it
                            // was dropped (matched along Y).
                            if resp.drag_stopped() {
                                drag_from = Some(idx);
                                drop_pointer = resp.interact_pointer_pos();
                            }
                            // Tab-group indicator: a colored strip along the pill's LEFT
                            // edge (the vertical-strip analogue of the horizontal band).
                            if let Some(color) = tab
                                .group
                                .and_then(|g| state.tab_groups.get(g))
                                .map(|g| g.color)
                            {
                                let r = resp.rect;
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_max(
                                        egui::pos2(r.left(), r.top() + 2.0),
                                        egui::pos2(r.left() + 2.0, r.bottom() - 2.0),
                                    ),
                                    0.0,
                                    color,
                                );
                            }
                            paint_tab_drag_settle(
                                ui,
                                resp.rect,
                                tab.engine,
                                TabAxis::Vertical,
                                settle_motion,
                            );
                            let resp = resp.on_hover_ui(|ui| tab_hover_card(ui, tab));
                            chrome_context_menu(&resp, |ui| {
                                if tab_context_menu_row(ui, "Move tab up", ChromeIcon::Up, idx > 0)
                                    .clicked()
                                {
                                    move_tab = Some((idx, idx - 1));
                                    ui.close_menu();
                                }
                                if tab_context_menu_row(
                                    ui,
                                    "Move tab down",
                                    ChromeIcon::Down,
                                    idx + 1 < state.tabs.len(),
                                )
                                .clicked()
                                {
                                    move_tab = Some((idx, idx + 1));
                                    ui.close_menu();
                                }
                                let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                                if tab_context_menu_row(ui, pin_label, ChromeIcon::Bookmark, true)
                                    .clicked()
                                {
                                    pin_tab = Some((idx, !tab.pinned));
                                    ui.close_menu();
                                }
                                if tab_context_menu_row(ui, "Duplicate tab", ChromeIcon::Page, true)
                                    .clicked()
                                {
                                    duplicate_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if tab_context_menu_row(
                                    ui,
                                    "Close other tabs",
                                    ChromeIcon::Close,
                                    state.tabs.len() > 1,
                                )
                                .clicked()
                                {
                                    close_others_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if tab_context_menu_row(
                                    ui,
                                    "Close tabs to the right",
                                    ChromeIcon::Forward,
                                    idx + 1 < state.tabs.len(),
                                )
                                .clicked()
                                {
                                    close_right_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if tab.group.is_none() {
                                    if tab_context_menu_row(
                                        ui,
                                        "Add tab to new group",
                                        ChromeIcon::Tabs,
                                        true,
                                    )
                                    .clicked()
                                    {
                                        group_tab = Some(idx);
                                        ui.close_menu();
                                    }
                                } else if tab_context_menu_row(
                                    ui,
                                    "Remove from group",
                                    ChromeIcon::Tabs,
                                    true,
                                )
                                .clicked()
                                {
                                    ungroup_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                                if tab_context_menu_row(ui, mute_label, ChromeIcon::Audio, true)
                                    .clicked()
                                {
                                    mute_tab = Some((idx, !tab.muted));
                                    ui.close_menu();
                                }
                                let autoplay_label = if tab.autoplay_blocked {
                                    "Allow autoplay"
                                } else {
                                    "Block autoplay"
                                };
                                if tab_context_menu_row(ui, autoplay_label, ChromeIcon::Play, true)
                                    .clicked()
                                {
                                    autoplay_tab = Some((idx, !tab.autoplay_blocked));
                                    ui.close_menu();
                                }
                                let dark_label = if tab.force_dark {
                                    "Disable force dark"
                                } else {
                                    "Enable force dark"
                                };
                                if tab_context_menu_row(ui, dark_label, ChromeIcon::DarkMode, true)
                                    .clicked()
                                {
                                    force_dark_tab = Some((idx, !tab.force_dark));
                                    ui.close_menu();
                                }
                                let reader_label = if tab.reader_mode {
                                    "Disable reader mode"
                                } else {
                                    "Enable reader mode"
                                };
                                if tab_context_menu_row(ui, reader_label, ChromeIcon::View, true)
                                    .clicked()
                                {
                                    reader_tab = Some((idx, !tab.reader_mode));
                                    ui.close_menu();
                                }
                                let scripts_label = if tab.user_scripts {
                                    "Disable site fixups"
                                } else {
                                    "Enable site fixups"
                                };
                                if tab_context_menu_row(ui, scripts_label, ChromeIcon::Edit, true)
                                    .clicked()
                                {
                                    user_scripts_tab = Some((idx, !tab.user_scripts));
                                    ui.close_menu();
                                }
                                chrome_separator(ui);
                                for container in ContainerProfile::ALL {
                                    if tab_context_menu_row(
                                        ui,
                                        container.label(),
                                        ChromeIcon::Privacy,
                                        tab.container != container,
                                    )
                                    .clicked()
                                    {
                                        container_tab = Some((idx, container));
                                        ui.close_menu();
                                    }
                                }
                                chrome_separator(ui);
                                for display_target in DisplayTarget::ALL {
                                    if tab_context_menu_row(
                                        ui,
                                        display_target.label(),
                                        ChromeIcon::View,
                                        tab.display_target != display_target,
                                    )
                                    .clicked()
                                    {
                                        display_tab = Some((idx, display_target));
                                        ui.close_menu();
                                    }
                                }
                                if tab_context_menu_row(ui, "Close tab", ChromeIcon::Close, true)
                                    .clicked()
                                {
                                    close = Some(idx);
                                    ui.close_menu();
                                }
                            });
                            // Speaker affordance for an audible/muted tab, click-to-mute.
                            if let Some(audio) =
                                tab_audio_button(ui, tab.session.audible(), tab.muted)
                            {
                                if audio.clicked() {
                                    mute_tab = Some((idx, !tab.muted));
                                }
                            }
                            // Pinned tabs hide the × (close via middle-click / menu).
                            if !tab.pinned && inline_close_button(ui).clicked() {
                                close = Some(idx);
                            }
                        });
                    }
                    tab_search_menu(ui, state);
                });
        });

    // Resolve a settled vertical drag to a concrete reorder against the pills.
    if let (Some(from), Some(pointer)) = (drag_from, drop_pointer) {
        if let Some(to) = tab_drag_target_index(&pill_rects, pointer, TabAxis::Vertical) {
            if to != from {
                move_tab = Some((from, to));
                drag_settle = state
                    .tabs
                    .get(from)
                    .map(|tab| (tab.id, tab_drag_settle_direction(from, to)));
            }
        }
    }

    #[cfg(test)]
    {
        let rects: Vec<egui::Rect> = pill_rects.iter().map(|(_, r)| *r).collect();
        ui.ctx()
            .data_mut(|d| d.insert_temp(tab_pill_rects_id(), rects));
    }

    if let Some((idx, muted)) = mute_tab {
        state.select_tab(idx);
        state.set_active_tab_muted(muted);
    } else if let Some((idx, blocked)) = autoplay_tab {
        state.select_tab(idx);
        state.set_active_tab_autoplay_blocked(blocked);
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
    } else if let Some((idx, pinned)) = pin_tab {
        state.set_tab_pinned(idx, pinned);
    } else if let Some(idx) = duplicate_tab_idx {
        state.duplicate_tab(idx);
    } else if let Some(idx) = close_others_idx {
        state.close_other_tabs(idx);
    } else if let Some(idx) = close_right_idx {
        state.close_tabs_to_the_right(idx);
    } else if let Some(idx) = group_tab {
        state.new_group_from_tab(idx);
    } else if let Some(idx) = ungroup_tab_idx {
        state.ungroup_tab(idx);
    } else if let Some((from, to)) = move_tab {
        state.move_tab(from, to);
        if let Some((tab_id, direction)) = drag_settle {
            note_tab_drag_settle(ui.ctx(), tab_id, TabAxis::Vertical, direction);
        }
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

/// Height reserved at the bottom of the vertical tab rail for the affordance
/// cluster (notifications / downloads / screenshots / recommendations).
const RAIL_CLUSTER_H: f32 = 40.0;

/// Preferred width of the rail Screenshots capture menu popup.
const RAIL_SCREENSHOTS_MENU_W: f32 = 220.0;

pub(super) fn vertical_rail_affordances_height() -> f32 {
    RAIL_CLUSTER_H
}

/// The bottom band of the vertical tab rail. The operator asked to move the
/// browser's notifications, drop-down (downloads / screenshots) and
/// recommendations affordances here; in vertical mode they are de-duplicated
/// from the horizontal toolbar and live as one compact icon row.
pub(super) fn vertical_rail_affordances(ui: &mut egui::Ui, state: &mut WebState) {
    scope(ui, |ui| {
        let band = ui.max_rect();
        // A 1px separator divides the cluster from the tab strip above it.
        ui.painter().hline(
            band.x_range(),
            band.top(),
            egui::Stroke::new(1.0, CHROME_OUTLINE),
        );
        egui::Frame::NONE
            .fill(CHROME_SURFACE_CONTAINER)
            .inner_margin(egui::Margin::same(4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    rail_notifications_button(ui, state);
                    rail_downloads_button(ui, state);
                    rail_screenshots_menu(ui, state);
                    rail_recommendations_button(ui, state);
                });
            });
    });
}

fn rail_notifications_button(ui: &mut egui::Ui, state: &mut WebState) {
    let tip = if state.notifications_unread > 0 {
        format!("Notifications ({} new)", state.notifications_unread)
    } else {
        "Notifications".to_owned()
    };
    if chrome_icon_button(
        ui,
        ChromeIcon::Notifications,
        &tip,
        true,
        state.notifications_open,
    )
    .clicked()
    {
        state.toggle_notifications();
    }
    // Only paints when there is a genuine unread count (0 → nothing).
    toolbar_count_badge(ui, state.notifications_unread as u64, &tip);
}

fn rail_downloads_button(ui: &mut egui::Ui, state: &mut WebState) {
    // The exact toolbar downloads drop-down block, moved onto the rail.
    let (active_downloads, total_downloads) = state.download_counts();
    let downloads_tip = downloads_toolbar_tip(active_downloads, total_downloads);
    if chrome_icon_button(
        ui,
        ChromeIcon::Downloads,
        &downloads_tip,
        true,
        state.downloads_open,
    )
    .clicked()
    {
        state.downloads_open = !state.downloads_open;
        if state.downloads_open {
            state.refresh_downloads();
        }
    }
    download_count_badge(ui, active_downloads, &downloads_tip);
}

fn rail_recommendations_button(ui: &mut egui::Ui, state: &mut WebState) {
    if chrome_icon_button(
        ui,
        ChromeIcon::Recommend,
        "Recommendations",
        true,
        state.recommendations_open,
    )
    .clicked()
    {
        state.toggle_recommendations();
    }
}

/// The 7 capture verbs the rail Screenshots menu exposes — the same seams the
/// horizontal toolbar's Capture button and the menubar Capture submenu drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RailCaptureAction {
    Viewport,
    FullPage,
    Mhtml,
    Annotated,
    Callout,
    Freehand,
    Region,
}

const RAIL_CAPTURE_ACTIONS: &[RailCaptureAction] = &[
    RailCaptureAction::Viewport,
    RailCaptureAction::FullPage,
    RailCaptureAction::Mhtml,
    RailCaptureAction::Annotated,
    RailCaptureAction::Callout,
    RailCaptureAction::Freehand,
    RailCaptureAction::Region,
];

impl RailCaptureAction {
    fn label(self, region_mode: bool) -> &'static str {
        match self {
            RailCaptureAction::Viewport => "Capture Viewport",
            RailCaptureAction::FullPage => "Capture Full Page",
            RailCaptureAction::Mhtml => "Capture Web Archive",
            RailCaptureAction::Annotated => "Capture with Annotation",
            RailCaptureAction::Callout => "Capture with Callout",
            RailCaptureAction::Freehand => "Capture Freehand Markup",
            RailCaptureAction::Region => {
                if region_mode {
                    "Cancel Region Capture"
                } else {
                    "Capture Region"
                }
            }
        }
    }

    fn apply(self, state: &mut WebState) {
        match self {
            RailCaptureAction::Viewport => state.capture_active_viewport(),
            RailCaptureAction::FullPage => state.capture_active_full_page(),
            RailCaptureAction::Mhtml => state.capture_active_mhtml(),
            RailCaptureAction::Annotated => state.capture_active_annotated_viewport(),
            RailCaptureAction::Callout => state.capture_active_callout_viewport(),
            RailCaptureAction::Freehand => state.capture_active_freehand_viewport(),
            RailCaptureAction::Region => {
                if state.capture_region_mode {
                    state.cancel_region_capture();
                } else {
                    state.start_region_capture();
                }
            }
        }
    }
}

fn rail_screenshots_popup_id() -> egui::Id {
    egui::Id::new("mde_web_rail_screenshots_menu_popup")
}

fn rail_screenshots_menu(ui: &mut egui::Ui, state: &mut WebState) {
    let can_capture = state.active_tab_has_frame();
    let region_mode = state.capture_region_mode;
    let popup_id = rail_screenshots_popup_id();
    let response = toolbar_icon_menu_anchor(
        ui,
        popup_id,
        ChromeIcon::Capture,
        CHROME_ICON,
        "Screenshots",
    );
    if response.clicked() || menu_anchor_keyboard_toggle(ui, &response) {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    let mut chosen: Option<RailCaptureAction> = None;
    egui::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            reserve_toolbar_popup_width(ui, RAIL_SCREENSHOTS_MENU_W);
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            chrome_popup_frame(ui, RAIL_SCREENSHOTS_MENU_W, |ui| {
                for action in RAIL_CAPTURE_ACTIONS {
                    if chrome_menu_row(
                        ui,
                        action.label(region_mode),
                        ChromeIcon::Capture,
                        can_capture,
                        "No painted page to capture",
                    )
                    .clicked()
                    {
                        chosen = Some(*action);
                        ui.memory_mut(|mem| mem.close_popup());
                    }
                }
            });
        },
    );
    if let Some(action) = chosen {
        action.apply(state);
    }
}

/// Which way a tab strip runs, so the shared drag-reorder hit-test knows whether
/// to compare drop points along X (horizontal strip) or Y (vertical strip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum TabAxis {
    Horizontal,
    Vertical,
}

/// Distance from a pill rect's centre to `point` along the strip's running axis.
fn tab_axis_distance(rect: egui::Rect, point: egui::Pos2, axis: TabAxis) -> f32 {
    match axis {
        TabAxis::Horizontal => (rect.center().x - point.x).abs(),
        TabAxis::Vertical => (rect.center().y - point.y).abs(),
    }
}

/// Given the laid-out tab pill rects (in tab order) and where a drag was
/// released, return the tab index whose slot the dragged tab should take — the
/// pill whose centre is nearest the drop point along the strip's axis. Reused by
/// both strip variants so horizontal and vertical drag-reorder share one rule.
pub(super) fn tab_drag_target_index(
    pills: &[(usize, egui::Rect)],
    drop: egui::Pos2,
    axis: TabAxis,
) -> Option<usize> {
    pills
        .iter()
        .min_by(|(_, a), (_, b)| {
            let da = tab_axis_distance(*a, drop, axis);
            let db = tab_axis_distance(*b, drop, axis);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| *idx)
}

/// The egui temp-memory key under which the horizontal/vertical strips stash the
/// laid-out pill rects, so egui-driven tests can aim pointer drags at real pill
/// centres (Buttons have no stable id to `read_response`).
#[cfg(test)]
pub(super) fn tab_pill_rects_id() -> egui::Id {
    egui::Id::new("browser-test-tab-pill-rects")
}

/// A cheap fingerprint of a favicon's PNG bytes, so [`tab_favicon_texture`] can
/// tell "the same favicon as last frame" from "the page just reported a new one"
/// without diffing the byte vector itself on every frame.
fn favicon_fingerprint(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Resolve this tab's favicon texture for the current frame.
///
/// Reuses [`Tab::favicon_cache`] when the underlying PNG bytes are unchanged from
/// last frame; otherwise PNG-decodes via the same `png`-crate path the boot
/// splash / offline-cache viewport already use ([`crate::chooser::decode_png_rgba`])
/// and caches the result. A decode failure caches an honest `None` rather than
/// panicking or re-attempting the decode every frame (§7).
pub(super) fn tab_favicon_texture(ctx: &egui::Context, tab: &mut Tab) -> Option<TextureHandle> {
    let bytes = tab.session.favicon()?;
    let fingerprint = favicon_fingerprint(bytes);
    if let Some(cache) = &tab.favicon_cache {
        if cache.fingerprint == fingerprint {
            return cache.texture.clone();
        }
    }
    let texture = crate::chooser::decode_png_rgba(bytes).map(|image| {
        ctx.load_texture(
            format!("browser-tab-favicon::{fingerprint:x}"),
            image,
            TextureOptions::LINEAR,
        )
    });
    tab.favicon_cache = Some(FaviconCache {
        fingerprint,
        texture: texture.clone(),
    });
    texture
}

/// Resolve one tab's favicon texture on demand.
///
/// This keeps the strip from allocating a frame-wide favicon vector before it
/// draws the pills. The per-tab decode/cache behavior remains owned by
/// [`tab_favicon_texture`].
pub(super) fn tab_favicon_texture_at(
    ctx: &egui::Context,
    tabs: &mut [Tab],
    index: usize,
) -> Option<TextureHandle> {
    tabs.get_mut(index)
        .and_then(|tab| tab_favicon_texture(ctx, tab))
}

fn browser_dashboard_title() -> String {
    "Search the web".to_owned()
}

pub(super) fn new_tab_dashboard(ui: &mut egui::Ui, state: &mut WebState) {
    let mut submit_search = false;
    let mut open_service: Option<String> = None;
    paint_new_tab_dashboard_backdrop(ui);
    let available_h = ui.available_height();
    let panel_h = 238.0;
    let available_w = bounded_available_width(ui);
    let panel_w = available_w
        .min(DASHBOARD_PANEL_W)
        .max(DASHBOARD_PANEL_MIN_W.min(available_w));
    ui.add_space(((available_h - panel_h) * 0.44).clamp(Style::SP_M, 160.0));
    ui.horizontal(|ui| {
        ui.add_space(((available_w - panel_w) * 0.5).max(0.0));
        ui.allocate_ui_with_layout(
            egui::vec2(panel_w, panel_h),
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                dashboard_heading(ui, panel_w);
                ui.add_space(Style::SP_M);
                let (resp, clicked) = dashboard_search_box(ui, &mut state.dashboard_query);
                state.chrome_edit_focus |= resp.has_focus();
                submit_search =
                    clicked || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                ui.add_space(Style::SP_M);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    let tile_w = dashboard_tile_width(panel_w);
                    for service in &state.speed_dial {
                        if dashboard_service_tile(ui, service, tile_w).clicked() {
                            open_service = Some(service.url.clone());
                        }
                    }
                });
                ui.add_space(Style::SP_S);
                browser_status_note(
                    ui,
                    ChromeIcon::Lock,
                    PRIVATE_MODE_EXPLAINER,
                    CHROME_TEXT_DIM,
                );
            },
        );
    });
    if submit_search {
        state.submit_dashboard_search();
    }
    if let Some(url) = open_service {
        state.open_mesh_service(url);
    }
}

fn paint_new_tab_dashboard_backdrop(ui: &egui::Ui) {
    let rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    if !rect.is_positive() {
        return;
    }
    ui.painter().rect_filled(rect, 0.0, CHROME_SURFACE);

    let band_h = (rect.height() * 0.34)
        .clamp(96.0, 176.0)
        .min(rect.height().max(1.0));
    let band = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(rect.right(), rect.top() + band_h),
    );
    ui.painter()
        .rect_filled(band, 0.0, CHROME_SURFACE_CONTAINER);

    let accent = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + band_h * 0.82),
        egui::vec2(rect.width().min(DASHBOARD_PANEL_W), 30.0),
    );
    ui.painter()
        .rect_filled(accent, 15.0, CHROME_PRIMARY_CONTAINER);
}

fn dashboard_heading(ui: &mut egui::Ui, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width.max(1.0), 40.0), egui::Sense::hover());
    let galley = ui
        .fonts(|fonts| fonts.layout_no_wrap(browser_dashboard_title(), font_id(32.0), CHROME_TEXT));
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(pos, galley, CHROME_TEXT);
}

fn dashboard_search_box(ui: &mut egui::Ui, query: &mut String) -> (egui::Response, bool) {
    let available = bounded_available_width(ui);
    let width = available
        .min(DASHBOARD_SEARCH_W)
        .max(DASHBOARD_SEARCH_MIN_W.min(available));
    let inner = egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(DASHBOARD_SEARCH_H * 0.5)
        .inner_margin(dashboard_search_margin())
        .show(ui, |ui| {
            ui.set_width((width - 28.0).max(1.0));
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let edit_w = (ui.available_width() - DASHBOARD_SEARCH_SUBMIT - 10.0).max(1.0);
                let response = ui.add_sized(
                    [edit_w, 34.0],
                    egui::TextEdit::singleline(query)
                        .hint_text(
                            RichText::new("Search the web")
                                .size(16.0)
                                .color(CHROME_TEXT_DIM),
                        )
                        .text_color(CHROME_TEXT)
                        .font(font_id(16.0))
                        .background_color(CHROME_SURFACE)
                        .margin(egui::Margin::symmetric(0, 0))
                        .frame(false),
                );
                let clicked = action_icon_button(
                    ui,
                    ChromeIcon::Search,
                    BrowserActionRole::Primary,
                    "Search the web",
                    egui::vec2(DASHBOARD_SEARCH_SUBMIT, DASHBOARD_SEARCH_SUBMIT),
                )
                .clicked();
                (response, clicked)
            })
            .inner
        });
    mde_egui::focus::paint_focus_ring(ui.painter(), inner.response.rect, inner.inner.0.has_focus());
    (inner.inner.0, inner.inner.1)
}

fn dashboard_tile_width(panel_w: f32) -> f32 {
    let panel_w = panel_w.max(1.0);
    if panel_w < DASHBOARD_TILE_MIN_W * 2.0 + 8.0 {
        panel_w
    } else if panel_w < 500.0 {
        ((panel_w - 8.0) / 2.0).clamp(DASHBOARD_TILE_MIN_W, DASHBOARD_TILE_MAX_W)
    } else {
        ((panel_w - 24.0) / 4.0).clamp(DASHBOARD_TILE_MIN_W, DASHBOARD_TILE_MAX_W)
    }
}

fn dashboard_service_icon(label: &str) -> ChromeIcon {
    match label.trim().to_ascii_lowercase().as_str() {
        "search" => ChromeIcon::Search,
        "music" => ChromeIcon::Audio,
        "docs" => ChromeIcon::Page,
        "status" => ChromeIcon::Security,
        _ => ChromeIcon::Page,
    }
}

fn dashboard_service_subtitle(url: &str) -> String {
    host_of(url)
        .map(|host| ellipsize(&host, 24))
        .unwrap_or_else(|| ellipsize(url.trim(), 24))
}

fn dashboard_service_tile(
    ui: &mut egui::Ui,
    service: &super::SpeedDialEntry,
    width: f32,
) -> egui::Response {
    let width = width.max(1.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, DASHBOARD_TILE_H), egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            true,
            format!("Open {}", service.label),
        )
    });
    let fill = animated_response_fill(ui, &response, CHROME_TOOLBAR, CHROME_TEXT, true);
    ui.painter().rect(
        rect,
        8.0,
        fill,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );

    let icon_rect = egui::Rect::from_min_size(
        rect.min + egui::vec2(10.0, 10.0),
        egui::vec2(DASHBOARD_TILE_ICON, DASHBOARD_TILE_ICON),
    );
    ui.painter().rect_filled(
        icon_rect,
        DASHBOARD_TILE_ICON * 0.5,
        CHROME_PRIMARY_CONTAINER,
    );
    paint_chrome_icon(
        ui.painter(),
        icon_rect.shrink(6.0),
        dashboard_service_icon(&service.label),
        CHROME_ON_PRIMARY_CONTAINER,
    );

    let text_left = icon_rect.right() + 9.0;
    let text_w = (rect.right() - text_left - 10.0).max(1.0);
    let label = ellipsize(&service.label, 18);
    let subtitle = dashboard_service_subtitle(&service.url);
    let label_galley =
        ui.fonts(|fonts| fonts.layout_no_wrap(label, font_id(CHROME_FONT), CHROME_TEXT));
    let subtitle_galley =
        ui.fonts(|fonts| fonts.layout_no_wrap(subtitle, font_id(Style::SMALL), CHROME_TEXT_DIM));
    let label_pos = egui::pos2(text_left, rect.top() + 17.0);
    let subtitle_pos = egui::pos2(text_left, rect.top() + 39.0);
    let clipped = ui.painter().with_clip_rect(egui::Rect::from_min_max(
        egui::pos2(text_left, rect.top()),
        egui::pos2(text_left + text_w, rect.bottom()),
    ));
    clipped.galley(label_pos, label_galley, CHROME_TEXT);
    clipped.galley(subtitle_pos, subtitle_galley, CHROME_TEXT_DIM);
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, service.hint.as_str())
}

/// A compact Browser chrome button in the local Material palette.
pub(super) fn nav_button(ui: &mut egui::Ui, icon: ChromeIcon, tip: &str, enabled: bool) -> bool {
    chrome_icon_button(ui, icon, tip, enabled, false).clicked()
}

/// Chrome/Edge-style trust signal for the omnibox's leading security chip,
/// derived purely from a URL's scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SecurityLevel {
    /// `https://` — a lock icon, neutral tone.
    Secure,
    /// `http://` — a "Not secure" icon/tone.
    NotSecure,
    /// `mesh://` and mesh-hosted services — trusted overlay.
    Mesh,
    /// `about:` / blank / new-tab / any other scheme.
    Neutral,
}

impl SecurityLevel {
    pub(super) const fn icon(self) -> ChromeIcon {
        match self {
            Self::Secure => ChromeIcon::Lock,
            Self::NotSecure => ChromeIcon::Warning,
            Self::Mesh => ChromeIcon::Security,
            Self::Neutral => ChromeIcon::Page,
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Secure => "Secure connection (HTTPS)",
            Self::NotSecure => "Not secure: plain HTTP",
            Self::Mesh => "Mesh: trusted overlay connection",
            Self::Neutral => "No connection security to report",
        }
    }

    pub(super) const fn tone(self) -> ChipTone {
        match self {
            Self::Secure | Self::Neutral => ChipTone::Neutral,
            Self::NotSecure => ChipTone::Warn,
            Self::Mesh => ChipTone::Info,
        }
    }
}

/// A short-list of common two-level public suffixes for the omnibox's eTLD+1
/// heuristic. This is deliberately not a vendored Public Suffix List.
const OMNIBOX_TWO_LEVEL_SUFFIXES: &[&str] = &["co.uk", "com.au", "co.jp", "org.uk"];

/// Chrome-style display breakdown of a URL for the unfocused omnibox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OmniboxDisplay {
    pub(super) scheme_shown: Option<String>,
    pub(super) host: String,
    pub(super) host_emphasis: Range<usize>,
    pub(super) rest: String,
    pub(super) security: SecurityLevel,
}

fn omnibox_etld1_range(host: &str) -> Range<usize> {
    if host.is_empty() {
        return 0..0;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return 0..host.len();
    }
    let last_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    let take = if OMNIBOX_TWO_LEVEL_SUFFIXES.contains(&last_two.as_str()) {
        3
    } else {
        2
    }
    .min(labels.len());
    let start_label = labels.len() - take;
    let start: usize = labels[..start_label].iter().map(|l| l.len() + 1).sum();
    start..host.len()
}

pub(super) fn omnibox_display(url: &str) -> OmniboxDisplay {
    let trimmed = url.trim();
    let (scheme_shown, security, after_scheme) =
        if let Some(rest) = trimmed.strip_prefix("https://") {
            (None, SecurityLevel::Secure, rest)
        } else if let Some(rest) = trimmed.strip_prefix("http://") {
            (Some("http://"), SecurityLevel::NotSecure, rest)
        } else if let Some(rest) = trimmed.strip_prefix("mesh://") {
            (Some("mesh://"), SecurityLevel::Mesh, rest)
        } else {
            return OmniboxDisplay {
                scheme_shown: (!trimmed.is_empty()).then(|| trimmed.to_owned()),
                host: String::new(),
                host_emphasis: 0..0,
                rest: String::new(),
                security: SecurityLevel::Neutral,
            };
        };

    let split_at = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (host_part, rest) = after_scheme.split_at(split_at);
    let host = host_part
        .strip_prefix("www.")
        .unwrap_or(host_part)
        .to_owned();
    let host_emphasis = omnibox_etld1_range(&host);

    OmniboxDisplay {
        scheme_shown: scheme_shown.map(str::to_owned),
        host,
        host_emphasis,
        rest: rest.to_owned(),
        security,
    }
}

pub(super) fn omnibox_layout_job(url: &str, font_id: egui::FontId) -> egui::text::LayoutJob {
    let display = omnibox_display(url);
    let mut job = egui::text::LayoutJob::default();
    let dim = omnibox_dim_format(font_id.clone());
    let strong = omnibox_strong_format(font_id);
    if display.host.is_empty() {
        if let Some(scheme) = &display.scheme_shown {
            job.append(scheme, 0.0, dim);
        }
        return job;
    }
    if let Some(scheme) = &display.scheme_shown {
        job.append(scheme, 0.0, dim.clone());
    }
    let Range { start, end } = display.host_emphasis;
    if start > 0 {
        job.append(&display.host[..start], 0.0, dim.clone());
    }
    job.append(&display.host[start..end], 0.0, strong);
    if end < display.host.len() {
        job.append(&display.host[end..], 0.0, dim.clone());
    }
    if !display.rest.is_empty() {
        job.append(&display.rest, 0.0, dim);
    }
    job
}

fn omnibox_text_clip_rect(rect: egui::Rect) -> egui::Rect {
    let inset = 4.0_f32.min((rect.width().max(0.0)) / 2.0);
    rect.shrink2(egui::vec2(inset, 0.0))
}

pub(super) fn paint_omnibox_inline_completion(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    draft: &str,
    tail: &str,
) {
    if draft.is_empty() || tail.is_empty() {
        return;
    }
    let font_id = font_id(OMNIBOX_FONT);
    let typed = ui.fonts(|f| f.layout_no_wrap(draft.to_owned(), font_id.clone(), CHROME_TEXT));
    let ghost = ui.fonts(|f| f.layout_no_wrap(tail.to_owned(), font_id, CHROME_TEXT_DIM));
    let text_clip = omnibox_text_clip_rect(rect);
    let text_pos = egui::pos2(
        text_clip.left() + typed.size().x,
        rect.center().y - ghost.size().y / 2.0,
    );
    ui.painter()
        .with_clip_rect(text_clip)
        .galley(text_pos, ghost, CHROME_TEXT_DIM);
}

/// An action chosen from the in-page right-click context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PageContextAction {
    Back,
    Forward,
    Reload,
    Edit(EditCommand),
}

fn page_context_edit_icon(command: EditCommand) -> ChromeIcon {
    match command {
        EditCommand::Undo
        | EditCommand::Redo
        | EditCommand::Cut
        | EditCommand::Copy
        | EditCommand::Paste
        | EditCommand::Delete
        | EditCommand::SelectAll => ChromeIcon::Edit,
    }
}

fn chrome_menu_row(
    ui: &mut egui::Ui,
    label: &str,
    icon: ChromeIcon,
    enabled: bool,
    disabled_tip: &'static str,
) -> egui::Response {
    let width = chrome_menu_row_width(bounded_available_width(ui)).max(1.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, OPTION_ROW_H),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    install_chrome_menu_row_accessibility(ui.ctx(), rect, label, enabled, disabled_tip);
    let fill = animated_response_fill(ui, &response, menu_item_fill(false), CHROME_TEXT, enabled);
    ui.painter().rect(
        rect,
        7.0,
        fill,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    let text_color = button_text(enabled);
    let icon_color = if enabled {
        CHROME_ICON
    } else {
        CHROME_TEXT_DIM
    };
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE - 1.0, OPTION_ICON_SIZE - 1.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, icon, icon_color);
    let text_left = icon_rect.right() + 6.0;
    let text_right = rect.right() - 8.0;
    if text_left < text_right {
        let text_rect = egui::Rect::from_min_max(
            egui::pos2(text_left, rect.top()),
            egui::pos2(text_right, rect.bottom()),
        );
        ui.painter().with_clip_rect(text_rect).text(
            egui::pos2(text_rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            font_id(CHROME_FONT),
            text_color,
        );
    }
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    if enabled {
        response
    } else {
        chrome_hover_text(response, disabled_tip)
    }
}

fn chrome_menu_row_width(available_width: f32) -> f32 {
    available_width.max(0.0)
}

fn chrome_menu_row_accesskit_id(label: &str, disabled_tip: &'static str) -> egui::Id {
    egui::Id::new(("browser-chrome-menu-row", label, disabled_tip))
}

fn chrome_menu_row_accesskit_value(enabled: bool, disabled_tip: &'static str) -> String {
    if enabled {
        "Available".to_owned()
    } else {
        format!("Unavailable: {disabled_tip}")
    }
}

fn install_chrome_menu_row_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    label: &str,
    enabled: bool,
    disabled_tip: &'static str,
) {
    let _ = ctx.accesskit_node_builder(chrome_menu_row_accesskit_id(label, disabled_tip), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(label);
        node.set_value(chrome_menu_row_accesskit_value(enabled, disabled_tip));
        node.set_bounds(accesskit_rect(rect));
        if enabled {
            node.add_action(egui::accesskit::Action::Click);
        }
    });
}

fn page_context_row(
    ui: &mut egui::Ui,
    label: &str,
    icon: ChromeIcon,
    enabled: bool,
) -> egui::Response {
    chrome_menu_row(
        ui,
        label,
        icon,
        enabled,
        "Unavailable in the current page context",
    )
}

fn page_context_separator(ui: &mut egui::Ui) {
    chrome_separator_with_inset(ui, 34.0);
}

/// Chrome-owned context menu for the page canvas. The caller applies the returned
/// action to the active session so the live page/input bridge stays in `web/mod.rs`.
pub(super) fn page_context_menu(
    resp: &egui::Response,
    can_back: bool,
    can_forward: bool,
    url: &str,
) -> Option<PageContextAction> {
    let mut action = None;
    chrome_context_menu(resp, |ui| {
        ui.spacing_mut().item_spacing.y = 3.0;

        if page_context_row(ui, "Back", ChromeIcon::Back, can_back).clicked() && can_back {
            action = Some(PageContextAction::Back);
            ui.close_menu();
        }
        if page_context_row(ui, "Forward", ChromeIcon::Forward, can_forward).clicked()
            && can_forward
        {
            action = Some(PageContextAction::Forward);
            ui.close_menu();
        }
        if page_context_row(ui, "Reload", ChromeIcon::Reload, true).clicked() {
            action = Some(PageContextAction::Reload);
            ui.close_menu();
        }
        page_context_separator(ui);
        if page_context_row(ui, "Cut", page_context_edit_icon(EditCommand::Cut), true).clicked() {
            action = Some(PageContextAction::Edit(EditCommand::Cut));
            ui.close_menu();
        }
        if page_context_row(ui, "Copy", page_context_edit_icon(EditCommand::Copy), true).clicked() {
            action = Some(PageContextAction::Edit(EditCommand::Copy));
            ui.close_menu();
        }
        if page_context_row(
            ui,
            "Paste",
            page_context_edit_icon(EditCommand::Paste),
            true,
        )
        .clicked()
        {
            action = Some(PageContextAction::Edit(EditCommand::Paste));
            ui.close_menu();
        }
        if page_context_row(
            ui,
            "Select all",
            page_context_edit_icon(EditCommand::SelectAll),
            true,
        )
        .clicked()
        {
            action = Some(PageContextAction::Edit(EditCommand::SelectAll));
            ui.close_menu();
        }
        page_context_separator(ui);
        if page_context_row(ui, "Copy page URL", ChromeIcon::Share, !url.is_empty()).clicked()
            && !url.is_empty()
        {
            ui.ctx().copy_text(url.to_owned());
            ui.close_menu();
        }
    });
    action
}

fn engine_toolbar_chip(ui: &mut egui::Ui, state: &WebState) -> egui::Response {
    let engine = state.engine;
    let tip = format!(
        "New tabs use {}. Open Browser Options to change engines.",
        engine_display_name(engine)
    );
    let (rect, response) = allocate_browser_icon_button(
        ui,
        true,
        egui::vec2(ENGINE_TOOLBAR_CHIP_W, CHROME_BUTTON),
        &tip,
    );
    let fill = animated_response_fill(
        ui,
        &response,
        engine_container(engine),
        engine_accent(engine),
        true,
    );
    ui.painter().rect(
        rect,
        ICON_BUTTON_RADIUS,
        fill,
        egui::Stroke::new(1.0, engine_accent(engine)),
        egui::StrokeKind::Inside,
    );

    let icon_rect =
        egui::Rect::from_center_size(rect.center() - egui::vec2(4.0, 0.0), egui::vec2(20.0, 20.0));
    paint_chrome_icon(
        ui.painter(),
        icon_rect,
        ChromeIcon::Engine,
        engine_on_container(engine),
    );
    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 9.0, rect.bottom() - 8.0),
        egui::vec2(ENGINE_TOOLBAR_BADGE, ENGINE_TOOLBAR_BADGE),
    );
    ui.painter().circle_filled(
        badge_rect.center(),
        ENGINE_TOOLBAR_BADGE * 0.5,
        engine_accent(engine),
    );
    let badge = engine_glyph(engine).to_owned();
    let badge_galley = ui.fonts(|fonts| fonts.layout_no_wrap(badge, font_id(10.0), CHROME_TOOLBAR));
    ui.painter().galley(
        badge_rect.center() - badge_galley.size() * 0.5,
        badge_galley,
        CHROME_TOOLBAR,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), response.rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn new_tab_type_button(ui: &mut egui::Ui, state: &WebState) -> egui::Response {
    let engine = state.engine;
    let tip = format!("Open a new tab with {}", engine_display_name(engine));
    let (rect, response) = allocate_browser_icon_button(
        ui,
        true,
        egui::vec2(NEW_TAB_TYPE_BUTTON_W, CHROME_BUTTON),
        &tip,
    );
    let fill = animated_response_fill(ui, &response, CHROME_TOOLBAR, CHROME_ICON, true);
    if fill != CHROME_TOOLBAR {
        ui.painter().rect_filled(rect, ICON_BUTTON_RADIUS, fill);
    }

    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 17.0, rect.center().y),
        egui::vec2(20.0, 20.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, ChromeIcon::NewTab, CHROME_ICON);

    let badge_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 12.0, rect.center().y),
        egui::vec2(ENGINE_TOOLBAR_BADGE + 4.0, ENGINE_TOOLBAR_BADGE),
    );
    ui.painter().rect(
        badge_rect,
        ENGINE_TOOLBAR_BADGE * 0.5,
        engine_container(engine),
        egui::Stroke::new(1.0, engine_accent(engine)),
        egui::StrokeKind::Inside,
    );
    let badge = engine_glyph(engine).to_owned();
    let badge_galley =
        ui.fonts(|fonts| fonts.layout_no_wrap(badge, font_id(10.0), engine_on_container(engine)));
    ui.painter().galley(
        badge_rect.center() - badge_galley.size() * 0.5,
        badge_galley,
        engine_on_container(engine),
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), response.rect, response.has_focus());
    chrome_hover_text(response, tip)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NavToolbarSlot {
    NewTabType,
    Back,
    RefreshStop,
    Forward,
    Go,
    PageActions,
    Passwords,
    Capture,
    Downloads,
    DownloadBadge,
    BlockedRequests,
    LoadingStatus,
    MediaToolbar,
    Options,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct NavToolbarModel {
    pub(super) left_of_location: Vec<NavToolbarSlot>,
    pub(super) right_of_location: Vec<NavToolbarSlot>,
    pub(super) trailing_reserve: f32,
}

fn nav_toolbar_slot_width(
    slot: NavToolbarSlot,
    active_downloads: usize,
    blocked_requests: u64,
) -> f32 {
    match slot {
        NavToolbarSlot::NewTabType => NEW_TAB_TYPE_BUTTON_W,
        NavToolbarSlot::Back
        | NavToolbarSlot::RefreshStop
        | NavToolbarSlot::Forward
        | NavToolbarSlot::Go
        | NavToolbarSlot::PageActions
        | NavToolbarSlot::Passwords
        | NavToolbarSlot::Capture
        | NavToolbarSlot::Downloads
        | NavToolbarSlot::LoadingStatus
        | NavToolbarSlot::Options => CHROME_BUTTON,
        NavToolbarSlot::DownloadBadge => toolbar_count_badge_width(active_downloads as u64),
        NavToolbarSlot::BlockedRequests => {
            CHROME_BUTTON + CHROME_GAP + toolbar_count_badge_width(blocked_requests)
        }
        NavToolbarSlot::MediaToolbar => media_toolbar_estimated_width(MediaToolbarDensity::Compact),
    }
}

fn nav_toolbar_slots_budget(
    compact: bool,
    slots: &[NavToolbarSlot],
    active_downloads: usize,
    blocked_requests: u64,
) -> f32 {
    if slots.is_empty() {
        return 0.0;
    }
    let slot_width: f32 = slots
        .iter()
        .map(|slot| nav_toolbar_slot_width(*slot, active_downloads, blocked_requests))
        .sum();
    let full_toolbar_breathing_gap = if compact { 0.0 } else { CHROME_GAP };
    slot_width + CHROME_GAP * slots.len() as f32 + full_toolbar_breathing_gap
}

pub(super) fn nav_toolbar_model(
    compact: bool,
    active_downloads: usize,
    blocked_requests: u64,
    loading: bool,
    has_media_toolbar: bool,
) -> NavToolbarModel {
    let left_of_location = vec![
        NavToolbarSlot::NewTabType,
        NavToolbarSlot::Back,
        NavToolbarSlot::RefreshStop,
        NavToolbarSlot::Forward,
    ];
    let mut right_of_location = if compact {
        vec![NavToolbarSlot::Go, NavToolbarSlot::Downloads]
    } else {
        vec![
            NavToolbarSlot::Go,
            NavToolbarSlot::PageActions,
            NavToolbarSlot::Passwords,
            NavToolbarSlot::Capture,
            NavToolbarSlot::Downloads,
        ]
    };

    if active_downloads > 0 {
        right_of_location.push(NavToolbarSlot::DownloadBadge);
    }
    if !compact {
        if blocked_requests > 0 {
            right_of_location.push(NavToolbarSlot::BlockedRequests);
        }
        if loading {
            right_of_location.push(NavToolbarSlot::LoadingStatus);
        }
        if has_media_toolbar {
            right_of_location.push(NavToolbarSlot::MediaToolbar);
        }
    }
    right_of_location.push(NavToolbarSlot::Options);

    let trailing_reserve = nav_toolbar_slots_budget(
        compact,
        &right_of_location,
        active_downloads,
        blocked_requests,
    );
    NavToolbarModel {
        left_of_location,
        right_of_location,
        trailing_reserve,
    }
}

fn nav_omnibox_trailing_reserve(
    compact: bool,
    active_downloads: usize,
    blocked_requests: u64,
    has_media_toolbar: bool,
) -> f32 {
    nav_toolbar_model(
        compact,
        active_downloads,
        blocked_requests,
        false,
        has_media_toolbar,
    )
    .trailing_reserve
}

fn nav_omnibox_trailing_reserve_with_loading(
    compact: bool,
    active_downloads: usize,
    blocked_requests: u64,
    loading: bool,
    has_media_toolbar: bool,
) -> f32 {
    nav_toolbar_model(
        compact,
        active_downloads,
        blocked_requests,
        loading,
        has_media_toolbar,
    )
    .trailing_reserve
}

fn nav_full_chrome_min_width_with_loading(
    active_downloads: usize,
    blocked_requests: u64,
    loading: bool,
) -> f32 {
    let leading_controls = NEW_TAB_TYPE_BUTTON_W + CHROME_BUTTON * 3.0 + CHROME_GAP * 4.0;
    leading_controls
        + nav_omnibox_trailing_reserve_with_loading(
            false,
            active_downloads,
            blocked_requests,
            loading,
            true,
        )
        + NAV_FULL_OMNIBOX_FLOOR
}

fn nav_full_chrome_min_width(active_downloads: usize, blocked_requests: u64) -> f32 {
    nav_full_chrome_min_width_with_loading(active_downloads, blocked_requests, false)
}

fn nav_chrome_uses_compact_layout_with_loading(
    available_width: f32,
    active_downloads: usize,
    blocked_requests: u64,
    loading: bool,
) -> bool {
    !available_width.is_finite()
        || available_width
            < nav_full_chrome_min_width_with_loading(active_downloads, blocked_requests, loading)
}

fn nav_chrome_uses_compact_layout(
    available_width: f32,
    active_downloads: usize,
    blocked_requests: u64,
) -> bool {
    nav_chrome_uses_compact_layout_with_loading(
        available_width,
        active_downloads,
        blocked_requests,
        false,
    )
}

fn nav_omnibox_widths(
    remaining_width: f32,
    compact: bool,
    active_downloads: usize,
    blocked_requests: u64,
    has_media_toolbar: bool,
) -> (f32, f32) {
    nav_omnibox_widths_with_loading(
        remaining_width,
        compact,
        active_downloads,
        blocked_requests,
        false,
        has_media_toolbar,
    )
}

fn nav_omnibox_widths_with_loading(
    remaining_width: f32,
    compact: bool,
    active_downloads: usize,
    blocked_requests: u64,
    loading: bool,
    has_media_toolbar: bool,
) -> (f32, f32) {
    let available = (remaining_width
        - nav_omnibox_trailing_reserve_with_loading(
            compact,
            active_downloads,
            blocked_requests,
            loading,
            has_media_toolbar,
        ))
    .max(NAV_OMNIBOX_TINY_MIN);
    let min_floor = if compact {
        NAV_COMPACT_OMNIBOX_MIN
    } else {
        NAV_FULL_OMNIBOX_MIN
    };
    (
        available,
        available.min(min_floor).max(NAV_OMNIBOX_TINY_MIN),
    )
}

fn omnibox_should_clear_on_edit_start(
    was_focused: bool,
    now_focused: bool,
    address: &str,
    page_url: &str,
) -> bool {
    if was_focused || !now_focused {
        return false;
    }
    let address = address.trim();
    let page_url = page_url.trim();
    !address.is_empty() && !page_url.is_empty() && address == page_url
}

/// The navigation chrome bar — a §4-token toolbar. Back / forward / reload act on
/// the active session; the address bar loads on submit. On a crashed tab, Reload
/// becomes a respawn request. The page-actions menu (BOOKMARKS-10) hangs off both
/// the toolbar star button and the address bar's right-click.
pub(super) fn nav_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    let (active_downloads, total_downloads) = state.download_counts();
    let blocked = state
        .tabs
        .get(state.active)
        .map_or(0, |t| t.session.blocked_count());
    let crashed = state
        .tabs
        .get(state.active)
        .is_some_and(|t| t.session.is_crashed());
    let nav = state
        .tabs
        .get(state.active)
        .map(|t| t.session.nav().clone())
        .unwrap_or_default();
    let has_tab = !state.tabs.is_empty();
    let loading_status = has_tab && !crashed && nav.loading;
    let compact_nav = nav_chrome_uses_compact_layout_with_loading(
        ui.available_width(),
        active_downloads,
        u64::from(blocked),
        loading_status,
    );
    let active_engine = state.tabs.get(state.active).map(|t| t.engine);
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
    let permission_summary = site_info_permission_summary(state);
    let has_page = has_tab && !crashed && !page_url.trim().is_empty();
    let downloads_tip = downloads_toolbar_tip(active_downloads, total_downloads);
    let is_bookmarked = has_page
        && state
            .bookmarked_urls
            .contains(super::bookmark_membership_key(&page_url));
    let has_media_toolbar = browser_media_toolbar_model(state).is_some();

    let mut accepted_suggestion: Option<String> = None;
    let was_omnibox_focused = state.omnibox_focused;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = CHROME_GAP;
        if new_tab_type_button(ui, state).clicked() {
            state.request_new_tab(state.engine);
        }

        if nav_button(
            ui,
            ChromeIcon::Back,
            "Back",
            has_tab && !crashed && nav.can_back,
        ) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_back();
            }
        }
        // Stop while CEF is loading; otherwise Reload respawns crashed tabs or
        // reloads the page. Servo currently has no real cancel-load hook (DD-2,
        // investigated 2026-07-10 — see `can_show_stop_control`), so its compact
        // chrome keeps the honest Reload control while loading.
        let can_stop = super::can_show_stop_control(has_tab, crashed, nav.loading, active_engine);
        let (nav_icon, nav_tip) = if can_stop {
            (ChromeIcon::Stop, "Stop loading")
        } else if crashed {
            (ChromeIcon::Reload, "Reload (restart page)")
        } else {
            (ChromeIcon::Reload, "Reload")
        };
        if nav_button(ui, nav_icon, nav_tip, has_tab) {
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
        if nav_button(
            ui,
            ChromeIcon::Forward,
            "Forward",
            has_tab && !crashed && nav.can_forward,
        ) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_forward();
            }
        }

        // The address bar fills the rest of the row.
        let (omnibox_desired_w, omnibox_min_w) = nav_omnibox_widths_with_loading(
            ui.available_width(),
            compact_nav,
            active_downloads,
            u64::from(blocked),
            loading_status,
            has_media_toolbar,
        );
        let resp = chrome_omnibox_field(
            ui,
            has_tab && !crashed,
            &mut state.address,
            "Enter an address",
            omnibox_desired_w,
            omnibox_min_w,
            "Enter an address",
            Some(super::omnibox_widget_id()),
            &page_url,
            {
                let active = state.active;
                let tabs = &state.tabs;
                move || {
                    tabs.get(active)
                        .map_or_else(Vec::new, |t| t.session.recent_resource_requests())
                }
            },
            permission_summary.as_ref(),
        );
        if omnibox_should_clear_on_edit_start(
            was_omnibox_focused,
            resp.has_focus(),
            &state.address,
            &page_url,
        ) {
            state.address.clear();
            state.suggestions.clear();
            ui.ctx().request_repaint();
        }
        // Latch omnibox focus for next frame's engine-sync + accelerator
        // guards (the same tracked-focus idiom as `Tab::page_focused`).
        state.omnibox_focused = resp.has_focus();
        state.chrome_edit_focus |= resp.has_focus();
        if resp.changed() && has_tab && !crashed {
            state.update_suggestions_for_address();
        }
        // OMNIBOX-STYLE — when the omnibox is NOT being edited, paint the
        // Chrome-style elided/emphasized read-out ON TOP of the TextEdit
        // (never touching its own layouter/cursor logic, so click-to-edit
        // cursor placement stays exactly as correct as it is today). Focused
        // editing always shows the full, unmodified draft underneath.
        if has_tab && !crashed && resp.has_focus() {
            if let Some(tail) = state.suggestions.inline_completion_tail() {
                paint_omnibox_inline_completion(ui, resp.rect, &state.address, &tail);
            }
        } else if has_tab && !crashed && !state.address.trim().is_empty() {
            let font_id = font_id(OMNIBOX_FONT);
            let job = omnibox_layout_job(&state.address, font_id);
            if !job.is_empty() {
                let galley = ui.fonts(|f| f.layout_job(job));
                ui.painter().rect_filled(resp.rect, 4.0, CHROME_SURFACE);
                let text_clip = omnibox_text_clip_rect(resp.rect);
                let text_pos = egui::pos2(
                    text_clip.left(),
                    resp.rect.center().y - galley.size().y / 2.0,
                );
                ui.painter()
                    .with_clip_rect(text_clip)
                    .galley(text_pos, galley, CHROME_TEXT);
            }
        }
        // Keyboard-navigate the suggestion dropdown: a single-line TextEdit ignores
        // vertical arrows, so intercepting Up/Down here (while the omnibox has focus)
        // is free and doesn't disturb caret motion. Enter then commits the highlight.
        if resp.has_focus() && has_tab && !crashed {
            if ui.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                state.suggestions.move_selection(1);
            } else if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                state.suggestions.move_selection(-1);
            }
        }
        // BOOKMARKS-10 — right-click the address bar for the same page actions
        // (bookmark / copy URL / Send-in-Chat) the toolbar star exposes.
        chrome_context_menu(&resp, |ui| {
            page_actions_menu(
                ui,
                state.bus_root.as_deref(),
                active_engine,
                is_bookmarked,
                &page_url,
                &page_title,
            );
        });
        let submit = resp.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && has_tab
            && !crashed;

        let go = chrome_icon_button(
            ui,
            ChromeIcon::Forward,
            "Go",
            has_tab && !crashed && !state.address.trim().is_empty(),
            false,
        )
        .clicked();

        if submit {
            // Enter commits the keyboard-highlighted suggestion if one is selected,
            // else the typed draft — Chrome's omnibox behavior.
            if let Some(selected) = state.suggestions.selected_value() {
                state.accept_suggestion(selected);
            } else {
                state.submit_address();
            }
        } else if go {
            state.submit_address();
        }

        if !compact_nav {
            // BOOKMARKS-10 — the page-actions menu (bookmark this page / copy its URL /
            // send it in Chat). The SAME three verbs also hang off the address bar's
            // right-click (above), so both the toolbar and the context menu reach them.
            page_actions_button(
                ui,
                has_page,
                is_bookmarked,
                state.bus_root.as_deref(),
                active_engine,
                &page_url,
                &page_title,
            );
            password_menu(
                ui,
                state,
                &page_url,
                has_page,
                active_engine == Some(BrowserEngine::Cef),
            );

            // Capture + Downloads are de-duplicated in vertical mode: the rail's
            // bottom affordance cluster owns them, so the toolbar drops them to
            // avoid two live copies. The View menu / Capture submenu still reach
            // both in either layout.
            if !state.vertical_tabs {
                if nav_button(
                    ui,
                    ChromeIcon::Capture,
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

                if chrome_icon_button(
                    ui,
                    ChromeIcon::Downloads,
                    &downloads_tip,
                    true,
                    state.downloads_open,
                )
                .clicked()
                {
                    state.downloads_open = !state.downloads_open;
                    if state.downloads_open {
                        state.refresh_downloads();
                    }
                }
                download_count_badge(ui, active_downloads, &downloads_tip);
            }

            // BOOKMARKS-7 — a compact "N blocked" shield when the ad-filter has dropped
            // requests on this page (honest 0 stays hidden). Reads the session's
            // per-page counter; the engine is compiled from the mackesd `adfilter` blob.
            if blocked > 0 {
                let top_blocked = state
                    .tabs
                    .get(state.active)
                    .map(|t| t.session.block_tally().top_domains(6))
                    .unwrap_or_default();
                ad_filter_chip(ui, u64::from(blocked), &top_blocked);
            }

            if has_tab && !crashed && nav.loading {
                loading_globe(ui, CHROME_BUTTON, "toolbar");
            }
            browser_media_toolbar(ui, state);
        } else if !state.vertical_tabs {
            // Compact toolbar keeps Downloads only in horizontal mode; the rail
            // cluster owns it when the vertical layout is active.
            if chrome_icon_button(
                ui,
                ChromeIcon::Downloads,
                &downloads_tip,
                true,
                state.downloads_open,
            )
            .clicked()
            {
                state.downloads_open = !state.downloads_open;
                if state.downloads_open {
                    state.refresh_downloads();
                }
            }
            download_count_badge(ui, active_downloads, &downloads_tip);
        }

        if chrome_icon_button(
            ui,
            ChromeIcon::Options,
            "Browser options",
            true,
            state.active_internal_page() == Some(super::BrowserInternalPage::Options),
        )
        .clicked()
        {
            state.open_options_tab();
        }
    });
    if has_tab && !crashed {
        accepted_suggestion = suggestions_panel(ui, state);
    }
    if let Some(suggestion) = accepted_suggestion {
        state.accept_suggestion(suggestion);
    }
}

/// The Browser page-actions menu (BOOKMARKS-10): the mesh-integration verbs on
/// the current page. Rendered by both the toolbar star and the address bar's
/// right-click context menu.
pub(super) fn page_actions_menu(
    ui: &mut egui::Ui,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    is_bookmarked: bool,
    url: &str,
    title: &str,
) {
    chrome_popup_frame(ui, PAGE_ACTIONS_MENU_W, |ui| {
        page_actions_menu_contents(ui, bus_root, engine, is_bookmarked, url, title);
    });
}

fn page_actions_menu_contents(
    ui: &mut egui::Ui,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    is_bookmarked: bool,
    url: &str,
    title: &str,
) {
    let has_page = !url.trim().is_empty();
    let bookmark_label = if is_bookmarked {
        "Bookmarked"
    } else {
        "Add bookmark"
    };
    let bookmark_enabled = has_page && !is_bookmarked;
    let bookmark_disabled_tip = if is_bookmarked {
        "Already in Bookmarks"
    } else {
        "No loaded page to bookmark"
    };
    if chrome_menu_row(
        ui,
        bookmark_label,
        ChromeIcon::Bookmark,
        bookmark_enabled,
        bookmark_disabled_tip,
    )
    .clicked()
    {
        super::publish(
            super::ACTION_BOOKMARKS_ADD,
            &super::bookmark_add_body(url, title),
        );
        ui.close_menu();
    }
    if chrome_menu_row(
        ui,
        "Copy URL",
        ChromeIcon::Page,
        has_page,
        "No loaded page URL to copy",
    )
    .clicked()
    {
        ui.ctx().copy_text(url.to_string());
        ui.close_menu();
    }
    if chrome_menu_row(
        ui,
        "Send in Chat",
        ChromeIcon::Share,
        has_page,
        "No loaded page to send",
    )
    .clicked()
    {
        super::publish(
            super::ACTION_CHAT_SEND,
            &super::chat_share_body(&super::local_hostname(), url, title),
        );
        ui.close_menu();
    }
    for target in [
        super::BrowserShareTarget::Peer,
        super::BrowserShareTarget::Phone,
        super::BrowserShareTarget::Email,
        super::BrowserShareTarget::Qr,
    ] {
        if chrome_menu_row(
            ui,
            &format!("Share to {}", target.label()),
            ChromeIcon::Share,
            has_page,
            "No loaded page to share",
        )
        .clicked()
        {
            super::publish_browser_share(bus_root, target, url, title);
            ui.close_menu();
        }
    }
    for target in [
        super::BrowserSendTabTarget::Node,
        super::BrowserSendTabTarget::Phone,
    ] {
        if chrome_menu_row(
            ui,
            &format!("Send tab to {}", target.label()),
            ChromeIcon::Tabs,
            has_page,
            "No loaded tab to send",
        )
        .clicked()
        {
            if let Some(engine) = engine {
                super::publish_browser_send_tab(bus_root, target, engine, url, title);
            }
            ui.close_menu();
        }
    }
}

const fn page_actions_tip(is_bookmarked: bool) -> &'static str {
    if is_bookmarked {
        "Bookmarked: copy URL, share, send tab"
    } else {
        "Page actions: bookmark, copy URL, share"
    }
}

fn page_actions_popup_id() -> egui::Id {
    egui::Id::new("mde_web_page_actions_menu_popup")
}

/// The toolbar star that opens the BOOKMARKS-10 [`page_actions_menu`].
pub(super) fn page_actions_button(
    ui: &mut egui::Ui,
    has_page: bool,
    is_bookmarked: bool,
    bus_root: Option<&Path>,
    engine: Option<BrowserEngine>,
    url: &str,
    title: &str,
) {
    let color = page_action_icon_color(has_page, is_bookmarked);
    let tip = page_actions_tip(is_bookmarked);
    let popup_id = page_actions_popup_id();
    let response = toolbar_icon_menu_anchor(ui, popup_id, ChromeIcon::Bookmark, color, tip);
    if response.clicked() || menu_anchor_keyboard_toggle(ui, &response) {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    egui::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            reserve_toolbar_popup_width(ui, PAGE_ACTIONS_MENU_W);
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            page_actions_menu(ui, bus_root, engine, is_bookmarked, url, title);
        },
    );
}

/// SECURITY-INFO — the plain-language headline for a [`SecurityLevel`].
pub(super) const fn security_headline(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::Secure => "Connection is secure",
        SecurityLevel::NotSecure => "Your connection to this site is not secure",
        SecurityLevel::Mesh => "Mesh service: trusted overlay",
        SecurityLevel::Neutral => "About this page",
    }
}

/// SECURITY-INFO — the [`site_info_panel`]'s content, derived from the current
/// page URL.
pub(super) struct SiteInfoSummary {
    pub(super) security: SecurityLevel,
    pub(super) headline: &'static str,
    pub(super) host: String,
    pub(super) host_emphasis: Range<usize>,
    pub(super) cert_line: Option<&'static str>,
    pub(super) confusable: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SiteInfoResourceSummary {
    pub(super) mixed_content_blocks: usize,
    pub(super) mixed_content_hosts: Vec<String>,
    pub(super) tracker_blocks: usize,
    pub(super) tracker_hosts: Vec<String>,
    pub(super) safe_browsing_blocks: usize,
    pub(super) safe_browsing_hosts: Vec<String>,
    pub(super) managed_policy_blocks: usize,
    pub(super) managed_policy_rules: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SiteInfoPermissionSummary {
    pub(super) host: String,
    pub(super) forgotten: bool,
    pub(super) session_grants: Vec<String>,
    pub(super) denied_prompts: Vec<String>,
}

/// Human-readable IDN homograph/spoofing warning for `host`, or `None` when it
/// is not a confusable/punycode risk.
pub(super) fn confusable_warning(host: &str) -> Option<String> {
    confusable_reason(host).map(|reason| {
        match reason {
            ConfusableReason::Punycode => {
                "Punycode/IDN address (xn--): verify this is the site you expect"
            }
            ConfusableReason::ConfusableBlock => {
                "Look-alike letters (Cyrillic/Greek): this site may impersonate another site"
            }
            ConfusableReason::MixedScript => {
                "Mixed-script address: letters from more than one alphabet can spoof a site name"
            }
        }
        .to_owned()
    })
}

pub(super) fn site_info_resource_summary(
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> SiteInfoResourceSummary {
    let mut mixed_content_blocks = 0usize;
    let mut mixed_content_hosts = BTreeSet::new();
    let mut tracker_blocks = 0usize;
    let mut tracker_hosts = BTreeSet::new();
    let mut safe_browsing_blocks = 0usize;
    let mut safe_browsing_hosts = BTreeSet::new();
    let mut managed_policy_blocks = 0usize;
    let mut managed_policy_rules = BTreeSet::new();
    for resource in recent.iter().filter(|resource| !resource.allowed) {
        let Some(blocked_by) = resource.blocked_by.as_deref() else {
            continue;
        };
        if blocked_by == "mixed-content:http" {
            mixed_content_blocks = mixed_content_blocks.saturating_add(1);
            if let Some(host) = host_of(&resource.url) {
                mixed_content_hosts.insert(host);
            }
        } else if let Some(rule) = blocked_by.strip_prefix("safe-browsing:") {
            safe_browsing_blocks = safe_browsing_blocks.saturating_add(1);
            safe_browsing_hosts.insert(rule.to_owned());
        } else if let Some(rule) = blocked_by.strip_prefix("managed-policy:") {
            managed_policy_blocks = managed_policy_blocks.saturating_add(1);
            managed_policy_rules.insert(rule.to_owned());
        } else {
            tracker_blocks = tracker_blocks.saturating_add(1);
            if let Some(host) = host_of(&resource.url) {
                tracker_hosts.insert(host);
            }
        }
    }
    SiteInfoResourceSummary {
        mixed_content_blocks,
        mixed_content_hosts: mixed_content_hosts.into_iter().take(4).collect(),
        tracker_blocks,
        tracker_hosts: tracker_hosts.into_iter().take(4).collect(),
        safe_browsing_blocks,
        safe_browsing_hosts: safe_browsing_hosts.into_iter().take(4).collect(),
        managed_policy_blocks,
        managed_policy_rules: managed_policy_rules.into_iter().take(4).collect(),
    }
}

pub(super) fn permission_kind_site_info_label(kind: u8) -> &'static str {
    match kind {
        0 => "geolocation",
        1 => "notifications",
        2 => "clipboard",
        3 => "camera",
        4 => "microphone",
        5 => "camera and microphone",
        _ => "device capability",
    }
}

pub(super) fn site_info_permission_summary(state: &WebState) -> Option<SiteInfoPermissionSummary> {
    let host = state.active_first_party()?;
    let forgotten = state
        .forgotten_permission_sites
        .iter()
        .any(|site| site == &host);
    let session_grants = state
        .granted_permissions
        .iter()
        .filter(|(origin, _)| host_of(origin).as_deref() == Some(host.as_str()))
        .map(|(_, kind)| permission_kind_site_info_label(*kind).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(4)
        .collect();
    let denied_prompts = state
        .site_permission_prompts
        .iter()
        .filter(|prompt| prompt.host == host)
        .map(|prompt| format!("{} {}", prompt.kind.wire(), prompt.decision))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(4)
        .collect();
    Some(SiteInfoPermissionSummary {
        host,
        forgotten,
        session_grants,
        denied_prompts,
    })
}

pub(super) fn site_info_summary(page_url: &str) -> SiteInfoSummary {
    let display = omnibox_display(page_url);
    let cert_line = matches!(display.security, SecurityLevel::Secure)
        .then_some("Certificate: valid; the connection is encrypted");
    let confusable = confusable_warning(&display.host);
    SiteInfoSummary {
        security: display.security,
        headline: security_headline(display.security),
        host: display.host,
        host_emphasis: display.host_emphasis,
        cert_line,
        confusable,
    }
}

/// Stable, ui-path-independent id for the [`site_info_panel`] popup.
pub(super) fn security_chip_popup_id() -> egui::Id {
    egui::Id::new("mde_web_security_chip_popup")
}

fn site_info_status_row(ui: &mut egui::Ui, icon: ChromeIcon, text: String, color: Color32) {
    ui.horizontal(|ui| {
        let (icon_rect, _) = ui.allocate_exact_size(
            egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
            egui::Sense::hover(),
        );
        paint_chrome_icon(ui.painter(), icon_rect, icon, color);
        ui.add(egui::Label::new(egui::RichText::new(text).small().color(color)).wrap());
    });
}

/// SECURITY-INFO — the Chrome-style "site information" popup opened by the
/// omnibox's security chip.
pub(super) fn site_info_panel(
    ui: &mut egui::Ui,
    page_url: &str,
    recent_resources: &[mde_web_preview_client::ResourceRequestStatus],
    permissions: Option<&SiteInfoPermissionSummary>,
) {
    let summary = site_info_summary(page_url);
    let resources = site_info_resource_summary(recent_resources);
    let security_color = tone_color(summary.security.tone());
    ui.set_max_width(300.0);
    ui.horizontal(|ui| {
        let (icon_rect, _) = ui.allocate_exact_size(
            egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
            egui::Sense::hover(),
        );
        paint_chrome_icon(
            ui.painter(),
            icon_rect,
            summary.security.icon(),
            security_color,
        );
        ui.label(
            egui::RichText::new(summary.headline)
                .color(security_color)
                .strong(),
        );
    });
    if summary.host.is_empty() {
        ui.label(egui::RichText::new("No page is currently loaded").color(CHROME_TEXT_DIM));
    } else {
        let font_id = font_id(CHROME_FONT);
        let mut job = egui::text::LayoutJob::default();
        let dim = egui::TextFormat {
            font_id: font_id.clone(),
            color: CHROME_TEXT_DIM,
            ..Default::default()
        };
        let strong = egui::TextFormat {
            font_id,
            color: CHROME_TEXT,
            ..Default::default()
        };
        let Range { start, end } = summary.host_emphasis;
        if start > 0 {
            job.append(&summary.host[..start], 0.0, dim.clone());
        }
        job.append(&summary.host[start..end], 0.0, strong);
        if end < summary.host.len() {
            job.append(&summary.host[end..], 0.0, dim);
        }
        ui.label(job);
    }
    if let Some(warn) = summary.confusable.as_deref() {
        site_info_status_row(ui, ChromeIcon::Warning, warn.to_owned(), CHROME_WARN);
    }
    if let Some(cert_line) = summary.cert_line {
        ui.label(
            egui::RichText::new(cert_line)
                .small()
                .color(CHROME_TEXT_DIM),
        );
    }
    if resources.managed_policy_blocks > 0 {
        let suffix = if resources.managed_policy_blocks == 1 {
            ""
        } else {
            "s"
        };
        site_info_status_row(
            ui,
            ChromeIcon::Warning,
            format!(
                "Managed policy blocked: {} resource{suffix}",
                resources.managed_policy_blocks
            ),
            CHROME_WARN,
        );
        if !resources.managed_policy_rules.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Matched policy: {}",
                        resources.managed_policy_rules.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
    }
    if resources.safe_browsing_blocks > 0 {
        let suffix = if resources.safe_browsing_blocks == 1 {
            ""
        } else {
            "s"
        };
        site_info_status_row(
            ui,
            ChromeIcon::Warning,
            format!(
                "Unsafe content blocked: {} resource{suffix}",
                resources.safe_browsing_blocks
            ),
            CHROME_WARN,
        );
        if !resources.safe_browsing_hosts.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Unsafe sites: {}",
                        resources.safe_browsing_hosts.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
    }
    if resources.mixed_content_blocks > 0 {
        let suffix = if resources.mixed_content_blocks == 1 {
            ""
        } else {
            "s"
        };
        site_info_status_row(
            ui,
            ChromeIcon::Warning,
            format!(
                "Insecure content blocked: {} public HTTP subresource{suffix}",
                resources.mixed_content_blocks
            ),
            CHROME_WARN,
        );
        if !resources.mixed_content_hosts.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Blocked content sites: {}",
                        resources.mixed_content_hosts.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
    }
    if resources.tracker_blocks > 0 {
        let suffix = if resources.tracker_blocks == 1 {
            ""
        } else {
            "s"
        };
        site_info_status_row(
            ui,
            ChromeIcon::Privacy,
            format!(
                "Privacy protection blocked: {} tracker/filter resource{suffix}",
                resources.tracker_blocks
            ),
            CHROME_TEXT_DIM,
        );
        if !resources.tracker_hosts.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Blocked tracker sites: {}",
                        resources.tracker_hosts.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
    }
    if let Some(permissions) = permissions {
        chrome_separator(ui);
        ui.label(
            egui::RichText::new("Permissions")
                .small()
                .strong()
                .color(CHROME_TEXT),
        );
        ui.add(
            egui::Label::new(
                egui::RichText::new("Sensitive capabilities are blocked by default")
                    .small()
                    .color(CHROME_TEXT_DIM),
            )
            .wrap(),
        );
        if !permissions.session_grants.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Allowed this session: {}",
                        permissions.session_grants.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
        if !permissions.denied_prompts.is_empty() {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "Denied prompts: {}",
                        permissions.denied_prompts.join(", ")
                    ))
                    .small()
                    .color(CHROME_TEXT_DIM),
                )
                .wrap(),
            );
        }
        if permissions.forgotten {
            site_info_status_row(
                ui,
                ChromeIcon::Warning,
                format!(
                    "{} permissions were forgotten; future requests must be approved again",
                    permissions.host
                ),
                CHROME_WARN,
            );
        }
    }
    chrome_separator(ui);
    ui.label(
        egui::RichText::new("Cookies & site data clear when you close the browser")
            .small()
            .color(CHROME_TEXT_DIM),
    );
}

/// OMNIBOX-STYLE — the leading security chip, reflecting the committed page URL.
pub(super) fn security_chip(
    ui: &mut egui::Ui,
    page_url: &str,
    recent_resources: &[mde_web_preview_client::ResourceRequestStatus],
    permissions: Option<&SiteInfoPermissionSummary>,
) {
    let security = omnibox_display(page_url).security;
    let popup_id = security_chip_popup_id();
    let enabled = ui.is_enabled();
    let (rect, resp) = allocate_browser_icon_button(
        ui,
        enabled,
        egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
        security.label(),
    );
    paint_transparent_icon_button_state(
        ui,
        &resp,
        rect,
        ICON_BUTTON_RADIUS,
        tone_color(security.tone()),
        enabled,
    );
    paint_chrome_icon(
        ui.painter(),
        rect,
        security.icon(),
        tone_color(security.tone()),
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, resp.has_focus());
    let resp = chrome_hover_text(resp, security.label());
    if resp.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    egui::popup_below_widget(
        ui,
        popup_id,
        &resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            site_info_popup_frame(ui, motion, page_url, recent_resources, permissions);
        },
    );
}

fn site_info_popup_frame(
    ui: &mut egui::Ui,
    motion: BrowserPopoverMotion,
    page_url: &str,
    recent_resources: &[mde_web_preview_client::ResourceRequestStatus],
    permissions: Option<&SiteInfoPermissionSummary>,
) {
    if motion.active {
        ui.ctx().request_repaint();
    }
    ui.multiply_opacity(motion.opacity.max(0.2));
    if motion.anchor_offset > 0.0 {
        ui.add_space(motion.anchor_offset);
    }
    chrome_popup_frame(ui, SITE_INFO_POPUP_W, |ui| {
        let scale_inset = ((1.0 - motion.scale) * 16.0).clamp(0.0, 1.0);
        if scale_inset > 0.0 {
            ui.horizontal(|ui| {
                ui.add_space(scale_inset);
                ui.vertical(|ui| site_info_panel(ui, page_url, recent_resources, permissions));
            });
        } else {
            site_info_panel(ui, page_url, recent_resources, permissions);
        }
    });
}

fn omnibox_security_button(
    ui: &mut egui::Ui,
    page_url: &str,
    recent_resources: impl FnOnce() -> Vec<mde_web_preview_client::ResourceRequestStatus>,
    permissions: Option<&SiteInfoPermissionSummary>,
) -> egui::Response {
    let security = omnibox_display(page_url).security;
    let popup_id = security_chip_popup_id();
    let enabled = ui.is_enabled();
    let (rect, resp) = allocate_browser_icon_button(
        ui,
        enabled,
        egui::vec2(OMNIBOX_SECURITY_SLOT_W - 4.0, CHROME_OMNIBOX_H - 4.0),
        security.label(),
    );
    paint_transparent_icon_button_state(
        ui,
        &resp,
        rect,
        ICON_BUTTON_RADIUS,
        tone_color(security.tone()),
        enabled,
    );
    paint_chrome_icon(
        ui.painter(),
        rect.shrink(2.0),
        security.icon(),
        tone_color(security.tone()),
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, resp.has_focus());
    let resp = chrome_hover_text(resp, security.label());
    if resp.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    egui::popup_below_widget(
        ui,
        popup_id,
        &resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            let recent_resources = recent_resources();
            site_info_popup_frame(ui, motion, page_url, &recent_resources, permissions);
        },
    );
    resp
}

/// Omnibox search `items` with any entry that duplicates a history hit removed
/// (a history-matched URL is already shown once, above, by
/// [`suggestions_panel`] — Chrome-style history-then-search ordering with no
/// repeats). Pure and paint-free so it's directly unit-testable.
pub(super) fn dedup_search_items<'a>(items: &'a [String], history: &[String]) -> Vec<&'a String> {
    items.iter().filter(|s| !history.contains(s)).collect()
}

fn suggestion_chip(
    ui: &mut egui::Ui,
    label: &str,
    icon: ChromeIcon,
    text_color: Color32,
    fill: Color32,
    access_label: &str,
    access_value: &str,
    access_kind: &'static str,
    index: usize,
    total: usize,
    selected: bool,
) -> egui::Response {
    let font = font_id(CHROME_FONT);
    let text_width = ui.fonts(|fonts| {
        fonts
            .layout_no_wrap(label.to_owned(), font.clone(), text_color)
            .size()
            .x
    });
    let width = (OPTION_ICON_SIZE + 30.0 + text_width).clamp(96.0, 280.0);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, CHROME_BUTTON), egui::Sense::click());
    install_suggestion_chip_accessibility(
        ui.ctx(),
        rect,
        access_kind,
        index,
        access_label,
        access_value,
        total,
        selected,
    );
    let fill = animated_response_fill(ui, &response, fill, CHROME_TEXT, true);
    ui.painter().rect(
        rect,
        ICON_BUTTON_RADIUS,
        fill,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 15.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE - 2.0, OPTION_ICON_SIZE - 2.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, icon, text_color);
    let text_rect = egui::Rect::from_min_max(
        egui::pos2(icon_rect.right() + 7.0, rect.top()),
        egui::pos2(rect.right() - 8.0, rect.bottom()),
    );
    ui.painter().with_clip_rect(text_rect).text(
        egui::pos2(text_rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font,
        text_color,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    response
}

fn suggestion_chip_accesskit_id(kind: &'static str, index: usize, label: &str) -> egui::Id {
    egui::Id::new(("browser-omnibox-suggestion", kind, index, label))
}

fn install_suggestion_chip_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    kind: &'static str,
    index: usize,
    label: &str,
    value: &str,
    total: usize,
    selected: bool,
) {
    let _ = ctx.accesskit_node_builder(suggestion_chip_accesskit_id(kind, index, label), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(label);
        node.set_value(format!("Suggestion {} of {total}: {value}", index + 1));
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
        if selected {
            node.set_selected(true);
        }
    });
}

pub(super) fn suggestions_panel(ui: &mut egui::Ui, state: &WebState) -> Option<String> {
    let history = &state.suggestions.history;
    let bookmarks = &state.suggestions.bookmarks;
    let files = &state.suggestions.files;
    let search_items = dedup_search_items(&state.suggestions.items, history);
    if bookmarks.is_empty()
        && files.is_empty()
        && history.is_empty()
        && search_items.is_empty()
        && state.suggestions.notice.is_none()
    {
        return None;
    }
    let mut accepted = None;
    // Flat render index tracking the keyboard highlight ([`SuggestionState::selected`])
    // across the bookmark → history → search sections, so a highlighted row gets an
    // accent fill and Up/Down move visibly.
    let selected = state.suggestions.selected;
    let total = bookmarks.len() + files.len() + history.len() + search_items.len();
    let mut idx = 0usize;
    let fill_for = |idx: usize| {
        if Some(idx) == selected {
            row_fill(true)
        } else {
            row_fill(false)
        }
    };
    ui.horizontal_wrapped(|ui| {
        ui.add_space(SUGGESTIONS_LEADING_INSET);
        if !bookmarks.is_empty() {
            browser_muted_note(ui, "Bookmarks");
            for bm in bookmarks {
                let label = ellipsize(&bm.title, 32);
                let access_label = format!("Open bookmark {}", bm.title);
                let access_value = format!("Bookmark, {}", bm.url);
                let clicked = chrome_hover_text(
                    suggestion_chip(
                        ui,
                        &label,
                        ChromeIcon::Bookmark,
                        CHROME_PRIMARY,
                        fill_for(idx),
                        &access_label,
                        &access_value,
                        "bookmark",
                        idx,
                        total,
                        Some(idx) == selected,
                    ),
                    format!("Bookmark: {}", bm.url),
                )
                .clicked();
                if clicked {
                    accepted = Some(bm.url.clone());
                }
                idx += 1;
            }
        }
        if !files.is_empty() {
            browser_muted_note(ui, "Files");
            for file in files {
                let label = ellipsize(&file.title, 32);
                let access_label = format!("Open file {}", file.title);
                let access_value = format!("File, {}", file.path.display());
                let clicked = chrome_hover_text(
                    suggestion_chip(
                        ui,
                        &label,
                        ChromeIcon::Page,
                        CHROME_TEXT,
                        fill_for(idx),
                        &access_label,
                        &access_value,
                        "file",
                        idx,
                        total,
                        Some(idx) == selected,
                    ),
                    format!("Open file: {}", file.path.display()),
                )
                .clicked();
                if clicked {
                    accepted = Some(file.url.clone());
                }
                idx += 1;
            }
        }
        if !history.is_empty() {
            browser_muted_note(ui, "History");
            for url in history {
                let label = ellipsize(url, 36);
                let access_label = format!("Open history entry {url}");
                let access_value = format!("History, {url}");
                let clicked = chrome_hover_text(
                    suggestion_chip(
                        ui,
                        &label,
                        ChromeIcon::History,
                        CHROME_TEXT,
                        fill_for(idx),
                        &access_label,
                        &access_value,
                        "history",
                        idx,
                        total,
                        Some(idx) == selected,
                    ),
                    format!("Visited: {url}"),
                )
                .clicked();
                if clicked {
                    accepted = Some(url.clone());
                }
                idx += 1;
            }
        }
        for suggestion in search_items {
            let label = ellipsize(suggestion, 36);
            let access_label = format!("Search for {suggestion}");
            let access_value = format!("Search, {suggestion}");
            let clicked = chrome_hover_text(
                suggestion_chip(
                    ui,
                    &label,
                    ChromeIcon::Search,
                    CHROME_TEXT,
                    fill_for(idx),
                    &access_label,
                    &access_value,
                    "search",
                    idx,
                    total,
                    Some(idx) == selected,
                ),
                format!("Search for {suggestion}"),
            )
            .clicked();
            if clicked {
                accepted = Some(suggestion.clone());
            }
            idx += 1;
        }
        if state.suggestions.items.is_empty() && history.is_empty() {
            if let Some(notice) = state.suggestions.notice.as_deref() {
                browser_muted_note(ui, notice);
            }
        }
    });
    accepted
}

/// How many of `total` fixed-width bookmark buttons fit on the single bar row of
/// `available` width. When every button fits, the return is `total` (no overflow
/// slot). Otherwise one `overflow_w`-wide ">>" slot is reserved and the return is
/// how many buttons precede it (possibly zero on a very narrow bar).
pub(super) fn bookmark_bar_visible_count(
    total: usize,
    available: f32,
    btn_w: f32,
    gap: f32,
    overflow_w: f32,
) -> usize {
    if total == 0 {
        return 0;
    }
    // Width if every button sits on the row: n buttons with (n-1) inter-button gaps.
    let all = total as f32 * btn_w + (total.saturating_sub(1)) as f32 * gap;
    if all <= available {
        return total;
    }
    // They don't all fit: reserve the overflow button (its own leading gap) and
    // pack as many leading buttons as the remaining width allows.
    let budget = available - overflow_w - gap;
    let mut count = 0usize;
    let mut used = 0.0f32;
    while count < total {
        let next = if count == 0 {
            btn_w
        } else {
            used + gap + btn_w
        };
        if next > budget {
            break;
        }
        used = next;
        count += 1;
    }
    // At least one bookmark always lands in the overflow menu here, so cap the
    // visible run at `total - 1` even if rounding would otherwise show them all.
    count.min(total.saturating_sub(1))
}

fn bookmark_bar_button(
    ui: &mut egui::Ui,
    label: &str,
    accessibility_label: &str,
    accessibility_value: &str,
    width: f32,
    tip: &str,
) -> egui::Response {
    let target_size = egui::vec2(width.max(44.0), CHROME_BUTTON);
    let (rect, response) = ui.allocate_exact_size(target_size, egui::Sense::click());
    install_bookmark_button_accessibility(ui.ctx(), rect, accessibility_label, accessibility_value);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            accessibility_label,
        )
    });
    let fill = animated_response_fill(ui, &response, CHROME_SURFACE, CHROME_TEXT, true);
    ui.painter().rect(
        rect,
        6.0,
        fill,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::vec2(14.0, 14.0),
    );
    paint_chrome_icon(
        ui.painter(),
        icon_rect,
        ChromeIcon::Bookmark,
        CHROME_TEXT_DIM,
    );
    let text_pos = egui::pos2(icon_rect.right() + 7.0, rect.center().y);
    let text_clip = bookmark_button_text_clip_rect(rect, icon_rect);
    ui.painter().with_clip_rect(text_clip).text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        label,
        font_id(CHROME_FONT),
        CHROME_TEXT,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn bookmark_button_text_clip_rect(rect: egui::Rect, icon_rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(icon_rect.right() + 7.0, rect.top()),
        egui::pos2(rect.right() - 8.0, rect.bottom()),
    )
    .intersect(rect)
}

fn bookmark_button_accesskit_id(label: &str, value: &str) -> egui::Id {
    egui::Id::new(("browser-bookmark-button", label, value))
}

fn install_bookmark_button_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    label: &str,
    value: &str,
) {
    let _ = ctx.accesskit_node_builder(bookmark_button_accesskit_id(label, value), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(format!("Open bookmark {label}"));
        node.set_value(value);
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
    });
}

fn bookmark_overflow_rows(
    ui: &mut egui::Ui,
    links: &[super::BookmarkBarLink],
) -> Option<(String, bool)> {
    let mut chosen = None;
    for link in links {
        let label = ellipsize(&link.title, 40);
        let width = bounded_available_width(ui).max(1.0);
        let resp =
            bookmark_bar_button(ui, &label, &link.title, &link.url, width, link.url.as_str());
        if resp.clicked() {
            chosen = Some((link.url.clone(), false));
            ui.close_menu();
        } else if resp.middle_clicked() {
            chosen = Some((link.url.clone(), true));
            ui.close_menu();
        }
    }
    chosen
}

fn bookmark_overflow_popup_id() -> egui::Id {
    egui::Id::new("mde_web_bookmark_overflow_popup")
}

/// Single-row bookmarks bar below the nav chrome.
pub(super) fn bookmarks_bar(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.bookmarks_bar_visible {
        return;
    }
    let links = state.bookmark_bar_links.clone();
    let mut chosen: Option<(String, bool)> = None;
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            if links.is_empty() {
                browser_muted_note(
                    ui,
                    "No bookmarks yet: add one from Bookmarks > Add Bookmark",
                );
                return;
            }
            ui.horizontal(|ui| {
                let visible = bookmark_bar_visible_count(
                    links.len(),
                    ui.available_width(),
                    BOOKMARK_BTN_W,
                    CHROME_GAP,
                    BOOKMARK_OVERFLOW_W,
                );
                for link in &links[..visible] {
                    let label = ellipsize(&link.title, BOOKMARK_TITLE_CHARS);
                    let resp = bookmark_bar_button(
                        ui,
                        &label,
                        &link.title,
                        &link.url,
                        BOOKMARK_BTN_W,
                        &format!("{}\n{}", link.title, link.url),
                    );
                    if resp.clicked() {
                        chosen = Some((link.url.clone(), false));
                    } else if resp.middle_clicked() {
                        chosen = Some((link.url.clone(), true));
                    }
                }
                if visible < links.len() {
                    let popup_id = bookmark_overflow_popup_id();
                    let response = toolbar_icon_menu_anchor(
                        ui,
                        popup_id,
                        ChromeIcon::Down,
                        CHROME_ICON,
                        "More bookmarks",
                    );
                    if response.clicked() || menu_anchor_keyboard_toggle(ui, &response) {
                        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
                    }
                    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
                    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
                    egui::popup_below_widget(
                        ui,
                        popup_id,
                        &response,
                        egui::PopupCloseBehavior::CloseOnClickOutside,
                        |ui| {
                            reserve_toolbar_popup_width(ui, BOOKMARK_OVERFLOW_MENU_W);
                            if motion.active {
                                ui.ctx().request_repaint();
                            }
                            ui.multiply_opacity(motion.opacity.max(0.2));
                            if motion.anchor_offset > 0.0 {
                                ui.add_space(motion.anchor_offset);
                            }
                            chrome_popup_frame(ui, BOOKMARK_OVERFLOW_MENU_W, |ui| {
                                if let Some(choice) = bookmark_overflow_rows(ui, &links[visible..])
                                {
                                    chosen = Some(choice);
                                    ui.memory_mut(|mem| mem.close_popup());
                                }
                            });
                        },
                    );
                }
            });
        });
    if let Some((url, new_tab)) = chosen {
        state.open_bookmark(url, new_tab);
    }
}

fn find_text_field(ui: &mut egui::Ui, enabled: bool, text: &mut String) -> egui::Response {
    chrome_text_field(
        ui,
        enabled,
        text,
        "Find in page",
        220.0,
        160.0,
        false,
        "Find in page",
        None,
    )
}

pub(super) fn find_chrome(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.find_open {
        return;
    }
    let enabled = state.can_drive_page_tools();
    let find_tally = (!state.find_query.trim().is_empty())
        .then(|| state.active_find_result())
        .flatten();
    let mut submit_forward = false;
    let mut submit_backward = false;
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Find")
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT_DIM),
                );
                let resp = find_text_field(ui, enabled, &mut state.find_query);
                state.chrome_edit_focus |= resp.has_focus();
                if let Some((active, count)) = find_tally {
                    let label = if count == 0 {
                        "No results".to_owned()
                    } else {
                        format!("{active}/{count}")
                    };
                    ui.label(
                        egui::RichText::new(label)
                            .size(CHROME_FONT)
                            .color(CHROME_TEXT_DIM),
                    );
                }
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && ui.input(|i| i.modifiers.shift) {
                    submit_backward = true;
                } else if enter {
                    submit_forward = true;
                }
                if nav_button(ui, ChromeIcon::Up, "Previous match", enabled) {
                    submit_backward = true;
                }
                if nav_button(ui, ChromeIcon::Down, "Next match", enabled) {
                    submit_forward = true;
                }
                if nav_button(ui, ChromeIcon::Close, "Close find", true) {
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

/// The toolbar password menu: fill a saved login into the current site's form, or
/// save a new credential for it. Session-only store, user-initiated fill.
#[derive(Default)]
struct PasswordMenuOutcome {
    fill: Option<(String, String)>,
    remove: Option<usize>,
    save: bool,
}

fn password_menu_matches(state: &WebState, host: &str) -> Vec<(usize, String, String)> {
    if host.is_empty() {
        return Vec::new();
    }
    state
        .session_logins
        .iter()
        .enumerate()
        .filter(|(_, login)| login.host == host)
        .map(|(idx, login)| (idx, login.username.clone(), login.password.clone()))
        .collect()
}

fn password_menu_contents(
    ui: &mut egui::Ui,
    state: &mut WebState,
    host: &str,
    matches: &[(usize, String, String)],
    has_page: bool,
    can_fill: bool,
) -> PasswordMenuOutcome {
    let width = chrome_popup_width(ui, PASSWORD_MENU_W);
    ui.set_min_width(width);
    ui.set_width(width);
    if host.is_empty() {
        browser_muted_note(ui, "No site loaded");
        return PasswordMenuOutcome::default();
    }

    let mut outcome = PasswordMenuOutcome::default();
    ui.label(
        RichText::new("Saved logins (this session)")
            .size(CHROME_FONT)
            .strong()
            .color(CHROME_TEXT),
    );
    if matches.is_empty() {
        browser_muted_note(ui, &format!("None saved for {}", ellipsize(host, 42)));
    } else {
        for (idx, username, password) in matches {
            ui.horizontal(|ui| {
                let label = format!("Fill {}", ellipsize(username, 30));
                let fill_width =
                    (bounded_available_width(ui) - CHROME_BUTTON - CHROME_GAP).max(72.0);
                if ui
                    .add_enabled(
                        has_page && can_fill,
                        action_button(label, BrowserActionRole::Primary)
                            .min_size(egui::vec2(fill_width, CHROME_BUTTON)),
                    )
                    .clicked()
                {
                    outcome.fill = Some((username.clone(), password.clone()));
                    ui.close_menu();
                }
                if action_icon_button(
                    ui,
                    ChromeIcon::Close,
                    BrowserActionRole::Quiet,
                    "Delete saved login",
                    egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
                )
                .clicked()
                {
                    outcome.remove = Some(*idx);
                    ui.close_menu();
                }
            });
        }
    }
    chrome_separator(ui);
    ui.label(
        RichText::new(format!("Save a login for {}", ellipsize(host, 42)))
            .size(CHROME_FONT)
            .color(CHROME_TEXT),
    );
    let field_width = bounded_available_width(ui).max(1.0);
    let field_min_width = PASSWORD_FIELD_MIN_W.min(field_width);
    let user_resp = chrome_text_field(
        ui,
        true,
        &mut state.login_user_draft,
        "username",
        field_width,
        field_min_width,
        false,
        "Username",
        None,
    );
    state.chrome_edit_focus |= user_resp.has_focus();
    let pass_resp = chrome_text_field(
        ui,
        true,
        &mut state.login_pass_draft,
        "password",
        field_width,
        field_min_width,
        true,
        "Password",
        None,
    );
    state.chrome_edit_focus |= pass_resp.has_focus();
    if ui
        .add(action_button("Save", BrowserActionRole::Primary))
        .clicked()
    {
        outcome.save = true;
        ui.close_menu();
    }
    outcome
}

fn password_popup_id() -> egui::Id {
    egui::Id::new("mde_web_password_menu_popup")
}

fn password_menu(
    ui: &mut egui::Ui,
    state: &mut WebState,
    page_url: &str,
    has_page: bool,
    can_fill: bool,
) {
    let host = host_of(page_url).unwrap_or_default();
    let mut fill: Option<(String, String)> = None;
    let mut remove: Option<usize> = None;
    let mut save = false;
    let matches = password_menu_matches(state, &host);
    let popup_id = password_popup_id();
    let response = toolbar_icon_menu_anchor(
        ui,
        popup_id,
        ChromeIcon::Lock,
        if has_page {
            CHROME_ICON
        } else {
            CHROME_TEXT_DIM
        },
        "Passwords and autofill",
    );
    if response.clicked() || menu_anchor_keyboard_toggle(ui, &response) {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    let popup_open = ui.memory(|mem| mem.is_popup_open(popup_id));
    let motion = popover_motion(ui.ctx(), popup_id, popup_open);
    egui::popup_below_widget(
        ui,
        popup_id,
        &response,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            reserve_toolbar_popup_width(ui, PASSWORD_MENU_W);
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            chrome_popup_frame(ui, PASSWORD_MENU_W, |ui| {
                let outcome =
                    password_menu_contents(ui, state, &host, &matches, has_page, can_fill);
                let close_popup =
                    outcome.fill.is_some() || outcome.remove.is_some() || outcome.save;
                fill = outcome.fill;
                remove = outcome.remove;
                save = outcome.save;
                if close_popup {
                    ui.memory_mut(|mem| mem.close_popup());
                }
            });
        },
    );
    if let Some((user, pass)) = fill {
        state.fill_active_login(host.clone(), user, pass);
    }
    if let Some(idx) = remove {
        state.remove_login(idx);
    }
    if save {
        let user = std::mem::take(&mut state.login_user_draft);
        let pass = std::mem::take(&mut state.login_pass_draft);
        state.save_login(&host, &user, &pass);
    }
}

pub(super) fn insecure_prompt(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(url) = state.insecure_prompt.clone() else {
        return;
    };
    scope(ui, |ui| insecure_prompt_contents(ui, state, &url));
}

fn insecure_prompt_contents(ui: &mut egui::Ui, state: &mut WebState, url: &str) {
    ui.horizontal_wrapped(|ui| {
        browser_status_note(ui, ChromeIcon::Warning, "HTTP connection", CHROME_WARN);
        ui.label(RichText::new(ellipsize(url, 64)).color(CHROME_TEXT_DIM));
        if chrome_hover_text(
            ui.add(action_button("Use HTTPS", BrowserActionRole::Primary)),
            "Upgrade this navigation to HTTPS",
        )
        .clicked()
        {
            state.upgrade_insecure_load();
        }
        if chrome_hover_text(
            ui.add(action_button("Continue HTTP", BrowserActionRole::Warning)),
            "Continue with the insecure HTTP URL",
        )
        .clicked()
        {
            state.continue_insecure_load();
        }
        if ui
            .add(action_button("Cancel", BrowserActionRole::Quiet))
            .clicked()
        {
            state.cancel_insecure_load();
        }
    });
}

pub(super) fn capture_notice(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(notice) = state.capture_notice.clone() else {
        return;
    };
    scope(ui, |ui| capture_notice_contents(ui, state, &notice));
}

fn capture_notice_contents(ui: &mut egui::Ui, state: &mut WebState, notice: &str) {
    let failed = notice.starts_with("Capture failed:")
        || notice.starts_with("PDF failed")
        || notice.starts_with("PDF viewer failed:")
        || notice.starts_with("Print failed:");
    let (icon, tone) = if failed {
        (ChromeIcon::Warning, CHROME_ERROR)
    } else {
        (ChromeIcon::Capture, CHROME_PRIMARY)
    };
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                browser_status_note(ui, icon, &notice, tone);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if action_icon_button(
                        ui,
                        ChromeIcon::Close,
                        BrowserActionRole::Quiet,
                        "Dismiss capture notice",
                        egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
                    )
                    .clicked()
                    {
                        state.capture_notice = None;
                    }
                });
            });
        });
}

pub(super) fn crashed_body(ui: &mut egui::Ui, reason: String, respawn_requested: &mut bool) {
    centered(ui, |ui| {
        body_icon_heading(ui, ChromeIcon::Warning, "This page crashed", CHROME_ERROR);
        ui.add_space(Style::SP_S);
        if !reason.is_empty() {
            browser_body_note(ui, reason);
        }
        ui.add_space(Style::SP_M);
        if ui
            .add(action_button("Reload", BrowserActionRole::Primary))
            .clicked()
        {
            *respawn_requested = true;
        }
    });
}

fn body_icon_heading(ui: &mut egui::Ui, icon: ChromeIcon, text: &str, color: Color32) {
    ui.horizontal(|ui| {
        let icon_size = Style::HEADING.max(24.0);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(icon_size, icon_size), egui::Sense::hover());
        paint_chrome_icon(ui.painter(), rect, icon, color);
        ui.add_space(Style::SP_XS);
        ui.label(RichText::new(text).size(Style::HEADING).color(color));
    });
}

pub(super) fn safe_browsing_interstitial_body(ui: &mut egui::Ui, url: &str) -> bool {
    let host = host_of(url).unwrap_or_else(|| url.trim().to_owned());
    let mut back = false;
    centered(ui, |ui| {
        body_icon_heading(ui, ChromeIcon::Warning, "Unsafe site blocked", CHROME_ERROR);
        ui.add_space(Style::SP_M);
        ui.label(
            RichText::new(format!(
                "{host} is on the mesh safe-browsing blocklist. This page was not loaded."
            ))
            .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(action_button("Back to safety", BrowserActionRole::Primary))
            .clicked()
        {
            back = true;
        }
    });
    back
}

pub(super) fn managed_policy_interstitial_body(
    ui: &mut egui::Ui,
    block: &ManagedPolicyBlock,
) -> bool {
    let host = host_of(&block.url).unwrap_or_else(|| block.url.trim().to_owned());
    let mut back = false;
    centered(ui, |ui| {
        body_icon_heading(ui, ChromeIcon::Warning, "Blocked by policy", CHROME_ERROR);
        ui.add_space(Style::SP_M);
        ui.label(
            RichText::new(format!(
                "{host} is blocked by managed Browser policy. Rule: {}",
                block.rule
            ))
            .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(action_button("Back to safety", BrowserActionRole::Primary))
            .clicked()
        {
            back = true;
        }
    });
    back
}

fn js_dialog_action_label(kind: u8) -> (&'static str, &'static str) {
    match kind {
        0 => ("alert", "accepted"),
        1 => ("confirm", "cancelled"),
        2 => ("prompt", "cancelled"),
        _ => ("dialog", "dismissed"),
    }
}

pub(super) fn js_dialog_notice(dialog: &JsDialog) -> String {
    let (kind, action) = js_dialog_action_label(dialog.kind);
    let origin = origin_label(&dialog.origin);
    let message = dialog.message.trim();
    let message = if message.is_empty() {
        "(empty message)".to_owned()
    } else {
        ellipsize(message, 96)
    };
    format!("Page {kind} from {origin} was {action}: {message}")
}

pub(super) fn origin_label(origin: &str) -> String {
    host_of(origin).unwrap_or_else(|| {
        let origin = origin.trim();
        if origin.is_empty() {
            "unknown origin".to_owned()
        } else {
            origin.to_owned()
        }
    })
}

pub(super) fn before_unload_prompt_text(prompt: &BeforeUnloadDialog) -> String {
    let origin = origin_label(&prompt.origin);
    let action = if prompt.is_reload { "reload" } else { "leave" };
    let message = prompt.message.trim();
    let message = if message.is_empty() {
        "(empty message)".to_owned()
    } else {
        ellipsize(message, 96)
    };
    format!("{origin} wants to {action} this page: {message}")
}

pub(super) fn before_unload_primary_label(prompt: &BeforeUnloadDialog) -> &'static str {
    if prompt.is_reload {
        "Reload"
    } else {
        "Leave"
    }
}

pub(super) fn passkey_consent_prompt_text(
    pending: &PendingPasskeyConsent,
    active_tab_id: Option<u64>,
) -> String {
    let origin = pending.display_origin();
    let background = if active_tab_id == Some(pending.tab_id) {
        ""
    } else {
        "Background tab: "
    };
    let account = pending
        .user_name
        .as_deref()
        .map(|name| format!(" for {}", ellipsize(name, 48)))
        .unwrap_or_default();
    format!(
        "{background}{origin} wants to {}{} on {} via {}",
        pending.verb(),
        account,
        pending.rp_id,
        engine_display_name(pending.engine)
    )
}

fn dialog_prompt_frame(ui: &mut egui::Ui, id: impl Hash, contents: impl FnOnce(&mut egui::Ui)) {
    let motion = dialog_prompt_motion(ui.ctx(), id);
    scope(ui, |ui| {
        egui::Frame::NONE
            .fill(prompt_fill())
            .inner_margin(chrome_prompt_margin())
            .show(ui, |ui| {
                if motion.active {
                    ui.ctx().request_repaint();
                }
                ui.multiply_opacity(motion.opacity.max(0.2));
                if motion.y_offset > 0.0 {
                    ui.add_space(motion.y_offset);
                }
                let scale_inset = ((1.0 - motion.scale) * 18.0).clamp(0.0, 1.0);
                if scale_inset > 0.0 {
                    ui.horizontal(|ui| {
                        ui.add_space(scale_inset);
                        ui.vertical(contents);
                    });
                } else {
                    contents(ui);
                }
            });
    });
}

pub(super) fn passkey_consent_prompt_bar(
    ui: &mut egui::Ui,
    pending: &PendingPasskeyConsent,
    active_tab_id: Option<u64>,
) -> Option<bool> {
    let mut decision = None;
    dialog_prompt_frame(
        ui,
        (
            "passkey",
            pending.tab_id,
            pending.client_request_id.as_str(),
        ),
        |ui| {
            ui.horizontal_wrapped(|ui| {
                browser_status_note(
                    ui,
                    ChromeIcon::Lock,
                    &passkey_consent_prompt_text(pending, active_tab_id),
                    CHROME_TEXT,
                );
                if ui
                    .add(action_button("Approve", BrowserActionRole::Primary))
                    .clicked()
                {
                    decision = Some(true);
                }
                if ui
                    .add(action_button("Deny", BrowserActionRole::Secondary))
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        },
    );
    decision
}

pub(super) fn permission_prompt_bar(ui: &mut egui::Ui, origin: &str, kind: u8) -> Option<bool> {
    let mut decision = None;
    dialog_prompt_frame(ui, ("permission", origin, kind), |ui| {
        ui.horizontal_wrapped(|ui| {
            browser_status_note(
                ui,
                ChromeIcon::Security,
                &format!("{origin} wants to {}", permission_kind_label(kind)),
                CHROME_TEXT,
            );
            if ui
                .add(action_button("Allow", BrowserActionRole::Primary))
                .clicked()
            {
                decision = Some(true);
            }
            if ui
                .add(action_button("Block", BrowserActionRole::Secondary))
                .clicked()
            {
                decision = Some(false);
            }
        });
    });
    decision
}

pub(super) fn permission_kind_label(kind: u8) -> &'static str {
    match kind {
        0 => "know your location",
        1 => "show notifications",
        2 => "access the clipboard",
        3 => "use your camera",
        4 => "use your microphone",
        5 => "use your camera and microphone",
        _ => "use a device capability",
    }
}

pub(super) fn before_unload_prompt_bar(
    ui: &mut egui::Ui,
    prompt: &BeforeUnloadDialog,
) -> Option<bool> {
    let mut decision = None;
    dialog_prompt_frame(ui, ("before-unload", prompt.id), |ui| {
        ui.horizontal_wrapped(|ui| {
            browser_status_note(
                ui,
                ChromeIcon::Warning,
                &before_unload_prompt_text(prompt),
                CHROME_WARN,
            );
            if ui
                .add(action_button(
                    before_unload_primary_label(prompt),
                    BrowserActionRole::Primary,
                ))
                .clicked()
            {
                decision = Some(true);
            }
            if ui
                .add(action_button("Stay", BrowserActionRole::Secondary))
                .clicked()
            {
                decision = Some(false);
            }
        });
    });
    decision
}

pub(super) fn login_save_prompt_bar(ui: &mut egui::Ui, host: &str, username: &str) -> Option<bool> {
    let mut decision = None;
    dialog_prompt_frame(ui, ("login-save", host, username), |ui| {
        ui.horizontal_wrapped(|ui| {
            browser_status_note(
                ui,
                ChromeIcon::Lock,
                &format!("Save login for {host} ({username})?"),
                CHROME_TEXT,
            );
            if ui
                .add(action_button("Save", BrowserActionRole::Primary))
                .clicked()
            {
                decision = Some(true);
            }
            if ui
                .add(action_button("Not now", BrowserActionRole::Secondary))
                .clicked()
            {
                decision = Some(false);
            }
        });
    });
    decision
}

pub(super) fn cert_error_host(err: &CertError) -> String {
    host_of(&err.url).unwrap_or_else(|| err.url.clone())
}

pub(super) fn cert_error_body(ui: &mut egui::Ui, err: &CertError, can_back: bool) -> bool {
    let mut back_to_safety = false;
    centered(ui, |ui| {
        body_icon_heading(
            ui,
            ChromeIcon::Warning,
            "Your connection is not private",
            CHROME_ERROR,
        );
        ui.add_space(Style::SP_S);
        ui.label(
            RichText::new(cert_error_host(err))
                .size(Style::HEADING)
                .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_S);
        browser_body_note(ui, err.message.as_str());
        ui.add_space(Style::SP_XS);
        ui.label(
            RichText::new(format!("Error code {}", err.code))
                .size(Style::SMALL)
                .color(CHROME_TEXT_DIM),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(action_button("Back to safety", BrowserActionRole::Primary))
            .clicked()
        {
            back_to_safety = true;
        }
        if !can_back {
            ui.add_space(Style::SP_XS);
            browser_body_note(ui, "No history to return to - this closes the tab.");
        }
    });
    back_to_safety
}

pub(super) fn browser_body_note(ui: &mut egui::Ui, msg: impl Into<String>) -> egui::Response {
    ui.label(
        RichText::new(msg.into())
            .size(Style::SMALL)
            .color(CHROME_TEXT_DIM),
    )
}

pub(super) fn cached_offline_body(
    ui: &mut egui::Ui,
    result: &BrowserOfflineCacheResult,
    unavailable_reason: Option<&str>,
) {
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Offline copy")
                        .size(Style::HEADING)
                        .color(CHROME_TEXT),
                );
                ui.label(
                    RichText::new("Ready")
                        .size(Style::SMALL)
                        .color(CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if chrome_hover_text(
                        ui.add(action_button("Copy", BrowserActionRole::Secondary)),
                        "Copy cached page text",
                    )
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
                        .color(CHROME_WARN),
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
                        .color(CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!("Text {} chars", result.text.chars().count()))
                        .size(Style::SMALL)
                        .color(CHROME_TEXT_DIM),
                );
                if result.cached_ms.is_some() {
                    ui.label(
                        RichText::new("Saved now")
                            .size(Style::SMALL)
                            .color(CHROME_TEXT_DIM),
                    );
                }
                if let Some(viewport) = &result.viewport {
                    ui.label(
                        RichText::new(format!("Preview {}x{}", viewport.width, viewport.height))
                            .size(Style::SMALL)
                            .color(CHROME_TEXT_DIM),
                    );
                }
            });
            ui.add_space(Style::SP_S);
            egui::ScrollArea::vertical()
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.text.as_str())
                            .size(Style::SMALL)
                            .color(CHROME_TEXT),
                    );
                });
        });
}

pub(super) fn empty_body(ui: &mut egui::Ui, notice: Option<&str>) {
    centered(ui, |ui| {
        ui.label(
            RichText::new("Browser")
                .size(Style::HEADING)
                .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_S);
        browser_body_note(ui, notice.unwrap_or(BROWSER_NO_LIVE_PAGE_NOTICE));
    });
}

/// Map a pointer position from egui panel space into the helper frame's **device
/// pixels**. The decoded frame is painted to fill `image_rect`, whose origin and
/// size may differ from the helper's frame size on HiDPI or resized seats.
pub(super) fn map_pointer_to_frame(
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

/// The device-pixel size the helper's frame should track.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "device extent is scaled, rounded, then clamped into [1, MAX_CHANNEL_DIM]"
)]
pub(super) fn frame_target_device_px(rect: egui::Rect, ppp: f32) -> (u32, u32) {
    let dim = |v: f32| -> u32 {
        if v.is_finite() {
            (v * ppp).round().clamp(1.0, MAX_CHANNEL_DIM as f32) as u32
        } else {
            1
        }
    };
    (dim(rect.width()), dim(rect.height()))
}

fn cursor_icon_for(kind: CursorKind) -> egui::CursorIcon {
    match kind {
        CursorKind::Default => egui::CursorIcon::Default,
        CursorKind::Pointer => egui::CursorIcon::PointingHand,
        CursorKind::Text => egui::CursorIcon::Text,
        CursorKind::Crosshair => egui::CursorIcon::Crosshair,
        CursorKind::Wait => egui::CursorIcon::Wait,
        CursorKind::Progress => egui::CursorIcon::Progress,
        CursorKind::Help => egui::CursorIcon::Help,
        CursorKind::Move => egui::CursorIcon::Move,
        CursorKind::Grab => egui::CursorIcon::Grab,
        CursorKind::Grabbing => egui::CursorIcon::Grabbing,
        CursorKind::NotAllowed => egui::CursorIcon::NotAllowed,
        CursorKind::ResizeHorizontal => egui::CursorIcon::ResizeHorizontal,
        CursorKind::ResizeVertical => egui::CursorIcon::ResizeVertical,
        CursorKind::ResizeNeSw => egui::CursorIcon::ResizeNeSw,
        CursorKind::ResizeNwSe => egui::CursorIcon::ResizeNwSe,
        CursorKind::ZoomIn => egui::CursorIcon::ZoomIn,
        CursorKind::ZoomOut => egui::CursorIcon::ZoomOut,
    }
}

/// Paint the active tab's decoded frame to fill the body and forward this frame's
/// egui input to the session.
pub(super) fn paint_body(ui: &mut egui::Ui, state: &mut WebState, active: usize) {
    let Some((tex_id, frame_size)) = state.tabs.get(active).and_then(|tab| {
        let texture = tab.texture.as_ref()?;
        Some((
            texture.id(),
            tab.last_frame.as_ref().map_or([0, 0], |frame| frame.size),
        ))
    }) else {
        return;
    };
    let rect = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    if !rect.is_positive() {
        return;
    }
    let resp = ui.allocate_rect(rect, egui::Sense::click_and_drag());
    let image_rect = rect;
    ui.painter().rect_filled(rect, 0.0, page_backdrop_fill());
    egui::Image::new(egui::load::SizedTexture::new(tex_id, image_rect.size()))
        .paint_at(ui, image_rect);

    if !state.capture_region_mode && resp.hovered() {
        if let Some(kind) = state.tabs.get(active).map(|tab| tab.session.cursor()) {
            ui.output_mut(|o| o.cursor_icon = cursor_icon_for(kind));
        }
    }

    if !state.capture_region_mode {
        let (can_back, can_forward, url) = state.tabs.get(active).map_or_else(
            || (false, false, String::new()),
            |tab| {
                let nav = tab.session.nav();
                (nav.can_back, nav.can_forward, nav.url.clone())
            },
        );
        if let Some(action) = page_context_menu(&resp, can_back, can_forward, &url) {
            state.apply_page_context_action(active, action);
        }
    }

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
        if page_focused {
            if let egui::Event::Ime(ime) = &event {
                if let Some(tab) = state.tabs.get_mut(active) {
                    match ime {
                        egui::ImeEvent::Preedit(text) => {
                            tab.session.ime_set_composition(text.clone());
                        }
                        egui::ImeEvent::Commit(text) => {
                            tab.session.ime_commit_text(text.clone());
                        }
                        egui::ImeEvent::Disabled => tab.session.ime_finish_composition(),
                        egui::ImeEvent::Enabled => {}
                    }
                    tab.last_activity = Instant::now();
                    tab.idle_suspended = false;
                }
                continue;
            }
        }
        let dragging_page = resp.dragged() || resp.drag_stopped();
        if let Some(event) =
            browser_input_event(&event, image_rect, frame_size, page_focused, dragging_page)
        {
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
    if page_focused {
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                rect: image_rect,
                cursor_rect: egui::Rect::from_min_size(
                    image_rect.left_top(),
                    egui::vec2(1.0, 18.0),
                ),
            });
        });
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
            ui.painter().rect_filled(overlay, 0.0, selection_wash());
            ui.painter().rect_stroke(
                overlay,
                0.0,
                egui::Stroke::new(1.0, CHROME_PRIMARY),
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

/// Convert page-owned egui input into helper-frame coordinates.
pub(super) fn browser_input_event(
    event: &egui::Event,
    rect: egui::Rect,
    frame_size: [usize; 2],
    browser_focused: bool,
    dragging_page: bool,
) -> Option<egui::Event> {
    match event {
        egui::Event::PointerMoved(pos) => {
            if rect.contains(*pos) || dragging_page {
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
            if rect.contains(*pos) || (browser_focused && dragging_page) {
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

#[cfg(test)]
mod tests {
    use super::super::BrowserInternalPage;
    use super::*;

    #[test]
    fn browser_material_state_layer_blends_over_the_local_surface() {
        let hover = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA);
        let pressed = state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_PRESSED_ALPHA);

        assert_ne!(hover, CHROME_TOOLBAR);
        assert_ne!(pressed, CHROME_TOOLBAR);
        assert_ne!(hover, pressed);
        assert_eq!(hover, Color32::from_rgb(237, 237, 237));
        assert_eq!(pressed, Color32::from_rgb(232, 232, 232));
    }

    #[test]
    fn browser_chrome_palette_matches_chrome_refresh_light_roles() {
        assert_eq!(CHROME_TOOLBAR, Color32::from_rgb(255, 255, 255));
        assert_eq!(CHROME_SURFACE, Color32::from_rgb(255, 255, 255));
        assert_eq!(CHROME_SURFACE_CONTAINER, Color32::from_rgb(237, 242, 252));
        assert_eq!(
            CHROME_SURFACE_CONTAINER_HIGH,
            Color32::from_rgb(232, 234, 242)
        );
        assert_eq!(CHROME_PRIMARY, Color32::from_rgb(11, 87, 208));
        assert_eq!(CHROME_PRIMARY_CONTAINER, Color32::from_rgb(211, 227, 253));
        assert_eq!(CHROME_TEXT, Color32::from_rgb(31, 31, 31));
        assert_eq!(CHROME_ICON, Color32::from_rgb(95, 99, 104));
        assert_eq!(CHROME_TEXT_DIM, CHROME_ICON);
        assert_eq!(CHROME_OUTLINE, Color32::from_rgb(218, 220, 224));
    }

    #[test]
    fn browser_chrome_open_widget_state_uses_browser_material_roles() {
        let ctx = egui::Context::default();
        Style::install(&ctx);

        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                scope(ui, |ui| {
                    let open = &ui.visuals().widgets.open;

                    assert_eq!(
                        open.weak_bg_fill,
                        state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA)
                    );
                    assert_eq!(
                        open.bg_fill,
                        state_layer(CHROME_TOOLBAR, CHROME_TEXT, STATE_HOVER_ALPHA)
                    );
                    assert_eq!(open.fg_stroke.color, CHROME_TEXT);
                    assert_eq!(open.bg_stroke.color, CHROME_OUTLINE);
                    assert_ne!(open.bg_fill, Style::BG);
                    assert_ne!(open.weak_bg_fill, Style::SURFACE);
                    assert_ne!(open.fg_stroke.color, Style::TEXT);
                });
            });
        });
    }

    #[test]
    fn browser_tab_depth_uses_chrome_neutral_depth_not_raw_black() {
        let active_shadow = tab_shadow_fill(true);
        let inactive_shadow = tab_shadow_fill(false);
        let badge_depth = tab_badge_depth_fill();

        for color in [active_shadow, inactive_shadow, badge_depth] {
            assert_ne!(color, Color32::BLACK);
            assert!(
                color.r() >= 218 && color.g() >= 220 && color.b() >= 224,
                "Browser tab depth should stay on light Chrome neutrals, got {color:?}"
            );
        }
        assert_ne!(active_shadow, inactive_shadow);
    }

    fn control_state_alpha_frame(
        ctx: &egui::Context,
        target_alpha: u8,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, f32) {
        let mut alpha = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                alpha = Some(animate_control_state_alpha_with_mode(
                    ctx,
                    "browser-control-state-test",
                    target_alpha,
                    mode,
                ));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            alpha.expect("control-state animation returned a value"),
        )
    }

    fn popover_motion_frame(
        ctx: &egui::Context,
        visible: bool,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, BrowserPopoverMotion) {
        let mut motion = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                motion = Some(popover_motion_with_mode(
                    ctx,
                    "browser-popover-motion-test",
                    visible,
                    mode,
                ));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            motion.expect("popover animation returned a value"),
        )
    }

    fn dialog_prompt_motion_frame(
        ctx: &egui::Context,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, BrowserDialogMotion) {
        let mut motion = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                motion = Some(dialog_prompt_motion_with_mode(
                    ctx,
                    "browser-dialog-motion-test",
                    mode,
                ));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            motion.expect("dialog prompt animation returned a value"),
        )
    }

    fn panel_motion_frame(
        ctx: &egui::Context,
        visible: bool,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, BrowserPanelMotion) {
        let mut motion = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                motion = Some(panel_motion_with_mode(
                    ctx,
                    "browser-panel-motion-test",
                    visible,
                    mode,
                ));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            motion.expect("panel animation returned a value"),
        )
    }

    fn page_motion_frame(
        ctx: &egui::Context,
        page_key: u64,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, BrowserPageMotion) {
        let mut motion = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                motion = Some(page_motion_with_mode(
                    ctx,
                    "browser-page-motion-test",
                    page_key,
                    mode,
                ));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            motion.expect("page animation returned a value"),
        )
    }

    fn note_tab_drag_settle_frame(
        ctx: &egui::Context,
        tab_id: u64,
        axis: TabAxis,
        direction: f32,
        time: f64,
    ) -> Duration {
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                note_tab_drag_settle(ctx, tab_id, axis, direction);
            },
        );
        out.viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay
    }

    fn tab_drag_settle_motion_frame(
        ctx: &egui::Context,
        tab_id: u64,
        axis: TabAxis,
        mode: MotionMode,
        time: f64,
    ) -> (Duration, BrowserDragSettleMotion) {
        let mut motion = None;
        let out = ctx.run(
            egui::RawInput {
                time: Some(time),
                ..Default::default()
            },
            |ctx| {
                motion = Some(tab_drag_settle_motion_with_mode(ctx, tab_id, axis, mode));
            },
        );
        let repaint_delay = out
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay;
        (
            repaint_delay,
            motion.expect("tab drag settle animation returned a value"),
        )
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

    fn painted_text_rects(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Rect)> {
        fn walk(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
            match shape {
                egui::Shape::Text(text) => {
                    out.push((
                        text.galley.text().to_owned(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                    ));
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

    fn painted_text_geometry(
        shapes: &[egui::epaint::ClippedShape],
    ) -> Vec<(String, egui::Rect, egui::Rect)> {
        fn walk(
            shape: &egui::Shape,
            clip_rect: egui::Rect,
            out: &mut Vec<(String, egui::Rect, egui::Rect)>,
        ) {
            match shape {
                egui::Shape::Text(text) => {
                    out.push((
                        text.galley.text().to_owned(),
                        egui::Rect::from_min_size(text.pos, text.galley.size()),
                        clip_rect,
                    ));
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, clip_rect, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
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

    fn accesskit_bounds_rect(node: &egui::accesskit::Node) -> egui::Rect {
        let bounds = node.bounds().expect("accesskit node has bounds");
        egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        )
    }

    fn painted_line_strokes(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Stroke> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Stroke>) {
            match shape {
                egui::Shape::LineSegment { stroke, .. } => out.push(*stroke),
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

    fn painted_path_strokes(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Stroke> {
        fn solid_stroke(path: &egui::epaint::PathShape) -> Option<egui::Stroke> {
            let color = match &path.stroke.color {
                egui::epaint::ColorMode::Solid(color) => *color,
                egui::epaint::ColorMode::UV(_) => return None,
            };
            (color != egui::Color32::TRANSPARENT && path.stroke.width > 0.0)
                .then_some(egui::Stroke::new(path.stroke.width, color))
        }

        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Stroke>) {
            match shape {
                egui::Shape::Path(path) => {
                    if let Some(stroke) = solid_stroke(path) {
                        out.push(stroke);
                    }
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

    fn painted_image_mesh_count(shapes: &[egui::epaint::ClippedShape]) -> usize {
        fn walk(shape: &egui::Shape, out: &mut usize) {
            match shape {
                egui::Shape::Mesh(mesh) if !mesh.vertices.is_empty() => *out += 1,
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = 0;
        for clipped in shapes {
            walk(&clipped.shape, &mut out);
        }
        out
    }

    fn has_vector_icon_stroke(shapes: &[egui::epaint::ClippedShape], color: egui::Color32) -> bool {
        painted_line_strokes(shapes)
            .iter()
            .any(|stroke| stroke.color == color && (stroke.width - 1.7).abs() < 0.01)
            || painted_path_strokes(shapes)
                .iter()
                .any(|stroke| stroke.color == color && (stroke.width - 1.7).abs() < 0.01)
    }

    fn has_browser_icon_paint(shapes: &[egui::epaint::ClippedShape], color: egui::Color32) -> bool {
        has_vector_icon_stroke(shapes, color) || painted_image_mesh_count(shapes) > 0
    }

    fn assert_browser_icon_painted(
        shapes: &[egui::epaint::ClippedShape],
        color: egui::Color32,
        surface: &str,
    ) {
        let lines = painted_line_strokes(shapes);
        let paths = painted_path_strokes(shapes);
        let image_meshes = painted_image_mesh_count(shapes);
        assert!(
            has_vector_icon_stroke(shapes, color) || image_meshes > 0,
            "{surface} must paint a Browser icon as vector color {color:?} or a YAMIS image: lines={lines:?} paths={paths:?} image_meshes={image_meshes}"
        );
    }

    fn painted_rect_fills(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Color32> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Color32>) {
            match shape {
                egui::Shape::Rect(rect) => {
                    if rect.fill != egui::Color32::TRANSPARENT {
                        out.push(rect.fill);
                    }
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

    fn painted_rects(
        shapes: &[egui::epaint::ClippedShape],
    ) -> Vec<(egui::Color32, egui::Stroke, egui::Rect)> {
        fn walk(
            shape: &egui::Shape,
            clip_rect: egui::Rect,
            out: &mut Vec<(egui::Color32, egui::Stroke, egui::Rect)>,
        ) {
            match shape {
                egui::Shape::Rect(rect) => {
                    let visible = rect.rect.intersect(clip_rect);
                    if visible.is_positive() {
                        out.push((rect.fill, rect.stroke, visible));
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, clip_rect, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
    }

    fn painted_rect_strokes(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Stroke> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Stroke>) {
            match shape {
                egui::Shape::Rect(rect) => {
                    if rect.stroke.color != egui::Color32::TRANSPARENT && rect.stroke.width > 0.0 {
                        out.push(rect.stroke);
                    }
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

    fn painted_rect_bounds(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Rect> {
        fn walk(shape: &egui::Shape, clip_rect: egui::Rect, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(rect) => {
                    let visible = rect.rect.intersect(clip_rect);
                    if visible.is_positive() {
                        out.push(visible);
                    }
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        walk(shape, clip_rect, out);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for clipped in shapes {
            walk(&clipped.shape, clipped.clip_rect, &mut out);
        }
        out
    }

    fn painted_raw_rect_bounds(shapes: &[egui::epaint::ClippedShape]) -> Vec<egui::Rect> {
        fn walk(shape: &egui::Shape, out: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(rect) => out.push(rect.rect),
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

    fn render_chrome_separator_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(260.0, 80.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_min_width(220.0);
                        chrome_separator(ui);
                        page_context_separator(ui);
                    });
                });
            },
        )
    }

    fn render_options_page_frame(ctx: &egui::Context, size: egui::Vec2) -> egui::FullOutput {
        let mut state = WebState::default();
        state.power_mode = true;
        state.open_options_tab();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                            ui.set_width(size.x);
                            ui.set_max_width(size.x);
                            ui.set_min_height(size.y);
                            ui.set_clip_rect(rect);
                            scope(ui, |ui| {
                                options_page(ui, &mut state);
                            });
                        });
                    });
            },
        )
    }

    fn render_omnibox_chrome_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_omnibox_chrome_frame_with_size(ctx, egui::vec2(860.0, 96.0))
    }

    fn render_omnibox_chrome_frame_with_size(
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        render_omnibox_chrome_frame_with_address(ctx, size, "https://example.test/mesh")
    }

    fn render_omnibox_chrome_frame_with_address(
        ctx: &egui::Context,
        size: egui::Vec2,
        address: &str,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        // These fixtures assert the horizontal nav toolbar's full affordance set
        // (Capture + Downloads sit right of the location bar). Those are
        // de-duplicated onto the rail in the default vertical layout, so render
        // the toolbar explicitly in horizontal mode here.
        state.vertical_tabs = false;
        let (shell, _helper) = std::os::unix::net::UnixStream::pair().expect("omnibox socketpair");
        let session =
            mde_web_preview_client::WebSession::from_stream(shell, None).expect("omnibox session");
        state.push_session(session);
        state.address = address.to_owned();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        nav_chrome(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_inline_completion_frame(
        ctx: &egui::Context,
        rect: egui::Rect,
        draft: &str,
        tail: &str,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 96.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        paint_omnibox_inline_completion(ui, rect, draft, tail);
                    });
                });
            },
        )
    }

    fn render_engine_toolbar_chip_frame(
        ctx: &egui::Context,
        engine: BrowserEngine,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        state.engine = engine;
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(96.0, 64.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = engine_toolbar_chip(ui, &state);
                    });
                });
            },
        )
    }

    fn render_password_menu_contents_frame(
        ctx: &egui::Context,
        host: &str,
        saved_login: bool,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        state.login_user_draft = "operator".to_owned();
        state.login_pass_draft = "secret".to_owned();
        if saved_login {
            state.save_login(host, "alice", "hunter2");
        }
        let matches = password_menu_matches(&state, host);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 220.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = password_menu_contents(ui, &mut state, host, &matches, true, true);
                    });
                });
            },
        )
    }

    fn render_password_menu_button_frame(ctx: &egui::Context, page_url: &str) -> egui::FullOutput {
        let mut state = WebState::default();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(96.0, 56.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        password_menu(ui, &mut state, page_url, !page_url.is_empty(), true);
                    });
                });
            },
        )
    }

    fn render_open_password_menu_popup_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        let host = "very-long-subdomain-for-passwords.example.test";
        state.save_login(
            host,
            "operator-with-a-long-username@example.test",
            "hunter2",
        );
        let page_url = format!("https://{host}/login");
        let mut render = |open: bool, time: f64| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(340.0, 260.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        scope(ui, |ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if open {
                                    ui.memory_mut(|mem| mem.open_popup(password_popup_id()));
                                }
                                password_menu(ui, &mut state, &page_url, true, true);
                            });
                        });
                    });
                },
            )
        };

        let _ = render(true, 0.0);
        let _ = render(false, 0.016);
        render(false, 1.0)
    }

    fn render_capture_notice_frame(ctx: &egui::Context, notice: &str) -> egui::FullOutput {
        let mut state = WebState::default();
        state.capture_notice = Some(notice.to_owned());
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 80.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    capture_notice(ui, &mut state);
                });
            },
        )
    }

    fn render_insecure_prompt_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.insecure_prompt = Some("http://plain.example/sensitive".to_owned());
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(680.0, 80.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    insecure_prompt(ui, &mut state);
                });
            },
        )
    }

    fn render_dialog_prompt_bars_frame(ctx: &egui::Context) -> egui::FullOutput {
        let passkey = PendingPasskeyConsent::from_handoff(
            7,
            BrowserEngine::Cef,
            r#"{"ceremony":"get","origin":"https://login.example","rp_id":"login.example"}"#
                .to_owned(),
            "passkey-render-test".to_owned(),
        )
        .expect("valid passkey handoff body");
        let before_unload = BeforeUnloadDialog {
            id: 11,
            message: "Unsaved work".to_owned(),
            origin: "https://docs.example.com/edit".to_owned(),
            is_reload: false,
        };

        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 220.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    passkey_consent_prompt_bar(ui, &passkey, Some(7));
                    permission_prompt_bar(ui, "https://camera.example", 3);
                    before_unload_prompt_bar(ui, &before_unload);
                    login_save_prompt_bar(ui, "docs.example.com", "mm");
                });
            },
        )
    }

    fn render_inline_close_button_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(64.0, 48.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = inline_close_button(ui);
                    });
                });
            },
        )
    }

    fn render_tab_audio_buttons_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(96.0, 48.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.horizontal(|ui| {
                            let _ = tab_audio_button(ui, true, false);
                            let _ = tab_audio_button(ui, false, true);
                        });
                    });
                });
            },
        )
    }

    fn render_tab_status_chips_frame(ctx: &egui::Context) -> egui::FullOutput {
        let chips = [
            TabStatusChip {
                icon: ChromeIcon::Privacy,
                label: "Work",
                tone: TabStatusChipTone::Accent,
            },
            TabStatusChip {
                icon: ChromeIcon::View,
                label: "Secondary Display",
                tone: TabStatusChipTone::Accent,
            },
            TabStatusChip {
                icon: ChromeIcon::DarkMode,
                label: "Force dark",
                tone: TabStatusChipTone::Accent,
            },
            TabStatusChip {
                icon: ChromeIcon::Edit,
                label: "Site fixups",
                tone: TabStatusChipTone::Accent,
            },
        ];
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(280.0, 56.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = tab_pill_sized(
                            ui,
                            "Example page",
                            true,
                            240.0,
                            BrowserEngine::Cef,
                            None,
                            &chips,
                        );
                    });
                });
            },
        )
    }

    fn render_tab_hover_card_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        let (shell, _helper) =
            std::os::unix::net::UnixStream::pair().expect("tab hover socketpair");
        let session =
            mde_web_preview_client::WebSession::from_stream(shell, None).expect("tab session");
        state.push_session_with_engine(session, BrowserEngine::Cef);
        state.tabs[0].force_dark = true;
        state.tabs[0].user_scripts = true;
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 120.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        tab_hover_card(ui, &state.tabs[0]);
                    });
                });
            },
        )
    }

    fn render_chrome_tooltip_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 96.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    chrome_tooltip(ui, "Search tabs");
                });
            },
        )
    }

    fn render_shell_invoked_browser_surfaces_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.insecure_prompt = Some("http://plain.example/sensitive".to_owned());
        state.capture_notice = Some("Capture saved".to_owned());
        state.history_open = true;
        state
            .history
            .record("https://example.test/", "Example Page", 1_000);

        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(760.0, 420.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        insecure_prompt(ui, &mut state);
                        capture_notice(ui, &mut state);
                        drawer_stack(ui, &mut state);
                        chrome_tooltip(ui, "Search tabs");
                    });
            },
        )
    }

    fn render_bookmarks_bar_overflow_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.bookmarks_bar_visible = true;
        state.bookmark_bar_links = (0..5)
            .map(|idx| super::super::BookmarkBarLink {
                title: format!("Bookmark {idx}"),
                url: format!("https://bookmark-{idx}.example/"),
            })
            .collect();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(190.0, 56.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        bookmarks_bar(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_open_bookmark_overflow_popup_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.bookmarks_bar_visible = true;
        state.bookmark_bar_links = (0..5)
            .map(|idx| super::super::BookmarkBarLink {
                title: format!("Bookmark {idx}"),
                url: format!("https://bookmark-{idx}.example/"),
            })
            .collect();
        let mut render = |open: bool, time: f64| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(320.0, 180.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        scope(ui, |ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if open {
                                    ui.memory_mut(|mem| {
                                        mem.open_popup(bookmark_overflow_popup_id())
                                    });
                                }
                                bookmarks_bar(ui, &mut state);
                            });
                        });
                    });
                },
            )
        };

        let _ = render(true, 0.0);
        let _ = render(false, 0.016);
        render(false, 1.0)
    }

    fn render_bookmark_overflow_rows_frame(ctx: &egui::Context) -> egui::FullOutput {
        let links: Vec<super::super::BookmarkBarLink> = (3..5)
            .map(|idx| super::super::BookmarkBarLink {
                title: format!("Bookmark {idx}"),
                url: format!("https://bookmark-{idx}.example/"),
            })
            .collect();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(300.0, 120.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_min_width(220.0);
                        let _ = bookmark_overflow_rows(ui, &links);
                    });
                });
            },
        )
    }

    fn render_long_bookmark_bar_button_frame(ctx: &egui::Context, title: &str) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(220.0, 72.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_max_width(BOOKMARK_BTN_W);
                        let _ = bookmark_bar_button(
                            ui,
                            title,
                            title,
                            "https://very-long-bookmark-title.example/",
                            BOOKMARK_BTN_W,
                            "https://very-long-bookmark-title.example/",
                        );
                    });
                });
            },
        )
    }

    fn render_page_context_rows_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_page_context_rows_frame_with_size(ctx, egui::vec2(420.0, 320.0))
    }

    fn render_page_context_rows_frame_with_size(
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.visuals_mut().override_text_color = Some(CHROME_TEXT);
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let _ = page_context_row(ui, "Back", ChromeIcon::Back, false);
                    let _ = page_context_row(ui, "Forward", ChromeIcon::Forward, true);
                    let _ = page_context_row(ui, "Reload", ChromeIcon::Reload, true);
                    page_context_separator(ui);
                    let _ =
                        page_context_row(ui, "Cut", page_context_edit_icon(EditCommand::Cut), true);
                    let _ = page_context_row(
                        ui,
                        "Copy",
                        page_context_edit_icon(EditCommand::Copy),
                        true,
                    );
                    let _ = page_context_row(
                        ui,
                        "Paste",
                        page_context_edit_icon(EditCommand::Paste),
                        true,
                    );
                    let _ = page_context_row(
                        ui,
                        "Select all",
                        page_context_edit_icon(EditCommand::SelectAll),
                        true,
                    );
                    page_context_separator(ui);
                    let _ = page_context_row(ui, "Copy page URL", ChromeIcon::Share, true);
                });
            },
        )
    }

    fn render_open_page_context_menu_frame(ctx: &egui::Context) -> egui::FullOutput {
        let input = |time| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(360.0, 240.0),
            )),
            time: Some(time),
            ..Default::default()
        };
        let paint = |ctx: &egui::Context, seed_open: bool| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(180.0, 32.0), egui::Sense::click());
                    ui.painter()
                        .rect_filled(rect, 6.0, CHROME_SURFACE_CONTAINER);
                    if seed_open {
                        let context_menu_id = egui::Id::new("__egui::context_menu");
                        let mut bar_state = egui::menu::BarState::load(ctx, context_menu_id);
                        **bar_state = Some(egui::menu::MenuRoot::new(
                            response.rect.left_bottom(),
                            response.id,
                        ));
                        bar_state.store(ctx, context_menu_id);
                    }
                    let _ = page_context_menu(&response, false, true, "https://example.test/");
                });
        };

        let _ = ctx.run(input(0.0), |ctx| paint(ctx, true));
        ctx.run(input(0.016), |ctx| paint(ctx, false))
    }

    fn render_tab_context_rows_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 320.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_min_width(220.0);
                        ui.spacing_mut().item_spacing.y = 3.0;
                        let _ = tab_context_menu_row(ui, "Move tab left", ChromeIcon::Back, false);
                        let _ =
                            tab_context_menu_row(ui, "Move tab right", ChromeIcon::Forward, true);
                        let _ = tab_context_menu_row(ui, "Pin tab", ChromeIcon::Bookmark, true);
                        let _ = tab_context_menu_row(ui, "Duplicate tab", ChromeIcon::Page, true);
                        chrome_separator(ui);
                        let _ =
                            tab_context_menu_row(ui, "Work container", ChromeIcon::Privacy, false);
                        let _ = tab_context_menu_row(ui, "Display 2", ChromeIcon::View, true);
                        let _ =
                            tab_context_menu_row(ui, "Enable site fixups", ChromeIcon::Edit, true);
                        let _ = tab_context_menu_row(ui, "Close tab", ChromeIcon::Close, true);
                    });
                });
            },
        )
    }

    fn render_page_actions_menu_frame(
        ctx: &egui::Context,
        is_bookmarked: bool,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(360.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_min_width(220.0);
                        page_actions_menu(
                            ui,
                            None,
                            Some(BrowserEngine::Cef),
                            is_bookmarked,
                            "https://example.test/",
                            "Example",
                        );
                    });
                });
            },
        )
    }

    fn render_collapsed_page_actions_menu_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 260.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        ui.set_max_width(0.0);
                        page_actions_menu(
                            ui,
                            None,
                            Some(BrowserEngine::Cef),
                            false,
                            "https://example.test/",
                            "Example",
                        );
                    });
            },
        )
    }

    fn render_page_actions_button_frame(ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(160.0, 56.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.horizontal(|ui| {
                            page_actions_button(ui, false, false, None, None, "", "");
                            page_actions_button(
                                ui,
                                true,
                                false,
                                None,
                                Some(BrowserEngine::Cef),
                                "https://example.test/",
                                "Example",
                            );
                            page_actions_button(
                                ui,
                                true,
                                true,
                                None,
                                Some(BrowserEngine::Cef),
                                "https://example.test/",
                                "Example",
                            );
                        });
                    });
                });
            },
        )
    }

    fn render_open_page_actions_popup_frame(ctx: &egui::Context) -> egui::FullOutput {
        let render = |ctx: &egui::Context, open: bool, time: f64| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(320.0, 260.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        scope(ui, |ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if open {
                                    ui.memory_mut(|mem| mem.open_popup(page_actions_popup_id()));
                                }
                                page_actions_button(
                                    ui,
                                    true,
                                    false,
                                    None,
                                    Some(BrowserEngine::Cef),
                                    "https://example.test/",
                                    "Example",
                                );
                            });
                        });
                    });
                },
            )
        };

        let _ = render(ctx, true, 0.0);
        let _ = render(ctx, false, 0.016);
        render(ctx, false, 1.0)
    }

    fn render_open_security_popup_frame(ctx: &egui::Context) -> egui::FullOutput {
        let render = |ctx: &egui::Context, open: bool, time: f64| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(340.0, 260.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        scope(ui, |ui| {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                if open {
                                    ui.memory_mut(|mem| mem.open_popup(security_chip_popup_id()));
                                }
                                omnibox_security_button(
                                    ui,
                                    "https://example.test/",
                                    Vec::new,
                                    None,
                                );
                            });
                        });
                    });
                },
            )
        };

        let _ = render(ctx, true, 0.0);
        let _ = render(ctx, false, 0.016);
        render(ctx, false, 1.0)
    }

    fn render_body_frame(
        ctx: &egui::Context,
        mut render: impl FnMut(&mut egui::Ui),
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    render(ui);
                });
            },
        )
    }

    fn render_security_chrome_frame(ctx: &egui::Context) -> egui::FullOutput {
        let recent_resources = vec![
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
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(680.0, 560.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        site_info_panel(ui, "https://xn--pple-43d.com/", &recent_resources, None);
                        chrome_separator(ui);
                        for url in [
                            "https://example.test/",
                            "http://example.test/",
                            "mesh://files.mesh/",
                            "about:blank",
                        ] {
                            ui.horizontal(|ui| {
                                security_chip(ui, url, &[], None);
                                site_info_panel(ui, url, &[], None);
                            });
                            chrome_separator(ui);
                        }
                    });
                });
            },
        )
    }

    fn render_ad_filter_chrome_frame(ctx: &egui::Context) -> egui::FullOutput {
        let blocked = vec![("ads.example".to_owned(), 3)];
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(260.0, 120.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ad_filter_chip(ui, 7, &blocked);
                        ad_filter_domain_row(ui, "ads.example", 3);
                    });
                });
            },
        )
    }

    fn render_ad_filter_hover_card_frame(ctx: &egui::Context) -> egui::FullOutput {
        let blocked = vec![("ads.example".to_owned(), 3)];
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(320.0, 120.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        ad_filter_hover_card(ui, 7, &blocked);
                    });
            },
        )
    }

    fn suggestions_panel_test_state() -> WebState {
        let mut state = WebState::default();
        state.suggestions.bookmarks = vec![super::super::BookmarkBarLink {
            title: "Example bookmark".to_owned(),
            url: "https://example.test/bookmark".to_owned(),
        }];
        state.suggestions.files = vec![super::super::BrowserFileSuggestion {
            title: "home-notes.md".to_owned(),
            path: std::path::PathBuf::from("/home/mm/home-notes.md"),
            url: "file:///home/mm/home-notes.md".to_owned(),
        }];
        state.suggestions.history = vec!["https://example.test/history".to_owned()];
        state.suggestions.items = vec![
            "example search".to_owned(),
            "https://example.test/history".to_owned(),
        ];
        state.suggestions.selected = Some(0);
        state
    }

    fn render_suggestions_panel_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_suggestions_panel_frame_with_input(ctx, Vec::new(), 0.0)
    }

    fn render_suggestions_panel_frame_with_input(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        let state = suggestions_panel_test_state();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 140.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = suggestions_panel(ui, &state);
                    });
                });
            },
        )
    }

    fn render_history_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.history_open = true;
        state
            .history
            .record("https://example.test/", "Example Page", 1_000);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::history_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_qr_share_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_qr_share_drawer_frame_with_size(ctx, egui::vec2(640.0, 360.0))
    }

    fn render_qr_share_drawer_frame_with_size(
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        state.latest_qr_share = Some(super::super::BrowserQrShareResult {
            host: "phone".to_owned(),
            url: "https://example.test/qr".to_owned(),
            title: "Example QR".to_owned(),
            preview: "https://example.test/qr".to_owned(),
            request_id: "01HQR-share-material".to_owned(),
            modules: vec![
                vec![true, true, true, false, true],
                vec![true, false, true, false, false],
                vec![true, true, true, true, false],
                vec![false, false, true, false, true],
                vec![true, false, false, true, true],
            ],
        });
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::qr_share_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_translation_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.latest_translation = Some(super::super::BrowserTranslationResult {
            host: "translation-worker".to_owned(),
            tab_index: 3,
            engine: BrowserEngine::Cef,
            url: "https://example.test/translate".to_owned(),
            title: "Example Translation".to_owned(),
            source_lang: "en".to_owned(),
            target_lang: "fr".to_owned(),
            translation: "Bonjour monde".to_owned(),
        });
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 260.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::translation_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn offline_cache_result_fixture() -> BrowserOfflineCacheResult {
        BrowserOfflineCacheResult {
            host: "browser-helper".to_owned(),
            cache_id: "01HARchivecopy-test".to_owned(),
            tab_index: 0,
            engine: BrowserEngine::Cef,
            url: "https://example.test/archive".to_owned(),
            title: "Example archive".to_owned(),
            text: "Saved page text".to_owned(),
            viewport: Some(super::super::OfflineCacheViewportImage {
                mime: "image/png".to_owned(),
                width: 320,
                height: 180,
                data_base64: "bm90LXBuZw==".to_owned(),
            }),
            resources: vec![super::super::OfflineCacheResource {
                url: "https://cdn.example.test/tracker.js".to_owned(),
                resource: "script".to_owned(),
                allowed: false,
                blocked_by: Some("Tracking protection".to_owned()),
            }],
            archive_mhtml: Some(super::super::OfflineCacheArchive {
                mime: "multipart/related".to_owned(),
                filename: "mde-browser-example.mhtml".to_owned(),
                bytes: 4096,
                data_base64: "bWVzaA==".to_owned(),
            }),
            pdf_snapshot: None,
            cached_ms: Some(123),
        }
    }

    fn render_offline_cache_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.latest_offline_cache = Some(offline_cache_result_fixture());
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(760.0, 240.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::offline_cache_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_empty_downloads_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.downloads_open = true;
        state.download_notice = Some("Open failed: file vanished".to_owned());
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::downloads_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_progress_downloads_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_progress_downloads_drawer_frame_with_size(ctx, egui::vec2(760.0, 360.0))
    }

    fn render_progress_downloads_drawer_frame_with_size(
        ctx: &egui::Context,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        state.downloads_open = true;
        let mut job = mde_files_egui::transfers::TransferJob::new(
            "/tmp/movie.webm",
            "/home/mm/Downloads",
            mde_files_egui::transfers::Method::BrowserDownload,
            mde_files_egui::transfers::TransferPolicy::default(),
        );
        job.id = "browser-running".to_owned();
        job.state = mde_files_egui::transfers::TransferState::Running;
        job.progress = Some(42);
        state.download_jobs = vec![job];
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::downloads_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_failed_downloads_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.downloads_open = true;
        let mut job = mde_files_egui::transfers::TransferJob::new(
            "/tmp/setup.exe",
            "/home/mm/Downloads",
            mde_files_egui::transfers::Method::BrowserDownload,
            mde_files_egui::transfers::TransferPolicy::default(),
        );
        job.id = "browser-failed".to_owned();
        job.state = mde_files_egui::transfers::TransferState::Failed;
        job.error = Some("checksum mismatch".to_owned());
        state.download_jobs = vec![job];
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(760.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::downloads_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_dangerous_downloads_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.downloads_open = true;
        state.pending_dangerous_download = Some(super::super::PendingDangerousDownload {
            id: 42,
            url: "https://downloads.example.test/setup.exe".to_owned(),
            filename: "setup.exe".to_owned(),
        });
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::downloads_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_print_settings_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        render_print_settings_drawer_frame_with_input(ctx, Vec::new(), 0.0)
    }

    fn render_print_settings_drawer_frame_with_input(
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        time: f64,
    ) -> egui::FullOutput {
        let mut state = WebState::default();
        state.print_settings_open = true;
        state.cups_settings.copies = 12;
        state.cups_settings.duplex = true;
        state.cups_settings.page_ranges = "1-5".to_owned();
        state.cups_notice = Some("CUPS service unavailable".to_owned());
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(760.0, 360.0),
                )),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::print_settings_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_site_styles_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.site_styles_open = true;
        state.site_style_host_draft = "example.test".to_owned();
        state.site_style_css_draft = "body { max-width: 80ch; }".to_owned();
        state.add_user_site_style();
        state.site_style_host_draft = "reader.example".to_owned();
        state.site_style_css_draft = "main { line-height: 1.6; }".to_owned();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::site_styles_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_spellcheck_error_drawer_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.latest_spellcheck = Some(super::super::BrowserSpellcheckResult::from_result(
            0,
            Err("hunspell not installed".to_owned()),
        ));
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 180.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::spellcheck_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_engine_and_speech_status_drawers_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.latest_security_update = Some(BrowserSecurityUpdateStatus {
            node: "node-1".to_owned(),
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
        });
        state.latest_read_aloud_status = Some(BrowserReadAloudStatus {
            node: "node-1".to_owned(),
            last_title: Some("Example".to_owned()),
            last_url: Some("https://example.test/".to_owned()),
            state: "speaking".to_owned(),
            last_error: None,
            accepted: 1,
            spoken: 0,
            rejected: 0,
            last_request_ms: Some(123),
            updated_ms: 124,
        });
        state.latest_voice_command_status = Some(BrowserVoiceCommandStatus {
            node: "node-1".to_owned(),
            last_url: Some("https://example.test/".to_owned()),
            last_mode: Some("command".to_owned()),
            state: "unavailable".to_owned(),
            last_error: Some("STT runtime is not configured".to_owned()),
            accepted: 1,
            transcribed: 0,
            rejected: 0,
            last_transcript_chars: None,
            last_request_ms: Some(223),
            updated_ms: 224,
        });

        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(760.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        drawers::security_update_drawer(ui, &mut state);
                        drawers::speech_status_drawer(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_new_tab_dashboard_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.dashboard_query = "mesh docs".to_owned();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 360.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        new_tab_dashboard(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_tab_search_results_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.open_options_tab();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(420.0, 320.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        ui.set_min_width(300.0);
                        state.tab_search_query = "options".to_owned();
                        let _ = tab_search_results(ui, &state);
                        state.tab_search_query = "does-not-match".to_owned();
                        let _ = tab_search_results(ui, &state);
                    });
                });
            },
        )
    }

    fn render_tab_search_menu_contents_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.open_options_tab();
        state.tab_search_query = "options".to_owned();
        drive_tab_search_menu_contents_frame(ctx, &mut state, Vec::new())
    }

    fn drive_tab_search_menu_contents_frame(
        ctx: &egui::Context,
        state: &mut WebState,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        drive_tab_search_menu_contents_frame_with_size(ctx, state, events, egui::vec2(420.0, 320.0))
    }

    fn drive_tab_search_menu_contents_frame_with_size(
        ctx: &egui::Context,
        state: &mut WebState,
        events: Vec<egui::Event>,
        size: egui::Vec2,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(0.0),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        let _ = tab_search_menu_contents(ui, state);
                    });
                });
            },
        )
    }

    fn pointer_button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn render_tab_search_menu_button_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.open_options_tab();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(96.0, 56.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        tab_search_menu(ui, &mut state);
                    });
                });
            },
        )
    }

    fn render_find_chrome_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        let (shell, _helper) = std::os::unix::net::UnixStream::pair().expect("find socketpair");
        let session =
            mde_web_preview_client::WebSession::from_stream(shell, None).expect("find session");
        state.push_session(session);
        state.find_open = true;
        state.find_query = "mesh search".to_owned();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 160.0),
                )),
                time: Some(0.0),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    scope(ui, |ui| {
                        find_chrome(ui, &mut state);
                    });
                });
            },
        )
    }

    fn assert_painted_text_color(
        painted: &[(String, egui::Color32)],
        label: &str,
        color: egui::Color32,
    ) {
        assert!(
            painted
                .iter()
                .any(|(text, painted_color)| text == label && *painted_color == color),
            "expected {label:?} to paint with {color:?}, got {painted:?}"
        );
    }

    fn assert_no_blank_painted_text(painted: &[(String, egui::Color32)], surface: &str) {
        assert!(
            painted.iter().all(|(text, _)| !text.trim().is_empty()),
            "{surface} must not paint blank stock button labels: {painted:?}"
        );
    }

    fn assert_browser_text_field_paint(out: &egui::FullOutput, label: &str, surface: &str) {
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, label, CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, color)| text == label
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "{surface} text field leaked shared shell text color for {label:?}: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "{surface} text field must paint a Browser surface fill: {fills:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "{surface} text field must paint Browser outline strokes: {strokes:?}"
        );
    }

    fn assert_rects_inside_viewport(out: &egui::FullOutput, width: f32, surface: &str) {
        let rects = painted_rect_bounds(&out.shapes);
        assert!(
            rects
                .iter()
                .filter(|rect| !(rect.left() <= 0.5 && rect.right() > width + 0.5))
                .all(|rect| rect.left() >= -0.5 && rect.right() <= width + 0.5),
            "{surface} painted rects must stay inside {width}px viewport: {rects:?}"
        );
    }

    fn assert_raw_drawer_rects_inside_viewport(out: &egui::FullOutput, width: f32, surface: &str) {
        let rects = painted_raw_rect_bounds(&out.shapes);
        assert!(
            rects
                .iter()
                .filter(|rect| !(rect.left() <= 0.5 && rect.right() > width + 0.5))
                .all(|rect| rect.left() >= -0.5 && rect.right() <= width + 0.5),
            "{surface} raw painted rects must stay inside {width}px viewport: {rects:?}"
        );
    }

    #[test]
    fn browser_option_rows_never_force_wider_than_the_available_column() {
        assert_eq!(option_row_width(-20.0), 0.0);
        assert_eq!(option_row_width(0.0), 0.0);
        assert_eq!(option_row_width(180.0), 180.0);
        assert_eq!(option_row_width(OPTION_ROW_MAX_W + 200.0), OPTION_ROW_MAX_W);
    }

    #[test]
    fn browser_options_disabled_rows_explain_their_command_gate() {
        use super::super::menubar::MenuAction;

        assert_eq!(
            disabled_option_tip(MenuAction::OpenAddress),
            "Type an address in a live tab first"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::Back),
            "No back-history entry is available"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::ReopenClosedTab),
            "No closed Browser tab is available to reopen"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::OpenFind),
            "Requires a live web page"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::TogglePowerMode),
            "Power Mode requires an open Browser tab"
        );
        for action in [MenuAction::OpenFind, MenuAction::TogglePowerMode] {
            let tip = disabled_option_tip(action);
            assert!(
                !tip.contains("helper-backed")
                    && !tip.contains("internal")
                    && !tip.contains("runtime"),
                "disabled Browser Options help must stay user-facing: {tip}"
            );
        }
        assert_eq!(
            disabled_option_tip(MenuAction::CaptureViewport),
            "Requires a live page with a painted frame"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::TogglePictureInPicture),
            "Start video playback in a Browser tab first"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::OpenLastPdf),
            "Requires a readable PDF saved from Browser"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::OpenChromiumDevtools),
            "Requires a live Chromium page"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::DownloadObservedMedia),
            "Requires a loaded page URL"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::PromptCameraPermission),
            "Requires a loaded first-party site"
        );
        assert_eq!(
            disabled_option_tip(MenuAction::ClearAllBrowsingData),
            "Open a non-crashed Browser tab first"
        );
    }

    #[test]
    fn browser_options_page_uses_compact_single_column_layout_when_narrow() {
        assert!(options_compact_layout(390.0));
        assert!(!options_compact_layout(900.0));

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_options_page_frame(&ctx, egui::vec2(390.0, 640.0));
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Browser Options", CHROME_TEXT);
        assert_painted_text_color(&texts, super::super::BROWSER_OPTIONS_URL, CHROME_TEXT_DIM);
        for label in [
            "Navigation",
            "Engines",
            "Input",
            "Rendering",
            "Instrumentation",
        ] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
        }
        assert_rects_inside_viewport(&out, 390.0, "narrow Browser Options page");
    }

    #[test]
    fn browser_options_compact_category_chips_fit_phone_width() {
        assert!(options_compact_layout(280.0));
        assert_eq!(options_category_chip_width("Instrumentation", 0.0), 0.0);
        assert!(options_category_chip_width("Instrumentation", 248.0) <= 172.0);
        assert!(options_category_chip_width("Instrumentation", 96.0) <= 96.0);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_options_page_frame(&ctx, egui::vec2(280.0, 640.0));
        let texts = painted_text(&out.shapes);

        for label in [
            "Navigation",
            "Engines",
            "Input",
            "Rendering",
            "Instrumentation",
        ] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "compact Browser Options category {label:?} leaked shared shell text color: {texts:?}"
            );
        }
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "compact Browser Options category chips should paint Chrome surface fills: {fills:?}"
        );
        assert_rects_inside_viewport(&out, 280.0, "phone-width Browser Options page");
    }

    #[test]
    fn browser_options_page_keeps_category_rail_when_wide() {
        assert!(!options_compact_layout(900.0));

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_options_page_frame(&ctx, egui::vec2(900.0, 640.0));
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Controls", CHROME_TEXT);
        assert_painted_text_color(&texts, super::super::BROWSER_OPTIONS_URL, CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Browser Options", CHROME_TEXT);
        assert_painted_text_color(&texts, "Use Chromium for New Tabs", CHROME_TEXT);
        assert_painted_text_color(&texts, "Open Typed Address", CHROME_TEXT);
        assert_painted_text_color(&texts, "Reload", CHROME_TEXT);
        assert_rects_inside_viewport(&out, 900.0, "wide Browser Options page");
    }

    #[test]
    fn browser_options_rows_export_accesskit_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_options_page_frame(&ctx, egui::vec2(900.0, 640.0));
        let nodes = accesskit_nodes(&out);
        let find = |label: &str| {
            nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing Browser Options AccessKit row {label:?}"))
        };

        let chromium = find("Use Chromium for New Tabs");
        let lightweight = find("Use Lightweight for New Tabs");
        for row in [chromium, lightweight] {
            assert_eq!(row.role(), egui::accesskit::Role::Button);
            assert!(
                row.supports_action(egui::accesskit::Action::Click),
                "enabled Browser Options rows must expose their command click action"
            );
        }
        let engine_values = [chromium.value(), lightweight.value()];
        assert!(
            engine_values.contains(&Some("On")) && engine_values.contains(&Some("Off")),
            "engine rows must expose checked state through AccessKit: {engine_values:?}"
        );
        assert_eq!(
            [chromium.is_selected(), lightweight.is_selected()]
                .into_iter()
                .filter(|selected| *selected == Some(true))
                .count(),
            1,
            "exactly one engine row should be selected"
        );

        let back = find("Back");
        assert_eq!(back.role(), egui::accesskit::Role::Button);
        assert_eq!(
            back.value(),
            Some("Unavailable: No back-history entry is available")
        );
        assert!(
            !back.supports_action(egui::accesskit::Action::Click),
            "disabled Browser Options rows must not expose a click action"
        );
    }

    #[test]
    fn browser_control_state_layers_use_shared_motion_driver() {
        let ctx = egui::Context::default();
        let _ = control_state_alpha_frame(&ctx, 0, MotionMode::Normal, 0.0);
        let (active_repaint, hover_alpha) =
            control_state_alpha_frame(&ctx, STATE_HOVER_ALPHA, MotionMode::Normal, 1.0 / 60.0);

        assert!(
            hover_alpha > 0.0 && hover_alpha < f32::from(STATE_HOVER_ALPHA),
            "normal control-state motion should ease toward hover alpha, got {hover_alpha}"
        );
        assert_eq!(
            active_repaint,
            Duration::ZERO,
            "active Browser control-state motion must keep the DRM loop warm"
        );

        let disabled = egui::Context::default();
        let _ = control_state_alpha_frame(&disabled, 0, MotionMode::Normal, 0.0);
        let (disabled_repaint, disabled_alpha) = control_state_alpha_frame(
            &disabled,
            STATE_HOVER_ALPHA,
            MotionMode::Disabled,
            1.0 / 60.0,
        );
        assert_eq!(disabled_alpha, f32::from(STATE_HOVER_ALPHA));
        assert_eq!(
            disabled_repaint,
            Duration::MAX,
            "disabled control-state motion should land at the endpoint and idle"
        );
    }

    #[test]
    fn browser_popovers_use_shared_popover_motion_driver() {
        let ctx = egui::Context::default();
        let _ = popover_motion_frame(&ctx, false, MotionMode::Normal, 0.0);
        let (active_repaint, entering) =
            popover_motion_frame(&ctx, true, MotionMode::Normal, 1.0 / 60.0);

        assert!(
            entering.opacity > 0.0 && entering.opacity < 1.0,
            "normal popover should fade toward visible, got {entering:?}"
        );
        assert!(
            (0.98..1.0).contains(&entering.scale),
            "normal popover should use a restrained anchor scale, got {entering:?}"
        );
        assert!(
            entering.anchor_offset > 0.0 && entering.anchor_offset <= 4.0,
            "normal popover should stay visually connected to the anchor, got {entering:?}"
        );
        assert!(entering.active);
        assert_eq!(
            active_repaint,
            Duration::ZERO,
            "active Browser popover motion must keep the DRM loop warm"
        );

        let reduced = egui::Context::default();
        let _ = popover_motion_frame(&reduced, false, MotionMode::Reduced, 0.0);
        let (reduced_repaint, reduced_entering) =
            popover_motion_frame(&reduced, true, MotionMode::Reduced, 1.0 / 60.0);
        assert!(
            reduced_entering.opacity > 0.0 && reduced_entering.opacity < 1.0,
            "reduced popover motion should keep a short fade, got {reduced_entering:?}"
        );
        assert_eq!(reduced_entering.scale, 1.0);
        assert_eq!(reduced_entering.anchor_offset, 0.0);
        assert_eq!(reduced_repaint, Duration::ZERO);

        let disabled = egui::Context::default();
        let _ = popover_motion_frame(&disabled, false, MotionMode::Disabled, 0.0);
        let (disabled_repaint, disabled_visible) =
            popover_motion_frame(&disabled, true, MotionMode::Disabled, 1.0 / 60.0);
        assert_eq!(disabled_visible.opacity, 1.0);
        assert_eq!(disabled_visible.scale, 1.0);
        assert_eq!(disabled_visible.anchor_offset, 0.0);
        assert!(!disabled_visible.active);
        assert_eq!(
            disabled_repaint,
            Duration::MAX,
            "disabled popover motion should land at the endpoint and idle"
        );
    }

    #[test]
    fn browser_prompt_bars_use_shared_dialog_motion_driver() {
        let ctx = egui::Context::default();
        let _ = dialog_prompt_motion_frame(&ctx, MotionMode::Normal, 0.0);
        let (active_repaint, entering) =
            dialog_prompt_motion_frame(&ctx, MotionMode::Normal, 1.0 / 60.0);

        assert!(
            entering.opacity > 0.0 && entering.opacity < 1.0,
            "normal prompt dialog should fade toward visible, got {entering:?}"
        );
        assert!(
            (0.97..1.0).contains(&entering.scale),
            "normal prompt dialog should use restrained scale, got {entering:?}"
        );
        assert!(
            entering.y_offset > 0.0 && entering.y_offset <= 3.0,
            "normal prompt dialog should use only a few px of travel, got {entering:?}"
        );
        assert!(entering.active);
        assert_eq!(active_repaint, Duration::ZERO);

        let reduced = egui::Context::default();
        let _ = dialog_prompt_motion_frame(&reduced, MotionMode::Reduced, 0.0);
        let (reduced_repaint, reduced_entering) =
            dialog_prompt_motion_frame(&reduced, MotionMode::Reduced, 1.0 / 60.0);
        assert!(
            reduced_entering.opacity > 0.0 && reduced_entering.opacity < 1.0,
            "reduced prompt dialog should keep a short fade, got {reduced_entering:?}"
        );
        assert_eq!(reduced_entering.scale, 1.0);
        assert_eq!(reduced_entering.y_offset, 0.0);
        assert_eq!(reduced_repaint, Duration::ZERO);

        let disabled = egui::Context::default();
        let _ = dialog_prompt_motion_frame(&disabled, MotionMode::Disabled, 0.0);
        let (disabled_repaint, disabled_visible) =
            dialog_prompt_motion_frame(&disabled, MotionMode::Disabled, 1.0 / 60.0);
        assert_eq!(disabled_visible.opacity, 1.0);
        assert_eq!(disabled_visible.scale, 1.0);
        assert_eq!(disabled_visible.y_offset, 0.0);
        assert!(!disabled_visible.active);
        assert_eq!(disabled_repaint, Duration::MAX);
    }

    #[test]
    fn browser_drawer_stack_uses_shared_panel_motion_driver() {
        let ctx = egui::Context::default();
        let _ = panel_motion_frame(&ctx, false, MotionMode::Normal, 0.0);
        let (active_repaint, entering) =
            panel_motion_frame(&ctx, true, MotionMode::Normal, 1.0 / 60.0);

        assert!(
            entering.opacity > 0.0 && entering.opacity < 1.0,
            "normal drawer panel should fade toward visible, got {entering:?}"
        );
        assert!(
            entering.y_offset > 0.0 && entering.y_offset <= 7.0,
            "normal drawer panel should travel a restrained distance, got {entering:?}"
        );
        assert!(entering.active);
        assert_eq!(active_repaint, Duration::ZERO);

        let reduced = egui::Context::default();
        let _ = panel_motion_frame(&reduced, false, MotionMode::Reduced, 0.0);
        let (reduced_repaint, reduced_entering) =
            panel_motion_frame(&reduced, true, MotionMode::Reduced, 1.0 / 60.0);
        assert!(
            reduced_entering.opacity > 0.0 && reduced_entering.opacity < 1.0,
            "reduced drawer panel should keep a short fade, got {reduced_entering:?}"
        );
        assert!(
            reduced_entering.y_offset > 0.0 && reduced_entering.y_offset <= 2.0,
            "reduced drawer panel should use shortened travel, got {reduced_entering:?}"
        );
        assert_eq!(reduced_repaint, Duration::ZERO);

        let disabled = egui::Context::default();
        let _ = panel_motion_frame(&disabled, false, MotionMode::Disabled, 0.0);
        let (disabled_repaint, disabled_visible) =
            panel_motion_frame(&disabled, true, MotionMode::Disabled, 1.0 / 60.0);
        assert_eq!(disabled_visible.opacity, 1.0);
        assert_eq!(disabled_visible.y_offset, 0.0);
        assert!(!disabled_visible.active);
        assert_eq!(disabled_repaint, Duration::MAX);

        let mut state = WebState::default();
        assert!(!drawer_stack_visible(&state));
        state.downloads_open = true;
        assert!(drawer_stack_visible(&state));
    }

    #[test]
    fn browser_active_body_switches_use_shared_page_motion_driver() {
        let ctx = egui::Context::default();
        let (_, initial) = page_motion_frame(&ctx, 1, MotionMode::Normal, 0.0);
        assert_eq!(
            initial.opacity, 1.0,
            "first Browser page body render should land settled so existing render tests stay stable"
        );
        assert!(!initial.active);

        let (active_repaint, entering) = page_motion_frame(&ctx, 2, MotionMode::Normal, 1.0 / 60.0);
        assert!(
            entering.opacity > 0.0 && entering.opacity < 1.0,
            "normal page switch should fade the incoming body, got {entering:?}"
        );
        assert!(entering.active);
        assert_eq!(
            active_repaint,
            Duration::ZERO,
            "active Browser page motion must keep the DRM loop warm"
        );

        let reduced = egui::Context::default();
        let _ = page_motion_frame(&reduced, 1, MotionMode::Reduced, 0.0);
        let (reduced_repaint, reduced_entering) =
            page_motion_frame(&reduced, 2, MotionMode::Reduced, 1.0 / 60.0);
        assert!(
            reduced_entering.opacity > 0.0 && reduced_entering.opacity < 1.0,
            "reduced page motion should keep a short cross-fade, got {reduced_entering:?}"
        );
        assert_eq!(reduced_repaint, Duration::ZERO);

        let disabled = egui::Context::default();
        let _ = page_motion_frame(&disabled, 1, MotionMode::Disabled, 0.0);
        let (disabled_repaint, disabled_visible) =
            page_motion_frame(&disabled, 2, MotionMode::Disabled, 1.0 / 60.0);
        assert_eq!(disabled_visible.opacity, 1.0);
        assert!(!disabled_visible.active);
        assert_eq!(
            disabled_repaint,
            Duration::MAX,
            "disabled page motion should land at the endpoint and idle"
        );

        let mut state = WebState::default();
        let new_tab_key = active_body_page_motion_key(&state);
        state
            .push_internal_page(BrowserInternalPage::Options)
            .expect("internal options page opens");
        let options_key = active_body_page_motion_key(&state);
        assert_ne!(
            new_tab_key, options_key,
            "Browser body motion key must change across internal page switches"
        );
    }

    #[test]
    fn browser_tab_drag_release_uses_shared_drag_settle_motion_driver() {
        let ctx = egui::Context::default();
        let (_, idle) =
            tab_drag_settle_motion_frame(&ctx, 42, TabAxis::Horizontal, MotionMode::Normal, 0.0);
        assert_eq!(idle, BrowserDragSettleMotion::settled());

        let release_repaint =
            note_tab_drag_settle_frame(&ctx, 42, TabAxis::Horizontal, 1.0, 1.0 / 60.0);
        assert_eq!(
            release_repaint,
            Duration::ZERO,
            "recording a tab drag release should wake the DRM loop for the settle frame"
        );
        let (active_repaint, settling) = tab_drag_settle_motion_frame(
            &ctx,
            42,
            TabAxis::Horizontal,
            MotionMode::Normal,
            2.0 / 60.0,
        );
        assert!(
            settling.offset.x > 0.0 && settling.offset.x <= TAB_DRAG_SETTLE_TRAVEL,
            "normal horizontal drag settle should start with restrained travel, got {settling:?}"
        );
        assert_eq!(settling.offset.y, 0.0);
        assert!(settling.accent_alpha > 0);
        assert!(settling.active);
        assert_eq!(
            active_repaint,
            Duration::ZERO,
            "active Browser drag-settle motion must keep the DRM loop warm"
        );

        let (_, other_tab) = tab_drag_settle_motion_frame(
            &ctx,
            7,
            TabAxis::Horizontal,
            MotionMode::Normal,
            3.0 / 60.0,
        );
        assert_eq!(
            other_tab,
            BrowserDragSettleMotion::settled(),
            "drag-settle state must be keyed to the moved tab id, not the tab index"
        );

        let disabled = egui::Context::default();
        let _ = note_tab_drag_settle_frame(&disabled, 42, TabAxis::Vertical, -1.0, 0.0);
        let (disabled_repaint, disabled_motion) = tab_drag_settle_motion_frame(
            &disabled,
            42,
            TabAxis::Vertical,
            MotionMode::Disabled,
            1.0 / 60.0,
        );
        assert_eq!(disabled_motion, BrowserDragSettleMotion::settled());
        assert_eq!(
            disabled_repaint,
            Duration::ZERO,
            "the release notification should schedule exactly one frame before disabled mode snaps"
        );
        let (disabled_idle_repaint, disabled_idle) = tab_drag_settle_motion_frame(
            &disabled,
            42,
            TabAxis::Vertical,
            MotionMode::Disabled,
            2.0 / 60.0,
        );
        assert_eq!(disabled_idle, BrowserDragSettleMotion::settled());
        assert_eq!(
            disabled_idle_repaint,
            Duration::MAX,
            "disabled drag-settle motion should land at the endpoint and idle"
        );
    }

    #[test]
    fn browser_chrome_tokens_are_local_material_roles() {
        assert_eq!(tab_stroke(true), CHROME_OUTLINE);
        assert_eq!(tab_stroke(false), CHROME_SURFACE_CONTAINER_HIGH);
        assert_eq!(tab_text(false), CHROME_TEXT_DIM);
        assert_eq!(row_fill(true), CHROME_PRIMARY_CONTAINER);
        assert_eq!(selected_text(true), CHROME_ON_PRIMARY_CONTAINER);
        assert_eq!(tone_color(ChipTone::Warn), CHROME_WARN);
        assert_eq!(page_backdrop_fill(), CHROME_SURFACE_CONTAINER);
        assert_eq!(tab_group_color(0), CHROME_GROUP_BLUE);
        assert_eq!(tab_group_color(1), CHROME_GROUP_GREEN);
        assert_eq!(tab_group_color(2), CHROME_GROUP_AMBER);
        assert_eq!(tab_group_color(3), CHROME_GROUP_RED);
        assert_eq!(tab_group_color(4), CHROME_GROUP_PURPLE);
        assert_eq!(tab_group_color(5), CHROME_GROUP_BLUE);
    }

    #[test]
    fn required_browser_icons_have_non_empty_painters() {
        for icon in ALL_BROWSER_ICONS {
            assert!(
                chrome_icon_painted_shape_count(*icon) > 0,
                "Browser-local icon {icon:?} must paint at least one shape"
            );
        }
        for icon in REQUIRED_BROWSER_ICONS {
            assert!(
                chrome_icon_painted_shape_count(*icon) > 0,
                "required Browser icon {icon:?} must paint at least one shape"
            );
        }
        assert_eq!(
            chrome_icon_painted_shape_count(ChromeIcon::Minus),
            1,
            "Browser stepper minus icon should paint one local line"
        );
        assert_eq!(
            chrome_icon_painted_shape_count(ChromeIcon::Plus),
            2,
            "Browser stepper plus fallback should remain non-empty if the YAMIS asset cannot load"
        );
    }

    #[test]
    fn browser_chrome_icons_prefer_yamis_assets_when_available() {
        let mapped = [
            (ChromeIcon::Back, IconId::ArrowLeft),
            (ChromeIcon::Forward, IconId::ArrowRight),
            (ChromeIcon::Options, IconId::Menu),
            (ChromeIcon::Downloads, IconId::Downloads),
            (ChromeIcon::Capture, IconId::Capture),
            (ChromeIcon::Bookmark, IconId::Bookmarks),
            (ChromeIcon::Security, IconId::Security),
            (ChromeIcon::Privacy, IconId::Security),
            (ChromeIcon::Warning, IconId::Warning),
            (ChromeIcon::Search, IconId::Search),
            (ChromeIcon::Find, IconId::Search),
            (ChromeIcon::Close, IconId::Close),
            (ChromeIcon::Reload, IconId::Reload),
            (ChromeIcon::Stop, IconId::Cancel),
            (ChromeIcon::Print, IconId::Print),
            (ChromeIcon::History, IconId::History),
            (ChromeIcon::Tabs, IconId::Tabs),
            (ChromeIcon::Engine, IconId::Internet),
            (ChromeIcon::NewTab, IconId::NewTab),
            (ChromeIcon::Plus, IconId::Add),
            (ChromeIcon::Up, IconId::ChevronUp),
            (ChromeIcon::Down, IconId::ArrowDown),
            (ChromeIcon::Check, IconId::Check),
            (ChromeIcon::Page, IconId::Page),
            (ChromeIcon::Edit, IconId::TextEdit),
            (ChromeIcon::View, IconId::View),
            (ChromeIcon::Power, IconId::Power),
            (ChromeIcon::Share, IconId::Share),
            (ChromeIcon::Audio, IconId::Audio),
            (ChromeIcon::Play, IconId::Play),
            (ChromeIcon::Pause, IconId::Pause),
            (ChromeIcon::MediaStop, IconId::MediaStop),
            (ChromeIcon::Previous, IconId::Previous),
            (ChromeIcon::Next, IconId::Next),
            (ChromeIcon::Minus, IconId::Remove),
            (ChromeIcon::ZoomIn, IconId::ZoomIn),
            (ChromeIcon::ZoomOut, IconId::ZoomOut),
            (ChromeIcon::VolumeDown, IconId::VolumeLow),
            (ChromeIcon::VolumeOff, IconId::VolumeMuted),
            (ChromeIcon::VolumeUp, IconId::Volume),
            (ChromeIcon::PictureInPicture, IconId::PictureInPicture),
            (ChromeIcon::DarkMode, IconId::DarkMode),
            (ChromeIcon::Lock, IconId::Lock),
            (ChromeIcon::Notifications, IconId::Notifications),
        ];
        for (chrome, yamis) in mapped {
            assert_eq!(
                chrome_icon_yamis_id(chrome),
                Some(yamis),
                "{chrome:?} should resolve through the YAMIS icon catalog"
            );
        }
        // Recommend has no YAMIS glyph and always paints its local star fallback.
        assert_eq!(chrome_icon_yamis_id(ChromeIcon::Recommend), None);
        assert!(
            chrome_icon_painted_shape_count(ChromeIcon::Recommend) > 0,
            "Recommend must paint a local fallback since it has no YAMIS asset"
        );
    }

    #[test]
    fn every_chrome_icon_maps_to_a_registered_carbon_glyph() {
        // The icon-standard foundation: every one of the 45 browser ChromeIcons
        // resolves to a Mackes-Carbon glyph embedded in the shared loader, and
        // that glyph rasterizes to a non-blank tinted mask.
        for icon in ALL_BROWSER_ICONS {
            let name = chrome_icon_carbon_name(*icon);
            assert!(
                mde_egui::carbon::carbon_svg_bytes(name).is_some(),
                "{icon:?} maps to Carbon glyph {name:?}, which must be embedded in the loader registry"
            );
            let raster = mde_egui::carbon::carbon_raster(name, 32, CHROME_TEXT);
            assert!(
                raster
                    .as_ref()
                    .is_some_and(|r| r.rgba.chunks_exact(4).any(|px| px[3] > 0)),
                "{icon:?} -> Carbon glyph {name:?} must rasterize to a non-blank mask"
            );
        }
    }

    fn sized_input(w: f32, h: f32) -> egui::RawInput {
        let mut input = egui::RawInput::default();
        input.screen_rect = Some(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(w, h),
        ));
        input
    }

    #[test]
    fn vertical_rail_affordance_cluster_paints_its_row() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        let out = ctx.run(sized_input(CHROME_TAB_RAIL_W, 240.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let rect = egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(CHROME_TAB_RAIL_W, RAIL_CLUSTER_H),
                );
                let mut cluster = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(rect)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                vertical_rail_affordances(&mut cluster, &mut state);
            });
        });
        assert!(
            !out.shapes.is_empty(),
            "the rail affordance cluster produced no primitives"
        );
    }

    #[test]
    fn notifications_drawer_paints_absorbed_notices_without_closing() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        let mut state = WebState::default();
        state.notifications_open = true;
        state.capture_notice = Some("QR share ready".to_owned());
        state.absorb_browser_notices();
        state.capture_notice = Some("Capture failed: no painted page".to_owned());
        state.absorb_browser_notices();
        assert_eq!(state.browser_notices.len(), 2);

        let out = ctx.run(sized_input(360.0, 400.0), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                scope(ui, |ui| notifications_drawer(ui, &mut state));
            });
        });
        assert!(
            !out.shapes.is_empty(),
            "the notifications drawer painted nothing while open with notices"
        );
        assert!(
            state.notifications_open,
            "rendering the drawer must not close it"
        );
    }

    #[test]
    fn vertical_mode_drops_the_toolbar_capture_and_downloads_affordances() {
        fn nav_shapes(vertical: bool) -> usize {
            let ctx = egui::Context::default();
            Style::install(&ctx);
            let mut state = WebState::default();
            state.vertical_tabs = vertical;
            let out = ctx.run(sized_input(1200.0, 800.0), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| nav_chrome(ui, &mut state));
            });
            out.shapes.len()
        }
        // The rail cluster owns Downloads + Capture in vertical mode, so the
        // toolbar paints strictly fewer primitives than the horizontal layout,
        // which still carries both affordances.
        assert!(
            nav_shapes(false) > nav_shapes(true),
            "vertical toolbar must suppress the Capture + Downloads affordances the rail owns"
        );
    }

    #[test]
    fn loading_globe_honors_reduce_motion_without_losing_the_static_status() {
        let (animated_phase, animated_repaint) = loading_globe_phase(0.75, false);
        assert!(animated_phase > 0.0, "motion-on globe advances phase");
        assert!(
            animated_repaint,
            "motion-on globe requests the throbber loop"
        );

        let (static_phase, static_repaint) = loading_globe_phase(0.75, true);
        assert_eq!(
            static_phase, 0.0,
            "reduced-motion globe parks on a stable frame"
        );
        assert!(
            !static_repaint,
            "reduced-motion globe must not schedule a 33 ms repaint loop"
        );
        assert!(
            loading_globe_painted_shape_count() > 0,
            "the parked globe still paints a real non-text loading status"
        );
    }

    #[test]
    fn browser_media_toolbar_density_preserves_omnibox_budget() {
        let reserve = media_toolbar_trailing_nav_min_width();
        let full = media_toolbar_estimated_width(MediaToolbarDensity::Full);
        let compact = media_toolbar_estimated_width(MediaToolbarDensity::Compact);
        let icon_only = media_toolbar_estimated_width(MediaToolbarDensity::IconOnly);

        assert!(full > compact);
        assert!(compact > icon_only);
        assert_eq!(
            media_toolbar_density(reserve + full),
            MediaToolbarDensity::Full
        );
        assert_eq!(
            media_toolbar_density(reserve + full - 1.0),
            MediaToolbarDensity::Compact
        );
        assert_eq!(
            media_toolbar_density(reserve + compact),
            MediaToolbarDensity::Compact
        );
        assert_eq!(
            media_toolbar_density(reserve + compact - 1.0),
            MediaToolbarDensity::IconOnly
        );
        assert_eq!(
            media_toolbar_density(reserve + icon_only),
            MediaToolbarDensity::IconOnly
        );
        assert_eq!(
            media_toolbar_density(reserve + icon_only - 1.0),
            MediaToolbarDensity::Hidden
        );
        assert_eq!(media_toolbar_density(f32::NAN), MediaToolbarDensity::Hidden);
    }

    #[test]
    fn navigation_toolbar_compacts_before_squeezing_the_address_bar() {
        let threshold = nav_full_chrome_min_width(0, 0);
        assert!(
            threshold > 680.0,
            "full Browser chrome budget should account for optional controls: {threshold}"
        );
        assert!(
            nav_chrome_uses_compact_layout(threshold - 1.0, 0, 0),
            "toolbar should shed optional controls before the address bar is crowded"
        );
        assert!(
            !nav_chrome_uses_compact_layout(threshold, 0, 0),
            "toolbar should keep full controls once the address bar floor is available"
        );
        assert!(
            nav_chrome_uses_compact_layout(f32::NAN, 0, 0),
            "non-finite width should use the bounded compact row"
        );

        let (desired, min) = nav_omnibox_widths(110.0, true, 0, 0, false);
        assert_eq!(desired, NAV_OMNIBOX_TINY_MIN);
        assert_eq!(min, NAV_OMNIBOX_TINY_MIN);

        let (desired, min) = nav_omnibox_widths(240.0, true, 0, 0, false);
        assert_eq!(desired, 165.0);
        assert_eq!(min, 165.0);
    }

    #[test]
    fn navigation_toolbar_budgets_active_download_badge() {
        assert_eq!(toolbar_count_badge_text(0).as_deref(), None);
        assert_eq!(toolbar_count_badge_text(7).as_deref(), Some("7"));
        assert_eq!(toolbar_count_badge_text(99).as_deref(), Some("99"));
        assert_eq!(toolbar_count_badge_text(100).as_deref(), Some("99+"));

        let empty_threshold = nav_full_chrome_min_width(0, 0);
        let active_threshold = nav_full_chrome_min_width(12, 0);
        assert_eq!(
            active_threshold - empty_threshold,
            download_count_badge_reserve(12)
        );
        assert!(
            nav_chrome_uses_compact_layout(empty_threshold, 12, 0),
            "active download badges must force full chrome to compact before squeezing the address bar"
        );
        assert!(
            !nav_chrome_uses_compact_layout(active_threshold, 12, 0),
            "full chrome can stay expanded once the active badge has been budgeted"
        );

        let no_badge_reserve = nav_omnibox_trailing_reserve(true, 0, 0, false);
        let badge_reserve = nav_omnibox_trailing_reserve(true, 12, 0, false);
        assert_eq!(
            badge_reserve - no_badge_reserve,
            download_count_badge_reserve(12)
        );

        let (desired, min) = nav_omnibox_widths(240.0, true, 12, 0, false);
        assert_eq!(desired, 134.0);
        assert_eq!(min, 134.0);
    }

    #[test]
    fn navigation_toolbar_budgets_blocked_request_badge() {
        let empty_threshold = nav_full_chrome_min_width(0, 0);
        let blocked_threshold = nav_full_chrome_min_width(0, 250);
        assert_eq!(
            blocked_threshold - empty_threshold,
            ad_filter_chip_reserve(250)
        );
        assert_eq!(
            toolbar_count_badge_text(250).as_deref(),
            Some("99+"),
            "high blocked-request counts must stay inside the fixed badge"
        );
        assert!(
            nav_chrome_uses_compact_layout(empty_threshold, 0, 250),
            "blocked-request badges must force full chrome to compact before squeezing the address bar"
        );
        assert!(
            !nav_chrome_uses_compact_layout(blocked_threshold, 0, 250),
            "full chrome can stay expanded once the blocked-request badge has been budgeted"
        );
    }

    #[test]
    fn compact_media_toolbar_label_elides_before_paint() {
        let label = "Now: A very long media title - An equally long artist";
        let compact = media_toolbar_label_text(label, MediaToolbarDensity::Compact);
        let full = media_toolbar_label_text(label, MediaToolbarDensity::Full);

        assert!(compact.starts_with("    Now:"));
        assert!(compact.contains("..."));
        assert!(compact.chars().count() <= 22);
        assert!(full.chars().count() <= 36);
        assert!(
            full.chars().count() > compact.chars().count(),
            "full toolbar keeps more metadata than the compact rail"
        );
    }

    #[test]
    fn browser_action_buttons_use_material_roles() {
        assert_eq!(
            action_button_fill(BrowserActionRole::Primary),
            CHROME_PRIMARY
        );
        assert_eq!(
            action_button_text(BrowserActionRole::Primary),
            CHROME_TOOLBAR
        );
        assert_eq!(
            action_button_stroke(BrowserActionRole::Primary),
            CHROME_PRIMARY
        );
        assert_eq!(
            action_button_fill(BrowserActionRole::Secondary),
            CHROME_TOOLBAR
        );
        assert_eq!(
            action_button_text(BrowserActionRole::Secondary),
            CHROME_TEXT
        );
        assert_eq!(
            action_button_stroke(BrowserActionRole::Secondary),
            CHROME_OUTLINE
        );
        assert_eq!(action_button_fill(BrowserActionRole::Warning), CHROME_WARN);
        assert_eq!(
            action_button_text(BrowserActionRole::Warning),
            CHROME_TOOLBAR
        );
        assert_eq!(
            action_button_text(BrowserActionRole::Quiet),
            CHROME_TEXT_DIM
        );
    }

    #[test]
    fn engine_selector_uses_browser_local_labels_and_state() {
        assert_eq!(engine_display_name(BrowserEngine::Cef), "Chromium");
        assert_eq!(engine_marker(BrowserEngine::Cef), "CEF");
        assert_eq!(engine_glyph(BrowserEngine::Cef), "C");
        assert_eq!(engine_display_name(BrowserEngine::Servo), "Lightweight");
        assert_eq!(engine_marker(BrowserEngine::Servo), "Servo");
        assert_eq!(engine_glyph(BrowserEngine::Servo), "S");
        assert_eq!(engine_accent(BrowserEngine::Cef), CHROME_PRIMARY);
        assert_eq!(engine_accent(BrowserEngine::Servo), CHROME_SUCCESS);
        assert_eq!(
            engine_container(BrowserEngine::Cef),
            CHROME_PRIMARY_CONTAINER
        );
        assert_eq!(
            engine_container(BrowserEngine::Servo),
            CHROME_SUCCESS_CONTAINER
        );
        assert_eq!(
            engine_on_container(BrowserEngine::Cef),
            CHROME_ON_PRIMARY_CONTAINER
        );
        assert_eq!(
            engine_on_container(BrowserEngine::Servo),
            CHROME_ON_SUCCESS_CONTAINER
        );
    }

    #[test]
    fn engine_toolbar_chip_uses_icon_first_badge_not_full_text_marker() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for engine in [BrowserEngine::Cef, BrowserEngine::Servo] {
            let out = render_engine_toolbar_chip_frame(&ctx, engine);
            let texts = painted_text(&out.shapes);

            assert_painted_text_color(&texts, engine_glyph(engine), CHROME_TOOLBAR);
            assert!(
                !texts.iter().any(|(text, _)| text == engine_marker(engine)),
                "toolbar engine chip must not paint the full engine marker as button text: {texts:?}"
            );

            let fills = painted_rect_fills(&out.shapes);
            assert!(
                fills.contains(&engine_container(engine)),
                "toolbar engine chip must paint the Browser engine container: {fills:?}"
            );

            let rect_strokes = painted_rect_strokes(&out.shapes);
            assert!(
                rect_strokes.iter().any(|stroke| {
                    stroke.color == engine_accent(engine) && (stroke.width - 1.0).abs() < 0.01
                }),
                "toolbar engine chip must paint the Browser engine outline: {rect_strokes:?}"
            );

            let icon_lines = painted_line_strokes(&out.shapes);
            assert!(
                icon_lines.iter().any(|stroke| {
                    stroke.color == engine_on_container(engine) && (stroke.width - 1.7).abs() < 0.01
                }),
                "toolbar engine chip must paint the Browser engine icon: {icon_lines:?}"
            );
        }
    }

    #[test]
    fn download_drawer_status_uses_browser_material_roles() {
        use mde_files_egui::transfers::TransferState;

        assert_eq!(download_state_color(TransferState::Done), CHROME_SUCCESS);
        assert_eq!(download_state_color(TransferState::Failed), CHROME_ERROR);
        assert_eq!(download_state_color(TransferState::Paused), CHROME_WARN);
        assert_eq!(download_state_color(TransferState::Queued), CHROME_TEXT_DIM);
        assert_eq!(
            download_state_color(TransferState::Running),
            CHROME_TEXT_DIM
        );
    }

    #[test]
    fn page_action_tokens_cover_disabled_plain_and_bookmarked_states() {
        assert_eq!(page_action_icon_color(false, false), CHROME_TEXT_DIM);
        assert_eq!(page_action_icon_color(true, false), CHROME_ICON);
        assert_eq!(page_action_icon_color(true, true), CHROME_PRIMARY);
        assert_eq!(
            page_actions_tip(true),
            "Bookmarked: copy URL, share, send tab"
        );
        assert!(
            !page_actions_tip(true).contains("edit"),
            "the toolbar must not advertise an edit flow the menu does not implement"
        );
    }

    #[test]
    fn omnibox_formats_use_browser_material_text_roles() {
        let font = font_id(13.0);
        assert_eq!(omnibox_dim_format(font.clone()).color, CHROME_TEXT_DIM);
        assert_eq!(omnibox_strong_format(font).color, CHROME_TEXT);
    }

    #[test]
    fn browser_omnibox_text_field_uses_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_omnibox_chrome_frame(&ctx);

        assert_browser_text_field_paint(&out, "https://example.test/mesh", "omnibox");

        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "example.test/mesh", CHROME_TEXT);
    }

    #[test]
    fn browser_omnibox_pretty_url_clips_long_text_to_location_field() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let address = "https://www.very-long-location-bar-hostname.example.test/this/path/keeps/going?query=wide";
        let pretty =
            "very-long-location-bar-hostname.example.test/this/path/keeps/going?query=wide";
        let out = render_omnibox_chrome_frame_with_address(&ctx, egui::vec2(360.0, 96.0), address);
        let address_rect = accesskit_nodes(&out)
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.role() == egui::accesskit::Role::TextInput)
            .map(accesskit_bounds_rect)
            .expect("omnibox text input node");
        let geometry = painted_text_geometry(&out.shapes);
        let (_, text_rect, clip_rect) = geometry
            .iter()
            .find(|(text, _, _)| text == pretty)
            .unwrap_or_else(|| panic!("pretty URL text was not painted: {geometry:?}"));

        assert!(
            text_rect.right() > address_rect.right(),
            "the long pretty URL should be wider than the field so this test proves clipping: text={text_rect:?} address={address_rect:?}"
        );
        assert!(
            clip_rect.left() >= address_rect.left() - 0.5
                && clip_rect.right() <= address_rect.right() + 0.5,
            "pretty URL clip must stay inside the location field: clip={clip_rect:?} address={address_rect:?}"
        );
    }

    #[test]
    fn focused_omnibox_inline_completion_clips_to_location_field() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let field_rect =
            egui::Rect::from_min_size(egui::pos2(18.0, 24.0), egui::vec2(112.0, CHROME_OMNIBOX_H));
        let tail = "/completion-tail-that-is-too-wide-for-the-field";
        let out = render_inline_completion_frame(
            &ctx,
            field_rect,
            "https://very-long-draft.example.test",
            tail,
        );
        let geometry = painted_text_geometry(&out.shapes);
        let (_, text_rect, clip_rect) = geometry
            .iter()
            .find(|(text, _, _)| text == tail)
            .unwrap_or_else(|| panic!("inline completion tail was not painted: {geometry:?}"));
        let expected_clip = omnibox_text_clip_rect(field_rect);

        assert!(
            text_rect.right() > expected_clip.right(),
            "the inline completion should be wider than the clipped field: text={text_rect:?} clip={expected_clip:?}"
        );
        assert_eq!(*clip_rect, expected_clip);
    }

    #[test]
    fn browser_omnibox_uses_readable_location_bar_metrics() {
        assert!(
            CHROME_BUTTON <= 21.0 && CHROME_TAB_H <= 22.0,
            "Browser toolbar controls and tabs should stay on the refined chrome scale"
        );
        assert!(
            OMNIBOX_FONT >= 15.5,
            "location text must be substantially larger than dense toolbar labels"
        );
        assert!(
            OMNIBOX_FONT >= CHROME_FONT + 4.0,
            "location text must not collapse back to compact toolbar typography"
        );
        assert!(
            CHROME_OMNIBOX_H >= 32.0,
            "location bar must keep enough height for the larger text without returning to the old thick chrome"
        );
        assert!(
            NAV_FULL_OMNIBOX_FLOOR >= 360.0,
            "full toolbar should preserve a useful address field before optional buttons"
        );
    }

    #[test]
    fn omnibox_clears_only_the_committed_url_when_editing_starts() {
        assert!(omnibox_should_clear_on_edit_start(
            false,
            true,
            "https://example.test/path",
            "https://example.test/path"
        ));
        assert!(!omnibox_should_clear_on_edit_start(
            true,
            true,
            "https://example.test/path",
            "https://example.test/path"
        ));
        assert!(!omnibox_should_clear_on_edit_start(
            false,
            true,
            "mesh draft",
            "https://example.test/path"
        ));
        assert!(!omnibox_should_clear_on_edit_start(
            false,
            false,
            "https://example.test/path",
            "https://example.test/path"
        ));
    }

    #[test]
    fn browser_toolbar_keeps_only_page_navigation_left_of_location() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_omnibox_chrome_frame_with_size(&ctx, egui::vec2(1180.0, 96.0));
        let nodes = accesskit_nodes(&out);
        let rect_for = |label: &str| {
            let node = nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing toolbar AccessKit node {label:?}: {nodes:?}"));
            accesskit_bounds_rect(node)
        };
        let rect_starting = |prefix: &str| {
            let node = nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label().is_some_and(|label| label.starts_with(prefix)))
                .unwrap_or_else(|| {
                    panic!("missing toolbar AccessKit node starting {prefix:?}: {nodes:?}")
                });
            accesskit_bounds_rect(node)
        };
        let text_input_rect = |value: &str| {
            let node = nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| {
                    node.role() == egui::accesskit::Role::TextInput && node.value() == Some(value)
                })
                .unwrap_or_else(|| {
                    panic!("missing toolbar text input with value {value:?}: {nodes:?}")
                });
            accesskit_bounds_rect(node)
        };

        let new_tab = rect_starting("Open a new tab with");
        let back = rect_for("Back");
        let reload = rect_for("Reload");
        let forward = rect_for("Forward");
        let address = text_input_rect("https://example.test/mesh");
        assert!(
            new_tab.left() < back.left()
                && back.left() < reload.left()
                && reload.left() < forward.left()
                && forward.left() < address.left(),
            "toolbar left cluster must be New Tab/type, Back, Reload/Stop, Forward, then Location: new_tab={new_tab:?} back={back:?} reload={reload:?} forward={forward:?} address={address:?}"
        );

        let forward_node = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Forward"))
            .expect("Forward node exists");
        assert!(
            !forward_node.supports_action(egui::accesskit::Action::Click),
            "Forward remains visible but disabled when no forward history exists"
        );

        let page_actions = rect_for("Page actions: bookmark, copy URL, share");
        let passwords = rect_for("Passwords and autofill");
        let capture = rect_for("Capture viewport");
        let downloads = rect_for("Downloads");
        let options = rect_for("Browser options");
        assert!(
            address.right() < page_actions.left()
                && page_actions.left() < passwords.left()
                && passwords.left() < capture.left()
                && capture.left() < downloads.left()
                && downloads.left() < options.left(),
            "non-navigation Browser actions must sit to the right of Location before the far-right menu: address={address:?} page_actions={page_actions:?} passwords={passwords:?} capture={capture:?} downloads={downloads:?} options={options:?}"
        );
    }

    #[test]
    fn page_context_menu_rows_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_context_rows_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Back", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Forward", CHROME_TEXT);
        assert_painted_text_color(&texts, "Reload", CHROME_TEXT);
        assert_painted_text_color(&texts, "Cut", CHROME_TEXT);
        assert_painted_text_color(&texts, "Copy", CHROME_TEXT);
        assert_painted_text_color(&texts, "Paste", CHROME_TEXT);
        assert_painted_text_color(&texts, "Select all", CHROME_TEXT);
        assert_painted_text_color(&texts, "Copy page URL", CHROME_TEXT);
    }

    #[test]
    fn page_context_menu_rows_clip_to_narrow_browser_chrome_width() {
        assert_eq!(chrome_menu_row_width(0.0), 0.0);
        assert_eq!(chrome_menu_row_width(124.0), 124.0);
        assert_eq!(chrome_menu_row_width(420.0), 420.0);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_context_rows_frame_with_size(&ctx, egui::vec2(180.0, 320.0));
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Back", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Copy page URL", CHROME_TEXT);
        assert_rects_inside_viewport(&out, 180.0, "narrow page context menu");
    }

    #[test]
    fn page_context_menu_native_frame_uses_browser_chrome_visuals() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);

        let out = render_open_page_context_menu_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Forward", CHROME_TEXT);
        assert_painted_text_color(&texts, "Copy page URL", CHROME_TEXT);

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            !fills
                .iter()
                .any(|fill| matches!(*fill, Style::BG | Style::SURFACE | Style::SURFACE_HI)),
            "Browser page context menu leaked shared shell-dark menu fills: {fills:?}"
        );
        assert_eq!(
            ctx.style().visuals.window_fill,
            Style::SURFACE,
            "Browser context-menu styling must restore the surrounding shell style"
        );
    }

    #[test]
    fn page_context_menu_rows_export_accesskit_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_page_context_rows_frame(&ctx);
        let nodes = accesskit_nodes(&out);
        let find = |label: &str| {
            nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label() == Some(label))
                .unwrap_or_else(|| {
                    panic!("missing page context AccessKit row {label:?}: {nodes:?}")
                })
        };

        let back = find("Back");
        assert_eq!(back.role(), egui::accesskit::Role::Button);
        assert_eq!(
            back.value(),
            Some("Unavailable: Unavailable in the current page context")
        );
        assert!(
            !back.supports_action(egui::accesskit::Action::Click),
            "disabled page context rows must not expose a click action"
        );

        let forward = find("Forward");
        assert_eq!(forward.role(), egui::accesskit::Role::Button);
        assert_eq!(forward.value(), Some("Available"));
        assert!(
            forward.supports_action(egui::accesskit::Action::Click),
            "enabled page context rows should expose the same click action as the painted row"
        );
    }

    #[test]
    fn tab_context_menu_rows_use_browser_material_icons_and_text() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_tab_context_rows_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Move tab left", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Move tab right", CHROME_TEXT);
        assert_painted_text_color(&texts, "Pin tab", CHROME_TEXT);
        assert_painted_text_color(&texts, "Duplicate tab", CHROME_TEXT);
        assert_painted_text_color(&texts, "Work container", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Display 2", CHROME_TEXT);
        assert_painted_text_color(&texts, "Enable site fixups", CHROME_TEXT);
        assert_painted_text_color(&texts, "Close tab", CHROME_TEXT);
        for label in [
            "Move tab left",
            "Move tab right",
            "Pin tab",
            "Duplicate tab",
            "Work container",
            "Display 2",
            "Enable site fixups",
            "Close tab",
        ] {
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "tab context label {label:?} leaked shared shell text color: {texts:?}"
            );
        }

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_TOOLBAR),
            "tab context rows must paint Browser toolbar fill: {fills:?}"
        );
        let outlines = painted_rect_strokes(&out.shapes);
        assert!(
            outlines
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "tab context rows must paint Browser outline strokes: {outlines:?}"
        );
        let lines = painted_line_strokes(&out.shapes);
        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_ICON),
            "enabled tab context rows must paint Browser icon marks: lines={lines:?} image_meshes={}",
            painted_image_mesh_count(&out.shapes)
        );
        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_TEXT_DIM),
            "disabled tab context rows must paint dim Browser icon marks: lines={lines:?} image_meshes={}",
            painted_image_mesh_count(&out.shapes)
        );
    }

    #[test]
    fn page_actions_menu_rows_use_browser_painted_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_actions_menu_frame(&ctx, false);
        let texts = painted_text(&out.shapes);

        for label in [
            "Add bookmark",
            "Copy URL",
            "Send in Chat",
            "Share to Peer",
            "Share to Phone",
            "Share to Email",
            "Share to QR",
            "Send tab to Node",
            "Send tab to Phone",
        ] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
        }
        for legacy in ['\u{2606}', '\u{29C9}', '\u{1F4AC}', '\u{21AA}', '\u{21E5}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "page action rows must not paint legacy glyph prefixes as text: {texts:?}"
            );
        }

        assert_browser_icon_painted(&out.shapes, CHROME_ICON, "page action rows");
    }

    #[test]
    fn page_actions_menu_marks_existing_bookmarks_without_duplicate_add() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_actions_menu_frame(&ctx, true);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Bookmarked", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, _)| text == "Add bookmark"),
            "a page already present in the system bookmark manager must not offer a duplicate add row: {texts:?}"
        );
        assert_painted_text_color(&texts, "Copy URL", CHROME_TEXT);
        assert_painted_text_color(&texts, "Send in Chat", CHROME_TEXT);
    }

    #[test]
    fn browser_popup_width_uses_clip_when_context_menu_starts_collapsed() {
        assert_eq!(
            chrome_popup_width_from_bounds(0.0, 320.0, PAGE_ACTIONS_MENU_W),
            PAGE_ACTIONS_MENU_W
        );
        assert_eq!(
            chrome_popup_width_from_bounds(112.0, 320.0, PAGE_ACTIONS_MENU_W),
            112.0
        );
        assert_eq!(
            chrome_popup_width_from_bounds(0.0, 0.0, PAGE_ACTIONS_MENU_W),
            PAGE_ACTIONS_MENU_W
        );
    }

    #[test]
    fn page_actions_menu_self_frames_collapsed_context_width() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);
        let out = render_collapsed_page_actions_menu_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Add bookmark", CHROME_TEXT);
        assert_painted_text_color(&texts, "Copy URL", CHROME_TEXT);
        let nodes = accesskit_nodes(&out);
        let add_row = nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Add bookmark")
                    && node.bounds().is_some())
                .then(|| accesskit_bounds_rect(node))
            })
            .expect("collapsed context path exposes Add bookmark row");
        assert!(
            add_row.width() >= PAGE_ACTIONS_MENU_W - 1.0,
            "page-actions menu entry point must not collapse into a thin wedge: {add_row:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "page-actions menu should self-paint a Browser popup surface: {fills:?}"
        );
    }

    #[test]
    fn browser_toolbar_popups_stay_inside_right_edge() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let page_actions = render_open_page_actions_popup_frame(&ctx);
        assert_rects_inside_viewport(&page_actions, 320.0, "page actions toolbar popup");

        let bookmarks = render_open_bookmark_overflow_popup_frame(&ctx);
        assert_rects_inside_viewport(&bookmarks, 320.0, "bookmark overflow toolbar popup");

        let security = render_open_security_popup_frame(&ctx);
        assert_rects_inside_viewport(&security, 340.0, "security chip toolbar popup");

        let passwords = render_open_password_menu_popup_frame(&ctx);
        assert_rects_inside_viewport(&passwords, 340.0, "password toolbar popup");
    }

    #[test]
    fn page_actions_toolbar_anchor_uses_painted_bookmark_icon() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_actions_button_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "page actions toolbar anchor");

        for legacy in ['\u{2605}', '\u{2606}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "page actions toolbar anchor must not paint legacy star glyph text: {texts:?}"
            );
        }

        let image_meshes = painted_image_mesh_count(&out.shapes);
        if image_meshes > 0 {
            assert!(
                image_meshes >= 3,
                "page actions toolbar anchor must paint disabled, available, and bookmarked YAMIS icons: image_meshes={image_meshes}"
            );
        } else {
            let path_strokes = painted_path_strokes(&out.shapes);
            for color in [CHROME_TEXT_DIM, CHROME_TEXT, CHROME_PRIMARY] {
                assert!(
                    path_strokes
                        .iter()
                        .any(|stroke| stroke.color == color && (stroke.width - 1.7).abs() < 0.01),
                    "page actions toolbar anchor must paint bookmark icon color {color:?}: {path_strokes:?}"
                );
            }
        }
    }

    #[test]
    fn page_actions_toolbar_popup_keeps_full_browser_menu_width() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_open_page_actions_popup_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Add bookmark", CHROME_TEXT);
        assert_painted_text_color(&texts, "Copy URL", CHROME_TEXT);
        let nodes = accesskit_nodes(&out);
        let add_row = nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Add bookmark")
                    && node.bounds().is_some())
                .then(|| accesskit_bounds_rect(node))
            })
            .expect("open page-actions popup exposes Add bookmark row");
        assert!(
            add_row.width() >= PAGE_ACTIONS_MENU_W - 1.0,
            "page-actions popup row must not collapse into a thin wedge: {add_row:?}"
        );
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.iter().any(|fill| *fill == CHROME_SURFACE),
            "page-actions popup should paint a Browser surface behind rows: {fills:?}"
        );
    }

    #[test]
    fn bookmark_overflow_toolbar_popup_keeps_full_browser_menu_width() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_open_bookmark_overflow_popup_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Bookmark 2", CHROME_TEXT);
        let nodes = accesskit_nodes(&out);
        let bookmark_row = nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::accesskit::Role::Button
                    && node.label() == Some("Open bookmark Bookmark 2")
                    && node.bounds().is_some())
                .then(|| accesskit_bounds_rect(node))
            })
            .expect("open bookmarks overflow popup exposes a bookmark row");
        assert!(
            bookmark_row.width() >= BOOKMARK_OVERFLOW_MENU_W - 24.0,
            "bookmarks overflow popup row must not collapse into a thin wedge: {bookmark_row:?}"
        );
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.iter().any(|fill| *fill == CHROME_SURFACE),
            "bookmarks overflow popup should paint a Browser popup surface: {fills:?}"
        );
    }

    #[test]
    fn browser_body_interstitials_use_painted_warning_icons_not_glyph_text() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let assert_warning_icon = |out: &egui::FullOutput| {
            let lines = painted_line_strokes(&out.shapes);
            assert!(
                lines
                    .iter()
                    .any(|stroke| stroke.color == CHROME_ERROR && (stroke.width - 1.7).abs() < 0.01),
                "body interstitial warning icon must paint Browser error line strokes: {lines:?}"
            );

            let paths = painted_path_strokes(&out.shapes);
            assert!(
                paths
                    .iter()
                    .any(|stroke| stroke.color == CHROME_ERROR && (stroke.width - 1.7).abs() < 0.01),
                "body interstitial warning icon must paint Browser error path strokes: {paths:?}"
            );
        };

        let assert_no_legacy_glyphs = |texts: &[(String, egui::Color32)]| {
            for legacy in ['\u{2190}', '\u{21BB}', '\u{26A0}', '\u{26D4}'] {
                assert!(
                    !texts.iter().any(|(text, _)| text.contains(legacy)),
                    "body interstitials must not paint legacy glyph prefixes as text: {texts:?}"
                );
            }
        };

        let err = CertError {
            url: "https://bad.example.com/".to_owned(),
            code: -202,
            message: "not trusted".to_owned(),
        };
        let out = render_body_frame(&ctx, |ui| {
            cert_error_body(ui, &err, false);
        });
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Your connection is not private", CHROME_ERROR);
        assert_painted_text_color(&texts, "Back to safety", CHROME_TOOLBAR);
        assert_no_legacy_glyphs(&texts);
        assert_warning_icon(&out);

        let mut respawn_requested = false;
        let out = render_body_frame(&ctx, |ui| {
            crashed_body(ui, "renderer exited".to_owned(), &mut respawn_requested);
        });
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "This page crashed", CHROME_ERROR);
        assert_painted_text_color(&texts, "Reload", CHROME_TOOLBAR);
        assert_no_legacy_glyphs(&texts);
        assert_warning_icon(&out);

        let out = render_body_frame(&ctx, |ui| {
            safe_browsing_interstitial_body(ui, "https://blocked.example/");
        });
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Unsafe site blocked", CHROME_ERROR);
        assert_painted_text_color(&texts, "Back to safety", CHROME_TOOLBAR);
        assert_no_legacy_glyphs(&texts);
        assert_warning_icon(&out);

        let block = ManagedPolicyBlock {
            url: "https://policy.example/".to_owned(),
            rule: "blocked-host".to_owned(),
        };
        let out = render_body_frame(&ctx, |ui| {
            managed_policy_interstitial_body(ui, &block);
        });
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Blocked by policy", CHROME_ERROR);
        assert_painted_text_color(&texts, "Back to safety", CHROME_TOOLBAR);
        assert_no_legacy_glyphs(&texts);
        assert_warning_icon(&out);
    }

    #[test]
    fn browser_security_chip_and_panel_use_painted_icons() {
        assert_eq!(SecurityLevel::Secure.icon(), ChromeIcon::Lock);
        assert_eq!(SecurityLevel::NotSecure.icon(), ChromeIcon::Warning);
        assert_eq!(SecurityLevel::Mesh.icon(), ChromeIcon::Security);
        assert_eq!(SecurityLevel::Neutral.icon(), ChromeIcon::Page);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_security_chrome_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "security chip");

        assert_painted_text_color(&texts, "Connection is secure", CHROME_TEXT_DIM);
        assert_painted_text_color(
            &texts,
            "Your connection to this site is not secure",
            CHROME_WARN,
        );
        assert_painted_text_color(&texts, "Mesh service: trusted overlay", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "About this page", CHROME_TEXT_DIM);
        assert_painted_text_color(
            &texts,
            "Punycode/IDN address (xn--): verify this is the site you expect",
            CHROME_WARN,
        );
        for label in [
            "Unsafe sites: malware.test",
            "Blocked content sites: cdn.example.test",
            "Blocked tracker sites: tracker.example.test",
        ] {
            assert_painted_text_color(&texts, label, CHROME_TEXT_DIM);
        }
        assert!(
            !texts.iter().any(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                text.contains('\u{2014}') || text.contains('\u{2192}') || lower.contains("host")
            }),
            "security chip and panel must not paint host wording or typographic dash/arrow glyph copy: {texts:?}"
        );
        for label in [
            "Managed policy blocked: 1 resource",
            "Unsafe content blocked: 1 resource",
            "Insecure content blocked: 1 public HTTP subresource",
        ] {
            assert_painted_text_color(&texts, label, CHROME_WARN);
        }
        assert_painted_text_color(
            &texts,
            "Privacy protection blocked: 1 tracker/filter resource",
            CHROME_TEXT_DIM,
        );
        for legacy in ['\u{1F512}', '\u{26A0}', '\u{1F6E1}', '\u{1F50E}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "security chip and panel must not paint legacy security emoji as text: {texts:?}"
            );
        }

        let line_strokes = painted_line_strokes(&out.shapes);
        assert!(
            line_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01),
            "secure/neutral security icons must paint Browser dim line strokes: {line_strokes:?}"
        );
        assert!(
            line_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "not-secure security icon must paint Browser warning line strokes: {line_strokes:?}"
        );

        let path_strokes = painted_path_strokes(&out.shapes);
        assert!(
            path_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "not-secure security icon must paint a Browser warning path stroke: {path_strokes:?}"
        );
        assert!(
            path_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01),
            "mesh security icon must paint a Browser primary shield path: {path_strokes:?}"
        );
    }

    #[test]
    fn security_chip_toolbar_popup_keeps_full_browser_site_info_width() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_open_security_popup_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Connection is secure", CHROME_TEXT_DIM);
        assert_painted_text_color(
            &texts,
            "Certificate: valid; the connection is encrypted",
            CHROME_TEXT_DIM,
        );

        let popup_surfaces = painted_rects(&out.shapes)
            .into_iter()
            .filter(|(fill, stroke, _)| {
                *fill == CHROME_SURFACE
                    && stroke.color == CHROME_OUTLINE
                    && (stroke.width - 1.0).abs() < 0.01
            })
            .collect::<Vec<_>>();
        assert!(
            popup_surfaces
                .iter()
                .any(|(_, _, rect)| rect.width() >= SITE_INFO_POPUP_W - 1.0),
            "site-info popup should reserve the Chrome panel width instead of rendering as a thin wedge: {popup_surfaces:?}"
        );
    }

    #[test]
    fn omnibox_security_button_defers_resource_snapshot_until_popup_is_open() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let snapshot_calls = std::cell::Cell::new(0usize);

        let render = |open: bool, time: f64| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(340.0, 260.0),
                    )),
                    time: Some(time),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        scope(ui, |ui| {
                            if open {
                                ui.memory_mut(|mem| mem.open_popup(security_chip_popup_id()));
                            }
                            omnibox_security_button(
                                ui,
                                "https://example.test/",
                                || {
                                    snapshot_calls.set(snapshot_calls.get().saturating_add(1));
                                    Vec::new()
                                },
                                None,
                            );
                        });
                    });
                },
            )
        };

        let _ = render(false, 0.0);
        assert_eq!(
            snapshot_calls.get(),
            0,
            "closed omnibox security popup must not clone resource history"
        );

        let _ = render(true, 0.016);
        assert_eq!(
            snapshot_calls.get(),
            1,
            "open omnibox security popup should snapshot resources exactly for the panel"
        );
    }

    #[test]
    fn browser_chrome_transient_surfaces_use_refined_margins() {
        assert_eq!(
            chrome_options_card_margin(),
            egui::Margin::symmetric(6, 4),
            "Browser Options category/command cards should not carry thick vertical chrome"
        );
        assert_eq!(
            dashboard_search_margin(),
            egui::Margin::symmetric(12, 4),
            "new-tab dashboard search keeps its wide pill shape with a refined vertical inset"
        );
        assert_eq!(
            chrome_prompt_margin(),
            egui::Margin::symmetric(6, 4),
            "Browser permission/passkey prompt bars should share the compact chrome inset"
        );
    }

    #[test]
    fn ad_filter_chip_uses_browser_icon_and_count_rows() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_ad_filter_chrome_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "7", CHROME_ON_PRIMARY_CONTAINER);
        assert_painted_text_color(&texts, "3", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "ads.example", CHROME_TEXT_DIM);
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_PRIMARY_CONTAINER),
            "ad-filter count should use the same fixed toolbar badge surface as downloads"
        );
        for legacy in ['\u{2298}', '\u{00D7}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "ad-filter chip and domain rows must not paint legacy glyph text: {texts:?}"
            );
        }
    }

    #[test]
    fn ad_filter_hover_card_uses_browser_tooltip_surface() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);
        let out = render_ad_filter_hover_card_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(
            &texts,
            "Ad-filter blocked 7 requests on this page",
            CHROME_TEXT,
        );
        assert_painted_text_color(&texts, "3", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "ads.example", CHROME_TEXT_DIM);

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "ad-filter hover card should paint the Browser tooltip surface: {fills:?}"
        );
        assert!(
            !fills
                .iter()
                .any(|color| matches!(*color, Style::BG | Style::SURFACE | Style::SURFACE_HI)),
            "ad-filter hover card leaked shared shell popup fills: {fills:?}"
        );
    }

    #[test]
    fn browser_suggestions_panel_uses_refined_leading_inset() {
        assert_eq!(
            SUGGESTIONS_LEADING_INSET,
            CHROME_BUTTON + CHROME_GAP,
            "suggestions should align to Browser chrome controls, not a page-scale gutter"
        );
        assert!(
            SUGGESTIONS_LEADING_INSET < Style::SP_XL * 2.0,
            "suggestions leading inset should stay compact: {SUGGESTIONS_LEADING_INSET}"
        );

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_suggestions_panel_frame(&ctx);
        let section_rect = painted_text_rects(&out.shapes)
            .into_iter()
            .find_map(|(text, rect)| (text == "Bookmarks").then_some(rect))
            .expect("suggestions panel renders the first section label");
        assert!(
            section_rect.left() < Style::SP_XL * 2.0,
            "first suggestion section should start near the location bar, got {section_rect:?}"
        );
    }

    #[test]
    fn bookmark_suggestions_use_browser_painted_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_suggestions_panel_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Bookmarks", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Example bookmark", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "Files", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "home-notes.md", CHROME_TEXT);
        assert_painted_text_color(&texts, "History", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "https://example.test/history", CHROME_TEXT);
        assert_painted_text_color(&texts, "example search", CHROME_TEXT);
        assert!(
            !texts
                .iter()
                .any(|(text, _)| text.contains('\u{2605}') || text.contains('\u{2606}')),
            "bookmark suggestions must not paint legacy star glyph text: {texts:?}"
        );

        assert_browser_icon_painted(&out.shapes, CHROME_PRIMARY, "bookmark suggestions");
    }

    #[test]
    fn browser_suggestion_hover_uses_browser_tooltip_surface() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);

        let first = render_suggestions_panel_frame(&ctx);
        let bookmark_rect = painted_text_rects(&first.shapes)
            .into_iter()
            .find_map(|(text, rect)| (text == "Example bookmark").then_some(rect))
            .expect("suggestions panel renders the bookmark chip label");
        let _ = render_suggestions_panel_frame_with_input(
            &ctx,
            vec![egui::Event::PointerMoved(bookmark_rect.center())],
            1.0,
        );
        let out = render_suggestions_panel_frame_with_input(&ctx, Vec::new(), 1.6);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(
            &texts,
            "Bookmark: https://example.test/bookmark",
            CHROME_TEXT,
        );
        assert!(
            !texts.iter().any(
                |(text, color)| text == "Bookmark: https://example.test/bookmark"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
            ),
            "suggestion hover leaked shared shell tooltip text color: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "suggestion hover should paint the Browser tooltip surface: {fills:?}"
        );
    }

    #[test]
    fn browser_suggestion_chips_export_accesskit_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_suggestions_panel_frame(&ctx);
        let nodes = accesskit_nodes(&out);
        let find = |label: &str| {
            nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing suggestion AccessKit node {label:?}"))
        };

        let bookmark = find("Open bookmark Example bookmark");
        assert_eq!(bookmark.role(), egui::accesskit::Role::Button);
        assert_eq!(
            bookmark.value(),
            Some("Suggestion 1 of 4: Bookmark, https://example.test/bookmark")
        );
        assert_eq!(bookmark.is_selected(), Some(true));
        assert!(bookmark.supports_action(egui::accesskit::Action::Click));

        assert_eq!(
            find("Open file home-notes.md").value(),
            Some("Suggestion 2 of 4: File, /home/mm/home-notes.md")
        );
        assert_eq!(
            find("Open history entry https://example.test/history").value(),
            Some("Suggestion 3 of 4: History, https://example.test/history")
        );
        assert_eq!(
            find("Search for example search").value(),
            Some("Suggestion 4 of 4: Search, example search")
        );
    }

    #[test]
    fn tab_search_results_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_tab_search_results_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Browser Options", CHROME_ON_PRIMARY_CONTAINER);
        assert_painted_text_color(&texts, "No matching tabs", CHROME_TEXT_DIM);
        for label in ["Browser Options", "No matching tabs"] {
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "tab-search label {label:?} leaked shared shell text color: {texts:?}"
            );
        }
    }

    #[test]
    fn tab_search_rows_clip_to_narrow_browser_chrome_width() {
        assert_eq!(tab_search_panel_width(0.0), 0.0);
        assert_eq!(tab_search_panel_width(180.0), 180.0);
        assert_eq!(tab_search_panel_width(240.0), 240.0);
        assert_eq!(tab_search_panel_width(500.0), TAB_SEARCH_PANEL_W);
        assert_eq!(tab_search_popup_content_width(240.0), 228.0);
        assert_eq!(tab_search_row_width(188.0), 188.0);
        let clear_threshold = CHROME_BUTTON + TAB_SEARCH_EDIT_MIN_W;
        assert!(!tab_search_clear_visible(true, clear_threshold - 1.0));
        assert!(tab_search_clear_visible(true, clear_threshold));
        assert_eq!(
            tab_search_edit_width(clear_threshold - 1.0, false),
            clear_threshold - 1.0
        );
        assert_eq!(
            tab_search_edit_width(clear_threshold, true),
            TAB_SEARCH_EDIT_MIN_W
        );

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let mut state = WebState::default();
        state.open_options_tab();
        state.tab_search_query = "options".to_owned();
        let out = drive_tab_search_menu_contents_frame_with_size(
            &ctx,
            &mut state,
            Vec::new(),
            egui::vec2(240.0, 320.0),
        );
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "options", CHROME_TEXT);
        assert_painted_text_color(&texts, "Browser Options", CHROME_ON_PRIMARY_CONTAINER);
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "tab-search popup should paint the shared Browser popup surface: {fills:?}"
        );
        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "tab-search popup should paint the shared Browser popup outline: {strokes:?}"
        );
        assert_rects_inside_viewport(&out, 240.0, "narrow tab-search menu");
    }

    #[test]
    fn tab_search_results_export_accesskit_buttons_for_switching_tabs() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_tab_search_results_frame(&ctx);
        let nodes = accesskit_nodes(&out);
        let row = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Switch to tab Browser Options"))
            .expect("tab-search result row should expose a named AccessKit node");

        assert_eq!(row.role(), egui::accesskit::Role::Button);
        assert_eq!(row.value(), Some("Tab 1 of 1, active"));
        assert_eq!(row.is_selected(), Some(true));
        assert!(
            row.supports_action(egui::accesskit::Action::Click),
            "tab-search result row must expose the same click action as the painted row"
        );
    }

    #[test]
    fn tab_search_toolbar_anchor_uses_browser_icon_button() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_tab_search_menu_button_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert!(
            texts.iter().all(|(text, _)| !text.trim().is_empty()),
            "tab-search toolbar anchor must not paint a blank stock menu-button text label: {texts:?}"
        );

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_ICON),
            "tab-search toolbar anchor must paint the Browser search icon"
        );
    }

    #[test]
    fn browser_chrome_text_fields_use_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let tab_search = render_tab_search_menu_contents_frame(&ctx);
        assert_browser_text_field_paint(&tab_search, "options", "tab search");

        let dashboard = render_new_tab_dashboard_frame(&ctx);
        assert_browser_text_field_paint(&dashboard, "mesh docs", "dashboard search");

        let password = render_password_menu_contents_frame(&ctx, "example.test", false);
        assert_browser_text_field_paint(&password, "operator", "password menu");
    }

    #[test]
    fn tab_search_field_exposes_icon_clear_button() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let mut state = WebState::default();
        state.open_options_tab();
        state.tab_search_query = "options".to_owned();

        let out = drive_tab_search_menu_contents_frame(&ctx, &mut state, Vec::new());
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "options", CHROME_TEXT);
        let clear_rect = ctx
            .read_response(tab_search_clear_button_id())
            .expect("live tab search query exposes a clear icon button")
            .rect;
        let active_before = state.active;

        drive_tab_search_menu_contents_frame(
            &ctx,
            &mut state,
            vec![
                egui::Event::PointerMoved(clear_rect.center()),
                pointer_button(clear_rect.center(), true),
            ],
        );
        drive_tab_search_menu_contents_frame(
            &ctx,
            &mut state,
            vec![pointer_button(clear_rect.center(), false)],
        );

        assert!(
            state.tab_search_query.is_empty(),
            "clicking the tab-search clear icon clears the query"
        );
        assert_eq!(
            state.active, active_before,
            "clearing tab search must not select a different tab"
        );
    }

    #[test]
    fn browser_new_tab_dashboard_uses_bing_style_search_language_and_centering() {
        assert_eq!(browser_dashboard_title(), "Search the web");

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_new_tab_dashboard_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Search the web", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, _)| text == "Construct Browser"),
            "new-tab dashboard should not lead with the old product-title search line: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|(text, color)| text == "Search" && *color == CHROME_TOOLBAR),
            "new-tab dashboard search submission should be an icon button, not a text button: {texts:?}"
        );
        let rects = painted_text_rects(&out.shapes);
        let title = rects
            .iter()
            .find_map(|(text, rect)| (text == "Search the web").then_some(*rect))
            .expect("dashboard title text was painted");
        assert!(
            (title.center().x - 360.0).abs() < 80.0,
            "dashboard title should be centered in the 720px test frame: {title:?}"
        );
        let query = rects
            .iter()
            .find_map(|(text, rect)| (text == "mesh docs").then_some(*rect))
            .expect("dashboard query text was painted");
        assert!(
            (query.center().x - 360.0).abs() < 160.0,
            "dashboard search text should remain visually centered in the 720px test frame: {query:?}"
        );
    }

    #[test]
    fn browser_new_tab_quick_links_render_as_bounded_chrome_tiles() {
        assert_eq!(dashboard_tile_width(96.0), 96.0);
        assert!(dashboard_tile_width(360.0) <= DASHBOARD_TILE_MAX_W);
        assert!(dashboard_tile_width(720.0) <= DASHBOARD_TILE_MAX_W);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_new_tab_dashboard_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Search", CHROME_TEXT);
        assert_painted_text_color(&texts, "Music", CHROME_TEXT);
        assert_painted_text_color(&texts, "Docs", CHROME_TEXT);
        assert_painted_text_color(&texts, "Status", CHROME_TEXT);
        assert_painted_text_color(&texts, "docs.mesh", CHROME_TEXT_DIM);

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE_CONTAINER),
            "new-tab dashboard should paint a centered light Browser backdrop band: {fills:?}"
        );
        assert!(
            fills.contains(&CHROME_PRIMARY),
            "new-tab dashboard should paint a primary icon submit control: {fills:?}"
        );
        assert!(
            fills.contains(&CHROME_TOOLBAR),
            "quick-link tiles should paint Chrome toolbar cards: {fills:?}"
        );
        assert!(
            fills.contains(&CHROME_PRIMARY_CONTAINER),
            "quick-link tiles should paint Chrome icon badges: {fills:?}"
        );
        for label in ["Music", "Docs", "Status"] {
            assert!(
                !texts.iter().any(|(text, color)| {
                    text == label
                        && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
                }),
                "new-tab quick-link `{label}` leaked shared shell text color: {texts:?}"
            );
        }
    }

    #[test]
    fn browser_new_tab_privacy_note_uses_painted_lock_icon() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_new_tab_dashboard_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, PRIVATE_MODE_EXPLAINER, CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, _)| text.contains('\u{1F512}')),
            "new-tab privacy note must not paint the legacy lock emoji as text: {texts:?}"
        );

        let rect_strokes = painted_rect_strokes(&out.shapes);
        assert!(
            rect_strokes.iter().any(|stroke| {
                stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01
            }),
            "new-tab privacy note must paint a Browser lock body stroke: {rect_strokes:?}"
        );
        let line_strokes = painted_line_strokes(&out.shapes);
        assert!(
            line_strokes.iter().any(|stroke| {
                stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01
            }),
            "new-tab privacy note must paint Browser lock shackle strokes: {line_strokes:?}"
        );
    }

    #[test]
    fn browser_drawer_actions_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_history_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "History", CHROME_TEXT);
        assert_painted_text_color(&texts, "this session only", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Clear", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Example Page", CHROME_TEXT);
        for label in ["History", "this session only", "Clear", "Example Page"] {
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "drawer label {label:?} leaked shared shell text color: {texts:?}"
            );
        }
    }

    #[test]
    fn browser_qr_share_drawer_uses_browser_material_matrix_colors() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_qr_share_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "QR share", CHROME_TEXT);
        assert_painted_text_color(&texts, "Ready", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Example QR", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "QR code 5x5", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, color)| text == "QR share"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "QR share drawer heading leaked shared shell text color: {texts:?}"
        );
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("01hqr") && !lower.contains("phone") && !lower.contains("module")
            }),
            "QR share drawer must not expose routing or matrix internals: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert_eq!(drawers::QR_MATRIX_LIGHT, CHROME_TOOLBAR);
        assert_eq!(drawers::QR_MATRIX_DARK, CHROME_TEXT);
        assert!(
            fills.contains(&drawers::QR_MATRIX_LIGHT),
            "QR share matrix must paint its quiet zone with Browser toolbar color: {fills:?}"
        );
        assert!(
            fills.contains(&drawers::QR_MATRIX_DARK),
            "QR share matrix dark modules must paint with Browser text color: {fills:?}"
        );
        assert!(
            !fills.contains(&egui::Color32::BLACK),
            "QR share matrix must not paint raw black fills inside Browser chrome: {fills:?}"
        );
    }

    #[test]
    fn browser_qr_share_drawer_matrix_clamps_to_narrow_drawer_width() {
        assert_eq!(drawers::qr_matrix_side(0.0), 0.0);
        assert_eq!(drawers::qr_matrix_side(72.0), 72.0);
        assert_eq!(drawers::qr_matrix_side(220.0), 168.0);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_qr_share_drawer_frame_with_size(&ctx, egui::vec2(112.0, 260.0));

        assert_rects_inside_viewport(&out, 112.0, "narrow QR share drawer");
        assert_raw_drawer_rects_inside_viewport(&out, 112.0, "narrow QR share drawer");
    }

    #[test]
    fn browser_translation_drawer_uses_user_facing_metadata() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_translation_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Translation", CHROME_TEXT);
        assert_painted_text_color(&texts, "en to fr", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Example Translation", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Text 13 chars", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Bonjour monde", CHROME_TEXT);
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("translation-worker")
                    && !lower.contains("tab ")
                    && !lower.contains("cef")
                    && !lower.contains("servo")
                    && !lower.contains("chars from")
            }),
            "translation drawer must not expose routing/session metadata: {texts:?}"
        );
    }

    #[test]
    fn browser_offline_copy_drawer_uses_user_facing_archive_copy() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_offline_cache_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Offline copy", CHROME_TEXT);
        assert_painted_text_color(&texts, "Ready", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Archive", CHROME_TEXT);
        assert_painted_text_color(&texts, "Text 15 chars", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Saved now", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Preview 320x180", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Web archive 4096 bytes", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Resources 1, blocked 1", CHROME_TEXT_DIM);
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("mhtml")
                    && !lower.contains("01harchivecopy")
                    && !lower.contains("cached ")
                    && !lower.contains("tab ")
                    && !lower.contains("cef")
                    && !lower.contains("servo")
                    && !lower.contains("png")
            }),
            "offline copy drawer must not expose implementation metadata: {texts:?}"
        );
    }

    #[test]
    fn browser_cached_offline_body_uses_user_facing_metadata() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let result = offline_cache_result_fixture();
        let out = render_body_frame(&ctx, |ui| {
            cached_offline_body(ui, &result, None);
        });
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Offline copy", CHROME_TEXT);
        assert_painted_text_color(&texts, "Ready", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Example archive", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Text 15 chars", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Saved now", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Preview 320x180", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Saved page text", CHROME_TEXT);
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("mhtml")
                    && !lower.contains("01harchivecopy")
                    && !lower.contains("cached ")
                    && !lower.contains("tab ")
                    && !lower.contains("cef")
                    && !lower.contains("servo")
                    && !lower.contains("png")
            }),
            "cached offline body must not expose implementation metadata: {texts:?}"
        );
    }

    #[test]
    fn browser_history_drawer_rows_use_browser_material_icon_rows() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_history_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Example Page", CHROME_TEXT);

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_TOOLBAR),
            "history visit rows must use the Browser Material row fill: {fills:?}"
        );

        let rect_strokes = painted_rect_strokes(&out.shapes);
        assert!(
            rect_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "history visit rows must paint Browser outline strokes: {rect_strokes:?}"
        );

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_ICON),
            "history visit rows must paint a Browser History icon: lines={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
    }

    #[test]
    fn browser_history_rows_export_accesskit_buttons() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_history_drawer_frame(&ctx);
        let nodes = accesskit_nodes(&out);
        let row = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Open history entry Example Page"))
            .unwrap_or_else(|| panic!("missing history row AccessKit node: {nodes:?}"));

        assert_eq!(row.role(), egui::accesskit::Role::Button);
        assert_eq!(row.value(), Some("https://example.test/"));
        assert!(
            row.supports_action(egui::accesskit::Action::Click),
            "history rows should expose the same click action as the painted row"
        );
    }

    #[test]
    fn browser_drawer_close_buttons_use_painted_close_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for (name, out) in [
            ("history", render_history_drawer_frame(&ctx)),
            ("downloads", render_empty_downloads_drawer_frame(&ctx)),
            ("print", render_print_settings_drawer_frame(&ctx)),
            ("site-styles", render_site_styles_drawer_frame(&ctx)),
        ] {
            let texts = painted_text(&out.shapes);
            assert!(
                texts.iter().all(|(text, _)| !text.trim().is_empty()),
                "{name} drawer icon controls must not paint blank stock button labels: {texts:?}"
            );
            assert!(
                !texts.iter().any(|(text, _)| text.contains('\u{00D7}')),
                "{name} drawer close button must not paint the legacy multiplication glyph as text: {texts:?}"
            );

            assert!(
                has_browser_icon_paint(&out.shapes, CHROME_TEXT_DIM),
                "{name} drawer close button must paint a Browser dim close icon: lines={:?} image_meshes={}",
                painted_line_strokes(&out.shapes),
                painted_image_mesh_count(&out.shapes)
            );
        }
    }

    #[test]
    fn browser_drawer_refresh_and_danger_warning_use_painted_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for (name, out) in [
            ("downloads", render_empty_downloads_drawer_frame(&ctx)),
            ("print", render_print_settings_drawer_frame(&ctx)),
        ] {
            let texts = painted_text(&out.shapes);
            assert!(
                !texts.iter().any(|(text, _)| text.contains('\u{21BB}')),
                "{name} refresh control must not paint the legacy reload glyph as text: {texts:?}"
            );

            assert!(
                has_browser_icon_paint(&out.shapes, CHROME_TEXT_DIM),
                "{name} drawer must paint Browser close and reload icons: lines={:?} image_meshes={}",
                painted_line_strokes(&out.shapes),
                painted_image_mesh_count(&out.shapes)
            );
        }

        let out = render_dangerous_downloads_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(
            &texts,
            "This type of file can harm your device",
            CHROME_WARN,
        );
        assert_painted_text_color(&texts, "setup.exe", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, _)| text.contains('\u{26A0}')),
            "dangerous download warning must not paint the legacy warning glyph as text: {texts:?}"
        );

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_WARN),
            "dangerous download warning must paint a Browser warning icon: lines={:?} paths={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_path_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
    }

    #[test]
    fn browser_drawer_error_notices_use_warning_icons_not_bang_prefixes() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for (name, out, labels) in [
            (
                "print",
                render_print_settings_drawer_frame(&ctx),
                &["Printer service unavailable"][..],
            ),
            (
                "download-notice",
                render_empty_downloads_drawer_frame(&ctx),
                &["Open failed: file vanished"][..],
            ),
            (
                "download-error",
                render_failed_downloads_drawer_frame(&ctx),
                &["checksum mismatch"][..],
            ),
        ] {
            let texts = painted_text(&out.shapes);
            for label in labels {
                assert_painted_text_color(&texts, label, CHROME_ERROR);
            }
            assert!(
                !texts
                    .iter()
                    .any(|(text, _)| text.trim_start().starts_with("! ")),
                "{name} drawer must not paint exclamation-prefixed warning text: {texts:?}"
            );

            assert!(
                has_browser_icon_paint(&out.shapes, CHROME_ERROR),
                "{name} drawer must paint a Browser error warning icon: lines={:?} paths={:?} image_meshes={}",
                painted_line_strokes(&out.shapes),
                painted_path_strokes(&out.shapes),
                painted_image_mesh_count(&out.shapes)
            );
        }
    }

    #[test]
    fn browser_status_drawers_use_material_icon_status_rows() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_engine_and_speech_status_drawers_frame(&ctx);
        let texts = painted_text(&out.shapes);

        for label in ["Browser engine update", "Browser speech"] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "status drawer heading {label:?} leaked shared shell text color: {texts:?}"
            );
        }
        assert_painted_text_color(&texts, "Update needed", CHROME_WARN);
        assert_painted_text_color(&texts, "Update failed", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Target Chromium 149.0.7827.201", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Installed Chromium old", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Stable channel", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Reading aloud", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "Voice unavailable", CHROME_WARN);
        assert_painted_text_color(&texts, "Example", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "https://example.test/", CHROME_TEXT_DIM);
        assert_painted_text_color(
            &texts,
            "Installed Chromium files do not match this build",
            CHROME_WARN,
        );
        assert_painted_text_color(&texts, "Installer unavailable", CHROME_WARN);
        assert_painted_text_color(&texts, "Voice input is not configured", CHROME_WARN);
        for forbidden in [
            "CEF",
            "TTS",
            "STT",
            "runtime",
            "/opt/mde/cef",
            "packaged manifest",
            "updater failed",
            "mismatch",
        ] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(forbidden)),
                "status drawer leaked raw engine update copy {forbidden:?}: {texts:?}"
            );
        }

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_TEXT),
            "status drawers must paint Browser heading icons: lines={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_PRIMARY),
            "speech drawer must paint Browser primary audio status icons: lines={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_WARN),
            "status drawer warning state and detail rows must paint Browser warning icons: lines={:?} paths={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_path_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
    }

    #[test]
    fn browser_spellcheck_error_uses_material_warning_status_row() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_spellcheck_error_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Spelling", CHROME_TEXT);
        assert_painted_text_color(&texts, "Spelling dictionary is not installed", CHROME_WARN);
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("hunspell")
                    && !lower.contains("runtime")
                    && !lower.contains("backend")
                    && !lower.contains("worker")
            }),
            "spellcheck drawer leaked backend copy: {texts:?}"
        );
        assert!(
            !texts.iter().any(
                |(text, color)| text == "Spelling dictionary is not installed"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
            ),
            "spellcheck error leaked shared shell text color: {texts:?}"
        );

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "spellcheck error must paint a Browser warning line icon: {lines:?}"
        );

        let paths = painted_path_strokes(&out.shapes);
        assert!(
            paths
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "spellcheck error must paint a Browser warning path icon: {paths:?}"
        );
    }

    #[test]
    fn browser_download_progress_uses_browser_material_progress_bar() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_progress_downloads_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "42%", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, color)| text == "42%"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "download progress text must not inherit shared shell text colors: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_PRIMARY),
            "download progress fill must use Browser primary color: {fills:?}"
        );
        assert!(
            fills.contains(&CHROME_SURFACE),
            "download progress track must use Browser surface color: {fills:?}"
        );
    }

    #[test]
    fn browser_download_progress_bar_clamps_to_narrow_drawer_width() {
        assert_eq!(drawers::drawer_progress_width(0.0), 0.0);
        assert_eq!(drawers::drawer_progress_width(88.0), 88.0);
        assert_eq!(drawers::drawer_progress_width(200.0), 120.0);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_progress_downloads_drawer_frame_with_size(&ctx, egui::vec2(112.0, 260.0));

        assert_painted_text_color(&painted_text(&out.shapes), "42%", CHROME_TEXT_DIM);
        assert_rects_inside_viewport(&out, 112.0, "narrow downloads drawer");
        assert_raw_drawer_rects_inside_viewport(&out, 112.0, "narrow downloads drawer");
    }

    #[test]
    fn browser_download_rows_export_accesskit_status() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_progress_downloads_drawer_frame(&ctx);
        let nodes = accesskit_nodes(&out);
        let row = nodes
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some("Download movie.webm"))
            .unwrap_or_else(|| panic!("missing download row AccessKit node: {nodes:?}"));

        assert_eq!(row.role(), egui::accesskit::Role::Row);
        let value = row.value().expect("download row value");
        assert!(value.contains("State running"), "{value}");
        assert!(value.contains("Route /tmp/movie.webm"), "{value}");
        assert!(value.contains("/home/mm/Downloads"), "{value}");
        assert!(value.contains("Progress 42%"), "{value}");
        assert_eq!(row.numeric_value(), Some(42.0));
        assert_eq!(row.min_numeric_value(), Some(0.0));
        assert_eq!(row.max_numeric_value(), Some(100.0));
        assert!(
            !row.supports_action(egui::accesskit::Action::Click),
            "download rows are read-only summaries; command buttons own actions"
        );
    }

    #[test]
    fn browser_download_drawer_header_uses_user_facing_status() {
        assert_eq!(
            drawers::download_drawer_subtitle(false, 0, 0),
            "Downloads unavailable"
        );
        assert_eq!(
            drawers::download_drawer_subtitle(true, 0, 0),
            "No downloads"
        );
        assert_eq!(drawers::download_drawer_subtitle(true, 1, 1), "1 active");
        assert_eq!(
            drawers::download_drawer_subtitle(true, 2, 3),
            "2 active / 3 total"
        );
        assert_eq!(drawers::download_drawer_subtitle(true, 0, 2), "2 complete");

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let progress = render_progress_downloads_drawer_frame(&ctx);
        let texts = painted_text(&progress.shapes);

        assert_painted_text_color(&texts, "Downloads", CHROME_TEXT);
        assert_painted_text_color(&texts, "1 active", CHROME_TEXT_DIM);
        assert!(
            texts
                .iter()
                .all(|(text, _)| !text.contains("browser_download")
                    && !text.contains("ledger")
                    && !text.contains("worker")),
            "download drawer header must not expose internal transfer names: {texts:?}"
        );
    }

    #[test]
    fn browser_download_toolbar_tip_uses_user_facing_status() {
        assert_eq!(downloads_toolbar_tip(0, 0), "Downloads");
        assert_eq!(downloads_toolbar_tip(1, 1), "Downloads: 1 active");
        assert_eq!(downloads_toolbar_tip(2, 3), "Downloads: 2 active / 3 total");
        assert_eq!(downloads_toolbar_tip(0, 1), "Downloads: 1 complete");
        assert_eq!(downloads_toolbar_tip(0, 4), "Downloads: 4 complete");

        for tip in [
            downloads_toolbar_tip(0, 0),
            downloads_toolbar_tip(1, 1),
            downloads_toolbar_tip(2, 3),
            downloads_toolbar_tip(0, 4),
        ] {
            assert!(
                !tip.contains("browser_download")
                    && !tip.contains("ledger")
                    && !tip.contains("helper"),
                "download toolbar tooltip must stay user-facing: {tip:?}"
            );
        }
    }

    #[test]
    fn browser_muted_notes_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_empty_downloads_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);
        let empty_note = texts
            .iter()
            .find(|(text, _)| {
                text == "No browser downloads yet"
                    || text == "Downloads are unavailable on this node"
            })
            .unwrap_or_else(|| panic!("empty downloads drawer note was not painted: {texts:?}"));

        assert!(
            texts.iter().all(|(text, _)| {
                !text.contains("browser_download")
                    && !text.contains("ledger")
                    && !text.contains("worker")
                    && !text.contains("helper")
            }),
            "downloads drawer muted notes must stay user-facing: {texts:?}"
        );
        assert_eq!(
            empty_note.1, CHROME_TEXT_DIM,
            "Browser muted notes must use Browser Material dim text: {texts:?}"
        );
        assert!(
            !matches!(
                empty_note.1,
                Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG
            ),
            "Browser muted notes must not inherit shared shell text colors: {texts:?}"
        );
    }

    #[test]
    fn browser_password_menu_text_uses_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let no_site = painted_text(&render_password_menu_contents_frame(&ctx, "", false).shapes);
        assert_painted_text_color(&no_site, "No site loaded", CHROME_TEXT_DIM);

        let empty =
            painted_text(&render_password_menu_contents_frame(&ctx, "example.test", false).shapes);
        assert_painted_text_color(&empty, "Saved logins (this session)", CHROME_TEXT);
        assert_painted_text_color(&empty, "None saved for example.test", CHROME_TEXT_DIM);
        assert_painted_text_color(&empty, "Save a login for example.test", CHROME_TEXT);
        assert_painted_text_color(&empty, "Save", CHROME_TOOLBAR);

        let saved =
            painted_text(&render_password_menu_contents_frame(&ctx, "example.test", true).shapes);
        assert_painted_text_color(&saved, "Fill alice", CHROME_TOOLBAR);

        for (set_name, texts, labels) in [
            ("no-site", &no_site, &["No site loaded"][..]),
            (
                "empty",
                &empty,
                &[
                    "Saved logins (this session)",
                    "None saved for example.test",
                    "Save a login for example.test",
                    "Save",
                ][..],
            ),
            ("saved", &saved, &["Fill alice"][..]),
        ] {
            for label in labels {
                assert!(
                    !texts.iter().any(|(text, color)| text == *label
                        && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                    "password menu {set_name} label {label:?} leaked shared shell text color: {texts:?}"
                );
            }
        }
    }

    #[test]
    fn browser_password_menu_toolbar_anchor_uses_painted_lock_icon() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_password_menu_button_frame(&ctx, "https://example.test/");
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "password toolbar anchor");
        assert!(
            !texts.iter().any(|(text, _)| text.contains('\u{1F511}')),
            "password toolbar anchor must not paint the legacy key emoji as text: {texts:?}"
        );

        assert_browser_icon_painted(&out.shapes, CHROME_TEXT, "password toolbar anchor");
    }

    #[test]
    fn password_toolbar_popup_keeps_full_browser_menu_width_and_bounds_text() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let out = render_open_password_menu_popup_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Saved logins (this session)", CHROME_TEXT);
        assert!(
            texts.iter().any(|(text, color)| {
                text.starts_with("Fill operator-with-a-long") && *color == CHROME_TOOLBAR
            }),
            "password popup should paint a bounded fill action for the long user: {texts:?}"
        );
        let nodes = accesskit_nodes(&out);
        let fill_row = nodes
            .iter()
            .find_map(|(_, node)| {
                (node.role() == egui::accesskit::Role::Button
                    && node
                        .label()
                        .is_some_and(|label| label.starts_with("Fill operator-with-a-long"))
                    && node.bounds().is_some())
                .then(|| accesskit_bounds_rect(node))
            })
            .expect("open password popup exposes a fill row");
        assert!(
            fill_row.width() >= PASSWORD_FIELD_MIN_W,
            "password popup fill row must not collapse into a thin wedge: {fill_row:?}"
        );
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.iter().any(|fill| *fill == CHROME_SURFACE),
            "password popup should paint a Browser surface behind rows: {fills:?}"
        );
        let text_rects = painted_text_rects(&out.shapes);
        for (text, rect) in text_rects
            .iter()
            .filter(|(text, _)| text.contains("very-long-subdomain") || text.starts_with("Fill "))
        {
            assert!(
                rect.width() <= PASSWORD_MENU_W,
                "password popup text should stay bounded by the menu width: {text:?} {rect:?}"
            );
        }
    }

    #[test]
    fn browser_dialog_prompt_messages_use_material_status_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_dialog_prompt_bars_frame(&ctx);
        let texts = painted_text(&out.shapes);

        for label in [
            "login.example wants to use a passkey on login.example via Chromium",
            "https://camera.example wants to use your camera",
            "Save login for docs.example.com (mm)?",
        ] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "prompt label {label:?} leaked shared shell text color: {texts:?}"
            );
        }
        assert_painted_text_color(
            &texts,
            "docs.example.com wants to leave this page: Unsaved work",
            CHROME_WARN,
        );
        for label in ["Approve", "Allow", "Leave", "Save"] {
            assert_painted_text_color(&texts, label, CHROME_TOOLBAR);
        }
        for label in ["Deny", "Block", "Stay", "Not now"] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
        }

        let prompt_text_icon = CHROME_TEXT.gamma_multiply(0.2);
        let prompt_warn_icon = CHROME_WARN.gamma_multiply(0.2);
        assert!(
            has_browser_icon_paint(&out.shapes, prompt_text_icon),
            "prompt bars must paint Browser lock/status icons as vectors or YAMIS images"
        );
        assert!(
            has_browser_icon_paint(&out.shapes, prompt_warn_icon),
            "before-unload prompt must paint a Browser warning icon as vector or YAMIS image"
        );
    }

    #[test]
    fn browser_insecure_prompt_uses_material_warning_status_row() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_insecure_prompt_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "HTTP connection", CHROME_WARN);
        assert_painted_text_color(&texts, "http://plain.example/sensitive", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Use HTTPS", CHROME_TOOLBAR);
        assert_painted_text_color(&texts, "Continue HTTP", CHROME_TOOLBAR);
        assert_painted_text_color(&texts, "Cancel", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, color)| text == "HTTP connection"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "HTTP prompt heading leaked shared shell text color: {texts:?}"
        );

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_WARN),
            "HTTP prompt must paint a Browser warning icon as a vector or YAMIS image"
        );
    }

    #[test]
    fn browser_capture_notice_uses_material_icon_status_rows() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let success = render_capture_notice_frame(&ctx, "Capture saved");
        let success_texts = painted_text(&success.shapes);
        assert_painted_text_color(&success_texts, "Capture saved", CHROME_PRIMARY);
        assert!(
            !success_texts
                .iter()
                .any(|(text, color)| text == "Capture saved"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "capture notice leaked shared shell text color: {success_texts:?}"
        );
        let success_fills = painted_rect_fills(&success.shapes);
        assert!(
            success_fills.contains(&CHROME_SURFACE_CONTAINER),
            "capture notice must use the Browser status surface: {success_fills:?}"
        );
        let success_rect_strokes = painted_rect_strokes(&success.shapes);
        assert!(
            success_rect_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "capture notice must paint a Browser outline stroke: {success_rect_strokes:?}"
        );
        assert!(
            has_browser_icon_paint(&success.shapes, CHROME_PRIMARY),
            "successful capture notice must paint the Browser Capture icon as a vector or YAMIS image"
        );

        let failed = render_capture_notice_frame(&ctx, "Capture failed: no painted page");
        let failed_texts = painted_text(&failed.shapes);
        assert_painted_text_color(
            &failed_texts,
            "Capture failed: no painted page",
            CHROME_ERROR,
        );
        assert!(
            !failed_texts.iter().any(|(text, _)| text.starts_with("! ")),
            "capture error notice must not paint exclamation-prefixed warning text: {failed_texts:?}"
        );
        assert!(
            has_browser_icon_paint(&failed.shapes, CHROME_ERROR),
            "failed capture notice must paint a Browser warning icon as a vector or YAMIS image"
        );
    }

    #[test]
    fn browser_close_action_buttons_use_painted_close_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for (name, out) in [
            (
                "password-delete",
                render_password_menu_contents_frame(&ctx, "example.test", true),
            ),
            (
                "capture-notice",
                render_capture_notice_frame(&ctx, "Capture saved"),
            ),
        ] {
            let texts = painted_text(&out.shapes);
            assert_no_blank_painted_text(&texts, name);
            assert!(
                !texts.iter().any(|(text, _)| text.contains('\u{00D7}')),
                "{name} close action must not paint the legacy multiplication glyph as text: {texts:?}"
            );

            let lines = painted_line_strokes(&out.shapes);
            assert!(
                lines
                    .iter()
                    .any(|stroke| stroke.color == CHROME_TEXT_DIM
                        && (stroke.width - 1.7).abs() < 0.01),
                "{name} close action must paint a Browser dim close line icon: {lines:?}"
            );
        }
    }

    #[test]
    fn browser_inline_close_button_uses_painted_close_icon() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_inline_close_button_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "tab close button");
        assert!(
            !texts.iter().any(|(text, _)| text.contains('\u{00D7}')),
            "tab close button must not paint the legacy multiplication glyph as text: {texts:?}"
        );

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01),
            "tab close button must paint a Browser close line icon: {lines:?}"
        );
    }

    #[test]
    fn browser_tab_audio_buttons_use_painted_volume_icons() {
        assert_eq!(
            audio_icon_for(true, false),
            Some((ChromeIcon::VolumeUp, "Mute tab"))
        );
        assert_eq!(
            audio_icon_for(false, true),
            Some((ChromeIcon::VolumeOff, "Unmute tab"))
        );
        assert_eq!(audio_icon_for(false, false), None);

        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_tab_audio_buttons_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "tab audio controls");
        for legacy in ['\u{1F507}', '\u{1F50A}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "tab audio controls must not paint legacy speaker emoji as text: {texts:?}"
            );
        }

        let lines = painted_line_strokes(&out.shapes);
        let dim_icon_lines = lines
            .iter()
            .filter(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01)
            .count();
        assert!(
            dim_icon_lines >= 8,
            "tab audio controls must paint Browser volume line icons, got {lines:?}"
        );
    }

    #[test]
    fn browser_tab_status_chips_use_painted_icons_not_title_prefixes() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_tab_status_chips_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Example page", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                lower.contains("userscripts") || lower.contains("curated userscripts")
            }),
            "tab status paint must not expose Userscripts wording: {texts:?}"
        );
        for legacy in ["W ", "D2 ", "D ", "\u{25D2} "] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "tab status must not be embedded as title-prefix text: {texts:?}"
            );
        }

        let primary_lines = painted_line_strokes(&out.shapes)
            .into_iter()
            .filter(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01)
            .count();
        let primary_paths = painted_path_strokes(&out.shapes)
            .into_iter()
            .filter(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01)
            .count();
        assert!(
            primary_lines + primary_paths >= 4,
            "tab status chips must paint Browser icon geometry, got lines={primary_lines} paths={primary_paths}"
        );
    }

    #[test]
    fn browser_tab_hover_card_uses_browser_material_icon_and_text() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_tab_hover_card_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert!(
            texts.iter().any(|(text, color)| {
                text.contains("Engine: Chromium") && *color == CHROME_TEXT_DIM
            }),
            "tab hover card must paint the engine summary with Browser dim text: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|(text, color)| text.contains("Site fixups") && *color == CHROME_TEXT_DIM),
            "tab hover card must paint Site fixups with Browser dim text: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                lower.contains("userscripts") || lower.contains("curated userscripts")
            }),
            "tab hover card must not expose Userscripts wording: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(text, color)| {
                text.contains("Engine: Chromium")
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
            }),
            "tab hover card leaked shared shell text color: {texts:?}"
        );

        assert_browser_icon_painted(&out.shapes, CHROME_TEXT_DIM, "tab hover card");
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "tab hover card should paint the Browser tooltip surface: {fills:?}"
        );
    }

    #[test]
    fn browser_chrome_tooltips_use_browser_material_text() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_chrome_tooltip_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Search tabs", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, color)| text == "Search tabs"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "Browser tooltip leaked shared shell text color: {texts:?}"
        );
        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.iter().any(|fill| *fill == CHROME_SURFACE),
            "Browser tooltip should paint its own light surface, not inherit dark shell popup fill: {fills:?}"
        );
    }

    #[test]
    fn browser_owned_surfaces_self_scope_over_dark_shell_visuals() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);

        let out = render_shell_invoked_browser_surfaces_frame(&ctx);
        let texts = painted_text(&out.shapes);
        for (label, expected) in [
            ("HTTP connection", CHROME_WARN),
            ("http://plain.example/sensitive", CHROME_TEXT_DIM),
            ("Use HTTPS", CHROME_TOOLBAR),
            ("Capture saved", CHROME_PRIMARY),
            ("Search tabs", CHROME_TEXT),
        ] {
            assert_painted_text_color(&texts, label, expected);
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "shell-invoked Browser surface {label:?} leaked shared shell text: {texts:?}"
            );
        }
        assert!(
            texts.iter().any(|(text, _)| text == "History"),
            "drawer stack should render Browser drawer content in the shell-invoked frame: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(text, color)| text == "History"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "drawer stack leaked shared shell text after self-scoping: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE_CONTAINER),
            "shell-invoked Browser notices must paint Chrome surface containers: {fills:?}"
        );
        assert!(
            !fills
                .iter()
                .any(|color| matches!(*color, Style::BG | Style::SURFACE | Style::SURFACE_HI)),
            "Browser-owned shell-invoked surfaces must not paint shared dark shell fills: {fills:?}"
        );
    }

    #[test]
    fn browser_bookmarks_overflow_anchor_uses_painted_down_icon() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_bookmarks_bar_overflow_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_no_blank_painted_text(&texts, "bookmarks overflow anchor");
        assert!(
            !texts.iter().any(|(text, _)| text.contains('\u{00BB}')),
            "bookmarks overflow anchor must not paint the legacy guillemet as text: {texts:?}"
        );

        let lines = painted_line_strokes(&out.shapes);
        let image_meshes = painted_image_mesh_count(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01)
                || image_meshes >= 2,
            "bookmarks overflow anchor must paint a Browser line icon or YAMIS image; lines={lines:?} image_meshes={image_meshes}"
        );
    }

    #[test]
    fn browser_bookmarks_bar_items_use_browser_material_buttons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        for (surface, out, label) in [
            (
                "bookmarks bar",
                render_bookmarks_bar_overflow_frame(&ctx),
                "Bookmark 0",
            ),
            (
                "bookmarks overflow rows",
                render_bookmark_overflow_rows_frame(&ctx),
                "Bookmark 3",
            ),
        ] {
            let texts = painted_text(&out.shapes);
            assert_painted_text_color(&texts, label, CHROME_TEXT);
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "{surface} bookmark label leaked shared shell text color: {texts:?}"
            );

            let fills = painted_rect_fills(&out.shapes);
            assert!(
                fills.contains(&CHROME_SURFACE),
                "{surface} bookmark button must paint a Browser surface fill: {fills:?}"
            );

            let strokes = painted_rect_strokes(&out.shapes);
            assert!(
                strokes
                    .iter()
                    .any(|stroke| stroke.color == CHROME_OUTLINE
                        && (stroke.width - 1.0).abs() < 0.01),
                "{surface} bookmark button must paint Browser outline strokes: {strokes:?}"
            );

            let paths = painted_path_strokes(&out.shapes);
            let image_meshes = painted_image_mesh_count(&out.shapes);
            assert!(
                paths
                    .iter()
                    .any(|stroke| stroke.color == CHROME_TEXT_DIM
                        && (stroke.width - 1.7).abs() < 0.01)
                    || image_meshes > 0,
                "{surface} bookmark button must paint a Browser bookmark path icon or YAMIS image: paths={paths:?} image_meshes={image_meshes}"
            );
        }
    }

    #[test]
    fn browser_bookmark_bar_long_titles_clip_to_bookmark_button() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        mde_egui::fonts::install(&ctx);
        let title =
            "A very long bookmark title that should not paint into the next bookmark button";
        let out = render_long_bookmark_bar_button_frame(&ctx, title);
        let expected_label = format!("Open bookmark {title}");
        let button = accesskit_nodes(&out)
            .iter()
            .map(|(_, node)| node)
            .find(|node| node.label() == Some(expected_label.as_str()))
            .map(accesskit_bounds_rect)
            .expect("long bookmark button exports an AccessKit bounds rect");
        let geometry = painted_text_geometry(&out.shapes);
        let (_, text_rect, clip_rect) = geometry
            .iter()
            .find(|(text, _, _)| text == title)
            .unwrap_or_else(|| panic!("long bookmark title was not painted: {geometry:?}"));

        assert!(
            text_rect.width() > button.width(),
            "the title must naturally exceed the button so this test proves clipping: text={text_rect:?} button={button:?}"
        );
        assert!(
            clip_rect.left() >= button.left() - 0.5
                && clip_rect.right() <= button.right() + 0.5
                && clip_rect.top() >= button.top() - 0.5
                && clip_rect.bottom() <= button.bottom() + 0.5,
            "bookmark title clip should stay inside the button bounds: clip={clip_rect:?} button={button:?}"
        );
        assert!(
            clip_rect.width() < text_rect.width(),
            "the title should be physically clipped instead of overpainting adjacent chrome: text={text_rect:?} clip={clip_rect:?}"
        );
    }

    #[test]
    fn browser_bookmark_buttons_export_accesskit_links() {
        fn assert_bookmark_node(out: &egui::FullOutput, label: &str, value: &str, surface: &str) {
            let nodes = accesskit_nodes(out);
            let row = nodes
                .iter()
                .map(|(_, node)| node)
                .find(|node| node.label() == Some(label))
                .unwrap_or_else(|| panic!("missing {surface} AccessKit node {label:?}: {nodes:?}"));

            assert_eq!(row.role(), egui::accesskit::Role::Button);
            assert_eq!(row.value(), Some(value));
            assert!(
                row.supports_action(egui::accesskit::Action::Click),
                "{surface} bookmark rows should expose the same click action as the painted button"
            );
        }

        let bar_ctx = egui::Context::default();
        bar_ctx.enable_accesskit();
        mde_egui::fonts::install(&bar_ctx);
        let bar = render_bookmarks_bar_overflow_frame(&bar_ctx);
        assert_bookmark_node(
            &bar,
            "Open bookmark Bookmark 0",
            "https://bookmark-0.example/",
            "bookmarks bar",
        );

        let overflow_ctx = egui::Context::default();
        overflow_ctx.enable_accesskit();
        mde_egui::fonts::install(&overflow_ctx);
        let overflow = render_bookmark_overflow_rows_frame(&overflow_ctx);
        assert_bookmark_node(
            &overflow,
            "Open bookmark Bookmark 3",
            "https://bookmark-3.example/",
            "bookmarks overflow",
        );
    }

    #[test]
    fn browser_print_drawer_uses_user_facing_printer_copy() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_print_settings_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Printer destination", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Printer service unavailable", CHROME_ERROR);
        assert_painted_text_color(
            &texts,
            "No printers discovered; system default is still usable",
            CHROME_TEXT_DIM,
        );
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("cups") && !lower.contains("lpstat") && !lower.starts_with("lp:")
            }),
            "print drawer leaked printer backend copy: {texts:?}"
        );
    }

    #[test]
    fn browser_print_drawer_toggles_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_print_settings_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Duplex", CHROME_TEXT);
        assert_painted_text_color(&texts, "Grayscale", CHROME_TEXT);
        for label in ["Duplex", "Grayscale"] {
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "print drawer toggle label {label:?} leaked shared shell text color: {texts:?}"
            );
        }
    }

    #[test]
    fn browser_drawer_hover_layers_use_chrome_on_color_roles() {
        assert_eq!(drawers::drawer_toggle_state_layer(false), CHROME_TEXT);
        assert_eq!(drawers::drawer_toggle_state_layer(true), CHROME_TOOLBAR);
        assert_eq!(drawers::drawer_choice_chip_state_layer(false), CHROME_TEXT);
        assert_eq!(
            drawers::drawer_choice_chip_state_layer(true),
            CHROME_ON_PRIMARY_CONTAINER
        );
    }

    #[test]
    fn browser_drawer_controls_use_refined_chrome_height() {
        assert_eq!(
            drawers::DRAWER_ICON_BUTTON_H,
            CHROME_BUTTON,
            "Browser drawer controls should follow the same refined height as the main Browser toolbar"
        );
        assert!(
            drawers::DRAWER_ICON_BUTTON_H < 24.0,
            "Browser drawers must not return to the older 24pt local control height"
        );
    }

    #[test]
    fn browser_print_drawer_selectors_use_browser_material_chips() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_print_settings_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        for label in ["System default", "Portrait", "Printer default"] {
            assert_painted_text_color(&texts, label, CHROME_ON_PRIMARY_CONTAINER);
        }
        for label in ["Landscape", "A4", "Letter", "Legal"] {
            assert_painted_text_color(&texts, label, CHROME_TEXT);
        }
        for label in [
            "System default",
            "Portrait",
            "Landscape",
            "Printer default",
            "A4",
            "Letter",
            "Legal",
        ] {
            assert!(
                !texts.iter().any(|(text, color)| text == label
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
                "print drawer selector label {label:?} leaked shared shell text color: {texts:?}"
            );
        }

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_PRIMARY_CONTAINER),
            "selected print drawer selector chips must paint Browser selected fill: {fills:?}"
        );
        assert!(
            fills.contains(&CHROME_SURFACE),
            "unselected print drawer selector chips must paint Browser surface fill: {fills:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.0).abs() < 0.01),
            "selected print drawer selector chips must paint Browser primary stroke: {strokes:?}"
        );
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "unselected print drawer selector chips must paint Browser outline stroke: {strokes:?}"
        );
    }

    #[test]
    fn browser_print_drawer_stepper_uses_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_print_settings_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "12", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, color)| text == "12"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "print drawer copy stepper leaked shared shell text color: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "print drawer copy stepper must paint a Browser surface value fill: {fills:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "print drawer copy stepper must paint Browser outline strokes: {strokes:?}"
        );

        assert!(
            has_browser_icon_paint(&out.shapes, CHROME_TEXT_DIM),
            "print drawer copy stepper must paint Browser plus/minus icons: lines={:?} image_meshes={}",
            painted_line_strokes(&out.shapes),
            painted_image_mesh_count(&out.shapes)
        );
    }

    #[test]
    fn browser_print_drawer_stepper_hover_uses_browser_tooltip_surface() {
        let ctx = egui::Context::default();
        Style::install(&ctx);
        mde_egui::fonts::install(&ctx);

        let first = render_print_settings_drawer_frame(&ctx);
        let copy_value_rect = painted_text_rects(&first.shapes)
            .into_iter()
            .find_map(|(text, rect)| (text == "12").then_some(rect))
            .expect("print drawer renders the copy-stepper value");
        let _ = render_print_settings_drawer_frame_with_input(
            &ctx,
            vec![egui::Event::PointerMoved(copy_value_rect.center())],
            1.0,
        );
        let out = render_print_settings_drawer_frame_with_input(&ctx, Vec::new(), 1.6);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Number of copies", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, color)| text == "Number of copies"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "print drawer stepper hover leaked shared shell text color: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "print drawer stepper hover should paint the Browser tooltip surface: {fills:?}"
        );
    }

    #[test]
    fn browser_drawer_text_fields_use_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        assert!(drawers::PRINT_PAGE_RANGE_HELP.is_ascii());
        assert!(
            !drawers::PRINT_PAGE_RANGE_HELP.contains('\u{2014}'),
            "print drawer page-range helper must avoid typographic dash glyphs"
        );

        let print_out = render_print_settings_drawer_frame(&ctx);
        let print_texts = painted_text(&print_out.shapes);
        assert_painted_text_color(&print_texts, "1-5", CHROME_TEXT);
        assert!(
            !print_texts.iter().any(|(text, color)| text == "1-5"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "print drawer text field leaked shared shell text color: {print_texts:?}"
        );

        let site_out = render_site_styles_drawer_frame(&ctx);
        let site_texts = painted_text(&site_out.shapes);
        assert_painted_text_color(&site_texts, "reader.example", CHROME_TEXT);
        assert!(
            !site_texts
                .iter()
                .any(|(text, color)| text == "reader.example"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "site-styles drawer text field leaked shared shell text color: {site_texts:?}"
        );

        for (name, out) in [("print", print_out), ("site-styles", site_out)] {
            let fills = painted_rect_fills(&out.shapes);
            assert!(
                fills.contains(&CHROME_SURFACE),
                "{name} drawer text field must paint a Browser surface fill: {fills:?}"
            );

            let strokes = painted_rect_strokes(&out.shapes);
            assert!(
                strokes
                    .iter()
                    .any(|stroke| stroke.color == CHROME_OUTLINE
                        && (stroke.width - 1.0).abs() < 0.01),
                "{name} drawer text field must paint a Browser outline stroke: {strokes:?}"
            );
        }
    }

    #[test]
    fn browser_site_styles_css_editor_uses_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_site_styles_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Site Styles", CHROME_TEXT);
        assert_painted_text_color(&texts, "Custom CSS for matching websites", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "Website", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "main { line-height: 1.6; }", CHROME_TEXT);
        assert_painted_text_color(
            &texts,
            "example.test - body { max-width: 80ch; }",
            CHROME_TEXT_DIM,
        );
        assert!(
            texts.iter().all(|(text, _)| {
                let lower = text.to_ascii_lowercase();
                !lower.contains("injected")
                    && !lower.contains("userscripts")
                    && !lower.contains("matching hosts")
                    && !lower.contains("site host")
                    && !lower.contains("css injected")
                    && !lower.contains("host")
            }),
            "site-styles drawer must not expose implementation delivery terms: {texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|(text, color)| text == "main { line-height: 1.6; }"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "site-styles CSS editor leaked shared shell text color: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "site-styles CSS editor must paint a Browser surface fill: {fills:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "site-styles CSS editor must paint Browser outline strokes: {strokes:?}"
        );
    }

    #[test]
    fn browser_find_bar_text_field_uses_browser_material_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_find_chrome_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "mesh search", CHROME_TEXT);
        assert!(
            !texts.iter().any(|(text, color)| text == "mesh search"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "find bar text field leaked shared shell text color: {texts:?}"
        );

        let fills = painted_rect_fills(&out.shapes);
        assert!(
            fills.contains(&CHROME_SURFACE),
            "find bar text field must paint a Browser surface fill: {fills:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "find bar text field must paint Browser outline strokes: {strokes:?}"
        );
    }

    #[test]
    fn browser_drawer_separators_use_browser_material_outline() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let print_out = render_print_settings_drawer_frame(&ctx);
        let print_lines = painted_line_strokes(&print_out.shapes);
        assert!(
            print_lines
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "print drawer inline divider must paint Browser outline strokes: {print_lines:?}"
        );

        let site_out = render_site_styles_drawer_frame(&ctx);
        let site_lines = painted_line_strokes(&site_out.shapes);
        assert!(
            site_lines
                .iter()
                .any(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "site-styles drawer section divider must paint Browser outline strokes: {site_lines:?}"
        );
    }

    #[test]
    fn browser_chrome_separators_use_browser_material_outline() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_chrome_separator_frame(&ctx);
        let lines = painted_line_strokes(&out.shapes);
        assert_eq!(
            lines.len(),
            2,
            "full-width and inset Browser separators should each paint one divider line: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|stroke| stroke.color == CHROME_OUTLINE && (stroke.width - 1.0).abs() < 0.01),
            "Browser separators must paint only Material outline strokes: {lines:?}"
        );
    }

    #[test]
    fn browser_chrome_uses_the_shared_inter_proportional_family() {
        assert_eq!(font_id(13.0).family, FontFamily::Proportional);
    }
}
