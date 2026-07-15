//! Secondary browser overlay drawers — the dismissible panels that render
//! *below* the navigation chrome for the active tab: print settings,
//! downloads ledger, QR share, translation, spellcheck, offline copy,
//! browser-engine update, and speech status. Extracted verbatim from
//! `web/mod.rs` (P2 shell-ux-7); each `*_drawer` is invoked once per
//! render pass from the `chrome_ui` drawer stack and reads/writes the
//! shared [`WebState`] through the existing Browser state/action seams.

use super::super::{
    offline_cache_viewport_display_size, offline_cache_viewport_texture, plural,
    short_transfer_name, spellcheck_occurrence_index, spellcheck_results_text,
    BrowserReadAloudStatus, BrowserVoiceCommandStatus, PaperSize, PrintOrientation,
};
use super::*;
use mde_egui::egui::{RichText, Sense};
use mde_egui::muted_note;
use mde_files_egui::transfers::{TransferState, TransferVerb};

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
                    RichText::new("CUPS destination")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
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
                    super::CHROME_ERROR,
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
                egui::ComboBox::from_label("Orientation")
                    .selected_text(state.cups_settings.orientation.label())
                    .show_ui(ui, |ui| {
                        for o in [PrintOrientation::Portrait, PrintOrientation::Landscape] {
                            ui.selectable_value(&mut state.cups_settings.orientation, o, o.label());
                        }
                    });
                egui::ComboBox::from_label("Paper")
                    .selected_text(state.cups_settings.paper_size.label())
                    .show_ui(ui, |ui| {
                        for p in [
                            PaperSize::Default,
                            PaperSize::A4,
                            PaperSize::Letter,
                            PaperSize::Legal,
                        ] {
                            ui.selectable_value(&mut state.cups_settings.paper_size, p, p.label());
                        }
                    });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Pages").size(Style::SMALL));
                    ui.add(
                        egui::TextEdit::singleline(&mut state.cups_settings.page_ranges)
                            .hint_text("all")
                            .desired_width(80.0),
                    )
                    .on_hover_text("Page range, e.g. 1-5,8 — empty prints all pages");
                });
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

/// The user-authored Site Styles editor (safe userscript slice — CSS only): add a
/// host + CSS rule that folds into the injected userscript bundle, or remove one.
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
                    RichText::new("your CSS, injected on matching hosts (Userscripts must be on)")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close site styles")
                        .clicked()
                    {
                        state.site_styles_open = false;
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Host").size(Style::SMALL));
                ui.add(
                    egui::TextEdit::singleline(&mut state.site_style_host_draft)
                        .hint_text("example.com")
                        .desired_width(140.0),
                );
                if ui
                    .add(egui::Button::new(RichText::new("Add").size(Style::SMALL)))
                    .clicked()
                {
                    state.add_user_site_style();
                }
            });
            ui.add(
                egui::TextEdit::multiline(&mut state.site_style_css_draft)
                    .hint_text("body{max-width:80ch;margin-inline:auto}")
                    .desired_rows(2)
                    .desired_width(320.0),
            );
            if !state.user_site_styles.is_empty() {
                ui.separator();
                for (i, style) in state.user_site_styles.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} \u{2014} {}",
                                style.host,
                                ellipsize(&style.css, 36)
                            ))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                        );
                        if ui.small_button("Remove").clicked() {
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
                    if ui
                        .small_button("\u{00D7}")
                        .on_hover_text("Close history")
                        .clicked()
                    {
                        close = true;
                    }
                    if !state.history.is_empty()
                        && ui
                            .small_button("Clear")
                            .on_hover_text("Forget this session's history")
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
                    for visit in state.history.visits() {
                        let label = if visit.title.trim().is_empty() {
                            visit.url.clone()
                        } else {
                            visit.title.clone()
                        };
                        let elided = if label.chars().count() > 72 {
                            format!("{}\u{2026}", label.chars().take(71).collect::<String>())
                        } else {
                            label
                        };
                        if ui
                            .add(
                                egui::Button::new(RichText::new(elided).size(Style::SMALL))
                                    .frame(false),
                            )
                            .on_hover_text(visit.url.clone())
                            .clicked()
                        {
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
                    RichText::new("browser_download ledger")
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
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
                    if !jobs.is_empty()
                        && ui
                            .small_button("Clear all")
                            .on_hover_text("Remove every download from this list")
                            .clicked()
                    {
                        clear_all = true;
                    }
                });
            });

            if let Some(notice) = &state.download_notice {
                ui.colored_label(
                    super::CHROME_ERROR,
                    RichText::new(format!("! {notice}")).size(Style::SMALL),
                );
            }

            if let Some(pending) = &pending_dangerous {
                ui.separator();
                egui::Frame::NONE
                    .fill(super::prompt_fill())
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new("\u{26A0} This type of file can harm your device")
                                    .size(Style::SMALL)
                                    .color(super::CHROME_WARN),
                            );
                        });
                        ui.label(
                            RichText::new(&pending.filename)
                                .size(Style::SMALL)
                                .color(super::CHROME_TEXT),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new(
                                    RichText::new("Keep")
                                        .size(Style::SMALL)
                                        .color(super::CHROME_WARN),
                                ))
                                .on_hover_text("Download it anyway")
                                .clicked()
                            {
                                keep_dangerous = true;
                            }
                            if ui
                                .add(egui::Button::new(
                                    RichText::new("Discard")
                                        .size(Style::SMALL)
                                        .color(super::CHROME_TEXT_DIM),
                                ))
                                .on_hover_text("Drop this download")
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
                            ui.add(
                                egui::ProgressBar::new(f32::from(progress) / 100.0)
                                    .desired_width((ui.available_width() * 0.55).max(120.0))
                                    .text(format!("{progress}%")),
                            );
                        }
                        if let Some(err) = &job.error {
                            ui.colored_label(
                                super::CHROME_ERROR,
                                RichText::new(format!("! {err}")).size(Style::SMALL),
                            );
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button("Remove")
                            .on_hover_text("Remove from list")
                            .clicked()
                        {
                            removed = Some(job.id.clone());
                        }
                        if let Some(url) = state.download_source_urls.get(&job.id) {
                            if ui
                                .small_button("Copy link")
                                .on_hover_text("Copy the download's source URL")
                                .clicked()
                            {
                                ui.ctx().copy_text(url.clone());
                                state.capture_notice = Some("Download link copied".to_owned());
                            }
                        }
                        if job.state == TransferState::Done {
                            if ui
                                .small_button("Show")
                                .on_hover_text("Show the completed download in its folder")
                                .clicked()
                            {
                                reveal_download = Some(job.id.clone());
                            }
                            if ui
                                .small_button("Open")
                                .on_hover_text("Open the completed download")
                                .clicked()
                            {
                                open_download = Some(job.id.clone());
                            }
                        }
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
                    RichText::new(result.request_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} modules from {}",
                        result.modules.len(),
                        result.host
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
                    RichText::new(format!(
                        "{} \u{2192} {}",
                        result.source_lang, result.target_lang
                    ))
                    .size(Style::SMALL)
                    .color(super::CHROME_TEXT_DIM),
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from tab {} / {}",
                        result.translation.chars().count(),
                        result.tab_index,
                        result.engine.label()
                    ))
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
                ui.label(
                    RichText::new(error)
                        .size(Style::SMALL)
                        .color(super::CHROME_WARN),
                );
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
                    RichText::new(result.cache_id.chars().take(12).collect::<String>())
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
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
                        .color(super::CHROME_TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "{} chars from tab {} / {}",
                        result.text.chars().count(),
                        result.tab_index,
                        result.engine.label()
                    ))
                    .size(Style::SMALL)
                    .color(super::CHROME_TEXT_DIM),
                );
                if let Some(cached_ms) = result.cached_ms {
                    ui.label(
                        RichText::new(format!("cached {cached_ms}"))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(viewport) = &result.viewport {
                    ui.label(
                        RichText::new(format!(
                            "viewport PNG {}x{}",
                            viewport.width, viewport.height
                        ))
                        .size(Style::SMALL)
                        .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(archive) = &result.archive_mhtml {
                    ui.label(
                        RichText::new(format!("MHTML {} bytes", archive.bytes))
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
                            "resources {} / {} blocked",
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
                    .on_hover_text("Cached viewport image");
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
                ui.label(
                    RichText::new("Browser engine update")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
                );
                ui.label(
                    RichText::new(status.state.as_str())
                        .size(Style::SMALL)
                        .color(super::tone_color(status.tone())),
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
                        .color(super::CHROME_TEXT_DIM),
                );
                if let Some(chromium) = &status.expected_chromium_version {
                    ui.label(
                        RichText::new(format!("Chromium {chromium}"))
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
                    );
                }
                if let Some(runtime) = &status.active_runtime {
                    ui.label(
                        RichText::new(runtime)
                            .size(Style::SMALL)
                            .color(super::CHROME_TEXT_DIM),
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
                ui.label(
                    RichText::new(detail)
                        .size(Style::SMALL)
                        .color(super::CHROME_WARN),
                );
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
                ui.label(
                    RichText::new("Browser speech")
                        .size(CHROME_FONT)
                        .color(super::CHROME_TEXT),
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
                            .color(super::tone_color(status.tone())),
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
                if let Some(error) = status.last_error.as_deref() {
                    ui.label(
                        RichText::new(error)
                            .size(Style::SMALL)
                            .color(super::CHROME_WARN),
                    );
                }
            }

            if let Some(status) = voice {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(status.chip_label())
                            .size(Style::SMALL)
                            .color(super::tone_color(status.tone())),
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
                if let Some(error) = status.last_error.as_deref() {
                    ui.label(
                        RichText::new(error)
                            .size(Style::SMALL)
                            .color(super::CHROME_WARN),
                    );
                }
            }
        });
}
