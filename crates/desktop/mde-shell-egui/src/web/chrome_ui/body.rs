//! Browser body routing.
//!
//! `web_panel` owns polling and top-level chrome layout; this module owns the
//! decision tree for which Browser body surface appears below the chrome.

use super::super::{
    cert_error_back_action, crash_reason, is_new_tab_url, shows_cert_interstitial,
    CertErrorBackAction, ManagedPolicyBlock, ManagedPolicyBlockTrigger, WebState,
};
use super::*;

pub(super) fn active_body(ui: &mut egui::Ui, state: &mut WebState) {
    // Read the active tab's status first so the crashed/cert-error arms can
    // mutate `state` (respawn flag, back/close) without holding a `&mut Tab`
    // borrow of it.
    let active = state.active;
    if state.managed_policy_block.is_none() {
        let helper_block = state.tabs.get(active).and_then(|tab| {
            tab.session.managed_policy_block().map(|url| {
                state
                    .managed_policy_block_for(url)
                    .unwrap_or_else(|| ManagedPolicyBlock {
                        url: url.to_owned(),
                        rule: "managed-policy".to_owned(),
                    })
            })
        });
        if let Some(block) = helper_block {
            state.block_managed_navigation(block, ManagedPolicyBlockTrigger::HelperDocument, None);
        }
    }
    if let Some(block) = state.managed_policy_block.clone() {
        if managed_policy_interstitial_body(ui, &block) {
            state.managed_policy_block = None;
            if let Some(tab) = state.active_tab() {
                tab.session.clear_managed_policy_block();
                tab.session.go_back();
            }
            state.mark_active_tab_activity();
        }
        return;
    }
    // Safe-browsing: a top-level navigation to an unsafe host shows a full-page
    // "unsafe site" interstitial (the request was already dropped upstream). Taken
    // before the normal body, mirroring the cert-error precedence.
    let sb_block = state
        .tabs
        .get(active)
        .and_then(|t| t.session.safe_browsing_block().map(str::to_owned));
    if let Some(url) = sb_block {
        if safe_browsing_interstitial_body(ui, &url) {
            if let Some(tab) = state.active_tab() {
                tab.session.go_back();
            }
            state.mark_active_tab_activity();
        }
        return;
    }
    // Permission prompt: an origin's pending capability request renders a small bar
    // atop the page (Allow/Block). A capability granted earlier this session
    // auto-allows inside `pending_permission_prompt` and never reaches here.
    // Guard: NEVER paint the bar over a crash/cert interstitial that will replace the
    // page below; a blocked/crashed page can't have raised the request, but keep the
    // precedence honest defensively (safe-browsing already returned above).
    let interstitial_below = state
        .tabs
        .get(active)
        .is_some_and(|t| t.session.is_crashed() || t.session.cert_error().is_some());
    if !interstitial_below {
        if let Some(pending) = state.pending_passkey_consent.clone() {
            let active_tab_id = state.tabs.get(active).map(|tab| tab.id);
            match passkey_consent_prompt_bar(ui, &pending, active_tab_id) {
                Some(true) => state.approve_pending_passkey(),
                Some(false) => state.deny_pending_passkey(),
                None => {}
            }
        } else if let Some(prompt) = state.pending_before_unload_prompt() {
            if let Some(proceed) = before_unload_prompt_bar(ui, &prompt) {
                state.answer_active_before_unload(prompt.id, proceed);
            }
        } else if let Some((origin, kind)) = state.pending_permission_prompt() {
            if let Some(allow) = permission_prompt_bar(ui, &origin, kind) {
                state.answer_active_permission(&origin, kind, allow);
            }
        } else if let Some(pending) = state.active_pending_login_save().cloned() {
            // "Save password?" offer for an auto-captured login submit.
            match login_save_prompt_bar(ui, &pending.host, &pending.username) {
                Some(true) => state.accept_pending_login_save(),
                Some(false) => state.dismiss_pending_login_save(),
                None => {}
            }
        }
    }
    let status = state.tabs.get(active).map(|t| {
        let is_crashed = t.session.is_crashed();
        let cert_error = t.session.cert_error().cloned();
        // `shows_cert_interstitial` is the single source of truth for the
        // crashed-wins precedence; fold its verdict into the option here so
        // the match arms below don't have to re-derive the ordering.
        let cert_interstitial =
            shows_cert_interstitial(is_crashed, cert_error.as_ref()).then_some(cert_error);
        (
            is_crashed,
            cert_interstitial.flatten(),
            t.texture.is_some(),
            is_new_tab_url(t.session.nav().url.trim()),
            crash_reason(&t.session),
            t.session.nav().can_back,
        )
    });
    match status {
        Some((true, _, _, _, reason, _)) => {
            if let Some(snapshot) = state.offline_cache_fallback_for_unavailable().cloned() {
                cached_offline_body(ui, &snapshot, Some(reason.as_str()));
            } else {
                crashed_body(ui, reason, &mut state.respawn_requested);
            }
        }
        // The engine blocks the navigation by default on a TLS/certificate
        // error (cert-error ENGINE half) and hands the shell a `CertError`;
        // this takes precedence over a normal frame/dashboard the same way
        // `is_crashed` does, checked right beside it, one arm down.
        Some((false, Some(err), _, _, _, can_back)) => {
            if cert_error_body(ui, &err, can_back) {
                match cert_error_back_action(can_back) {
                    CertErrorBackAction::GoBack => {
                        if let Some(tab) = state.active_tab() {
                            tab.session.go_back();
                        }
                        state.mark_active_tab_activity();
                    }
                    CertErrorBackAction::CloseTab => state.close_tab(active),
                }
            }
        }
        Some((false, None, _, true, _, _)) => new_tab_dashboard(ui, state),
        Some((false, None, true, false, _, _)) => paint_body(ui, state, active),
        Some((false, None, false, false, _, _)) => {
            // Connected, no first frame yet: an honest loading note, never a blank.
            centered(ui, |ui| {
                browser_body_note(ui, "Loading the page\u{2026}");
            });
        }
        None => {
            let cached = state.offline_cache_fallback_for_unavailable().cloned();
            // The honest gated body: a `live-helper` build shows the NAMED gate
            // notice when one is set; the default build always shows the standard
            // gated caption.
            #[cfg(feature = "live-helper")]
            let notice = state.gate_notice.as_deref();
            #[cfg(not(feature = "live-helper"))]
            let notice: Option<&str> = None;
            if let Some(snapshot) = cached {
                cached_offline_body(ui, &snapshot, notice);
            } else {
                empty_body(ui, notice);
            }
        }
    }
}
