//! Browser-local Chrome-style visual scope.
//!
//! This module is the first slice of the BROWSER-CHROME `web/chrome_ui/`
//! extraction: the Browser gets a local light chrome treatment without changing
//! the helper/session control path. Page pixels still come from the active engine;
//! this scope only affects shell-owned tabs, toolbar, menus, drawers, and the new
//! tab dashboard.

use mde_egui::egui::{self, Color32, FontFamily, FontId, TextStyle};
use mde_egui::Style;

/// Chrome's UI face is Roboto; this slice pins the browser chrome onto egui's
/// proportional family until the actual Roboto font asset is embedded.
pub(super) const CHROME_FONT_FAMILY: FontFamily = FontFamily::Proportional;

const CHROME_BG: Color32 = Color32::from_rgb(248, 250, 253);
pub(super) const CHROME_TOOLBAR: Color32 = Color32::from_rgb(255, 255, 255);
const CHROME_TOOLBAR_HOVER: Color32 = Color32::from_rgb(241, 243, 244);
const CHROME_TOOLBAR_ACTIVE: Color32 = Color32::from_rgb(232, 240, 254);
const CHROME_BORDER: Color32 = Color32::from_rgb(218, 220, 224);
pub(super) const CHROME_TEXT: Color32 = Color32::from_rgb(32, 33, 36);
pub(super) const CHROME_TEXT_DIM: Color32 = Color32::from_rgb(95, 99, 104);

pub(super) const fn button_text(enabled: bool) -> Color32 {
    if enabled {
        CHROME_TEXT
    } else {
        CHROME_TEXT_DIM
    }
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
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(12.0, CHROME_FONT_FAMILY.clone()),
    );
    style.text_styles.insert(
        TextStyle::Body,
        FontId::new(13.0, CHROME_FONT_FAMILY.clone()),
    );

    let visuals = &mut style.visuals;
    visuals.dark_mode = false;
    visuals.override_text_color = Some(CHROME_TEXT);
    visuals.panel_fill = CHROME_BG;
    visuals.window_fill = CHROME_TOOLBAR;
    visuals.extreme_bg_color = CHROME_TOOLBAR;
    visuals.faint_bg_color = CHROME_BG;
    visuals.widgets.noninteractive.bg_fill = CHROME_BG;
    visuals.widgets.noninteractive.fg_stroke.color = CHROME_TEXT_DIM;
    visuals.widgets.noninteractive.bg_stroke.color = CHROME_BORDER;
    visuals.widgets.inactive.bg_fill = CHROME_TOOLBAR;
    visuals.widgets.inactive.fg_stroke.color = CHROME_TEXT;
    visuals.widgets.inactive.bg_stroke.color = CHROME_BORDER;
    visuals.widgets.hovered.bg_fill = CHROME_TOOLBAR_HOVER;
    visuals.widgets.hovered.fg_stroke.color = CHROME_TEXT;
    visuals.widgets.active.bg_fill = CHROME_TOOLBAR_ACTIVE;
    visuals.widgets.active.fg_stroke.color = CHROME_TEXT;
    visuals.selection.bg_fill = Style::ACCENT;
    visuals.selection.stroke.color = CHROME_TEXT;
}
