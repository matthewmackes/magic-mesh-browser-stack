//! Secondary browser overlay drawers — the dismissible panels that render
//! *below* the navigation chrome for the active tab: print settings,
//! downloads status, QR share, translation, spellcheck, offline copy,
//! browser-engine update, and speech status. Extracted verbatim from
//! `web/mod.rs` (P2 shell-ux-7); each `*_drawer` is invoked once per
//! render pass from the `chrome_ui` drawer stack and reads/writes the
//! shared [`WebState`] through the existing Browser state/action seams.

use super::super::{
    offline_cache_viewport_display_size, offline_cache_viewport_texture, plural,
    printer_error_label, short_transfer_name, spellcheck_occurrence_index, spellcheck_results_text,
    BrowserReadAloudStatus, BrowserVoiceCommandStatus, PaperSize, PrintOrientation,
};
use super::*;
use mde_egui::egui::{RichText, Sense};
use mde_files_egui::transfers::{TransferState, TransferVerb};

const DRAWER_ICON_BUTTON_W: f32 = 28.0;
const DRAWER_ICON_BUTTON_H: f32 = 24.0;
const DRAWER_CONTROL_RADIUS: f32 = 6.0;
pub(super) const QR_MATRIX_LIGHT: egui::Color32 = super::CHROME_TOOLBAR;
pub(super) const QR_MATRIX_DARK: egui::Color32 = super::CHROME_TEXT;
pub(super) const PRINT_PAGE_RANGE_HELP: &str = "Page range, e.g. 1-5,8: empty prints all pages";

pub(super) fn download_drawer_subtitle(
    worker_present: bool,
    active: usize,
    total: usize,
) -> String {
    if total > 0 {
        if active > 0 && active < total {
            format!("{active} active / {total} total")
        } else if active > 0 {
            format!("{active} active")
        } else if total == 1 {
            "1 complete".to_owned()
        } else {
            format!("{total} complete")
        }
    } else if worker_present {
        "No downloads".to_owned()
    } else {
        "Downloads unavailable".to_owned()
    }
}

fn download_row_accesskit_id(job: &mde_files_egui::transfers::TransferJob) -> egui::Id {
    egui::Id::new(("browser-download-row", job.id.as_str()))
}

fn download_row_accesskit_value(job: &mde_files_egui::transfers::TransferJob) -> String {
    let mut parts = vec![
        format!("State {}", job.state.label()),
        format!("Route {}", job.route()),
    ];
    if let Some(progress) = job.progress {
        parts.push(format!("Progress {}%", progress.min(100)));
    }
    if job.policy.verify {
        parts.push("Verification enabled".to_owned());
    }
    if let Some(error) = &job.error {
        parts.push(format!("Error {error}"));
    }
    parts.join(", ")
}

fn install_download_row_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    job: &mde_files_egui::transfers::TransferJob,
) {
    let _ = ctx.accesskit_node_builder(download_row_accesskit_id(job), |node| {
        node.set_role(egui::accesskit::Role::Row);
        node.set_label(format!("Download {}", short_transfer_name(job)));
        node.set_value(download_row_accesskit_value(job));
        node.set_bounds(accesskit_rect(rect));
        if let Some(progress) = job.progress {
            node.set_numeric_value(f64::from(progress.min(100)));
            node.set_min_numeric_value(0.0);
            node.set_max_numeric_value(100.0);
        }
    });
}

fn history_row_accesskit_id(index: usize, url: &str) -> egui::Id {
    egui::Id::new(("browser-history-row", index, url))
}

fn install_history_row_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    index: usize,
    label: &str,
    url: &str,
) {
    let _ = ctx.accesskit_node_builder(history_row_accesskit_id(index, url), |node| {
        node.set_role(egui::accesskit::Role::Button);
        node.set_label(format!("Open history entry {label}"));
        node.set_value(url);
        node.set_bounds(accesskit_rect(rect));
        node.add_action(egui::accesskit::Action::Click);
    });
}

fn drawer_button_widget(
    label: impl Into<String>,
    role: BrowserActionRole,
) -> egui::Button<'static> {
    egui::Button::new(
        RichText::new(label.into())
            .size(Style::SMALL)
            .color(action_button_text(role)),
    )
    .fill(action_button_fill(role))
    .stroke(egui::Stroke::new(1.0, action_button_stroke(role)))
    .corner_radius(DRAWER_CONTROL_RADIUS)
    .min_size(egui::vec2(28.0, 24.0))
}

fn drawer_button(
    ui: &mut egui::Ui,
    label: impl Into<String>,
    role: BrowserActionRole,
    tip: &str,
) -> egui::Response {
    let response = ui.add(drawer_button_widget(label, role));
    mde_egui::focus::paint_focus_ring(ui.painter(), response.rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn drawer_icon_button(
    ui: &mut egui::Ui,
    icon: ChromeIcon,
    role: BrowserActionRole,
    tip: &str,
) -> egui::Response {
    drawer_icon_button_impl(ui, true, icon, role, tip)
}

fn drawer_icon_button_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    icon: ChromeIcon,
    role: BrowserActionRole,
    tip: &str,
) -> egui::Response {
    drawer_icon_button_impl(ui, enabled, icon, role, tip)
}

fn drawer_icon_button_impl(
    ui: &mut egui::Ui,
    enabled: bool,
    icon: ChromeIcon,
    role: BrowserActionRole,
    tip: &str,
) -> egui::Response {
    let enabled = ui.is_enabled() && enabled;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(DRAWER_ICON_BUTTON_W, DRAWER_ICON_BUTTON_H),
        sense,
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));

    let fill = animated_response_fill(
        ui,
        &response,
        action_button_fill(role),
        action_button_text(role),
        enabled,
    );
    ui.painter().rect(
        rect,
        DRAWER_CONTROL_RADIUS,
        fill,
        egui::Stroke::new(1.0, action_button_stroke(role)),
        egui::StrokeKind::Inside,
    );
    let icon_color = if enabled {
        action_button_text(role)
    } else {
        super::CHROME_TEXT_DIM
    };
    paint_chrome_icon(ui.painter(), rect, icon, icon_color);
    mde_egui::focus::paint_focus_ring(ui.painter(), response.rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn drawer_close_button(ui: &mut egui::Ui, tip: &str) -> egui::Response {
    drawer_icon_button(ui, ChromeIcon::Close, BrowserActionRole::Quiet, tip)
}

fn drawer_status_row(ui: &mut egui::Ui, icon: ChromeIcon, text: &str, color: egui::Color32) {
    ui.horizontal_wrapped(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), Sense::hover());
        paint_chrome_icon(ui.painter(), rect, icon, color);
        ui.label(RichText::new(text).size(Style::SMALL).color(color));
    });
}

fn drawer_tone_icon(tone: ChipTone, fallback: ChromeIcon) -> ChromeIcon {
    match tone {
        ChipTone::Ok => ChromeIcon::Check,
        ChipTone::Warn | ChipTone::Danger => ChromeIcon::Warning,
        ChipTone::Info | ChipTone::Neutral => fallback,
    }
}

fn drawer_text_field(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: f32,
    tip: &str,
) -> egui::Response {
    let inner = egui::Frame::NONE
        .fill(super::CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, super::CHROME_OUTLINE))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::singleline(text)
                    .hint_text(
                        RichText::new(hint)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    )
                    .desired_width(width)
                    .font(font_id(Style::SMALL))
                    .text_color(super::CHROME_TEXT)
                    .background_color(super::CHROME_SURFACE)
                    .margin(egui::Margin::symmetric(0, 0))
                    .frame(false),
            )
        });
    mde_egui::focus::paint_focus_ring(ui.painter(), inner.response.rect, inner.inner.has_focus());
    chrome_hover_text(inner.inner, tip)
}

fn drawer_multiline_text_field(
    ui: &mut egui::Ui,
    text: &mut String,
    hint: &str,
    width: f32,
    rows: usize,
    tip: &str,
) -> egui::Response {
    let inner = egui::Frame::NONE
        .fill(super::CHROME_SURFACE)
        .stroke(egui::Stroke::new(1.0, super::CHROME_OUTLINE))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(6, 3))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(text)
                    .hint_text(
                        RichText::new(hint)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    )
                    .desired_width(width)
                    .desired_rows(rows)
                    .font(font_id(Style::SMALL))
                    .text_color(super::CHROME_TEXT)
                    .background_color(super::CHROME_SURFACE)
                    .margin(egui::Margin::symmetric(0, 0))
                    .frame(false),
            )
        });
    mde_egui::focus::paint_focus_ring(ui.painter(), inner.response.rect, inner.inner.has_focus());
    chrome_hover_text(inner.inner, tip)
}

fn drawer_stepper(ui: &mut egui::Ui, value: &mut u16, min: u16, max: u16, tip: &str) {
    *value = (*value).clamp(min, max);
    ui.horizontal(|ui| {
        if drawer_icon_button_enabled(
            ui,
            *value > min,
            ChromeIcon::Minus,
            BrowserActionRole::Quiet,
            "Decrease",
        )
        .clicked()
        {
            *value = (*value).saturating_sub(1).max(min);
        }

        let text = (*value).to_string();
        egui::Frame::NONE
            .fill(super::CHROME_SURFACE)
            .stroke(egui::Stroke::new(1.0, super::CHROME_OUTLINE))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(8, 2))
            .show(ui, |ui| {
                ui.set_min_width(22.0);
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(text)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT),
                    );
                });
            })
            .response
            .on_hover_ui(|ui| chrome_tooltip(ui, tip));

        if drawer_icon_button_enabled(
            ui,
            *value < max,
            ChromeIcon::Plus,
            BrowserActionRole::Quiet,
            "Increase",
        )
        .clicked()
        {
            *value = (*value).saturating_add(1).min(max);
        }
    })
    .response
    .on_hover_ui(|ui| chrome_tooltip(ui, tip));
}

fn drawer_progress_bar(ui: &mut egui::Ui, progress: u8) {
    let progress = progress.min(100);
    let fraction = f32::from(progress) / 100.0;
    let width = (ui.available_width() * 0.55).max(120.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 16.0), Sense::hover());
    let label = format!("{progress}%");
    let font = font_id(Style::SMALL);
    let galley = ui.fonts(|fonts| fonts.layout_no_wrap(label, font, super::CHROME_TEXT_DIM));
    let track_right = (rect.right() - galley.size().x - Style::SP_S).max(rect.left() + 32.0);
    let track = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.center().y - 3.0),
        egui::pos2(track_right, rect.center().y + 3.0),
    );
    ui.painter().rect_filled(track, 3.0, super::CHROME_SURFACE);
    ui.painter().rect_stroke(
        track,
        3.0,
        egui::Stroke::new(1.0, super::CHROME_OUTLINE),
        egui::StrokeKind::Inside,
    );
    if progress > 0 {
        let fill_w = (track.width() * fraction).max(2.0);
        let fill = egui::Rect::from_min_size(track.min, egui::vec2(fill_w, track.height()));
        ui.painter().rect_filled(fill, 3.0, super::CHROME_PRIMARY);
    }
    ui.painter().galley(
        egui::pos2(
            track.right() + Style::SP_S,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        super::CHROME_TEXT_DIM,
    );
}

fn drawer_button_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    label: impl Into<String>,
    role: BrowserActionRole,
    tip: &str,
) -> egui::Response {
    let response = ui.add_enabled(enabled, drawer_button_widget(label, role));
    mde_egui::focus::paint_focus_ring(ui.painter(), response.rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn drawer_toggle(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    let font = font_id(Style::SMALL);
    let galley = ui.fonts(|fonts| fonts.layout_no_wrap(label.to_owned(), font, super::CHROME_TEXT));
    let target_size = egui::vec2((galley.size().x + 36.0).max(82.0), 24.0);
    let (rect, response) = ui.allocate_exact_size(target_size, Sense::click());
    if response.clicked() {
        *checked = !*checked;
    }

    let hover_fill = if *checked {
        super::CHROME_PRIMARY
    } else {
        super::CHROME_TOOLBAR
    };
    let fill = animated_response_fill(ui, &response, hover_fill, super::CHROME_TEXT, true);
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 6.0, rect.center().y - 7.0),
        egui::vec2(14.0, 14.0),
    );
    ui.painter().rect_filled(box_rect, 3.0, fill);
    ui.painter().rect_stroke(
        box_rect,
        3.0,
        egui::Stroke::new(
            1.0,
            if *checked {
                super::CHROME_PRIMARY
            } else {
                super::CHROME_OUTLINE
            },
        ),
        egui::StrokeKind::Inside,
    );
    if *checked {
        let check = [
            egui::pos2(box_rect.left() + 3.0, box_rect.center().y),
            egui::pos2(box_rect.left() + 6.0, box_rect.bottom() - 3.5),
            egui::pos2(box_rect.right() - 3.0, box_rect.top() + 3.5),
        ];
        ui.painter().line_segment(
            [check[0], check[1]],
            egui::Stroke::new(1.7, super::CHROME_TOOLBAR),
        );
        ui.painter().line_segment(
            [check[1], check[2]],
            egui::Stroke::new(1.7, super::CHROME_TOOLBAR),
        );
    }
    ui.painter().galley(
        egui::pos2(
            box_rect.right() + 7.0,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        super::CHROME_TEXT,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    response
}

fn drawer_choice_chip(ui: &mut egui::Ui, label: &str, selected: bool, tip: &str) -> egui::Response {
    let font = font_id(Style::SMALL);
    let text_color = super::selected_text(selected);
    let galley = ui.fonts(|fonts| fonts.layout_no_wrap(label.to_owned(), font, text_color));
    let target_size = egui::vec2((galley.size().x + 18.0).max(44.0), 24.0);
    let (rect, response) = ui.allocate_exact_size(target_size, Sense::click());
    let base = if selected {
        super::CHROME_PRIMARY_CONTAINER
    } else {
        super::CHROME_SURFACE
    };
    let fill = animated_response_fill(ui, &response, base, super::CHROME_TEXT, true);
    let stroke = if selected {
        super::CHROME_PRIMARY
    } else {
        super::CHROME_OUTLINE
    };
    ui.painter().rect(
        rect,
        6.0,
        fill,
        egui::Stroke::new(1.0, stroke),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        egui::pos2(
            rect.center().x - galley.size().x * 0.5,
            rect.center().y - galley.size().y * 0.5,
        ),
        galley,
        text_color,
    );
    mde_egui::focus::paint_focus_ring(ui.painter(), rect, response.has_focus());
    chrome_hover_text(response, tip)
}

fn drawer_separator(ui: &mut egui::Ui) {
    let width = ui.available_width().max(1.0);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 9.0), Sense::hover());
    let y = rect.center().y;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left(), y),
            egui::pos2(rect.right().max(rect.left() + 1.0), y),
        ],
        egui::Stroke::new(1.0, super::CHROME_OUTLINE),
    );
}

fn drawer_inline_separator(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 24.0), Sense::hover());
    let x = rect.center().x;
    ui.painter().line_segment(
        [
            egui::pos2(x, rect.top() + 4.0),
            egui::pos2(x, rect.bottom() - 4.0),
        ],
        egui::Stroke::new(1.0, super::CHROME_OUTLINE),
    );
}

pub(super) fn print_settings_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.print_settings_open {
        return;
    }

    let printers = state.cups_printers.clone();
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Print")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new("Printer destination")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close print settings").clicked() {
                        state.print_settings_open = false;
                    }
                    if drawer_icon_button(
                        ui,
                        ChromeIcon::Reload,
                        BrowserActionRole::Quiet,
                        "Refresh printers",
                    )
                    .clicked()
                    {
                        state.refresh_cups_printers();
                    }
                });
            });

            if let Some(notice) = &state.cups_notice {
                let notice = printer_error_label(notice)
                    .unwrap_or_else(|| "Printer list unavailable".into());
                drawer_status_row(ui, ChromeIcon::Warning, &notice, super::CHROME_ERROR);
            }

            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Destination")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                if drawer_choice_chip(
                    ui,
                    "System default",
                    state.cups_settings.destination.is_none(),
                    "Use the system default printer",
                )
                .clicked()
                {
                    state.cups_settings.destination = None;
                }
                let selected_destination = state.cups_settings.destination.clone();
                let selected_destination_name = selected_destination.as_deref();
                let selected_destination_is_listed = selected_destination
                    .as_deref()
                    .map(|selected| printers.iter().any(|printer| printer.name == selected))
                    .unwrap_or(true);
                for printer in &printers {
                    let selected = selected_destination_name == Some(printer.name.as_str());
                    let label = if printer.is_default {
                        format!("{} (default)", printer.name)
                    } else {
                        printer.name.clone()
                    };
                    if drawer_choice_chip(ui, &label, selected, "Select printer").clicked() {
                        state.cups_settings.destination = Some(printer.name.clone());
                    }
                }
                if let Some(destination) =
                    selected_destination.filter(|_| !selected_destination_is_listed)
                {
                    let _ = drawer_choice_chip(
                        ui,
                        &destination,
                        true,
                        "The selected printer is not in the latest printer list",
                    );
                }

                drawer_inline_separator(ui);
                ui.label(
                    RichText::new("Copies")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                drawer_stepper(
                    ui,
                    &mut state.cups_settings.copies,
                    1,
                    99,
                    "Number of copies",
                );
                chrome_hover_text(
                    drawer_toggle(ui, &mut state.cups_settings.duplex, "Duplex"),
                    "Print on both sides when the destination supports it",
                );
                chrome_hover_text(
                    drawer_toggle(ui, &mut state.cups_settings.grayscale, "Grayscale"),
                    "Request grayscale output from the printer",
                );
                drawer_inline_separator(ui);
                ui.label(
                    RichText::new("Orientation")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                for orientation in [PrintOrientation::Portrait, PrintOrientation::Landscape] {
                    if drawer_choice_chip(
                        ui,
                        orientation.label(),
                        state.cups_settings.orientation == orientation,
                        "Set print orientation",
                    )
                    .clicked()
                    {
                        state.cups_settings.orientation = orientation;
                    }
                }
                drawer_inline_separator(ui);
                ui.label(
                    RichText::new("Paper")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                for paper in [
                    PaperSize::Default,
                    PaperSize::A4,
                    PaperSize::Letter,
                    PaperSize::Legal,
                ] {
                    if drawer_choice_chip(
                        ui,
                        paper.label(),
                        state.cups_settings.paper_size == paper,
                        "Set print paper size",
                    )
                    .clicked()
                    {
                        state.cups_settings.paper_size = paper;
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Pages")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                drawer_text_field(
                    ui,
                    &mut state.cups_settings.page_ranges,
                    "all",
                    80.0,
                    PRINT_PAGE_RANGE_HELP,
                );
                if drawer_button_enabled(
                    ui,
                    state.can_drive_page_tools(),
                    "Print",
                    BrowserActionRole::Primary,
                    "Queue this page as a print job",
                )
                .clicked()
                {
                    state.print_active_page();
                }
            });

            if printers.is_empty() {
                browser_muted_note(ui, "No printers discovered; system default is still usable");
            }
        });
}

/// The user-authored Site Styles editor: add a website + CSS rule, or remove one.
/// Session-only, like the other browser chrome toggles.
pub(super) fn site_styles_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.site_styles_open {
        return;
    }
    let mut remove: Option<usize> = None;
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Site Styles")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new("Custom CSS for matching websites")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close site styles").clicked() {
                        state.site_styles_open = false;
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Website")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                drawer_text_field(
                    ui,
                    &mut state.site_style_host_draft,
                    "example.com",
                    140.0,
                    "Website for this style",
                );
                if drawer_button(ui, "Add", BrowserActionRole::Secondary, "Add site style")
                    .clicked()
                {
                    state.add_user_site_style();
                }
            });
            drawer_multiline_text_field(
                ui,
                &mut state.site_style_css_draft,
                "body{max-width:80ch;margin-inline:auto}",
                320.0,
                2,
                "Custom CSS for matching websites",
            );
            if !state.user_site_styles.is_empty() {
                drawer_separator(ui);
                for (i, style) in state.user_site_styles.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} - {}",
                                style.host,
                                ellipsize(&style.css, 36)
                            ))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                        );
                        if drawer_button(
                            ui,
                            "Remove",
                            BrowserActionRole::Quiet,
                            "Remove this site style",
                        )
                        .clicked()
                        {
                            remove = Some(i);
                        }
                    });
                }
            }
        });
    if let Some(i) = remove {
        state.remove_user_site_style(i);
    }
}

/// The session-only History drawer (B3): most-recent-first visits, click to
/// navigate, Clear to forget. In-memory only — nothing here is persisted.
pub(super) fn history_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.history_open {
        return;
    }
    let mut open_url: Option<String> = None;
    let mut clear = false;
    let mut close = false;
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("History")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new("this session only")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close history").clicked() {
                        close = true;
                    }
                    if !state.history.is_empty()
                        && drawer_button(
                            ui,
                            "Clear",
                            BrowserActionRole::Quiet,
                            "Forget this session's history",
                        )
                        .clicked()
                    {
                        clear = true;
                    }
                });
            });
            if state.history.is_empty() {
                ui.label(
                    RichText::new("No pages visited this session")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                return;
            }
            egui::ScrollArea::vertical()
                .max_height(220.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (index, visit) in state.history.visits().enumerate() {
                        let label = if visit.title.trim().is_empty() {
                            visit.url.clone()
                        } else {
                            visit.title.clone()
                        };
                        let elided = ellipsize(&label, 72);
                        let response =
                            super::chrome_menu_row(ui, &elided, ChromeIcon::History, true, "");
                        install_history_row_accessibility(
                            ui.ctx(),
                            response.rect,
                            index,
                            &label,
                            &visit.url,
                        );
                        if chrome_hover_text(response, visit.url.clone()).clicked() {
                            open_url = Some(visit.url.clone());
                        }
                    }
                });
        });
    if close {
        state.history_open = false;
    }
    if clear {
        state.history.clear();
    }
    if let Some(url) = open_url {
        state.load_target(url);
        state.history_open = false;
    }
}

pub(super) fn downloads_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    if !state.downloads_open {
        return;
    }

    let mut action: Option<TransferVerb> = None;
    let mut removed: Option<String> = None;
    let mut open_download: Option<String> = None;
    let mut reveal_download: Option<String> = None;
    let mut clear_all = false;
    let mut keep_dangerous = false;
    let mut discard_dangerous = false;
    let worker_present = state.transfers.worker_present();
    let jobs = state.download_jobs.clone();
    let active_jobs = jobs.iter().filter(|job| !job.state.is_terminal()).count();
    let subtitle = download_drawer_subtitle(worker_present, active_jobs, jobs.len());
    let pending_dangerous = state.pending_dangerous_download.clone();
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Downloads")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new(subtitle)
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close downloads").clicked() {
                        state.downloads_open = false;
                    }
                    if drawer_icon_button(
                        ui,
                        ChromeIcon::Reload,
                        BrowserActionRole::Quiet,
                        "Refresh downloads",
                    )
                    .clicked()
                    {
                        state.refresh_downloads();
                    }
                    if !jobs.is_empty()
                        && drawer_button(
                            ui,
                            "Clear all",
                            BrowserActionRole::Quiet,
                            "Remove every download from this list",
                        )
                        .clicked()
                    {
                        clear_all = true;
                    }
                });
            });

            if let Some(notice) = &state.download_notice {
                drawer_status_row(ui, ChromeIcon::Warning, notice, super::CHROME_ERROR);
            }

            if let Some(pending) = &pending_dangerous {
                drawer_separator(ui);
                egui::Frame::NONE
                    .fill(super::prompt_fill())
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        drawer_status_row(
                            ui,
                            ChromeIcon::Warning,
                            "This type of file can harm your device",
                            super::CHROME_WARN,
                        );
                        ui.label(
                            RichText::new(&pending.filename)
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT),
                        );
                        ui.horizontal(|ui| {
                            if drawer_button(
                                ui,
                                "Keep",
                                BrowserActionRole::Warning,
                                "Download it anyway",
                            )
                            .clicked()
                            {
                                keep_dangerous = true;
                            }
                            if drawer_button(
                                ui,
                                "Discard",
                                BrowserActionRole::Quiet,
                                "Drop this download",
                            )
                            .clicked()
                            {
                                discard_dangerous = true;
                            }
                        });
                    });
            }

            if jobs.is_empty() {
                let message = if worker_present {
                    "No browser downloads yet"
                } else {
                    "Downloads are unavailable on this node"
                };
                browser_muted_note(ui, message);
                return;
            }

            for job in jobs.iter().take(6) {
                drawer_separator(ui);
                let row = ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(short_transfer_name(job))
                                    .size(Style::SMALL)
                                    .color(super::CHROME_TEXT),
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
                                        .color(super::CHROME_TEXT_DIM),
                                );
                            }
                        });
                        ui.label(
                            RichText::new(job.route())
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                        );
                        if let Some(progress) = job.progress {
                            drawer_progress_bar(ui, progress);
                        }
                        if let Some(err) = &job.error {
                            drawer_status_row(ui, ChromeIcon::Warning, err, super::CHROME_ERROR);
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if drawer_button(ui, "Remove", BrowserActionRole::Quiet, "Remove from list")
                            .clicked()
                        {
                            removed = Some(job.id.clone());
                        }
                        if let Some(url) = state.download_source_urls.get(&job.id) {
                            if drawer_button(
                                ui,
                                "Copy link",
                                BrowserActionRole::Secondary,
                                "Copy the download's source URL",
                            )
                            .clicked()
                            {
                                ui.ctx().copy_text(url.clone());
                                state.capture_notice = Some("Download link copied".to_owned());
                            }
                        }
                        if job.state == TransferState::Done {
                            if drawer_button(
                                ui,
                                "Show",
                                BrowserActionRole::Secondary,
                                "Show the completed download in its folder",
                            )
                            .clicked()
                            {
                                reveal_download = Some(job.id.clone());
                            }
                            if drawer_button(
                                ui,
                                "Open",
                                BrowserActionRole::Secondary,
                                "Open the completed download",
                            )
                            .clicked()
                            {
                                open_download = Some(job.id.clone());
                            }
                        }
                        if !job.state.is_terminal()
                            && drawer_button(ui, "Cancel", BrowserActionRole::Quiet, "Cancel")
                                .clicked()
                        {
                            action = Some(TransferVerb::Cancel(job.id.clone()));
                        }
                        if job.state.can_resume()
                            && drawer_button(ui, "Resume", BrowserActionRole::Secondary, "Resume")
                                .clicked()
                        {
                            action = Some(TransferVerb::Resume(job.id.clone()));
                        }
                        if job.state.can_pause()
                            && drawer_button(ui, "Pause", BrowserActionRole::Secondary, "Pause")
                                .clicked()
                        {
                            action = Some(TransferVerb::Pause(job.id.clone()));
                        }
                    });
                });
                install_download_row_accessibility(ui.ctx(), row.response.rect, job);
            }

            let hidden = jobs.len().saturating_sub(6);
            if hidden > 0 {
                browser_muted_note(
                    ui,
                    &format!("{hidden} older browser download{} hidden", plural(hidden)),
                );
            }
        });

    if keep_dangerous {
        state.keep_pending_dangerous_download();
    }
    if discard_dangerous {
        state.discard_pending_dangerous_download();
    }
    if let Some(verb) = action {
        state.dispatch_download_verb(verb);
    }
    if let Some(id) = open_download {
        state.open_download(&id);
    }
    if let Some(id) = reveal_download {
        state.reveal_download(&id);
    }
    if let Some(id) = removed {
        state.dismiss_download(&id);
    }
    if clear_all {
        state.dismiss_all_downloads();
    }
}

pub(super) fn qr_share_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_qr_share.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("QR share")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new("Ready")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close QR share").clicked() {
                        state.latest_qr_share = None;
                    }
                    if drawer_button(
                        ui,
                        "Copy",
                        BrowserActionRole::Secondary,
                        "Copy QR share URL",
                    )
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "QR code {}x{}",
                        result.modules.len(),
                        result.modules.first().map_or(0, Vec::len)
                    ))
                    .size(Style::SMALL)
                    .color(super::CHROME_TEXT_DIM),
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
    painter.rect_filled(rect, 2.0, QR_MATRIX_LIGHT);
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
                QR_MATRIX_DARK,
            );
        }
    }
}

pub(super) fn translation_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_translation.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Translation")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new(format!("{} to {}", result.source_lang, result.target_lang))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close translation").clicked() {
                        state.latest_translation = None;
                    }
                    if drawer_button(
                        ui,
                        "Copy",
                        BrowserActionRole::Secondary,
                        "Copy translated text",
                    )
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!("Text {} chars", result.translation.chars().count()))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
            });
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.translation.as_str())
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT),
                    );
                });
        });
}

pub(super) fn spellcheck_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_spellcheck.clone() else {
        return;
    };
    if !result.is_visible() {
        return;
    }
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Spelling")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(RichText::new(result.summary()).size(Style::SMALL).color(
                    if result.error.is_some() {
                        super::CHROME_WARN
                    } else {
                        super::CHROME_TEXT_DIM
                    },
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close spelling results").clicked() {
                        state.latest_spellcheck = None;
                    }
                    if !result.misses.is_empty()
                        && drawer_button(
                            ui,
                            "Copy",
                            BrowserActionRole::Secondary,
                            "Copy spelling results",
                        )
                        .clicked()
                    {
                        ui.ctx().copy_text(spellcheck_results_text(&result.misses));
                        state.capture_notice = Some("Spelling results copied".to_owned());
                    }
                });
            });

            if let Some(error) = result.user_facing_error() {
                drawer_status_row(ui, ChromeIcon::Warning, &error, super::CHROME_WARN);
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
                                    .color(super::CHROME_WARN),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "chars {}..{}",
                                    miss.chars.start, miss.chars.end
                                ))
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                            );
                            if miss.suggestions.is_empty() {
                                ui.label(
                                    RichText::new("no suggestions")
                                        .size(Style::SMALL)
                                        .color(super::CHROME_TEXT_DIM),
                                );
                            } else {
                                ui.label(
                                    RichText::new("suggest:")
                                        .size(Style::SMALL)
                                        .color(super::CHROME_TEXT_DIM),
                                );
                                for suggestion in miss.suggestions.iter().take(4) {
                                    if drawer_button(
                                        ui,
                                        suggestion.as_str(),
                                        BrowserActionRole::Secondary,
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
                                    if drawer_button(
                                        ui,
                                        "all",
                                        BrowserActionRole::Quiet,
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
                                .color(super::CHROME_TEXT_DIM),
                        );
                    }
                });
        });
}

pub(super) fn offline_cache_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(result) = state.latest_offline_cache.clone() else {
        return;
    };
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Offline copy")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new("Ready")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Close offline copy").clicked() {
                        state.latest_offline_cache = None;
                    }
                    if drawer_button(
                        ui,
                        "Copy",
                        BrowserActionRole::Secondary,
                        "Copy cached page text",
                    )
                    .clicked()
                    {
                        ui.ctx().copy_text(result.text.clone());
                        state.capture_notice = Some("Offline copy text copied".to_owned());
                    }
                    if result.archive_mhtml.is_some()
                        && drawer_button(
                            ui,
                            "Archive",
                            BrowserActionRole::Secondary,
                            "Save cached offline web archive",
                        )
                        .clicked()
                    {
                        state.save_latest_offline_cache_archive();
                    }
                    if result.pdf_snapshot.is_some()
                        && drawer_button(
                            ui,
                            "PDF",
                            BrowserActionRole::Secondary,
                            "Open cached PDF snapshot",
                        )
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!("Text {} chars", result.text.chars().count()))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                if result.cached_ms.is_some() {
                    ui.label(
                        RichText::new("Saved now")
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(viewport) = &result.viewport {
                    ui.label(
                        RichText::new(format!("Preview {}x{}", viewport.width, viewport.height))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(archive) = &result.archive_mhtml {
                    ui.label(
                        RichText::new(format!("Web archive {} bytes", archive.bytes))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
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
                            "Resources {}, blocked {}",
                            result.resources.len(),
                            blocked
                        ))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
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
                    .on_hover_ui(|ui| chrome_tooltip(ui, "Cached viewport image"));
                }
            }
            egui::ScrollArea::vertical()
                .max_height(140.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(result.text.as_str())
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT),
                    );
                });
        });
}

pub(super) fn security_update_drawer(ui: &mut egui::Ui, state: &mut WebState) {
    let Some(status) = state.latest_security_update.clone() else {
        return;
    };
    if !status.is_actionable() {
        return;
    }
    egui::Frame::NONE
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                drawer_status_row(
                    ui,
                    ChromeIcon::Engine,
                    "Browser engine update",
                    super::CHROME_TEXT,
                );
                let tone = status.tone();
                drawer_status_row(
                    ui,
                    drawer_tone_icon(tone, ChromeIcon::Engine),
                    status.drawer_state_label(),
                    super::tone_color(tone),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Hide browser engine update status").clicked() {
                        state.latest_security_update = None;
                    }
                });
            });

            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(status.updater_label())
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                if let Some(target) = status.target_chromium_label() {
                    ui.label(
                        RichText::new(target)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(installed) = status.installed_chromium_label() {
                    ui.label(
                        RichText::new(installed)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(channel) = status.channel_label() {
                    ui.label(
                        RichText::new(channel)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
            });

            for detail in status.user_facing_details() {
                drawer_status_row(ui, ChromeIcon::Warning, &detail, super::CHROME_WARN);
            }
        });
}

pub(super) fn speech_status_drawer(ui: &mut egui::Ui, state: &mut WebState) {
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
        .fill(super::CHROME_SURFACE_CONTAINER)
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                drawer_status_row(ui, ChromeIcon::Audio, "Browser speech", super::CHROME_TEXT);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if drawer_close_button(ui, "Hide browser speech status").clicked() {
                        state.latest_read_aloud_status = None;
                        state.latest_voice_command_status = None;
                    }
                });
            });

            if let Some(status) = read_aloud {
                ui.horizontal_wrapped(|ui| {
                    let tone = status.tone();
                    drawer_status_row(
                        ui,
                        drawer_tone_icon(tone, ChromeIcon::Audio),
                        &status.chip_label(),
                        super::tone_color(tone),
                    );
                    if let Some(title) = status.last_title.as_deref() {
                        ui.label(
                            RichText::new(title)
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                        );
                    } else if let Some(url) = status.last_url.as_deref() {
                        ui.label(
                            RichText::new(url)
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                        );
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} accepted / {} spoken / {} rejected",
                            status.accepted, status.spoken, status.rejected
                        ))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                    );
                });
                if let Some(error) = status.user_facing_error() {
                    drawer_status_row(ui, ChromeIcon::Warning, &error, super::CHROME_WARN);
                }
            }

            if let Some(status) = voice {
                ui.horizontal_wrapped(|ui| {
                    let tone = status.tone();
                    drawer_status_row(
                        ui,
                        drawer_tone_icon(tone, ChromeIcon::Search),
                        &status.chip_label(),
                        super::tone_color(tone),
                    );
                    if let Some(url) = status.last_url.as_deref() {
                        ui.label(
                            RichText::new(url)
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                        );
                    }
                    if let Some(chars) = status.last_transcript_chars {
                        ui.label(
                            RichText::new(format!("{chars} transcript chars"))
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT_DIM),
                        );
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} accepted / {} transcribed / {} rejected",
                            status.accepted, status.transcribed, status.rejected
                        ))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                    );
                });
                if let Some(error) = status.user_facing_error() {
                    drawer_status_row(ui, ChromeIcon::Warning, &error, super::CHROME_WARN);
                }
            }
        });
}
