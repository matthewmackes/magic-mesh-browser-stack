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
    sync::Arc,
    time::{Duration, Instant},
};

use mde_egui::egui::{
    self, Color32, FontFamily, FontId, RichText, TextStyle, TextureHandle, TextureOptions,
};
use mde_egui::menubar::Entry;
use mde_egui::{AnimatedScalar, ChipTone, Motion, MotionMode, MotionPreset, Style};
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
    downloads_drawer, history_drawer, offline_cache_drawer, print_settings_drawer, qr_share_drawer,
    security_update_drawer, site_styles_drawer, speech_status_drawer, spellcheck_drawer,
    translation_drawer,
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
    body::active_body(ui, state);
}

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

/// Chrome's UI face is Roboto, registered as a named family by `mde-egui`'s
/// shared font installer. Keeping it named, not proportional, preserves Inter as
/// the shell-wide prose face while Browser gets its Material/Chrome exception.
pub(super) fn chrome_font_family() -> FontFamily {
    FontFamily::Name(Arc::from(mde_egui::fonts::BROWSER_CHROME_FAMILY))
}

pub(super) const CHROME_SURFACE: Color32 = Color32::from_rgb(248, 250, 253);
pub(super) const CHROME_SURFACE_CONTAINER: Color32 = Color32::from_rgb(241, 243, 244);
pub(super) const CHROME_SURFACE_CONTAINER_HIGH: Color32 = Color32::from_rgb(232, 234, 237);
pub(super) const CHROME_TOOLBAR: Color32 = Color32::from_rgb(255, 255, 255);
pub(super) const CHROME_PRIMARY: Color32 = Color32::from_rgb(11, 87, 208);
pub(super) const CHROME_PRIMARY_CONTAINER: Color32 = Color32::from_rgb(211, 227, 253);
pub(super) const CHROME_ON_PRIMARY_CONTAINER: Color32 = Color32::from_rgb(4, 30, 73);
pub(super) const CHROME_SUCCESS_CONTAINER: Color32 = Color32::from_rgb(196, 238, 208);
pub(super) const CHROME_ON_SUCCESS_CONTAINER: Color32 = Color32::from_rgb(8, 65, 30);
pub(super) const CHROME_OUTLINE: Color32 = Color32::from_rgb(218, 220, 224);
pub(super) const CHROME_TEXT: Color32 = Color32::from_rgb(32, 33, 36);
pub(super) const CHROME_TEXT_DIM: Color32 = Color32::from_rgb(95, 99, 104);
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
const CHROME_SEPARATOR_H: f32 = 9.0;
const OPTION_ROW_H: f32 = 30.0;
const OPTION_ICON_SIZE: f32 = 18.0;
const MEDIA_CLUSTER_LABEL_W: f32 = 132.0;
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
        (true, false) => CHROME_TEXT,
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
        BrowserEngine::Cef => "CEF / Chromium",
        BrowserEngine::Servo => "Servo",
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

pub(super) fn chrome_tooltip(ui: &mut egui::Ui, text: &str) {
    ui.set_max_width(260.0);
    ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(Style::SMALL)
                .color(CHROME_TEXT_DIM),
        )
        .wrap(),
    );
}

pub(super) fn chrome_hover_text(
    response: egui::Response,
    text: impl Into<String>,
) -> egui::Response {
    let text = text.into();
    response.on_hover_ui(move |ui| chrome_tooltip(ui, text.as_str()))
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

fn ad_filter_chip(ui: &mut egui::Ui, blocked: u64, top_blocked: &[(String, u64)]) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(CHROME_BUTTON, CHROME_BUTTON),
            egui::Sense::hover(),
        );
        paint_chrome_icon(ui.painter(), rect, ChromeIcon::Privacy, CHROME_TEXT_DIM);
        ui.label(
            RichText::new(blocked.to_string())
                .size(CHROME_FONT)
                .color(CHROME_TEXT_DIM),
        );
    })
    .response
    .on_hover_ui(|ui| {
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
    let width = ui.available_width().max(1.0);
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
    }
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
            CHROME_TEXT
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
        CHROME_TEXT
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
}

fn apply_visuals(ui: &mut egui::Ui) {
    let style = ui.style_mut();
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
    ui.painter().rect_filled(
        shadow,
        1.0,
        Color32::from_black_alpha(if active { 28 } else { 8 }),
    );
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
                    Color32::from_black_alpha(18),
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
            label: "Curated userscripts",
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
    let mut picked = None;
    ui.add_space(CHROME_GAP);
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
        .corner_radius(ICON_BUTTON_RADIUS)
        .inner_margin(egui::Margin::symmetric(3, 1))
        .show(ui, |ui| {
            ui.set_height(CHROME_BUTTON);
            ui.horizontal(|ui| {
                let tone = if model.background {
                    CHROME_PRIMARY
                } else if model.muted {
                    CHROME_TEXT_DIM
                } else {
                    CHROME_TEXT
                };
                let label_text = format!("    {}", model.label);
                let label = chrome_hover_text(
                    ui.add_sized(
                        egui::vec2(MEDIA_CLUSTER_LABEL_W, CHROME_BUTTON),
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
        "Engine" => "Runtime",
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

fn option_row(
    ui: &mut egui::Ui,
    item: &mde_egui::menubar::Item<super::menubar::MenuAction>,
) -> Option<super::menubar::MenuAction> {
    let width = ui.available_width().clamp(320.0, 760.0);
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
    let text_color = button_text(item.enabled);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 17.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
    );
    paint_chrome_icon(ui.painter(), icon_rect, action_icon(item.id), text_color);
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
    ui.painter().text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        item.label.as_str(),
        font_id(CHROME_FONT + 1.0),
        text_color,
    );
    if let Some(shortcut) = &item.shortcut {
        ui.painter().text(
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
        chrome_hover_text(response, "Unavailable in the current browser context")
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

pub(super) fn options_page(ui: &mut egui::Ui, state: &mut WebState) {
    let menus = super::menubar::chrome_menus(state);
    let mut picked = None;
    egui::Frame::NONE
        .fill(CHROME_SURFACE)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_height(ui.available_height());
                egui::Frame::NONE
                    .fill(CHROME_SURFACE_CONTAINER)
                    .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(8))
                    .show(ui, |ui| {
                        ui.set_width(154.0);
                        ui.label(
                            RichText::new("Runtime")
                                .size(CHROME_FONT + 3.0)
                                .color(CHROME_TEXT),
                        );
                        ui.label(
                            RichText::new(super::BROWSER_OPTIONS_URL)
                                .size(CHROME_FONT)
                                .color(CHROME_TEXT_DIM),
                        );
                        ui.add_space(8.0);
                        for menu in &menus {
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
                    });
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .id_salt("browser-options-page")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_min_width(420.0);
                        ui.label(
                            RichText::new("Browser Options")
                                .size(CHROME_FONT + 8.0)
                                .color(CHROME_TEXT),
                        );
                        ui.label(
                            RichText::new("Command and runtime controls")
                                .size(CHROME_FONT)
                                .color(CHROME_TEXT_DIM),
                        );
                        ui.add_space(10.0);
                        for menu in &menus {
                            egui::Frame::NONE
                                .fill(CHROME_SURFACE_CONTAINER)
                                .stroke(egui::Stroke::new(1.0, CHROME_OUTLINE))
                                .corner_radius(8.0)
                                .inner_margin(egui::Margin::symmetric(8, 7))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let rect = egui::Rect::from_center_size(
                                            ui.next_widget_position() + egui::vec2(9.0, 9.0),
                                            egui::vec2(OPTION_ICON_SIZE, OPTION_ICON_SIZE),
                                        );
                                        ui.allocate_space(egui::vec2(22.0, 20.0));
                                        paint_chrome_icon(
                                            ui.painter(),
                                            rect,
                                            menu_icon(&menu.title),
                                            CHROME_PRIMARY,
                                        );
                                        ui.label(
                                            RichText::new(technical_menu_label(&menu.title))
                                                .size(CHROME_FONT + 4.0)
                                                .color(CHROME_TEXT),
                                        );
                                    });
                                    ui.add_space(5.0);
                                    render_options_entries(ui, &menu.entries, &mut picked);
                                });
                            ui.add_space(10.0);
                        }
                    });
            });
        });
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
    ui.set_max_width(260.0);
    ui.horizontal_wrapped(|ui| {
        let (icon_rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
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

fn tab_search_result_row(ui: &mut egui::Ui, label: &str, active: bool) -> egui::Response {
    let width = ui.available_width().max(288.0);
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
    ui.painter().text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font_id(CHROME_FONT),
        text_color,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    response
}

fn tab_search_results(ui: &mut egui::Ui, state: &WebState) -> Option<usize> {
    let mut select = None;
    tab_search_separator(ui);
    let matches = matching_tab_indices(&state.tabs, &state.tab_search_query);
    egui::ScrollArea::vertical()
        .max_height(260.0)
        .show(ui, |ui| {
            if matches.is_empty() {
                browser_muted_note(ui, "No matching tabs");
            }
            for idx in matches {
                let active = idx == state.active;
                let label = tab_search_row_label(&state.tabs[idx]);
                if tab_search_result_row(ui, &label, active).clicked() {
                    select = Some(idx);
                }
            }
        });
    select
}

fn tab_search_menu_contents(ui: &mut egui::Ui, state: &mut WebState) -> Option<usize> {
    ui.set_min_width(300.0);
    let resp = chrome_text_field(
        ui,
        true,
        &mut state.tab_search_query,
        "Search tabs",
        f32::INFINITY,
        220.0,
        false,
        "Search tabs",
        None,
    );
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

    // Resolve/cache each tab's favicon texture BEFORE the (immutable) pill loop
    // below — see `resolve_tab_favicon_textures`.
    let favicon_textures = resolve_tab_favicon_textures(ui.ctx(), &mut state.tabs);

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
                for (idx, tab) in state.tabs.iter().enumerate() {
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
                        favicon_textures.get(idx).and_then(Option::as_ref),
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
                    tab_response
                        .on_hover_ui(|ui| tab_hover_card(ui, tab))
                        .context_menu(|ui| {
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
                                "Disable userscripts"
                            } else {
                                "Enable userscripts"
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

    // Resolve/cache each tab's favicon texture BEFORE the (immutable) pill loop
    // below — see `resolve_tab_favicon_textures`.
    let favicon_textures = resolve_tab_favicon_textures(ui.ctx(), &mut state.tabs);

    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width((CHROME_TAB_RAIL_W - 8.0).max(CHROME_NEW_TAB_W));
            egui::ScrollArea::vertical()
                .id_salt("browser-vertical-tabs")
                .max_height(ui.available_height().max(CHROME_TAB_H * 3.0))
                .show(ui, |ui| {
                    for (idx, tab) in state.tabs.iter().enumerate() {
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
                                favicon_textures.get(idx).and_then(Option::as_ref),
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
                            resp.on_hover_ui(|ui| tab_hover_card(ui, tab))
                                .context_menu(|ui| {
                                    if tab_context_menu_row(
                                        ui,
                                        "Move tab up",
                                        ChromeIcon::Up,
                                        idx > 0,
                                    )
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
                                    let pin_label =
                                        if tab.pinned { "Unpin tab" } else { "Pin tab" };
                                    if tab_context_menu_row(
                                        ui,
                                        pin_label,
                                        ChromeIcon::Bookmark,
                                        true,
                                    )
                                    .clicked()
                                    {
                                        pin_tab = Some((idx, !tab.pinned));
                                        ui.close_menu();
                                    }
                                    if tab_context_menu_row(
                                        ui,
                                        "Duplicate tab",
                                        ChromeIcon::Page,
                                        true,
                                    )
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
                                    let mute_label =
                                        if tab.muted { "Unmute tab" } else { "Mute tab" };
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
                                    if tab_context_menu_row(
                                        ui,
                                        autoplay_label,
                                        ChromeIcon::Play,
                                        true,
                                    )
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
                                    if tab_context_menu_row(
                                        ui,
                                        dark_label,
                                        ChromeIcon::DarkMode,
                                        true,
                                    )
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
                                    if tab_context_menu_row(
                                        ui,
                                        reader_label,
                                        ChromeIcon::View,
                                        true,
                                    )
                                    .clicked()
                                    {
                                        reader_tab = Some((idx, !tab.reader_mode));
                                        ui.close_menu();
                                    }
                                    let scripts_label = if tab.user_scripts {
                                        "Disable userscripts"
                                    } else {
                                        "Enable userscripts"
                                    };
                                    if tab_context_menu_row(
                                        ui,
                                        scripts_label,
                                        ChromeIcon::Edit,
                                        true,
                                    )
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
                                    if tab_context_menu_row(
                                        ui,
                                        "Close tab",
                                        ChromeIcon::Close,
                                        true,
                                    )
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

/// Resolve (and cache) every tab's favicon texture for this frame, in tab order.
///
/// One mutable pass over `tabs` up front, so the tab-strip render loops below —
/// which already borrow each `Tab` by shared reference while building its pill
/// label + context menu — can index into the returned slice instead of fighting
/// this cache for a second `&mut Tab`.
fn resolve_tab_favicon_textures(
    ctx: &egui::Context,
    tabs: &mut [Tab],
) -> Vec<Option<TextureHandle>> {
    tabs.iter_mut()
        .map(|tab| tab_favicon_texture(ctx, tab))
        .collect()
}

pub(super) fn new_tab_dashboard(ui: &mut egui::Ui, state: &mut WebState) {
    let mut submit_search = false;
    let mut open_service: Option<String> = None;
    centered(ui, |ui| {
        ui.label(
            egui::RichText::new("Quasar Browser")
                .size(Style::HEADING)
                .color(CHROME_TEXT),
        );
        // Private-by-default explainer — the browser has no persistent profile
        // (sandbox has no writable $HOME); make that posture legible on the front
        // door instead of only in the Privacy menu.
        browser_status_note(
            ui,
            ChromeIcon::Lock,
            PRIVATE_MODE_EXPLAINER,
            CHROME_TEXT_DIM,
        );
        ui.add_space(Style::SP_M);
        ui.horizontal(|ui| {
            let resp = chrome_text_field(
                ui,
                true,
                &mut state.dashboard_query,
                "Search the mesh",
                420.0,
                260.0,
                false,
                "Search the mesh",
                None,
            );
            state.chrome_edit_focus |= resp.has_focus();
            submit_search = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add(action_button("Search", BrowserActionRole::Primary))
                .clicked()
            {
                submit_search = true;
            }
        });
        ui.add_space(Style::SP_M);
        ui.horizontal_wrapped(|ui| {
            for service in &state.speed_dial {
                if chrome_hover_text(
                    ui.add(
                        action_button(service.label.clone(), BrowserActionRole::Secondary)
                            .min_size(egui::vec2(112.0, Style::SP_XL)),
                    ),
                    service.hint.as_str(),
                )
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

pub(super) fn paint_omnibox_inline_completion(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    draft: &str,
    tail: &str,
) {
    if draft.is_empty() || tail.is_empty() {
        return;
    }
    let font_id = font_id(CHROME_FONT);
    let typed = ui.fonts(|f| f.layout_no_wrap(draft.to_owned(), font_id.clone(), CHROME_TEXT));
    let ghost = ui.fonts(|f| f.layout_no_wrap(tail.to_owned(), font_id, CHROME_TEXT_DIM));
    let text_pos = egui::pos2(
        rect.left() + 4.0 + typed.size().x,
        rect.center().y - ghost.size().y / 2.0,
    );
    ui.painter().galley(text_pos, ghost, CHROME_TEXT_DIM);
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
    let width = ui.available_width().max(188.0);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, OPTION_ROW_H),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    let fill = animated_response_fill(ui, &response, menu_item_fill(false), CHROME_TEXT, enabled);
    ui.painter().rect(
        rect,
        7.0,
        fill,
        egui::Stroke::new(1.0, CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    let text_color = button_text(enabled);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 16.0, rect.center().y),
        egui::vec2(OPTION_ICON_SIZE - 1.0, OPTION_ICON_SIZE - 1.0),
    );
    paint_chrome_icon(ui.painter(), icon_rect, icon, text_color);
    ui.painter().text(
        egui::pos2(rect.left() + 34.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font_id(CHROME_FONT),
        text_color,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    if enabled {
        response
    } else {
        chrome_hover_text(response, disabled_tip)
    }
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
    resp.context_menu(|ui| {
        ui.set_min_width(210.0);
        ui.visuals_mut().override_text_color = Some(CHROME_TEXT);
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

/// The navigation chrome bar — a §4-token toolbar. Back / forward / reload act on
/// the active session; the address bar loads on submit. On a crashed tab, Reload
/// becomes a respawn request. The page-actions menu (BOOKMARKS-10) hangs off both
/// the toolbar star button and the address bar's right-click.
pub(super) fn nav_chrome(ui: &mut egui::Ui, state: &mut WebState) {
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
    let recent_resources = state
        .tabs
        .get(state.active)
        .map_or_else(Vec::new, |t| t.session.recent_resource_requests());
    let permission_summary = site_info_permission_summary(state);
    let has_page = has_tab && !crashed && !page_url.trim().is_empty();

    let mut accepted_suggestion: Option<String> = None;
    ui.horizontal(|ui| {
        // Back / forward — enabled only when the live session offers the history.
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

        if chrome_icon_button(
            ui,
            ChromeIcon::NewTab,
            &format!("Open a new tab with {}", engine_display_name(state.engine)),
            true,
            false,
        )
        .clicked()
        {
            state.request_new_tab(state.engine);
        }
        if engine_toolbar_chip(ui, state).clicked() {
            state.open_options_tab();
        }

        // BOOKMARKS-10 — the page-actions menu (bookmark this page / copy its URL /
        // send it in Chat). The SAME three verbs also hang off the address bar's
        // right-click (below), so both the toolbar and the context menu reach them.
        let is_bookmarked = has_page
            && state
                .bookmarked_urls
                .contains(super::bookmark_membership_key(&page_url));
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

        let (active_downloads, total_downloads) = state.download_counts();
        let downloads_tip = if total_downloads == 0 {
            "Downloads"
        } else {
            "Downloads from the shared Transfers ledger"
        };
        if chrome_icon_button(
            ui,
            ChromeIcon::Downloads,
            downloads_tip,
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
        if active_downloads > 0 {
            ui.label(
                RichText::new(active_downloads.to_string())
                    .size(CHROME_FONT)
                    .color(CHROME_PRIMARY),
            );
        }

        // BOOKMARKS-7 — a compact "N blocked" shield when the ad-filter has dropped
        // requests on this page (honest 0 stays hidden). Reads the session's
        // per-page counter; the engine is compiled from the mackesd `adfilter` blob.
        if blocked > 0 {
            ui.add_space(CHROME_GAP);
            // The by-domain breakdown behind the count (uBlock-style detail): the top
            // blocked domains for THIS page, surfaced on hover of the shield.
            let top_blocked = state
                .tabs
                .get(state.active)
                .map(|t| t.session.block_tally().top_domains(6))
                .unwrap_or_default();
            ad_filter_chip(ui, u64::from(blocked), &top_blocked);
        }

        ui.add_space(CHROME_GAP);

        if has_tab && !crashed && nav.loading {
            loading_globe(ui, CHROME_BUTTON, "toolbar");
            ui.add_space(CHROME_GAP);
        }
        browser_media_toolbar(ui, state);

        // OMNIBOX-STYLE — the leading security chip reflects the CURRENT
        // (committed) page URL's scheme, never the in-progress edit draft.
        security_chip(
            ui,
            &page_url,
            &recent_resources,
            permission_summary.as_ref(),
        );
        ui.add_space(CHROME_GAP);

        // The address bar fills the rest of the row.
        let resp = chrome_text_field(
            ui,
            has_tab && !crashed,
            &mut state.address,
            "Enter an address",
            (ui.available_width() - (CHROME_BUTTON * 2.0 + Style::SP_XL)).max(160.0),
            160.0,
            false,
            "Enter an address",
            Some(super::omnibox_widget_id()),
        );
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
            let font_id = font_id(CHROME_FONT);
            let job = omnibox_layout_job(&state.address, font_id);
            if !job.is_empty() {
                let galley = ui.fonts(|f| f.layout_job(job));
                ui.painter().rect_filled(resp.rect, 4.0, CHROME_SURFACE);
                let text_pos = egui::pos2(
                    resp.rect.left() + 4.0,
                    resp.rect.center().y - galley.size().y / 2.0,
                );
                ui.painter().galley(text_pos, galley, CHROME_TEXT);
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
    url: &str,
    title: &str,
) {
    let has_page = !url.trim().is_empty();
    if chrome_menu_row(
        ui,
        "Add bookmark",
        ChromeIcon::Bookmark,
        has_page,
        "No loaded page to bookmark",
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
    let tip = if is_bookmarked {
        "Bookmarked: page actions, edit bookmark, copy URL, share"
    } else {
        "Page actions: bookmark, copy URL, share"
    };
    let popup_id = egui::Id::new("mde_web_page_actions_menu_popup");
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
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            page_actions_menu(ui, bus_root, engine, url, title);
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
                "Punycode/IDN host (xn--): verify this is the site you expect"
            }
            ConfusableReason::ConfusableBlock => {
                "Look-alike letters (Cyrillic/Greek): this host may impersonate another site"
            }
            ConfusableReason::MixedScript => {
                "Mixed-script host: letters from more than one alphabet can spoof a name"
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
                        "Unsafe hosts: {}",
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
                        "Blocked content hosts: {}",
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
                        "Blocked tracker hosts: {}",
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
                egui::RichText::new("Sensitive capabilities default to deny")
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
                    "{} permissions were forgotten; future requests re-prompt under default deny",
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
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            let scale_inset = ((1.0 - motion.scale) * 16.0).clamp(0.0, 1.0);
            if scale_inset > 0.0 {
                ui.horizontal(|ui| {
                    ui.add_space(scale_inset);
                    ui.vertical(|ui| site_info_panel(ui, page_url, recent_resources, permissions));
                });
            } else {
                site_info_panel(ui, page_url, recent_resources, permissions);
            }
        },
    );
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

pub(super) fn suggestions_panel(ui: &mut egui::Ui, state: &WebState) -> Option<String> {
    let history = &state.suggestions.history;
    let bookmarks = &state.suggestions.bookmarks;
    let search_items = dedup_search_items(&state.suggestions.items, history);
    if bookmarks.is_empty()
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
    let mut idx = 0usize;
    let fill_for = |idx: usize| {
        if Some(idx) == selected {
            row_fill(true)
        } else {
            row_fill(false)
        }
    };
    ui.horizontal_wrapped(|ui| {
        ui.add_space(Style::SP_XL * 4.0);
        if !bookmarks.is_empty() {
            browser_muted_note(ui, "Bookmarks");
            for bm in bookmarks {
                let label = ellipsize(&bm.title, 32);
                let clicked = suggestion_chip(
                    ui,
                    &label,
                    ChromeIcon::Bookmark,
                    CHROME_PRIMARY,
                    fill_for(idx),
                )
                .on_hover_ui(|ui| chrome_tooltip(ui, &format!("Bookmark: {}", bm.url)))
                .clicked();
                if clicked {
                    accepted = Some(bm.url.clone());
                }
                idx += 1;
            }
        }
        if !history.is_empty() {
            browser_muted_note(ui, "History");
            for url in history {
                let label = ellipsize(url, 36);
                let clicked =
                    suggestion_chip(ui, &label, ChromeIcon::History, CHROME_TEXT, fill_for(idx))
                        .on_hover_ui(|ui| chrome_tooltip(ui, &format!("Visited: {url}")))
                        .clicked();
                if clicked {
                    accepted = Some(url.clone());
                }
                idx += 1;
            }
        }
        for suggestion in search_items {
            let label = ellipsize(suggestion, 36);
            let clicked =
                suggestion_chip(ui, &label, ChromeIcon::Search, CHROME_TEXT, fill_for(idx))
                    .on_hover_ui(|ui| chrome_tooltip(ui, &format!("Search for {suggestion}")))
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
    width: f32,
    tip: &str,
) -> egui::Response {
    let target_size = egui::vec2(width.max(44.0), CHROME_BUTTON);
    let (rect, response) = ui.allocate_exact_size(target_size, egui::Sense::click());
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
    ui.painter().text(
        egui::pos2(icon_rect.right() + 7.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        font_id(CHROME_FONT),
        CHROME_TEXT,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn bookmark_overflow_rows(
    ui: &mut egui::Ui,
    links: &[super::BookmarkBarLink],
) -> Option<(String, bool)> {
    let mut chosen = None;
    for link in links {
        let label = ellipsize(&link.title, 40);
        let width = ui.available_width().max(180.0);
        let resp = bookmark_bar_button(ui, &label, &link.title, width, link.url.as_str());
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
                    let popup_id = egui::Id::new("mde_web_bookmark_overflow_popup");
                    let response = toolbar_icon_menu_anchor(
                        ui,
                        popup_id,
                        ChromeIcon::Down,
                        CHROME_TEXT,
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
                            if motion.active {
                                ui.ctx().request_repaint();
                            }
                            ui.multiply_opacity(motion.opacity.max(0.2));
                            if motion.anchor_offset > 0.0 {
                                ui.add_space(motion.anchor_offset);
                            }
                            if let Some(choice) = bookmark_overflow_rows(ui, &links[visible..]) {
                                chosen = Some(choice);
                                ui.memory_mut(|mem| mem.close_popup());
                            }
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
    ui.set_min_width(260.0);
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
        browser_muted_note(ui, &format!("None saved for {host}"));
    } else {
        for (idx, username, password) in matches {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        has_page && can_fill,
                        action_button(format!("Fill {username}"), BrowserActionRole::Primary),
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
        RichText::new(format!("Save a login for {host}"))
            .size(CHROME_FONT)
            .color(CHROME_TEXT),
    );
    let user_resp = chrome_text_field(
        ui,
        true,
        &mut state.login_user_draft,
        "username",
        f32::INFINITY,
        160.0,
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
        f32::INFINITY,
        160.0,
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
    let popup_id = egui::Id::new("mde_web_password_menu_popup");
    let response = toolbar_icon_menu_anchor(
        ui,
        popup_id,
        ChromeIcon::Lock,
        button_text(has_page),
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
            if motion.active {
                ui.ctx().request_repaint();
            }
            ui.multiply_opacity(motion.opacity.max(0.2));
            if motion.anchor_offset > 0.0 {
                ui.add_space(motion.anchor_offset);
            }
            let outcome = password_menu_contents(ui, state, &host, &matches, has_page, can_fill);
            let close_popup = outcome.fill.is_some() || outcome.remove.is_some() || outcome.save;
            fill = outcome.fill;
            remove = outcome.remove;
            save = outcome.save;
            if close_popup {
                ui.memory_mut(|mem| mem.close_popup());
            }
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
    ui.horizontal_wrapped(|ui| {
        browser_status_note(ui, ChromeIcon::Warning, "HTTP connection", CHROME_WARN);
        ui.label(RichText::new(ellipsize(&url, 64)).color(CHROME_TEXT_DIM));
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
        pending.engine.label()
    )
}

fn dialog_prompt_frame(ui: &mut egui::Ui, id: impl Hash, contents: impl FnOnce(&mut egui::Ui)) {
    let motion = dialog_prompt_motion(ui.ctx(), id);
    egui::Frame::NONE
        .fill(prompt_fill())
        .inner_margin(egui::Margin::symmetric(8, 6))
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
                    RichText::new(result.cache_id.chars().take(12).collect::<String>())
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
                    RichText::new(format!(
                        "{} chars from {}",
                        result.text.chars().count(),
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(CHROME_TEXT_DIM),
                );
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
            RichText::new("Sandboxed browser")
                .size(Style::HEADING)
                .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_S);
        browser_body_note(
            ui,
            notice.unwrap_or(
                "The sandboxed Servo browser renders here in the shell. A live session \
                 attaches on a GPU seat (BOOKMARKS-5/6 live path is gated).",
            ),
        );
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
        let dragging_page = resp.dragged();
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
        assert_eq!(hover, Color32::from_rgb(238, 238, 238));
        assert_eq!(pressed, Color32::from_rgb(232, 232, 233));
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

    fn render_omnibox_chrome_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        let (shell, _helper) = std::os::unix::net::UnixStream::pair().expect("omnibox socketpair");
        let session =
            mde_web_preview_client::WebSession::from_stream(shell, None).expect("omnibox session");
        state.push_session(session);
        state.address = "https://example.test/mesh".to_owned();
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(860.0, 96.0),
                )),
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
                    scope(ui, |ui| {
                        capture_notice(ui, &mut state);
                    });
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
                    scope(ui, |ui| {
                        insecure_prompt(ui, &mut state);
                    });
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
                    scope(ui, |ui| {
                        passkey_consent_prompt_bar(ui, &passkey, Some(7));
                        permission_prompt_bar(ui, "https://camera.example", 3);
                        before_unload_prompt_bar(ui, &before_unload);
                        login_save_prompt_bar(ui, "docs.example.com", "mm");
                    });
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
                    scope(ui, |ui| {
                        chrome_tooltip(ui, "Search tabs");
                    });
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

    fn render_page_context_rows_frame(ctx: &egui::Context) -> egui::FullOutput {
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
                    ui.set_min_width(210.0);
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
                        let _ = tab_context_menu_row(ui, "Close tab", ChromeIcon::Close, true);
                    });
                });
            },
        )
    }

    fn render_page_actions_menu_frame(ctx: &egui::Context) -> egui::FullOutput {
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
                            "https://example.test/",
                            "Example",
                        );
                    });
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

    fn render_suggestions_panel_frame(ctx: &egui::Context) -> egui::FullOutput {
        let mut state = WebState::default();
        state.suggestions.bookmarks = vec![super::super::BookmarkBarLink {
            title: "Example bookmark".to_owned(),
            url: "https://example.test/bookmark".to_owned(),
        }];
        state.suggestions.history = vec!["https://example.test/history".to_owned()];
        state.suggestions.items = vec![
            "example search".to_owned(),
            "https://example.test/history".to_owned(),
        ];
        state.suggestions.selected = Some(0);
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(640.0, 140.0),
                )),
                time: Some(0.0),
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
                        drawers::qr_share_drawer(ui, &mut state);
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
                time: Some(0.0),
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
                        let _ = tab_search_menu_contents(ui, &mut state);
                    });
                });
            },
        )
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
            "Browser stepper plus icon should paint two local lines"
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
        assert_eq!(engine_display_name(BrowserEngine::Cef), "CEF / Chromium");
        assert_eq!(engine_marker(BrowserEngine::Cef), "CEF");
        assert_eq!(engine_glyph(BrowserEngine::Cef), "C");
        assert_eq!(engine_display_name(BrowserEngine::Servo), "Servo");
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
                !texts
                    .iter()
                    .any(|(text, _)| text == engine_marker(engine)),
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
        assert_eq!(page_action_icon_color(true, false), CHROME_TEXT);
        assert_eq!(page_action_icon_color(true, true), CHROME_PRIMARY);
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
        assert_painted_text_color(&texts, "Close tab", CHROME_TEXT);
        for label in [
            "Move tab left",
            "Move tab right",
            "Pin tab",
            "Duplicate tab",
            "Work container",
            "Display 2",
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
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "enabled tab context rows must paint Browser line icons: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01),
            "disabled tab context rows must paint dim Browser line icons: {lines:?}"
        );
    }

    #[test]
    fn page_actions_menu_rows_use_browser_painted_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_page_actions_menu_frame(&ctx);
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

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "page action rows must paint Browser line icons: {lines:?}"
        );
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
            "Punycode/IDN host (xn--): verify this is the site you expect",
            CHROME_WARN,
        );
        assert!(
            !texts
                .iter()
                .any(|(text, _)| text.contains('\u{2014}') || text.contains('\u{2192}')),
            "security chip and panel must not paint typographic dash/arrow glyph copy: {texts:?}"
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
    fn ad_filter_chip_uses_browser_icon_and_count_rows() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_ad_filter_chrome_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "7", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "3", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "ads.example", CHROME_TEXT_DIM);
        for legacy in ['\u{2298}', '\u{00D7}'] {
            assert!(
                !texts.iter().any(|(text, _)| text.contains(legacy)),
                "ad-filter chip and domain rows must not paint legacy glyph text: {texts:?}"
            );
        }

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01),
            "ad-filter chip must paint a Browser shield/privacy line icon: {lines:?}"
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
        assert_painted_text_color(&texts, "History", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "https://example.test/history", CHROME_TEXT);
        assert_painted_text_color(&texts, "example search", CHROME_TEXT);
        assert!(
            !texts
                .iter()
                .any(|(text, _)| text.contains('\u{2605}') || text.contains('\u{2606}')),
            "bookmark suggestions must not paint legacy star glyph text: {texts:?}"
        );

        let path_strokes = painted_path_strokes(&out.shapes);
        assert!(
            path_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01),
            "bookmark suggestions must paint a Browser bookmark path icon: {path_strokes:?}"
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
    fn tab_search_toolbar_anchor_uses_browser_icon_button() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_tab_search_menu_button_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert!(
            texts.iter().all(|(text, _)| !text.trim().is_empty()),
            "tab-search toolbar anchor must not paint a blank stock menu-button text label: {texts:?}"
        );

        let line_strokes = painted_line_strokes(&out.shapes);
        assert!(
            line_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "tab-search toolbar anchor must paint the Browser search line icon: {line_strokes:?}"
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
        assert_painted_text_color(&texts, "Example QR", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, color)| text == "QR share"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "QR share drawer heading leaked shared shell text color: {texts:?}"
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

        let icon_strokes = painted_line_strokes(&out.shapes);
        assert!(
            icon_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "history visit rows must paint a Browser History icon: {icon_strokes:?}"
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

            let lines = painted_line_strokes(&out.shapes);
            assert!(
                lines
                    .iter()
                    .any(|stroke| stroke.color == CHROME_TEXT_DIM
                        && (stroke.width - 1.7).abs() < 0.01),
                "{name} drawer close button must paint a Browser dim close line icon: {lines:?}"
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

            let dim_lines = painted_line_strokes(&out.shapes)
                .into_iter()
                .filter(|stroke| {
                    stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01
                })
                .count();
            assert!(
                dim_lines >= 4,
                "{name} drawer must paint Browser close and reload line icons: {dim_lines}"
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

        let warning_lines = painted_line_strokes(&out.shapes);
        assert!(
            warning_lines
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "dangerous download warning must paint a Browser warning line icon: {warning_lines:?}"
        );
        let warning_paths = painted_path_strokes(&out.shapes);
        assert!(
            warning_paths
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "dangerous download warning must paint a Browser warning path icon: {warning_paths:?}"
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
                &["CUPS service unavailable"][..],
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

            let warning_lines = painted_line_strokes(&out.shapes);
            assert!(
                warning_lines
                    .iter()
                    .any(|stroke| stroke.color == CHROME_ERROR
                        && (stroke.width - 1.7).abs() < 0.01),
                "{name} drawer must paint a Browser error warning line icon: {warning_lines:?}"
            );
            let warning_paths = painted_path_strokes(&out.shapes);
            assert!(
                warning_paths
                    .iter()
                    .any(|stroke| stroke.color == CHROME_ERROR
                        && (stroke.width - 1.7).abs() < 0.01),
                "{name} drawer must paint a Browser error warning path icon: {warning_paths:?}"
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
        assert_painted_text_color(&texts, "mismatch", CHROME_WARN);
        assert_painted_text_color(&texts, "TTS speaking", CHROME_PRIMARY);
        assert_painted_text_color(&texts, "Voice unavailable", CHROME_WARN);
        assert_painted_text_color(&texts, "Example", CHROME_TEXT_DIM);
        assert_painted_text_color(&texts, "https://example.test/", CHROME_TEXT_DIM);
        assert_painted_text_color(
            &texts,
            "active CEF runtime does not match packaged manifest",
            CHROME_WARN,
        );
        assert_painted_text_color(&texts, "installer unavailable", CHROME_WARN);
        assert_painted_text_color(&texts, "STT runtime is not configured", CHROME_WARN);

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "status drawers must paint Browser heading line icons: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01),
            "speech drawer must paint Browser primary audio status icons: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .filter(|stroke| stroke.color == CHROME_WARN
                    && (stroke.width - 1.7).abs() < 0.01)
                .count()
                >= 5,
            "status drawer warning state and detail rows must paint Browser warning line icons: {lines:?}"
        );

        let paths = painted_path_strokes(&out.shapes);
        assert!(
            paths
                .iter()
                .filter(|stroke| stroke.color == CHROME_WARN
                    && (stroke.width - 1.7).abs() < 0.01)
                .count()
                >= 5,
            "status drawer warning state and detail rows must paint Browser warning path icons: {paths:?}"
        );
    }

    #[test]
    fn browser_spellcheck_error_uses_material_warning_status_row() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_spellcheck_error_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);

        assert_painted_text_color(&texts, "Spelling", CHROME_TEXT);
        assert_painted_text_color(&texts, "hunspell not installed", CHROME_WARN);
        assert!(
            !texts
                .iter()
                .any(|(text, color)| text == "hunspell not installed"
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
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
    fn browser_muted_notes_use_browser_material_text_tokens() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_empty_downloads_drawer_frame(&ctx);
        let texts = painted_text(&out.shapes);
        let empty_note = texts
            .iter()
            .find(|(text, _)| {
                text == "No browser downloads yet"
                    || text == "Transfers worker ledger is not present on this node"
            })
            .unwrap_or_else(|| panic!("empty downloads drawer note was not painted: {texts:?}"));

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

        let lines = painted_line_strokes(&out.shapes);
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "password toolbar anchor must paint a Browser line icon: {lines:?}"
        );
    }

    #[test]
    fn browser_dialog_prompt_messages_use_material_status_icons() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);
        let out = render_dialog_prompt_bars_frame(&ctx);
        let texts = painted_text(&out.shapes);

        for label in [
            "login.example wants to use a passkey on login.example via CEF",
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
        let text_lines = painted_line_strokes(&out.shapes);
        assert!(
            text_lines
                .iter()
                .any(|stroke| stroke.color == prompt_text_icon
                    && (stroke.width - 1.7).abs() < 0.01),
            "prompt bars must paint Browser lock/status line icons: {text_lines:?}"
        );
        assert!(
            text_lines
                .iter()
                .any(|stroke| stroke.color == prompt_warn_icon
                    && (stroke.width - 1.7).abs() < 0.01),
            "before-unload prompt must paint a Browser warning line icon: {text_lines:?}"
        );

        let path_strokes = painted_path_strokes(&out.shapes);
        assert!(
            path_strokes
                .iter()
                .any(|stroke| stroke.color == prompt_text_icon
                    && (stroke.width - 1.7).abs() < 0.01),
            "prompt bars must paint Browser security path icons: {path_strokes:?}"
        );
        assert!(
            path_strokes
                .iter()
                .any(|stroke| stroke.color == prompt_warn_icon
                    && (stroke.width - 1.7).abs() < 0.01),
            "before-unload prompt must paint a Browser warning path icon: {path_strokes:?}"
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

        let warning_lines = painted_line_strokes(&out.shapes);
        assert!(
            warning_lines
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "HTTP prompt must paint a Browser warning line icon: {warning_lines:?}"
        );
        let warning_paths = painted_path_strokes(&out.shapes);
        assert!(
            warning_paths
                .iter()
                .any(|stroke| stroke.color == CHROME_WARN && (stroke.width - 1.7).abs() < 0.01),
            "HTTP prompt must paint a Browser warning path icon: {warning_paths:?}"
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
            success_rect_strokes
                .iter()
                .any(|stroke| stroke.color == CHROME_PRIMARY && (stroke.width - 1.7).abs() < 0.01),
            "successful capture notice must paint the Browser Capture icon: {success_rect_strokes:?}"
        );

        let failed = render_capture_notice_frame(&ctx, "Capture failed: no painted page");
        let failed_texts = painted_text(&failed.shapes);
        assert_painted_text_color(
            &failed_texts,
            "Capture failed: no painted page",
            CHROME_ERROR,
        );
        assert!(
            !failed_texts
                .iter()
                .any(|(text, _)| text.starts_with("! ")),
            "capture error notice must not paint exclamation-prefixed warning text: {failed_texts:?}"
        );
        let failed_lines = painted_line_strokes(&failed.shapes);
        assert!(
            failed_lines
                .iter()
                .any(|stroke| stroke.color == CHROME_ERROR && (stroke.width - 1.7).abs() < 0.01),
            "failed capture notice must paint a Browser warning line icon: {failed_lines:?}"
        );
        let failed_paths = painted_path_strokes(&failed.shapes);
        assert!(
            failed_paths
                .iter()
                .any(|stroke| stroke.color == CHROME_ERROR && (stroke.width - 1.7).abs() < 0.01),
            "failed capture notice must paint a Browser warning path icon: {failed_paths:?}"
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
                text.contains("Engine: CEF / Chromium") && *color == CHROME_TEXT_DIM
            }),
            "tab hover card must paint the engine summary with Browser dim text: {texts:?}"
        );
        assert!(
            !texts.iter().any(|(text, color)| {
                text.contains("Engine: CEF / Chromium")
                    && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)
            }),
            "tab hover card leaked shared shell text color: {texts:?}"
        );

        let strokes = painted_rect_strokes(&out.shapes);
        assert!(
            strokes.iter().any(|stroke| {
                stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01
            }),
            "tab hover card must paint a Browser tab icon stroke: {strokes:?}"
        );
    }

    #[test]
    fn browser_chrome_tooltips_use_browser_material_text() {
        let ctx = egui::Context::default();
        mde_egui::fonts::install(&ctx);

        let out = render_chrome_tooltip_frame(&ctx);
        let texts = painted_text(&out.shapes);
        assert_painted_text_color(&texts, "Search tabs", CHROME_TEXT_DIM);
        assert!(
            !texts.iter().any(|(text, color)| text == "Search tabs"
                && matches!(*color, Style::TEXT | Style::TEXT_DIM | Style::TEXT_STRONG)),
            "Browser tooltip leaked shared shell text color: {texts:?}"
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
        assert!(
            lines
                .iter()
                .any(|stroke| stroke.color == CHROME_TEXT && (stroke.width - 1.7).abs() < 0.01),
            "bookmarks overflow anchor must paint a Browser line icon: {lines:?}"
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
            assert!(
                paths
                    .iter()
                    .any(|stroke| stroke.color == CHROME_TEXT_DIM
                        && (stroke.width - 1.7).abs() < 0.01),
                "{surface} bookmark button must paint a Browser bookmark path icon: {paths:?}"
            );
        }
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

        let dim_icon_lines = painted_line_strokes(&out.shapes)
            .iter()
            .filter(|stroke| stroke.color == CHROME_TEXT_DIM && (stroke.width - 1.7).abs() < 0.01)
            .count();
        assert!(
            dim_icon_lines >= 3,
            "print drawer copy stepper must paint Browser plus/minus icon strokes: {dim_icon_lines}"
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

        assert_painted_text_color(&texts, "main { line-height: 1.6; }", CHROME_TEXT);
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
    fn browser_chrome_uses_the_named_roboto_family() {
        assert_eq!(
            font_id(13.0).family,
            FontFamily::Name(std::sync::Arc::from(mde_egui::fonts::BROWSER_CHROME_FAMILY))
        );
    }
}
