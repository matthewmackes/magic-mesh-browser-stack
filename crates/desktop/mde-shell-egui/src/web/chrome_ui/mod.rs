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
    time::Instant,
};

use mde_egui::egui::{
    self, Color32, FontFamily, FontId, RichText, TextStyle, TextureHandle, TextureOptions,
};
use mde_egui::menubar::Entry;
use mde_egui::{muted_note, ChipTone, Style};
use mde_web_preview_client::{
    confusable_reason, host_of, BeforeUnloadDialog, CertError, ConfusableReason, CursorKind,
    EditCommand, JsDialog, SessionState,
};

mod accessibility;
mod body;
mod drawers;
use super::{
    browser_capture_dir, ellipsize, media_metadata_chip_label, BrowserEngine,
    BrowserOfflineCacheResult, ContainerProfile, DeviceProfile, DisplayTarget, FaviconCache,
    ManagedPolicyBlock, PendingPasskeyConsent, PixelRegion, Tab, UserAgentOverride, WebState,
    CHROME_BUTTON, CHROME_FONT, CHROME_GAP, CHROME_NEW_TAB_W, CHROME_OMNIBOX_H, CHROME_TAB_CLOSE,
    CHROME_TAB_H, CHROME_TAB_MIN_W, CHROME_TAB_PINNED_W, CHROME_TAB_W, MAX_CHANNEL_DIM,
    PRIVATE_MODE_EXPLAINER, RESIZE_DEBOUNCE,
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

const STATE_HOVER_ALPHA: u8 = 20;
const STATE_FOCUS_ALPHA: u8 = 26;
const STATE_PRESSED_ALPHA: u8 = 26;
const CHROME_TAB_RADIUS: f32 = 8.0;
const TAB_FAVICON_SIZE: f32 = 16.0;
const TAB_ENGINE_BADGE_H: f32 = 14.0;
const ENGINE_NEW_TAB_W: f32 = 134.0;
const ENGINE_SEGMENT_W: f32 = 118.0;
const ENGINE_CONTROL_H: f32 = 42.0;
const ENGINE_SEGMENT_RADIUS: f32 = 16.0;
const ACTION_BUTTON_RADIUS: f32 = 8.0;

/// The fixed slot width of one bookmark button on the single-row bar. Fixed so the
/// overflow split ([`bookmark_bar_visible_count`]) is exact — no font measuring.
pub(super) const BOOKMARK_BTN_W: f32 = 132.0;
/// The width reserved for the ">>" overflow menu button when the row can't hold
/// every bookmark.
pub(super) const BOOKMARK_OVERFLOW_W: f32 = 26.0;
/// The elision budget for a bookmark button's title (fits inside [`BOOKMARK_BTN_W`]
/// at [`CHROME_FONT`]); the full title rides the hover tooltip.
const BOOKMARK_TITLE_CHARS: usize = 18;

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

pub(super) const fn page_action_text(enabled: bool) -> Color32 {
    button_text(enabled)
}

pub(super) const fn page_action_star(
    has_page: bool,
    is_bookmarked: bool,
) -> (&'static str, Color32) {
    match (has_page, is_bookmarked) {
        (false, _) => ("\u{2606}", CHROME_TEXT_DIM),
        (true, true) => ("\u{2605}", CHROME_PRIMARY),
        (true, false) => ("\u{2606}", CHROME_TEXT),
    }
}

pub(super) const fn tab_fill(active: bool) -> Color32 {
    if active {
        CHROME_TOOLBAR
    } else {
        CHROME_SURFACE_CONTAINER
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

pub(super) const fn control_fill(selected: bool) -> Color32 {
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

pub(super) const fn engine_primary_label(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "CEF",
        BrowserEngine::Servo => "Servo",
    }
}

pub(super) const fn engine_supporting_label(engine: BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Cef => "Chromium",
        BrowserEngine::Servo => "Rust engine",
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

pub(super) const fn engine_new_tab_fill(engine: BrowserEngine) -> Color32 {
    engine_accent(engine)
}

pub(super) const fn engine_new_tab_text(_engine: BrowserEngine) -> &'static str {
    "New tab"
}

pub(super) const fn engine_new_tab_supporting_text(engine: BrowserEngine) -> &'static str {
    engine_display_name(engine)
}

pub(super) fn engine_tab_count_label(count: usize) -> String {
    if count == 1 {
        "1 tab".to_owned()
    } else {
        format!("{count} tabs")
    }
}

pub(super) const fn engine_segment_fill(engine: BrowserEngine, selected: BrowserEngine) -> Color32 {
    if matches!(
        (engine, selected),
        (BrowserEngine::Cef, BrowserEngine::Cef) | (BrowserEngine::Servo, BrowserEngine::Servo)
    ) {
        engine_container(engine)
    } else {
        CHROME_TOOLBAR
    }
}

pub(super) const fn engine_segment_text(engine: BrowserEngine, selected: BrowserEngine) -> Color32 {
    if matches!(
        (engine, selected),
        (BrowserEngine::Cef, BrowserEngine::Cef) | (BrowserEngine::Servo, BrowserEngine::Servo)
    ) {
        engine_on_container(engine)
    } else {
        CHROME_TEXT
    }
}

pub(super) const fn engine_segment_supporting_text(
    engine: BrowserEngine,
    selected: BrowserEngine,
) -> Color32 {
    if matches!(
        (engine, selected),
        (BrowserEngine::Cef, BrowserEngine::Cef) | (BrowserEngine::Servo, BrowserEngine::Servo)
    ) {
        engine_on_container(engine)
    } else {
        CHROME_TEXT_DIM
    }
}

pub(super) const fn engine_segment_stroke(
    engine: BrowserEngine,
    selected: BrowserEngine,
) -> Color32 {
    if matches!(
        (engine, selected),
        (BrowserEngine::Cef, BrowserEngine::Cef) | (BrowserEngine::Servo, BrowserEngine::Servo)
    ) {
        engine_accent(engine)
    } else {
        CHROME_OUTLINE
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
) -> egui::Response {
    // `click_and_drag` keeps activation, middle-click close, and drag-reorder on
    // the same browser-tab affordance while egui handles the click/drag threshold.
    let response = ui.add(
        egui::Button::new("")
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(CHROME_TAB_RADIUS)
            .min_size(egui::vec2(width, CHROME_TAB_H))
            .sense(egui::Sense::click_and_drag()),
    );
    let accent = engine_accent(engine);
    let r = response.rect;
    let pressed = response.is_pointer_button_down_on();
    let active_fill = state_layer(CHROME_TOOLBAR, accent, 12);
    let inactive_fill = state_layer(CHROME_SURFACE_CONTAINER, accent, 6);
    let fill = if active {
        if pressed {
            state_layer(active_fill, accent, STATE_PRESSED_ALPHA)
        } else if response.hovered() {
            state_layer(active_fill, accent, STATE_HOVER_ALPHA)
        } else {
            active_fill
        }
    } else if pressed {
        state_layer(CHROME_SURFACE_CONTAINER, CHROME_TEXT, STATE_PRESSED_ALPHA)
    } else if response.hovered() {
        state_layer(CHROME_SURFACE_CONTAINER, accent, STATE_HOVER_ALPHA)
    } else {
        inactive_fill
    };

    let shadow = egui::Rect::from_min_max(
        egui::pos2(r.left() + 3.0, r.bottom() - 1.0),
        egui::pos2(r.right() - 3.0, r.bottom()),
    );
    ui.painter().rect_filled(
        shadow,
        1.0,
        Color32::from_black_alpha(if active { 18 } else { 8 }),
    );
    ui.painter().rect(
        r,
        CHROME_TAB_RADIUS,
        fill,
        egui::Stroke::new(
            if active { 1.25 } else { 1.0 },
            if active || response.hovered() {
                state_layer(CHROME_OUTLINE, accent, if active { 128 } else { 96 })
            } else {
                tab_stroke(active)
            },
        ),
        egui::StrokeKind::Inside,
    );

    let indicator = egui::Rect::from_min_max(
        egui::pos2(
            r.left() + 7.0,
            if active { r.top() } else { r.bottom() - 2.0 },
        ),
        egui::pos2(
            if active {
                r.right() - 7.0
            } else {
                (r.left() + 24.0).min(r.right() - 7.0)
            },
            if active { r.top() + 3.0 } else { r.bottom() },
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
                state_layer(CHROME_TOOLBAR, accent, 32)
            };
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
                engine_marker(engine),
                font_id(CHROME_FONT - 2.5),
                if active { CHROME_TOOLBAR } else { accent },
            );
        }

        let text_left = icon_rect.right() + 6.0;
        let text_right = if show_badge {
            badge.left() - 6.0
        } else {
            r.right() - 8.0
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

fn tab_engine_badge_width(engine: BrowserEngine) -> f32 {
    match engine {
        BrowserEngine::Cef => 28.0,
        BrowserEngine::Servo => 42.0,
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
        state_layer(CHROME_TOOLBAR, accent, if active { 36 } else { 22 }),
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
    ui.add(
        egui::Button::new(
            egui::RichText::new("\u{00D7}")
                .size(CHROME_FONT)
                .color(CHROME_TEXT_DIM),
        )
        .fill(Color32::TRANSPARENT)
        .corner_radius(CHROME_TAB_CLOSE / 2.0)
        .min_size(egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H)),
    )
    .on_hover_text("Close tab")
}

/// Which speaker glyph (and hover label) a tab shows, if any.
pub(super) fn audio_glyph_for(audible: bool, muted: bool) -> Option<(&'static str, &'static str)> {
    if muted {
        Some(("\u{1F507}", "Unmute tab")) // 🔇
    } else if audible {
        Some(("\u{1F50A}", "Mute tab")) // 🔊
    } else {
        None
    }
}

pub(super) fn tab_audio_glyph(
    ui: &mut egui::Ui,
    audible: bool,
    muted: bool,
) -> Option<egui::Response> {
    let (glyph, hover) = audio_glyph_for(audible, muted)?;
    Some(
        ui.add(
            egui::Button::new(
                egui::RichText::new(glyph)
                    .size(CHROME_FONT)
                    .color(CHROME_TEXT_DIM),
            )
            .fill(Color32::TRANSPARENT)
            .corner_radius(CHROME_TAB_CLOSE / 2.0)
            .min_size(egui::vec2(CHROME_TAB_CLOSE, CHROME_TAB_H)),
        )
        .on_hover_text(hover),
    )
}

pub(super) fn compact_menu_item(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(CHROME_FONT)
            .color(CHROME_TEXT),
    )
    .min_size(egui::vec2(124.0, CHROME_TAB_H))
}

/// Render the Browser-local toolbar command menu and return any picked action.
fn chrome_menu_button(state: &WebState, ui: &mut egui::Ui) -> Option<super::menubar::MenuAction> {
    let menus = super::menubar::chrome_menus(state);
    let mut picked = None;
    ui.menu_button(
        RichText::new("\u{22EE}")
            .size(CHROME_FONT + 2.0)
            .color(CHROME_TEXT),
        |ui| {
            ui.set_min_width(220.0);
            for menu in &menus {
                ui.menu_button(
                    RichText::new(menu.title.as_str())
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT),
                    |ui| render_chrome_menu_entries(ui, &menu.entries, &mut picked),
                );
            }
        },
    )
    .response
    .on_hover_text("Customize and control Browser");
    picked
}

fn render_chrome_menu_entries(
    ui: &mut egui::Ui,
    entries: &[Entry<super::menubar::MenuAction>],
    picked: &mut Option<super::menubar::MenuAction>,
) {
    for entry in entries {
        match entry {
            Entry::Item(item) => {
                let mut label = String::new();
                if item.checked == Some(true) {
                    label.push_str("\u{2713} ");
                }
                label.push_str(&item.label);
                if let Some(shortcut) = &item.shortcut {
                    label.push_str("    ");
                    label.push_str(shortcut);
                }
                let response = ui.add_enabled(
                    item.enabled,
                    egui::Button::new(
                        RichText::new(label)
                            .size(CHROME_FONT)
                            .color(button_text(item.enabled)),
                    )
                    .fill(menu_item_fill(item.checked == Some(true))),
                );
                if response.clicked() && item.enabled {
                    *picked = Some(item.id);
                    ui.close_menu();
                }
            }
            Entry::Submenu { label, entries, .. } => {
                ui.menu_button(
                    RichText::new(label.as_str())
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT),
                    |ui| render_chrome_menu_entries(ui, entries, picked),
                );
            }
            Entry::Separator => {
                ui.separator();
            }
            Entry::Caption(caption) => {
                ui.label(
                    RichText::new(caption.as_str())
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT_DIM),
                );
            }
        }
    }
}

pub(super) fn tab_label(tab: &Tab) -> String {
    let title = tab.session.title().trim();
    let url = tab.session.nav().url.trim();
    let base = if !title.is_empty() {
        title
    } else if !url.is_empty() {
        url
    } else {
        "New tab"
    };
    let mut markers = String::new();
    if tab.idle_suspended {
        markers.push_str("\u{25D2} ");
    } else {
        match tab.session.state() {
            SessionState::Loading => markers.push_str("\u{25CC} "),
            SessionState::Live => {}
            SessionState::Crashed { .. } => markers.push_str("! "),
        }
    }
    markers.push_str(tab.container.marker());
    markers.push_str(tab.display_target.marker());
    if tab.muted {
        markers.push_str("M ");
    }
    if tab.autoplay_blocked {
        markers.push_str("A ");
    }
    if tab.force_dark {
        markers.push_str("D ");
    }
    if tab.reader_mode {
        markers.push_str("R ");
    }
    if tab.user_scripts {
        markers.push_str("S ");
    }
    markers.push_str(tab.user_agent.marker());
    markers.push_str(tab.device_profile.marker());

    if markers.is_empty() {
        ellipsize(base, 28)
    } else {
        format!("{markers}{}", ellipsize(base, 24))
    }
}

pub(super) fn tab_hover(tab: &Tab) -> String {
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
    ui.label(tab_hover(tab));
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
            q.is_empty()
                || tab.session.title().to_ascii_lowercase().contains(&q)
                || tab.session.nav().url.to_ascii_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// A one-line label for a tab-search result row: page title, URL, then "New tab".
fn tab_search_row_label(tab: &Tab) -> String {
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

/// Chrome's "Search tabs" dropdown: live-filtered, clickable tab chooser.
pub(super) fn tab_search_menu(ui: &mut egui::Ui, state: &mut WebState) {
    let mut select: Option<usize> = None;
    ui.menu_button(
        egui::RichText::new("\u{1F50D}") // 🔍
            .size(CHROME_FONT)
            .color(CHROME_TEXT_DIM),
        |ui| {
            ui.set_min_width(300.0);
            ui.add(
                egui::TextEdit::singleline(&mut state.tab_search_query)
                    .hint_text("Search tabs")
                    .desired_width(f32::INFINITY),
            );
            ui.separator();
            let matches = matching_tab_indices(&state.tabs, &state.tab_search_query);
            egui::ScrollArea::vertical()
                .max_height(260.0)
                .show(ui, |ui| {
                    if matches.is_empty() {
                        ui.weak("No matching tabs");
                    }
                    for idx in matches {
                        let active = idx == state.active;
                        let label = tab_search_row_label(&state.tabs[idx]);
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new(label)
                                        .size(CHROME_FONT)
                                        .color(selected_text(active)),
                                )
                                .fill(row_fill(active))
                                .min_size(egui::vec2(288.0, CHROME_TAB_H)),
                            )
                            .clicked()
                        {
                            select = Some(idx);
                            ui.close_menu();
                        }
                    }
                });
        },
    )
    .response
    .on_hover_text("Search tabs");
    if let Some(idx) = select {
        state.select_tab(idx);
        state.tab_search_query.clear();
    }
}

pub(super) fn engine_new_tab_buttons(ui: &mut egui::Ui, state: &mut WebState, vertical: bool) {
    let selected = state.engine;
    let cef_tabs = state
        .tabs
        .iter()
        .filter(|tab| tab.engine == BrowserEngine::Cef)
        .count();
    let servo_tabs = state
        .tabs
        .iter()
        .filter(|tab| tab.engine == BrowserEngine::Servo)
        .count();
    let mut open_selected = false;
    let mut selected_engine = None;

    let mut controls = |ui: &mut egui::Ui, full_width: bool| {
        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
        let plus_w = if full_width {
            ui.available_width()
        } else {
            ENGINE_NEW_TAB_W
        };
        if engine_new_tab_button(ui, selected, plus_w).clicked() {
            open_selected = true;
        }
        if full_width {
            ui.add_space(CHROME_GAP);
        }
        for engine in [BrowserEngine::Cef, BrowserEngine::Servo] {
            let width = if full_width {
                ui.available_width()
            } else {
                ENGINE_SEGMENT_W
            };
            let count = match engine {
                BrowserEngine::Cef => cef_tabs,
                BrowserEngine::Servo => servo_tabs,
            };
            if engine_segment_button(ui, engine, selected, width, count).clicked() {
                selected_engine = Some(engine);
            }
        }
    };

    let dock = if vertical {
        egui::Frame::NONE
            .fill(state_layer(
                CHROME_SURFACE_CONTAINER_HIGH,
                engine_accent(selected),
                12,
            ))
            .stroke(egui::Stroke::new(
                1.0,
                state_layer(CHROME_OUTLINE, engine_accent(selected), 80),
            ))
            .corner_radius(ENGINE_SEGMENT_RADIUS)
            .inner_margin(egui::Margin::symmetric(5, 5))
            .show(ui, |ui| {
                ui.vertical(|ui| controls(ui, true));
            })
    } else {
        egui::Frame::NONE
            .fill(state_layer(
                CHROME_SURFACE_CONTAINER_HIGH,
                engine_accent(selected),
                12,
            ))
            .stroke(egui::Stroke::new(
                1.0,
                state_layer(CHROME_OUTLINE, engine_accent(selected), 80),
            ))
            .corner_radius(ENGINE_SEGMENT_RADIUS)
            .inner_margin(egui::Margin::symmetric(5, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| controls(ui, false));
            })
    };
    let dock_rect = dock.response.rect;
    let rail = if vertical {
        egui::Rect::from_min_max(
            egui::pos2(dock_rect.left() + 1.0, dock_rect.top() + 10.0),
            egui::pos2(dock_rect.left() + 3.0, dock_rect.bottom() - 10.0),
        )
    } else {
        egui::Rect::from_min_max(
            egui::pos2(dock_rect.left() + 12.0, dock_rect.top() + 1.0),
            egui::pos2(dock_rect.right() - 12.0, dock_rect.top() + 3.0),
        )
    };
    ui.painter()
        .rect_filled(rail, 1.0, engine_accent(selected).gamma_multiply(0.72));

    if let Some(engine) = selected_engine {
        state.select_engine(engine);
    }
    if open_selected {
        state.request_new_tab(selected);
    }
}

fn engine_new_tab_button(ui: &mut egui::Ui, engine: BrowserEngine, width: f32) -> egui::Response {
    let response = ui.add(
        egui::Button::new("")
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(ENGINE_SEGMENT_RADIUS)
            .min_size(egui::vec2(width, ENGINE_CONTROL_H)),
    );
    let r = response.rect;
    let accent = engine_new_tab_fill(engine);
    let fill = if response.is_pointer_button_down_on() {
        state_layer(accent, CHROME_TEXT, STATE_PRESSED_ALPHA)
    } else if response.hovered() {
        state_layer(accent, CHROME_TOOLBAR, STATE_HOVER_ALPHA)
    } else {
        accent
    };
    let shadow = egui::Rect::from_min_max(
        egui::pos2(r.left() + 5.0, r.bottom() - 1.0),
        egui::pos2(r.right() - 5.0, r.bottom()),
    );
    ui.painter()
        .rect_filled(shadow, 1.0, Color32::from_black_alpha(22));
    ui.painter().rect(
        r,
        ENGINE_SEGMENT_RADIUS,
        fill,
        egui::Stroke::new(1.0, accent),
        egui::StrokeKind::Inside,
    );
    let highlight = egui::Rect::from_min_max(
        egui::pos2(r.left() + 10.0, r.top() + 1.0),
        egui::pos2(r.right() - 10.0, r.top() + 2.0),
    );
    ui.painter()
        .rect_filled(highlight, 1.0, Color32::from_white_alpha(72));

    let glyph = egui::Rect::from_center_size(
        egui::pos2(r.left() + 17.0, r.center().y),
        egui::vec2(18.0, 18.0),
    );
    ui.painter()
        .circle_filled(glyph.center(), 9.0, Color32::from_white_alpha(48));
    ui.painter().text(
        glyph.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        font_id(CHROME_FONT + 3.0),
        CHROME_TOOLBAR,
    );
    let text_left = glyph.right() + 7.0;
    let clip = egui::Rect::from_min_max(
        egui::pos2(text_left, r.top()),
        egui::pos2(r.right() - 8.0, r.bottom()),
    );
    let painter = ui.painter().with_clip_rect(clip);
    painter.text(
        egui::pos2(text_left, r.top() + 14.0),
        egui::Align2::LEFT_CENTER,
        engine_new_tab_text(engine),
        font_id(CHROME_FONT + 1.0),
        CHROME_TOOLBAR,
    );
    painter.text(
        egui::pos2(text_left, r.bottom() - 11.0),
        egui::Align2::LEFT_CENTER,
        engine_new_tab_supporting_text(engine),
        font_id(CHROME_FONT - 2.0),
        Color32::from_white_alpha(210),
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), r, response.has_focus());
    response.on_hover_text(format!("Open a new {} tab", engine_display_name(engine)))
}

fn engine_segment_button(
    ui: &mut egui::Ui,
    engine: BrowserEngine,
    selected: BrowserEngine,
    width: f32,
    tab_count: usize,
) -> egui::Response {
    let is_selected = engine == selected;
    let response = ui.add(
        egui::Button::new("")
            .fill(Color32::TRANSPARENT)
            .stroke(egui::Stroke::NONE)
            .corner_radius(ENGINE_SEGMENT_RADIUS)
            .min_size(egui::vec2(width, ENGINE_CONTROL_H)),
    );
    let r = response.rect;
    let accent = engine_accent(engine);
    let base_fill = engine_segment_fill(engine, selected);
    let fill = if response.is_pointer_button_down_on() {
        state_layer(base_fill, CHROME_TEXT, STATE_PRESSED_ALPHA)
    } else if response.hovered() && !is_selected {
        state_layer(base_fill, accent, STATE_HOVER_ALPHA)
    } else {
        base_fill
    };
    ui.painter().rect(
        r,
        ENGINE_SEGMENT_RADIUS,
        fill,
        egui::Stroke::new(
            if is_selected { 1.5 } else { 1.0 },
            engine_segment_stroke(engine, selected),
        ),
        egui::StrokeKind::Inside,
    );
    if is_selected {
        let glow = egui::Rect::from_min_max(
            egui::pos2(r.left() + 7.0, r.top() + 1.0),
            egui::pos2(r.right() - 7.0, r.top() + 3.0),
        );
        ui.painter().rect_filled(glow, 1.0, accent);
    }

    let marker = egui::Rect::from_center_size(
        egui::pos2(r.left() + 18.0, r.center().y),
        egui::vec2(22.0, 22.0),
    );
    ui.painter().rect_filled(
        marker,
        11.0,
        if is_selected {
            accent
        } else {
            state_layer(CHROME_TOOLBAR, accent, 40)
        },
    );
    ui.painter().text(
        marker.center(),
        egui::Align2::CENTER_CENTER,
        engine_glyph(engine),
        font_id(CHROME_FONT - 1.0),
        if is_selected { CHROME_TOOLBAR } else { accent },
    );

    let text_left = marker.right() + 7.0;
    let clip = egui::Rect::from_min_max(
        egui::pos2(text_left, r.top()),
        egui::pos2(r.right() - 6.0, r.bottom()),
    );
    let painter = ui.painter().with_clip_rect(clip);
    painter.text(
        egui::pos2(text_left, r.top() + 14.0),
        egui::Align2::LEFT_CENTER,
        engine_primary_label(engine),
        font_id(CHROME_FONT + 0.5),
        engine_segment_text(engine, selected),
    );
    let supporting = format!(
        "{} / {}",
        engine_supporting_label(engine),
        engine_tab_count_label(tab_count)
    );
    painter.text(
        egui::pos2(text_left, r.bottom() - 11.0),
        egui::Align2::LEFT_CENTER,
        supporting,
        font_id(CHROME_FONT - 2.0),
        engine_segment_supporting_text(engine, selected),
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), r, response.has_focus());
    response.on_hover_text(format!(
        "Use {} for future Browser tabs",
        engine_display_name(engine)
    ))
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
                    );
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
                    tab_response
                        .on_hover_ui(|ui| tab_hover_card(ui, tab))
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
                            let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                            if ui.add(compact_menu_item(pin_label)).clicked() {
                                pin_tab = Some((idx, !tab.pinned));
                                ui.close_menu();
                            }
                            if ui.add(compact_menu_item("Duplicate tab")).clicked() {
                                duplicate_tab_idx = Some(idx);
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    state.tabs.len() > 1,
                                    compact_menu_item("Close other tabs"),
                                )
                                .clicked()
                            {
                                close_others_idx = Some(idx);
                                ui.close_menu();
                            }
                            if ui
                                .add_enabled(
                                    idx + 1 < state.tabs.len(),
                                    compact_menu_item("Close tabs to the right"),
                                )
                                .clicked()
                            {
                                close_right_idx = Some(idx);
                                ui.close_menu();
                            }
                            if tab.group.is_none() {
                                if ui.add(compact_menu_item("Add tab to new group")).clicked() {
                                    group_tab = Some(idx);
                                    ui.close_menu();
                                }
                            } else if ui.add(compact_menu_item("Remove from group")).clicked() {
                                ungroup_tab_idx = Some(idx);
                                ui.close_menu();
                            }
                            let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                            if ui.add(compact_menu_item(mute_label)).clicked() {
                                mute_tab = Some((idx, !tab.muted));
                                ui.close_menu();
                            }
                            let autoplay_label = if tab.autoplay_blocked {
                                "Allow autoplay"
                            } else {
                                "Block autoplay"
                            };
                            if ui.add(compact_menu_item(autoplay_label)).clicked() {
                                autoplay_tab = Some((idx, !tab.autoplay_blocked));
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
                    // Speaker glyph for an audible/muted tab, click-to-mute.
                    if let Some(audio) = tab_audio_glyph(ui, tab.session.audible(), tab.muted) {
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
                engine_new_tab_buttons(ui, state, false);
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

    // Resolve/cache each tab's favicon texture BEFORE the (immutable) pill loop
    // below — see `resolve_tab_favicon_textures`.
    let favicon_textures = resolve_tab_favicon_textures(ui.ctx(), &mut state.tabs);

    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::same(4))
        .show(ui, |ui| {
            ui.set_width(184.0);
            egui::ScrollArea::vertical()
                .id_salt("browser-vertical-tabs")
                .max_height(ui.available_height())
                .show(ui, |ui| {
                    for (idx, tab) in state.tabs.iter().enumerate() {
                        let active = idx == state.active;
                        // Pinned tabs collapse to a compact favicon-only pill.
                        let label = if tab.pinned {
                            String::new()
                        } else {
                            tab_label(tab)
                        };
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
                            );
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
                                let pin_label = if tab.pinned { "Unpin tab" } else { "Pin tab" };
                                if ui.add(compact_menu_item(pin_label)).clicked() {
                                    pin_tab = Some((idx, !tab.pinned));
                                    ui.close_menu();
                                }
                                if ui.add(compact_menu_item("Duplicate tab")).clicked() {
                                    duplicate_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        state.tabs.len() > 1,
                                        compact_menu_item("Close other tabs"),
                                    )
                                    .clicked()
                                {
                                    close_others_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if ui
                                    .add_enabled(
                                        idx + 1 < state.tabs.len(),
                                        compact_menu_item("Close tabs to the right"),
                                    )
                                    .clicked()
                                {
                                    close_right_idx = Some(idx);
                                    ui.close_menu();
                                }
                                if tab.group.is_none() {
                                    if ui.add(compact_menu_item("Add tab to new group")).clicked() {
                                        group_tab = Some(idx);
                                        ui.close_menu();
                                    }
                                } else if ui.add(compact_menu_item("Remove from group")).clicked() {
                                    ungroup_tab_idx = Some(idx);
                                    ui.close_menu();
                                }
                                let mute_label = if tab.muted { "Unmute tab" } else { "Mute tab" };
                                if ui.add(compact_menu_item(mute_label)).clicked() {
                                    mute_tab = Some((idx, !tab.muted));
                                    ui.close_menu();
                                }
                                let autoplay_label = if tab.autoplay_blocked {
                                    "Allow autoplay"
                                } else {
                                    "Block autoplay"
                                };
                                if ui.add(compact_menu_item(autoplay_label)).clicked() {
                                    autoplay_tab = Some((idx, !tab.autoplay_blocked));
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
                            // Speaker glyph for an audible/muted tab, click-to-mute.
                            if let Some(audio) =
                                tab_audio_glyph(ui, tab.session.audible(), tab.muted)
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
                    engine_new_tab_buttons(ui, state, true);
                    tab_search_menu(ui, state);
                });
        });

    // Resolve a settled vertical drag to a concrete reorder against the pills.
    if let (Some(from), Some(pointer)) = (drag_from, drop_pointer) {
        if let Some(to) = tab_drag_target_index(&pill_rects, pointer, TabAxis::Vertical) {
            if to != from {
                move_tab = Some((from, to));
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
    } else if let Some(idx) = close {
        state.close_tab(idx);
    } else if let Some(idx) = select {
        state.select_tab(idx);
    }
}

/// Which way a tab strip runs, so the shared drag-reorder hit-test knows whether
/// to compare drop points along X (horizontal strip) or Y (vertical strip).
#[derive(Clone, Copy)]
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
        ui.label(
            egui::RichText::new(PRIVATE_MODE_EXPLAINER)
                .small()
                .color(CHROME_TEXT_DIM),
        );
        ui.add_space(Style::SP_M);
        ui.horizontal(|ui| {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut state.dashboard_query)
                    .desired_width(420.0)
                    .hint_text("Search the mesh")
                    .text_color(CHROME_TEXT),
            );
            state.chrome_edit_focus |= resp.has_focus();
            submit_search = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("Search").color(CHROME_TEXT),
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
                            egui::RichText::new(service.label.as_str())
                                .size(Style::BODY)
                                .color(CHROME_TEXT),
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

/// A compact Browser chrome button in the local Material palette.
pub(super) fn nav_button(ui: &mut egui::Ui, glyph: &str, tip: &str, enabled: bool) -> bool {
    ui.add_enabled(
        enabled,
        egui::Button::new(
            egui::RichText::new(glyph)
                .size(CHROME_FONT)
                .color(button_text(enabled)),
        )
        .fill(control_fill(false))
        .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
    )
    .on_hover_text(tip)
    .clicked()
}

/// Chrome/Edge-style trust signal for the omnibox's leading security chip,
/// derived purely from a URL's scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SecurityLevel {
    /// `https://` — a lock glyph, neutral tone.
    Secure,
    /// `http://` — a "Not secure" glyph/tone.
    NotSecure,
    /// `mesh://` and mesh-hosted services — trusted overlay.
    Mesh,
    /// `about:` / blank / new-tab / any other scheme.
    Neutral,
}

impl SecurityLevel {
    pub(super) const fn glyph(self) -> &'static str {
        match self {
            Self::Secure => "\u{1F512}",
            Self::NotSecure => "\u{26A0}",
            Self::Mesh => "\u{1F6E1}",
            Self::Neutral => "\u{1F50E}",
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Secure => "Secure connection (HTTPS)",
            Self::NotSecure => "Not secure \u{2014} plain HTTP",
            Self::Mesh => "Mesh \u{2014} trusted overlay connection",
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
        if ui
            .add_enabled(can_back, egui::Button::new("Back"))
            .clicked()
        {
            action = Some(PageContextAction::Back);
            ui.close_menu();
        }
        if ui
            .add_enabled(can_forward, egui::Button::new("Forward"))
            .clicked()
        {
            action = Some(PageContextAction::Forward);
            ui.close_menu();
        }
        if ui.button("Reload").clicked() {
            action = Some(PageContextAction::Reload);
            ui.close_menu();
        }
        ui.separator();
        if ui.button("Cut").clicked() {
            action = Some(PageContextAction::Edit(EditCommand::Cut));
            ui.close_menu();
        }
        if ui.button("Copy").clicked() {
            action = Some(PageContextAction::Edit(EditCommand::Copy));
            ui.close_menu();
        }
        if ui.button("Paste").clicked() {
            action = Some(PageContextAction::Edit(EditCommand::Paste));
            ui.close_menu();
        }
        if ui.button("Select all").clicked() {
            action = Some(PageContextAction::Edit(EditCommand::SelectAll));
            ui.close_menu();
        }
        ui.separator();
        if ui
            .add_enabled(!url.is_empty(), egui::Button::new("Copy page URL"))
            .clicked()
        {
            ui.ctx().copy_text(url.to_owned());
            ui.close_menu();
        }
    });
    action
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
    let mut toolbar_action: Option<super::menubar::MenuAction> = None;
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
        let can_stop = super::can_show_stop_control(has_tab, crashed, nav.loading, active_engine);
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
            .button(
                egui::RichText::new(downloads_label)
                    .size(CHROME_FONT)
                    .color(if state.downloads_open {
                        CHROME_PRIMARY
                    } else {
                        CHROME_TEXT
                    }),
            )
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
            // The by-domain breakdown behind the count (uBlock-style detail): the top
            // blocked domains for THIS page, surfaced on hover of the shield.
            let top_blocked = state
                .tabs
                .get(state.active)
                .map(|t| t.session.block_tally().top_domains(6))
                .unwrap_or_default();
            ui.label(
                egui::RichText::new(format!("\u{2298} {blocked}"))
                    .size(CHROME_FONT)
                    .color(CHROME_TEXT_DIM),
            )
            .on_hover_ui(|ui| {
                ui.label(format!(
                    "Ad-filter blocked {blocked} request{} on this page",
                    if blocked == 1 { "" } else { "s" }
                ));
                if !top_blocked.is_empty() {
                    ui.separator();
                    for (domain, count) in &top_blocked {
                        ui.label(
                            egui::RichText::new(format!("{count}\u{00D7}  {domain}"))
                                .small()
                                .color(CHROME_TEXT_DIM),
                        );
                    }
                }
            });
        }

        ui.add_space(CHROME_GAP);

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
        let field = egui::TextEdit::singleline(&mut state.address)
            .id(super::omnibox_widget_id())
            .desired_width((ui.available_width() - (CHROME_BUTTON * 2.0 + Style::SP_XL)).max(160.0))
            .hint_text("Enter an address")
            .text_color(CHROME_TEXT)
            .font(egui::TextStyle::Small)
            .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H));
        let resp = ui.add_enabled(has_tab && !crashed, field);
        // Latch omnibox focus for next frame's engine-sync + accelerator
        // guards (the same tracked-focus idiom as `Tab::page_focused`).
        state.omnibox_focused = resp.has_focus();
        state.chrome_edit_focus |= resp.has_focus();
        // The branded 2px accent focus ring (mde_egui::focus, the dock/Console/Start
        // idiom) on the primary keyboard target, so keyboard-only users get a clear,
        // consistent focus indicator instead of egui's faint default outline (a11y).
        mde_egui::focus::paint_focus_ring(ui.painter(), resp.rect, resp.has_focus());
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
                let bg = ui.visuals().extreme_bg_color;
                let corner_radius = ui.visuals().widgets.inactive.corner_radius;
                ui.painter().rect_filled(resp.rect, corner_radius, bg);
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

        let go = ui
            .add_enabled(
                has_tab && !crashed && !state.address.trim().is_empty(),
                egui::Button::new(
                    egui::RichText::new("\u{2192}")
                        .size(CHROME_FONT)
                        .color(CHROME_TEXT),
                )
                .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
            )
            .on_hover_text("Go")
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

        toolbar_action = chrome_menu_button(state, ui);
    });
    if let Some(action) = toolbar_action {
        super::menubar::apply(ui.ctx(), state, action);
    }
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
    let text = page_action_text(has_page);
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(egui::RichText::new("\u{2606}  Add bookmark").color(text)),
        )
        .clicked()
    {
        super::publish(
            super::ACTION_BOOKMARKS_ADD,
            &super::bookmark_add_body(url, title),
        );
        ui.close_menu();
    }
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(egui::RichText::new("\u{29C9}  Copy URL").color(text)),
        )
        .clicked()
    {
        ui.ctx().copy_text(url.to_string());
        ui.close_menu();
    }
    if ui
        .add_enabled(
            has_page,
            egui::Button::new(egui::RichText::new("\u{1F4AC}  Send in Chat").color(text)),
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
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    egui::RichText::new(format!("{}  Share to {}", "\u{21AA}", target.label()))
                        .color(text),
                ),
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
        if ui
            .add_enabled(
                has_page,
                egui::Button::new(
                    egui::RichText::new(format!("{}  Send tab to {}", "\u{21E5}", target.label()))
                        .color(text),
                ),
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
    let (glyph, color) = page_action_star(has_page, is_bookmarked);
    let tip = if is_bookmarked {
        "Bookmarked \u{2014} page actions: edit bookmark, copy URL, share"
    } else {
        "Page actions \u{2014} bookmark, copy URL, share"
    };
    ui.menu_button(
        egui::RichText::new(glyph).size(CHROME_FONT).color(color),
        |ui| {
            page_actions_menu(ui, bus_root, engine, url, title);
        },
    )
    .response
    .on_hover_text(tip);
}

/// SECURITY-INFO — the plain-language headline for a [`SecurityLevel`].
pub(super) const fn security_headline(level: SecurityLevel) -> &'static str {
    match level {
        SecurityLevel::Secure => "Connection is secure",
        SecurityLevel::NotSecure => "Your connection to this site is not secure",
        SecurityLevel::Mesh => "Mesh service \u{2014} trusted overlay",
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
                "Punycode/IDN host (xn--) \u{2014} verify this is the site you expect"
            }
            ConfusableReason::ConfusableBlock => {
                "Look-alike letters (Cyrillic/Greek) \u{2014} this host may impersonate another site"
            }
            ConfusableReason::MixedScript => {
                "Mixed-script host \u{2014} letters from more than one alphabet can spoof a name"
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
        .then_some("Certificate: valid \u{2014} the connection is encrypted");
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
        ui.label(
            egui::RichText::new(summary.security.glyph())
                .size(CHROME_FONT)
                .color(security_color),
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
        ui.label(
            egui::RichText::new(format!("\u{26A0} {warn}"))
                .small()
                .color(CHROME_WARN),
        );
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
        ui.label(
            egui::RichText::new(format!(
                "\u{26A0} Managed policy blocked: {} resource{suffix}",
                resources.managed_policy_blocks
            ))
            .small()
            .color(CHROME_WARN),
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
        ui.label(
            egui::RichText::new(format!(
                "\u{26A0} Unsafe content blocked: {} resource{suffix}",
                resources.safe_browsing_blocks
            ))
            .small()
            .color(CHROME_WARN),
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
        ui.label(
            egui::RichText::new(format!(
                "\u{26A0} Insecure content blocked: {} public HTTP subresource{suffix}",
                resources.mixed_content_blocks
            ))
            .small()
            .color(CHROME_WARN),
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
        ui.label(
            egui::RichText::new(format!(
                "Privacy protection blocked: {} tracker/filter resource{suffix}",
                resources.tracker_blocks
            ))
            .small()
            .color(CHROME_TEXT_DIM),
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
        ui.separator();
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
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!(
                        "{} permissions were forgotten; future requests re-prompt under default deny",
                        permissions.host
                    ))
                    .small()
                    .color(CHROME_WARN),
                )
                .wrap(),
            );
        }
    }
    ui.separator();
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
    let resp = ui
        .add(
            egui::Button::new(
                egui::RichText::new(security.glyph())
                    .size(CHROME_FONT)
                    .color(tone_color(security.tone())),
            )
            .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
        )
        .on_hover_text(security.label());
    if resp.clicked() {
        ui.memory_mut(|mem| mem.toggle_popup(popup_id));
    }
    egui::popup_below_widget(
        ui,
        popup_id,
        &resp,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| site_info_panel(ui, page_url, recent_resources, permissions),
    );
}

/// Omnibox search `items` with any entry that duplicates a history hit removed
/// (a history-matched URL is already shown once, above, by
/// [`suggestions_panel`] — Chrome-style history-then-search ordering with no
/// repeats). Pure and paint-free so it's directly unit-testable.
pub(super) fn dedup_search_items<'a>(items: &'a [String], history: &[String]) -> Vec<&'a String> {
    items.iter().filter(|s| !history.contains(s)).collect()
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
            muted_note(ui, "Bookmarks");
            for bm in bookmarks {
                let clicked = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(format!("\u{2605} {}", ellipsize(&bm.title, 32)))
                                .size(CHROME_FONT)
                                .color(CHROME_PRIMARY),
                        )
                        .fill(fill_for(idx))
                        .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                    )
                    .on_hover_text(format!("Bookmark: {}", bm.url))
                    .clicked();
                if clicked {
                    accepted = Some(bm.url.clone());
                }
                idx += 1;
            }
        }
        if !history.is_empty() {
            muted_note(ui, "History");
            for url in history {
                let clicked = ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new(ellipsize(url, 36))
                                .size(CHROME_FONT)
                                .color(CHROME_TEXT),
                        )
                        .fill(fill_for(idx))
                        .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                    )
                    .on_hover_text(format!("Visited: {url}"))
                    .clicked();
                if clicked {
                    accepted = Some(url.clone());
                }
                idx += 1;
            }
        }
        for suggestion in search_items {
            let clicked = ui
                .add(
                    egui::Button::new(
                        egui::RichText::new(ellipsize(suggestion, 36))
                            .size(CHROME_FONT)
                            .color(CHROME_TEXT),
                    )
                    .fill(fill_for(idx))
                    .min_size(egui::vec2(96.0, CHROME_BUTTON)),
                )
                .on_hover_text(format!("Search for {suggestion}"))
                .clicked();
            if clicked {
                accepted = Some(suggestion.clone());
            }
            idx += 1;
        }
        if state.suggestions.items.is_empty() && history.is_empty() {
            if let Some(notice) = state.suggestions.notice.as_deref() {
                muted_note(ui, notice);
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
                muted_note(
                    ui,
                    "No bookmarks yet \u{2014} add one from Bookmarks \u{2192} Add Bookmark",
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
                    let resp = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(ellipsize(&link.title, BOOKMARK_TITLE_CHARS))
                                    .size(CHROME_FONT)
                                    .color(CHROME_TEXT),
                            )
                            .fill(control_fill(false))
                            .min_size(egui::vec2(BOOKMARK_BTN_W, CHROME_BUTTON)),
                        )
                        .on_hover_text(format!("{}\n{}", link.title, link.url));
                    if resp.clicked() {
                        chosen = Some((link.url.clone(), false));
                    } else if resp.middle_clicked() {
                        chosen = Some((link.url.clone(), true));
                    }
                }
                if visible < links.len() {
                    ui.menu_button(
                        egui::RichText::new("\u{00BB}")
                            .size(CHROME_FONT)
                            .color(CHROME_TEXT),
                        |ui| {
                            for link in &links[visible..] {
                                let resp = ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new(ellipsize(&link.title, 40))
                                                .size(CHROME_FONT)
                                                .color(CHROME_TEXT),
                                        )
                                        .fill(control_fill(false)),
                                    )
                                    .on_hover_text(link.url.clone());
                                if resp.clicked() {
                                    chosen = Some((link.url.clone(), false));
                                    ui.close_menu();
                                } else if resp.middle_clicked() {
                                    chosen = Some((link.url.clone(), true));
                                    ui.close_menu();
                                }
                            }
                        },
                    )
                    .response
                    .on_hover_text("More bookmarks");
                }
            });
        });
    if let Some((url, new_tab)) = chosen {
        state.open_bookmark(url, new_tab);
    }
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
                let resp = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut state.find_query)
                        .desired_width(220.0)
                        .hint_text("Find in page")
                        .text_color(CHROME_TEXT)
                        .font(egui::TextStyle::Small)
                        .min_size(egui::vec2(160.0, CHROME_OMNIBOX_H)),
                );
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

/// The toolbar password menu: fill a saved login into the current site's form, or
/// save a new credential for it. Session-only store, user-initiated fill.
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
    let matches: Vec<(usize, String, String)> = if host.is_empty() {
        Vec::new()
    } else {
        state
            .session_logins
            .iter()
            .enumerate()
            .filter(|(_, login)| login.host == host)
            .map(|(idx, login)| (idx, login.username.clone(), login.password.clone()))
            .collect()
    };
    ui.menu_button(
        RichText::new("\u{1F511}")
            .size(CHROME_FONT)
            .color(CHROME_TEXT_DIM),
        |ui| {
            ui.set_min_width(260.0);
            if host.is_empty() {
                ui.weak("No site loaded");
                return;
            }
            ui.label(
                RichText::new("Saved logins (this session)")
                    .size(CHROME_FONT)
                    .strong(),
            );
            if matches.is_empty() {
                ui.weak(format!("None saved for {host}"));
            } else {
                for (idx, username, password) in &matches {
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                has_page && can_fill,
                                action_button(
                                    format!("Fill {username}"),
                                    BrowserActionRole::Primary,
                                ),
                            )
                            .clicked()
                        {
                            fill = Some((username.clone(), password.clone()));
                            ui.close_menu();
                        }
                        if ui
                            .add(action_button("\u{00D7}", BrowserActionRole::Quiet))
                            .on_hover_text("Delete saved login")
                            .clicked()
                        {
                            remove = Some(*idx);
                            ui.close_menu();
                        }
                    });
                }
            }
            ui.separator();
            ui.label(RichText::new(format!("Save a login for {host}")).size(CHROME_FONT));
            ui.add(
                egui::TextEdit::singleline(&mut state.login_user_draft)
                    .hint_text("username")
                    .desired_width(f32::INFINITY),
            );
            ui.add(
                egui::TextEdit::singleline(&mut state.login_pass_draft)
                    .password(true)
                    .hint_text("password")
                    .desired_width(f32::INFINITY),
            );
            if ui
                .add(action_button("Save", BrowserActionRole::Primary))
                .clicked()
            {
                save = true;
                ui.close_menu();
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
        ui.label(
            RichText::new("HTTP connection")
                .size(CHROME_FONT)
                .color(CHROME_WARN),
        );
        ui.label(RichText::new(ellipsize(&url, 64)).color(CHROME_TEXT_DIM));
        if ui
            .add(action_button("Use HTTPS", BrowserActionRole::Primary))
            .on_hover_text("Upgrade this navigation to HTTPS")
            .clicked()
        {
            state.upgrade_insecure_load();
        }
        if ui
            .add(action_button("Continue HTTP", BrowserActionRole::Warning))
            .on_hover_text("Continue with the insecure HTTP URL")
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
    let tone = if notice.starts_with("Capture failed:")
        || notice.starts_with("PDF failed")
        || notice.starts_with("PDF viewer failed:")
        || notice.starts_with("Print failed:")
    {
        CHROME_ERROR
    } else {
        CHROME_PRIMARY
    };
    egui::Frame::NONE
        .fill(CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.colored_label(tone, RichText::new(notice).size(Style::SMALL));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            action_button("\u{00D7}", BrowserActionRole::Quiet)
                                .min_size(egui::vec2(CHROME_BUTTON, CHROME_BUTTON)),
                        )
                        .on_hover_text("Dismiss capture notice")
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
        ui.label(
            RichText::new("This page crashed")
                .size(Style::HEADING)
                .color(CHROME_ERROR),
        );
        ui.add_space(Style::SP_S);
        if !reason.is_empty() {
            browser_body_note(ui, reason);
        }
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("\u{21BB} Reload").color(CHROME_TEXT),
            ))
            .clicked()
        {
            *respawn_requested = true;
        }
    });
}

pub(super) fn safe_browsing_interstitial_body(ui: &mut egui::Ui, url: &str) -> bool {
    let host = host_of(url).unwrap_or_else(|| url.trim().to_owned());
    let mut back = false;
    centered(ui, |ui| {
        ui.label(
            RichText::new("\u{26A0} Unsafe site blocked")
                .size(Style::HEADING)
                .color(CHROME_ERROR),
        );
        ui.add_space(Style::SP_M);
        ui.label(
            RichText::new(format!(
                "{host} is on the mesh safe-browsing blocklist. This page was not loaded."
            ))
            .color(CHROME_TEXT),
        );
        ui.add_space(Style::SP_M);
        if ui
            .add(egui::Button::new(
                RichText::new("Back to safety").color(CHROME_TEXT),
            ))
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
        ui.label(
            RichText::new("\u{26D4} Blocked by policy")
                .size(Style::HEADING)
                .color(CHROME_ERROR),
        );
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
            .add(egui::Button::new(
                RichText::new("Back to safety").color(CHROME_TEXT),
            ))
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

pub(super) fn passkey_consent_prompt_bar(
    ui: &mut egui::Ui,
    pending: &PendingPasskeyConsent,
    active_tab_id: Option<u64>,
) -> Option<bool> {
    let mut decision = None;
    egui::Frame::NONE
        .fill(prompt_fill())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(passkey_consent_prompt_text(pending, active_tab_id))
                        .color(CHROME_TEXT),
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

pub(super) fn permission_prompt_bar(ui: &mut egui::Ui, origin: &str, kind: u8) -> Option<bool> {
    let mut decision = None;
    egui::Frame::NONE
        .fill(prompt_fill())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{origin} wants to {}", permission_kind_label(kind)))
                        .color(CHROME_TEXT),
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

pub(super) fn before_unload_prompt_bar(
    ui: &mut egui::Ui,
    prompt: &BeforeUnloadDialog,
) -> Option<bool> {
    let mut decision = None;
    egui::Frame::NONE
        .fill(prompt_fill())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(before_unload_prompt_text(prompt)).color(CHROME_TEXT));
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
    egui::Frame::NONE
        .fill(prompt_fill())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("Save login for {host} ({username})?"))
                        .color(CHROME_TEXT),
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
        ui.label(
            RichText::new("Your connection is not private")
                .size(Style::HEADING)
                .color(CHROME_ERROR),
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
            .add(egui::Button::new(
                RichText::new("\u{2190} Back to safety").color(CHROME_TEXT),
            ))
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
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
    let image_rect = fit_rect_preserving_aspect(rect, texture_size);
    ui.painter().rect_filled(rect, 0.0, Style::SURFACE);
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

    #[test]
    fn browser_chrome_tokens_are_local_material_roles() {
        assert_eq!(tab_fill(true), CHROME_TOOLBAR);
        assert_eq!(tab_fill(false), CHROME_SURFACE_CONTAINER);
        assert_eq!(tab_stroke(true), CHROME_OUTLINE);
        assert_eq!(tab_stroke(false), CHROME_SURFACE_CONTAINER_HIGH);
        assert_eq!(tab_text(false), CHROME_TEXT_DIM);
        assert_eq!(row_fill(true), CHROME_PRIMARY_CONTAINER);
        assert_eq!(selected_text(true), CHROME_ON_PRIMARY_CONTAINER);
        assert_eq!(tone_color(ChipTone::Warn), CHROME_WARN);
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
        assert_eq!(engine_primary_label(BrowserEngine::Cef), "CEF");
        assert_eq!(engine_supporting_label(BrowserEngine::Cef), "Chromium");
        assert_eq!(engine_marker(BrowserEngine::Cef), "CEF");
        assert_eq!(engine_glyph(BrowserEngine::Cef), "C");
        assert_eq!(engine_display_name(BrowserEngine::Servo), "Servo");
        assert_eq!(engine_primary_label(BrowserEngine::Servo), "Servo");
        assert_eq!(engine_supporting_label(BrowserEngine::Servo), "Rust engine");
        assert_eq!(engine_marker(BrowserEngine::Servo), "Servo");
        assert_eq!(engine_glyph(BrowserEngine::Servo), "S");
        assert_eq!(
            engine_segment_fill(BrowserEngine::Cef, BrowserEngine::Cef),
            CHROME_PRIMARY_CONTAINER
        );
        assert_eq!(
            engine_segment_text(BrowserEngine::Cef, BrowserEngine::Cef),
            CHROME_ON_PRIMARY_CONTAINER
        );
        assert_eq!(
            engine_segment_stroke(BrowserEngine::Cef, BrowserEngine::Cef),
            CHROME_PRIMARY
        );
        assert_eq!(
            engine_segment_fill(BrowserEngine::Servo, BrowserEngine::Servo),
            CHROME_SUCCESS_CONTAINER
        );
        assert_eq!(
            engine_segment_text(BrowserEngine::Servo, BrowserEngine::Servo),
            CHROME_ON_SUCCESS_CONTAINER
        );
        assert_eq!(
            engine_segment_stroke(BrowserEngine::Servo, BrowserEngine::Servo),
            CHROME_SUCCESS
        );
        assert_eq!(
            engine_segment_fill(BrowserEngine::Servo, BrowserEngine::Cef),
            CHROME_TOOLBAR
        );
        assert_eq!(
            engine_segment_text(BrowserEngine::Servo, BrowserEngine::Cef),
            CHROME_TEXT
        );
        assert_eq!(
            engine_segment_stroke(BrowserEngine::Servo, BrowserEngine::Cef),
            CHROME_OUTLINE
        );
        assert_eq!(engine_new_tab_fill(BrowserEngine::Cef), CHROME_PRIMARY);
        assert_eq!(engine_new_tab_fill(BrowserEngine::Servo), CHROME_SUCCESS);
        assert_eq!(engine_new_tab_text(BrowserEngine::Cef), "New tab");
        assert_eq!(engine_new_tab_text(BrowserEngine::Servo), "New tab");
        assert_eq!(
            engine_new_tab_supporting_text(BrowserEngine::Cef),
            "CEF / Chromium"
        );
        assert_eq!(
            engine_new_tab_supporting_text(BrowserEngine::Servo),
            "Servo"
        );
        assert_eq!(engine_tab_count_label(0), "0 tabs");
        assert_eq!(engine_tab_count_label(1), "1 tab");
        assert_eq!(engine_tab_count_label(2), "2 tabs");
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
        assert_eq!(page_action_text(true), CHROME_TEXT);
        assert_eq!(page_action_text(false), CHROME_TEXT_DIM);
        assert_eq!(
            page_action_star(false, false),
            ("\u{2606}", CHROME_TEXT_DIM)
        );
        assert_eq!(page_action_star(true, false), ("\u{2606}", CHROME_TEXT));
        assert_eq!(page_action_star(true, true), ("\u{2605}", CHROME_PRIMARY));
    }

    #[test]
    fn omnibox_formats_use_browser_material_text_roles() {
        let font = font_id(13.0);
        assert_eq!(omnibox_dim_format(font.clone()).color, CHROME_TEXT_DIM);
        assert_eq!(omnibox_strong_format(font).color, CHROME_TEXT);
    }

    #[test]
    fn browser_chrome_uses_the_named_roboto_family() {
        assert_eq!(
            font_id(13.0).family,
            FontFamily::Name(std::sync::Arc::from(mde_egui::fonts::BROWSER_CHROME_FAMILY))
        );
    }
}
