//! Browser chrome accessibility and body layout helpers.
//!
//! These helpers describe the shell-owned Browser surface and page canvas to
//! AccessKit. They live with `chrome_ui` because the labels summarize the
//! Browser chrome/page presentation rather than helper-session state mutation.

use super::*;

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
    if tab.autoplay_blocked {
        tools.push("autoplay blocked");
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
    if let Some(page) = tab.internal_page {
        return format!(
            "Browser internal page, {}, {}, internal, container {}, display target {}, {}",
            page.title(),
            page.url(),
            tab.container.label(),
            tab.display_target.label(),
            tab_accessibility_tools(tab)
        );
    }

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
        engine_display_name(tab.engine),
        tab_accessibility_state(tab),
        security,
        tab.container.label(),
        tab.display_target.label(),
        tab_accessibility_tools(tab)
    )
}

fn browser_gate_notice(state: &WebState) -> &str {
    #[cfg(feature = "live-helper")]
    {
        state
            .gate_notice
            .as_deref()
            .unwrap_or(BROWSER_NO_LIVE_PAGE_NOTICE)
    }
    #[cfg(not(feature = "live-helper"))]
    {
        let _ = state;
        BROWSER_NO_LIVE_PAGE_NOTICE
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

pub(super) fn install_browser_accessibility(
    ctx: &egui::Context,
    rect: egui::Rect,
    state: &WebState,
) {
    let summary = browser_accessibility_summary(state);
    let _ = ctx.accesskit_node_builder(browser_accessibility_id(), |node| {
        node.set_role(egui::accesskit::Role::Status);
        node.set_live(egui::accesskit::Live::Polite);
        node.set_label("Browser status");
        node.set_value(summary);
        node.set_bounds(accesskit_rect(rect));
    });
}

pub(super) fn install_browser_page_accessibility(
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

/// Center `content` vertically + horizontally in the remaining Browser body.
pub(super) fn centered(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.5 - Style::SP_XL);
        content(ui);
    });
}
