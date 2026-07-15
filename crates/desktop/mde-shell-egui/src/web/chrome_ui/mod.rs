//! Browser-local Chrome-style visual scope.
//!
//! This module is the first slice of the BROWSER-CHROME `web/chrome_ui/`
//! extraction: the Browser gets a local light chrome treatment without changing
//! the helper/session control path. Page pixels still come from the active engine;
//! this scope only affects shell-owned tabs, toolbar, menus, drawers, and the new
//! tab dashboard.

use std::sync::Arc;

use mde_egui::egui::{self, Color32, FontFamily, FontId, TextStyle};
use mde_egui::{muted_note, ChipTone, Style};
use mde_web_preview_client::SessionState;

use super::{
    centered, ellipsize, media_metadata_chip_label, BrowserEngine, ContainerProfile, DeviceProfile,
    DisplayTarget, Tab, UserAgentOverride, WebState, CHROME_BUTTON, CHROME_FONT, CHROME_GAP,
    CHROME_NEW_TAB_W, CHROME_OMNIBOX_H, CHROME_TAB_CLOSE, CHROME_TAB_H, CHROME_TAB_MIN_W,
    CHROME_TAB_W, PRIVATE_MODE_EXPLAINER,
};

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
pub(super) const CHROME_OUTLINE: Color32 = Color32::from_rgb(218, 220, 224);
pub(super) const CHROME_TEXT: Color32 = Color32::from_rgb(32, 33, 36);
pub(super) const CHROME_TEXT_DIM: Color32 = Color32::from_rgb(95, 99, 104);
pub(super) const CHROME_SUCCESS: Color32 = Color32::from_rgb(20, 108, 46);
pub(super) const CHROME_WARN: Color32 = Color32::from_rgb(177, 91, 0);
pub(super) const CHROME_ERROR: Color32 = Color32::from_rgb(179, 38, 30);

const STATE_HOVER_ALPHA: u8 = 20;
const STATE_FOCUS_ALPHA: u8 = 26;
const STATE_PRESSED_ALPHA: u8 = 26;

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
) -> egui::Response {
    // `click_and_drag` keeps activation, middle-click close, and drag-reorder on
    // the same browser-tab affordance while egui handles the click/drag threshold.
    ui.add(
        egui::Button::new(
            egui::RichText::new(label)
                .size(CHROME_FONT)
                .color(tab_text(active)),
        )
        .fill(tab_fill(active))
        .min_size(egui::vec2(width, CHROME_TAB_H))
        .sense(egui::Sense::click_and_drag()),
    )
}

pub(super) fn inline_close_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(
        egui::Button::new(
            egui::RichText::new("\u{00D7}")
                .size(CHROME_FONT)
                .color(CHROME_TEXT_DIM),
        )
        .fill(control_fill(false))
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
            .fill(control_fill(false))
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
    let autoplay = if tab.autoplay_blocked { "A " } else { "" };
    let force_dark = if tab.force_dark { "D " } else { "" };
    let reader = if tab.reader_mode { "R " } else { "" };
    let user_scripts = if tab.user_scripts { "S " } else { "" };
    let user_agent = tab.user_agent.marker();
    let device_profile = tab.device_profile.marker();
    format!(
        "{state} {container}{display}{muted}{autoplay}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}{}",
        ellipsize(base, 24)
    )
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
            "{state}{container}{display}{audio}{now_playing}{autoplay}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
        )
    } else {
        format!(
            "{state} - {url}{container}{display}{audio}{now_playing}{autoplay}{force_dark}{reader}{user_scripts}{user_agent}{device_profile}"
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

pub(super) fn tab_favicon_image(ui: &mut egui::Ui, texture: Option<&egui::TextureHandle>) {
    const TAB_FAVICON_SIZE: f32 = 16.0;
    let size = egui::vec2(TAB_FAVICON_SIZE, TAB_FAVICON_SIZE);
    match texture {
        Some(handle) => {
            ui.add(egui::Image::new(egui::load::SizedTexture::new(
                handle.id(),
                size,
            )));
        }
        None => {
            ui.allocate_space(size);
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
    let mut button = |ui: &mut egui::Ui, engine: BrowserEngine| {
        let label = format!("+{}", engine.label());
        let mut widget = egui::Button::new(
            egui::RichText::new(label)
                .size(CHROME_FONT)
                .color(CHROME_TEXT),
        )
        .fill(control_fill(false))
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
    button(ui, BrowserEngine::Servo);
    button(ui, BrowserEngine::Cef);
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
    let permission_summary = super::site_info_permission_summary(state);
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
        super::page_actions_button(
            ui,
            has_page,
            is_bookmarked,
            state.bus_root.as_deref(),
            active_engine,
            &page_url,
            &page_title,
        );
        super::password_menu(
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
                        Style::ACCENT
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
                                .color(Style::TEXT_DIM),
                        );
                    }
                }
            });
        }

        ui.add_space(CHROME_GAP);

        // OMNIBOX-STYLE — the leading security chip reflects the CURRENT
        // (committed) page URL's scheme, never the in-progress edit draft.
        super::security_chip(
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
                super::paint_omnibox_inline_completion(ui, resp.rect, &state.address, &tail);
            }
        } else if has_tab && !crashed && !state.address.trim().is_empty() {
            let font_id = font_id(CHROME_FONT);
            let job = super::omnibox_layout_job(&state.address, font_id);
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
            super::page_actions_menu(
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

        toolbar_action = super::menubar::show_chrome_menu(state, ui);
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
        assert_eq!(tab_fill(false), CHROME_SURFACE_CONTAINER_HIGH);
        assert_eq!(tab_text(false), CHROME_TEXT_DIM);
        assert_eq!(row_fill(true), CHROME_PRIMARY_CONTAINER);
        assert_eq!(selected_text(true), CHROME_ON_PRIMARY_CONTAINER);
        assert_eq!(tone_color(ChipTone::Warn), CHROME_WARN);
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
