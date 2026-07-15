//! Browser-local Chrome-style visual scope.
//!
//! This module is the first slice of the BROWSER-CHROME `web/chrome_ui/`
//! extraction: the Browser gets a local light chrome treatment without changing
//! the helper/session control path. Page pixels still come from the active engine;
//! this scope only affects shell-owned tabs, toolbar, menus, drawers, and the new
//! tab dashboard.

use std::sync::Arc;

use mde_egui::egui::{self, Color32, FontFamily, FontId, TextStyle};
use mde_egui::ChipTone;

use super::{ellipsize, BrowserEngine, Tab, WebState, CHROME_FONT, CHROME_NEW_TAB_W, CHROME_TAB_H};

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
