//! The Browser surface's **bus wire codec** and URL/omnibox helpers — the pure,
//! non-UI functions that translate between the egui surface and the daemon-owned
//! message bus.
//!
//! Three cohesive concerns live here, all headless and unit-asserted:
//!  * **Request bodies + topics** — the `*_body` builders (bookmark add, ad-filter
//!    domain, chat/platform share, follow-me send-tab, permission prompt, passkey,
//!    read-aloud, translate, offline-cache snapshot, voice command, session sync,
//!    tab suspend, protocol handoff, notify) and the `browser_*_topic` strings the
//!    daemon workers subscribe to.
//!  * **Result parsers** — the `parse_*` decoders that turn daemon result bodies
//!    (translation, share route + QR, offline-cache bundle, read-aloud / voice /
//!    passkey / security-update status, voice transcript) back into typed records,
//!    with the offline-cache/PDF byte + filename validators and the HTTP-cache URL
//!    canonicalizer.
//!  * **URL + omnibox utilities** — omnibox target resolution, the suggestions
//!    fetch/parse path, Chromium DevTools frontend discovery, scheme/host checks,
//!    percent-encoding, https upgrade, and the `ExternalProtocol` (tel/mailto/magnet)
//!    + `BrowserShareTarget` / `BrowserSendTabTarget` handoff enums.
//!
//! `use super::*` pulls in the parent's engine/state/request/result types, the bus
//! `publish*` helpers, and `std`/`serde_json` re-exports. A pure relocation from the
//! `web` god-module — no behaviour change.

use super::*;

/// Build the `action/bookmarks/add` body for the live page. Pure — the wire shape
/// is asserted headless. `source` is omitted, so the worker mints the default
/// `Source::Manual` (a page the user bookmarked in-app).
pub(super) fn bookmark_add_body(url: &str, title: &str) -> String {
    serde_json::json!({ "url": url, "title": title }).to_string()
}

pub(super) fn adfilter_domain_body(domain: &str) -> String {
    serde_json::json!({ "domain": domain.trim() }).to_string()
}

pub(super) fn browser_site_blocking_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    host: &str,
    enabled: bool,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_site_blocking",
        "policy": "adfilter_site_override",
        "decision": if enabled { "enable" } else { "disable" },
        "site_blocking": if enabled { "enabled" } else { "disabled" },
        "enforcement": "request_filter",
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host.trim(),
        "title": title.trim(),
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

pub(super) fn browser_display_target_body(
    tab_index: usize,
    tab: &Tab,
    display_target: DisplayTarget,
) -> String {
    serde_json::json!({
        "op": "browser_display_target",
        "tab_index": tab_index,
        "engine": tab.engine.wire(),
        "target": display_target.wire(),
        "url": tab.session.nav().url.as_str(),
        "title": tab.session.title(),
    })
    .to_string()
}

/// Build the `action/chat/send` body sharing the live page into Chat. A link is
/// carried as the NOTIFY-CHAT [`MessageKind::Clipboard`] kind — its `preview`
/// (the title, falling back to the URL) shows in the timeline and its `full` (the
/// exact URL) is what a one-click re-copy puts back. Pure: the `kind` is a real
/// `mde_chat::MessageKind`, so it round-trips straight into what the worker
/// accepts (the same shape Files' `chat_bridge` writes).
pub(super) fn chat_share_body(to: &str, url: &str, title: &str) -> String {
    let preview = if title.trim().is_empty() { url } else { title };
    let kind = MessageKind::Clipboard {
        preview: preview.to_string(),
        full: url.to_string(),
    };
    let kind_val = serde_json::to_value(&kind).unwrap_or(serde_json::Value::Null);
    serde_json::json!({ "scope": "peer", "to": to, "kind": kind_val }).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserShareTarget {
    Peer,
    Phone,
    Email,
    Qr,
}

impl BrowserShareTarget {
    const fn wire(self) -> &'static str {
        match self {
            Self::Peer => "peer",
            Self::Phone => "phone",
            Self::Email => "email",
            Self::Qr => "qr",
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Peer => "Peer",
            Self::Phone => "Phone",
            Self::Email => "Email",
            Self::Qr => "QR",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "peer" => Some(Self::Peer),
            "phone" => Some(Self::Phone),
            "email" => Some(Self::Email),
            "qr" => Some(Self::Qr),
            _ => None,
        }
    }

    fn destination(self) -> Option<(String, String)> {
        match self {
            Self::Phone => browser_phone_target_destination(),
            Self::Peer | Self::Email | Self::Qr => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BrowserSendTabTarget {
    Node,
    Phone,
}

impl BrowserSendTabTarget {
    const fn wire(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Phone => "phone",
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Node => "Node",
            Self::Phone => "Phone",
        }
    }

    fn destination(self) -> Option<(String, String)> {
        match self {
            Self::Node => browser_node_target_destination(),
            Self::Phone => browser_phone_target_destination(),
        }
    }
}

fn browser_node_target_destination() -> Option<(String, String)> {
    std::env::var("MDE_BROWSER_SEND_NODE_TARGET")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .and_then(|id| {
            let local = local_hostname();
            if sanitize_endpoint_id(&id) == sanitize_endpoint_id(&local) {
                None
            } else {
                let label = std::env::var("MDE_BROWSER_SEND_NODE_LABEL")
                    .ok()
                    .map(|label| label.trim().to_owned())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| id.clone());
                Some((id, label))
            }
        })
}

fn sanitize_endpoint_id(value: &str) -> String {
    value
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                Some(c)
            } else if c.is_ascii_whitespace() {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

fn browser_phone_target_destination() -> Option<(String, String)> {
    std::env::var("MDE_BROWSER_SEND_PHONE_TARGET")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
        .map(|id| {
            let label = std::env::var("MDE_BROWSER_SEND_PHONE_LABEL")
                .ok()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| id.clone());
            (id, label)
        })
}

/// Build the browser-owned platform share handoff. The receiving surfaces are
/// intentionally outside Browser ownership, so this publishes a stable typed verb
/// instead of pretending to complete peer/email/QR delivery in-process.
pub(super) fn browser_share_body(target: BrowserShareTarget, url: &str, title: &str) -> String {
    let title = title.trim();
    let preview = if title.is_empty() { url } else { title };
    let mut body = serde_json::json!({
        "op": "browser_share",
        "target": target.wire(),
        "url": url,
        "title": title,
        "preview": preview,
        "source": "browser",
        "host": local_hostname(),
    });
    if let Some((id, label)) = target.destination() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("target_id".to_owned(), serde_json::json!(id));
            obj.insert("target_label".to_owned(), serde_json::json!(label));
        }
    }
    body.to_string()
}

pub(super) fn publish_browser_share(
    root: Option<&Path>,
    target: BrowserShareTarget,
    url: &str,
    title: &str,
) {
    let body = browser_share_body(target, url, title);
    if root.is_some() {
        publish_to_bus(root, ACTION_BROWSER_SHARE, &body);
    } else {
        publish(ACTION_BROWSER_SHARE, &body);
    }
}

/// Build the browser-owned send-tab handoff for BROWSER-DD-7. Target selection and
/// delivery live in the session-sync / phone owners, so the Browser publishes the
/// current tab's URL/title/engine metadata and lets those owners route it.
pub(super) fn browser_send_tab_body(
    target: BrowserSendTabTarget,
    engine: BrowserEngine,
    url: &str,
    title: &str,
) -> String {
    let title = title.trim();
    let preview = if title.is_empty() { url } else { title };
    let mut body = serde_json::json!({
        "op": "browser_send_tab",
        "target": target.wire(),
        "engine": engine.wire(),
        "url": url,
        "title": title,
        "preview": preview,
        "source": "browser",
        "host": local_hostname(),
    });
    if let Some((id, label)) = target.destination() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("target_id".to_owned(), serde_json::json!(id));
            obj.insert("target_label".to_owned(), serde_json::json!(label));
        }
    }
    body.to_string()
}

pub(super) fn publish_browser_send_tab(
    root: Option<&Path>,
    target: BrowserSendTabTarget,
    engine: BrowserEngine,
    url: &str,
    title: &str,
) -> bool {
    if matches!(target, BrowserSendTabTarget::Node) && target.destination().is_none() {
        return false;
    }
    let body = browser_send_tab_body(target, engine, url, title);
    if root.is_some() {
        publish_to_bus(root, ACTION_BROWSER_SEND_TAB, &body);
    } else {
        publish(ACTION_BROWSER_SEND_TAB, &body);
    }
    true
}

pub(super) fn browser_permission_prompt_body(
    kind: DevicePermissionKind,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    site: &str,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_permission_prompt",
        "permission": kind.wire(),
        "decision": "deny",
        "enforcement": "helper_default_deny",
        "engine": engine.wire(),
        "url": url,
        "title": title.trim(),
        "site": site,
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

fn browser_runtime_permission_wire(kind: u8) -> &'static str {
    match kind {
        0 => "geolocation",
        1 => "notifications",
        2 => "clipboard",
        3 => "camera",
        4 => "microphone",
        5 => "camera_microphone",
        _ => "unknown",
    }
}

pub(super) fn browser_permission_decision_body(
    engine: BrowserEngine,
    origin: &str,
    kind: u8,
    allow: bool,
    enforcement: &str,
    url: &str,
    title: &str,
    decided_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_permission_decision",
        "permission": browser_runtime_permission_wire(kind),
        "permission_kind": kind,
        "decision": if allow { "allow" } else { "deny" },
        "grant_scope": if allow { "session" } else { "none" },
        "enforcement": enforcement.trim(),
        "engine": engine.wire(),
        "origin": origin.trim(),
        "origin_host": host_of(origin).unwrap_or_else(|| origin.trim().to_owned()),
        "url": url.trim(),
        "title": title.trim(),
        "source": "browser",
        "node": local_hostname(),
        "decided_ms": decided_ms,
    })
    .to_string()
}

pub(super) fn browser_permission_revoke_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    host: &str,
    revoked_grants: usize,
    cleared_prompt_decisions: usize,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_permission_revoke",
        "decision": "revoke",
        "enforcement": "session_permission_store",
        "permission_policy": "default_deny",
        "scope": "current_site",
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host.trim(),
        "title": title.trim(),
        "revoked_grants": revoked_grants,
        "cleared_prompt_decisions": cleared_prompt_decisions,
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

pub(super) fn browser_credential_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    host: &str,
    decision: &str,
    trigger: &str,
    credential_count: usize,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_credential",
        "decision": decision.trim(),
        "enforcement": "session_credential_store",
        "privacy": "redacted",
        "scope": "session_only",
        "trigger": trigger.trim(),
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host.trim(),
        "title": title.trim(),
        "credential_count": credential_count,
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

pub(super) fn browser_policy_block_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    rule: &str,
    trigger: &str,
    blocked_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_policy_block",
        "policy": "managed_url",
        "decision": "block",
        "enforcement": "pre_network",
        "trigger": trigger,
        "engine": engine.wire(),
        "url": url,
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "title": title.trim(),
        "rule": rule.trim(),
        "source": "browser",
        "node": local_hostname(),
        "blocked_ms": blocked_ms,
    })
    .to_string()
}

pub(super) fn browser_safe_browsing_block_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    rule: &str,
    trigger: &str,
    blocked_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_safe_browsing_block",
        "policy": "safe_browsing",
        "decision": "block",
        "enforcement": "pre_network",
        "trigger": trigger.trim(),
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "title": title.trim(),
        "rule": rule.trim(),
        "source": "browser",
        "node": local_hostname(),
        "blocked_ms": blocked_ms,
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn browser_policy_source_status_body(
    op: &str,
    policy: &str,
    source_path: &Path,
    state: &str,
    item_count: usize,
    effective_count: usize,
    checked_ms: u64,
    loaded_ms: Option<u64>,
    error: Option<&str>,
) -> String {
    serde_json::json!({
        "op": op.trim(),
        "policy": policy.trim(),
        "state": state.trim(),
        "source_path": source_path.to_string_lossy(),
        "item_count": item_count,
        "effective_count": effective_count,
        "loaded_ms": loaded_ms,
        "error": error.map(str::trim),
        "source": "browser",
        "node": local_hostname(),
        "checked_ms": checked_ms,
    })
    .to_string()
}

pub(super) fn browser_certificate_error_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    code: i32,
    message: &str,
    blocked_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_certificate_error",
        "policy": "tls_certificate",
        "decision": "block",
        "enforcement": "engine_certificate_validation",
        "reason": "certificate_error",
        "trigger": "top_level_navigation",
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "title": title.trim(),
        "code": code,
        "message": message.trim(),
        "source": "browser",
        "node": local_hostname(),
        "blocked_ms": blocked_ms,
    })
    .to_string()
}

pub(super) fn browser_insecure_download_block_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    trigger: &str,
    blocked_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_insecure_download_block",
        "policy": "insecure_transport",
        "decision": "block",
        "enforcement": "pre_network",
        "reason": "plain_http_download",
        "trigger": trigger.trim(),
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "title": title.trim(),
        "source": "browser",
        "node": local_hostname(),
        "blocked_ms": blocked_ms,
    })
    .to_string()
}

pub(super) fn browser_insecure_navigation_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    decision: &str,
    trigger: &str,
    enforcement: &str,
    upgraded_url: Option<&str>,
    decided_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_insecure_navigation",
        "policy": "insecure_transport",
        "decision": decision.trim(),
        "enforcement": enforcement.trim(),
        "reason": "plain_http_navigation",
        "trigger": trigger.trim(),
        "engine": engine.wire(),
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "upgraded_url": upgraded_url.map(str::trim),
        "title": title.trim(),
        "source": "browser",
        "node": local_hostname(),
        "decided_ms": decided_ms,
    })
    .to_string()
}

pub(super) fn browser_mixed_content_block_body(
    engine: BrowserEngine,
    page_url: &str,
    url: &str,
    title: &str,
    resource: u8,
    trigger: &str,
    blocked_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_mixed_content_block",
        "policy": "mixed_content",
        "decision": "block",
        "enforcement": "pre_network",
        "reason": "plain_http_subresource",
        "trigger": trigger.trim(),
        "engine": engine.wire(),
        "page_url": page_url.trim(),
        "page_host": host_of(page_url).unwrap_or_else(|| page_url.trim().to_owned()),
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "title": title.trim(),
        "resource": browser_resource_type_name(resource),
        "source": "browser",
        "node": local_hostname(),
        "blocked_ms": blocked_ms,
    })
    .to_string()
}

fn browser_resource_type_name(resource: u8) -> &'static str {
    match mde_web_preview_client::resource_from_wire(resource) {
        mde_web_preview_client::ResourceType::Document => "document",
        mde_web_preview_client::ResourceType::Subdocument => "subdocument",
        mde_web_preview_client::ResourceType::Stylesheet => "stylesheet",
        mde_web_preview_client::ResourceType::Script => "script",
        mde_web_preview_client::ResourceType::Image => "image",
        mde_web_preview_client::ResourceType::Font => "font",
        mde_web_preview_client::ResourceType::Media => "media",
        mde_web_preview_client::ResourceType::Object => "object",
        mde_web_preview_client::ResourceType::XmlHttpRequest => "xmlhttprequest",
        mde_web_preview_client::ResourceType::Ping => "ping",
        mde_web_preview_client::ResourceType::WebSocket => "websocket",
        mde_web_preview_client::ResourceType::Other => "other",
    }
}

pub(super) fn browser_site_data_clear_body(
    engine: BrowserEngine,
    url: &str,
    title: &str,
    host: &str,
    scope: &str,
    cleared_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_site_data_clear",
        "decision": "clear",
        "enforcement": "session_memory_only",
        "scope": scope,
        "engine": engine.wire(),
        "url": url,
        "host": host.trim(),
        "title": title.trim(),
        "source": "browser",
        "node": local_hostname(),
        "cleared_ms": cleared_ms,
    })
    .to_string()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BrowserBrowsingDataClearCounts {
    pub(super) history_entries: usize,
    pub(super) downloads: usize,
    pub(super) reopen_entries: usize,
    pub(super) saved_logins: usize,
    pub(super) permission_grants: usize,
}

pub(super) fn browser_browsing_data_clear_body(
    engine: BrowserEngine,
    active_url: &str,
    active_title: &str,
    active_host: &str,
    counts: BrowserBrowsingDataClearCounts,
    cleared_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_browsing_data_clear",
        "decision": "clear",
        "enforcement": "session_memory_only",
        "scope": "all_session",
        "engine": engine.wire(),
        "active_url": active_url.trim(),
        "active_host": active_host.trim(),
        "active_title": active_title.trim(),
        "history_entries": counts.history_entries,
        "downloads": counts.downloads,
        "reopen_entries": counts.reopen_entries,
        "saved_logins": counts.saved_logins,
        "permission_grants": counts.permission_grants,
        "source": "browser",
        "node": local_hostname(),
        "cleared_ms": cleared_ms,
    })
    .to_string()
}

pub(super) fn browser_download_danger_body(
    download_id: u64,
    url: &str,
    filename: &str,
    decision: &str,
    updated_ms: u64,
) -> String {
    serde_json::json!({
        "op": "browser_download_danger",
        "decision": decision.trim(),
        "enforcement": "dangerous_file_gate",
        "reason": "dangerous_extension",
        "download_id": download_id,
        "url": url.trim(),
        "host": host_of(url).unwrap_or_else(|| url.trim().to_owned()),
        "filename": filename.trim(),
        "source": "browser",
        "node": local_hostname(),
        "updated_ms": updated_ms,
    })
    .to_string()
}

pub(super) fn browser_passkey_body(
    engine: BrowserEngine,
    helper_body: &str,
) -> Result<String, String> {
    let helper: serde_json::Value =
        serde_json::from_str(helper_body).map_err(|err| format!("invalid helper JSON: {err}"))?;
    let ceremony = status_required_str(&helper, "ceremony", "passkey helper event")?;
    if !matches!(ceremony.as_str(), "create" | "get") {
        return Err("unsupported ceremony".to_owned());
    }
    let origin = status_required_str(&helper, "origin", "passkey helper event")?;
    let rp_id = status_required_str(&helper, "rp_id", "passkey helper event")?;
    let challenge_b64url =
        status_required_str(&helper, "challenge_b64url", "passkey helper event")?;

    let mut body = serde_json::json!({
        "op": "browser_passkey",
        "source": "browser",
        "host": local_hostname(),
        "engine": engine.wire(),
        "ceremony": ceremony,
        "origin": origin,
        "rp_id": rp_id,
        "challenge_b64url": challenge_b64url,
    });
    let Some(obj) = body.as_object_mut() else {
        return Err("could not build passkey body".to_owned());
    };
    for key in ["user_handle_b64url", "user_name"] {
        if let Some(value) = optional_trimmed_str(&helper, key) {
            obj.insert(key.to_owned(), serde_json::json!(value));
        }
    }
    if let Some(value) = optional_trimmed_str(&helper, "client_request_id") {
        obj.insert("client_request_id".to_owned(), serde_json::json!(value));
    }
    // security-2: forward the shim's user-presence (user-gesture) signal so the
    // daemon sets the WebAuthn User Present bit only when a human interaction was
    // actually observed, rather than hardcoding it. Absent => not present.
    let user_present = helper
        .get("user_present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    obj.insert("user_present".to_owned(), serde_json::json!(user_present));
    if let Some(timeout_ms) = helper.get("timeout_ms").and_then(serde_json::Value::as_u64) {
        obj.insert("timeout_ms".to_owned(), serde_json::json!(timeout_ms));
    }
    if let Some(credentials) = helper
        .get("allow_credentials")
        .and_then(serde_json::Value::as_array)
    {
        let credentials = credentials
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|credential| !credential.is_empty())
            .take(64)
            .collect::<Vec<_>>();
        obj.insert(
            "allow_credentials".to_owned(),
            serde_json::json!(credentials),
        );
    }
    Ok(body.to_string())
}

pub(super) fn browser_passkey_shell_approved_body(handoff_body: &str) -> Result<String, String> {
    let mut body: serde_json::Value = serde_json::from_str(handoff_body)
        .map_err(|err| format!("invalid passkey handoff JSON: {err}"))?;
    let Some(obj) = body.as_object_mut() else {
        return Err("passkey handoff is not an object".to_owned());
    };
    obj.insert("user_present".to_owned(), serde_json::json!(true));
    obj.insert("shell_consent".to_owned(), serde_json::json!(true));
    obj.insert(
        "presence_source".to_owned(),
        serde_json::json!("browser_shell_prompt"),
    );
    serde_json::to_string(&body).map_err(|err| format!("passkey handoff encode: {err}"))
}

pub(super) fn browser_passkey_denied_body(client_request_id: &str, reason: &str) -> String {
    serde_json::json!({
        "op": "browser_passkey_denied",
        "source": "browser",
        "client_request_id": client_request_id.trim(),
        "error": reason.trim(),
    })
    .to_string()
}

pub(super) fn passkey_client_request_id(helper_body: &str) -> Option<String> {
    let helper: serde_json::Value = serde_json::from_str(helper_body).ok()?;
    optional_trimmed_str(&helper, "client_request_id")
}

pub(super) const READ_ALOUD_TEXT_MAX_CHARS: usize = 20_000;
pub(super) const TRANSLATE_TEXT_MAX_CHARS: usize = 20_000;
pub(super) const TRANSLATION_RESULT_MAX_CHARS: usize = 40_000;
pub(super) const OFFLINE_CACHE_TEXT_MAX_CHARS: usize = 64_000;
const OFFLINE_CACHE_VIEWPORT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub(super) const OFFLINE_CACHE_MHTML_MAX_BYTES: usize = 4 * 1024 * 1024;
const OFFLINE_CACHE_PDF_MAX_BYTES: usize = 8 * 1024 * 1024;
const OFFLINE_CACHE_RESOURCE_MAX_COUNT: usize = 128;
const OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS: usize = 2_048;
pub(super) const MEDIA_SNIFFER_MAX_COUNT: usize = 128;
pub(super) const MEDIA_SNIFFER_URL_MAX_CHARS: usize = 2_048;
pub(super) const SCRAPE_CRAWL_SEED_MAX_COUNT: usize = 64;
pub(super) const SCRAPE_CRAWL_MANIFEST_MAX_COUNT: usize = 128;
pub(super) const SCRAPE_EXTRACT_TEXT_MAX_CHARS: usize = 64_000;
pub(super) const SCRAPE_ARTICLE_TEXT_MAX_CHARS: usize = 16_000;
pub(super) const SCRAPE_DOM_LINK_MAX_COUNT: usize = 64;
pub(super) const SCRAPE_DOM_HEADING_MAX_COUNT: usize = 32;
pub(super) const SCRAPE_DOM_TEXT_MAX_CHARS: usize = 240;

pub(super) fn browser_read_aloud_body(request: &ReadAloudRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, READ_ALOUD_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    serde_json::json!({
        "op": "browser_read_aloud",
        "source": "browser",
        "host": local_hostname(),
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    })
    .to_string()
}

pub(super) fn browser_translate_target_lang() -> String {
    let raw =
        std::env::var("MDE_BROWSER_TRANSLATE_TARGET_LANG").unwrap_or_else(|_| "en".to_owned());
    let lang = raw.trim();
    if lang.is_empty() {
        "en".to_owned()
    } else {
        clamp_chars(lang, 32)
    }
}

pub(super) fn browser_translate_body(request: &TranslateRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, TRANSLATE_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    serde_json::json!({
        "op": "browser_translate",
        "source": "browser",
        "host": local_hostname(),
        "privacy": "offline_or_mesh_only",
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "source_lang": request.source_lang.trim(),
        "target_lang": request.target_lang.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    })
    .to_string()
}

pub(super) fn browser_offline_cache_body(request: &OfflineCacheRequest, text: &str) -> String {
    let trimmed = text.trim();
    let original_chars = trimmed.chars().count();
    let text = clamp_chars(trimmed, OFFLINE_CACHE_TEXT_MAX_CHARS);
    let text_chars = text.chars().count();
    let mut body = serde_json::json!({
        "op": "browser_offline_cache",
        "source": "browser",
        "host": local_hostname(),
        "privacy": "offline_or_mesh_only",
        "tab_index": request.tab_index,
        "engine": request.engine.wire(),
        "url": request.url,
        "title": request.title.trim(),
        "text": text,
        "text_chars": text_chars,
        "truncated": text_chars < original_chars,
    });
    if let Some(viewport) = &request.viewport {
        body["viewport_image"] = serde_json::json!({
            "mime": &viewport.mime,
            "width": viewport.width,
            "height": viewport.height,
            "data": &viewport.data_base64,
        });
    }
    if !request.resources.is_empty() {
        body["resource_manifest"] = serde_json::Value::Array(
            request
                .resources
                .iter()
                .map(|resource| {
                    serde_json::json!({
                        "url": &resource.url,
                        "resource": &resource.resource,
                        "allowed": resource.allowed,
                        "blocked_by": &resource.blocked_by,
                    })
                })
                .collect(),
        );
    }
    if let Some(archive) = offline_cache_mhtml_archive(request, &text, unix_ms()) {
        body["archive_mhtml"] = serde_json::json!({
            "mime": &archive.mime,
            "filename": &archive.filename,
            "bytes": archive.bytes,
            "data": &archive.data_base64,
        });
    }
    if let Some(pdf) = &request.pdf_snapshot {
        body["pdf_snapshot"] = serde_json::json!({
            "mime": &pdf.mime,
            "filename": &pdf.filename,
            "bytes": pdf.bytes,
            "data": &pdf.data_base64,
        });
    }
    body.to_string()
}

pub(super) fn offline_cache_mhtml_archive(
    request: &OfflineCacheRequest,
    text: &str,
    unix_ms: u64,
) -> Option<OfflineCacheArchive> {
    let viewport_png = request
        .viewport
        .as_ref()
        .and_then(|viewport| {
            base64::engine::general_purpose::STANDARD
                .decode(viewport.data_base64.as_str())
                .ok()
        })
        .filter(|bytes| bytes.len() <= OFFLINE_CACHE_VIEWPORT_MAX_BYTES);
    let bytes = offline_cache_mhtml_document(
        &request.url,
        &request.title,
        unix_ms,
        text,
        viewport_png.as_deref(),
    );
    if bytes.is_empty() || bytes.len() > OFFLINE_CACHE_MHTML_MAX_BYTES {
        return None;
    }
    Some(OfflineCacheArchive {
        mime: "multipart/related".to_owned(),
        filename: capture_mhtml_filename_for(&request.url, &request.title, unix_ms),
        bytes: bytes.len(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub(super) fn offline_cache_viewport_image(
    frame: &egui::ColorImage,
) -> Option<OfflineCacheViewportImage> {
    let [width, height] = frame.size;
    let png = encode_color_image_png(frame).ok()?;
    if png.len() > OFFLINE_CACHE_VIEWPORT_MAX_BYTES {
        return None;
    }
    Some(OfflineCacheViewportImage {
        mime: "image/png".to_owned(),
        width,
        height,
        data_base64: base64::engine::general_purpose::STANDARD.encode(png),
    })
}

pub(super) fn offline_cache_resource_manifest(
    recent: &[mde_web_preview_client::ResourceRequestStatus],
) -> Vec<OfflineCacheResource> {
    recent
        .iter()
        .rev()
        .take(OFFLINE_CACHE_RESOURCE_MAX_COUNT)
        .filter_map(|resource| {
            let url = resource.url.trim();
            if url.is_empty() {
                return None;
            }
            Some(OfflineCacheResource {
                url: clamp_chars(url, OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS),
                resource: offline_cache_resource_type_name(resource.resource).to_owned(),
                allowed: resource.allowed,
                blocked_by: resource
                    .blocked_by
                    .as_deref()
                    .map(|rule| clamp_chars(rule.trim(), OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS)),
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

pub(super) fn offline_cache_resource_type_name(resource: u8) -> &'static str {
    match mde_web_preview_client::resource_from_wire(resource) {
        mde_web_preview_client::ResourceType::Document => "document",
        mde_web_preview_client::ResourceType::Subdocument => "subdocument",
        mde_web_preview_client::ResourceType::Stylesheet => "stylesheet",
        mde_web_preview_client::ResourceType::Script => "script",
        mde_web_preview_client::ResourceType::Image => "image",
        mde_web_preview_client::ResourceType::Font => "font",
        mde_web_preview_client::ResourceType::Media => "media",
        mde_web_preview_client::ResourceType::Object => "object",
        mde_web_preview_client::ResourceType::XmlHttpRequest => "xhr",
        mde_web_preview_client::ResourceType::Ping => "ping",
        mde_web_preview_client::ResourceType::WebSocket => "websocket",
        mde_web_preview_client::ResourceType::Other => "other",
    }
}

pub(super) fn offline_cache_pdf_snapshot(saved: &SavedPdf) -> Option<OfflineCachePdf> {
    let bytes = std::fs::read(&saved.path).ok()?;
    if validate_offline_cache_pdf_bytes(&bytes, bytes.len()).is_err() {
        return None;
    }
    let filename = saved
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| valid_offline_pdf_filename(name))
        .map(str::to_owned)
        .unwrap_or_else(|| pdf_filename_for(&saved.url, &saved.title, unix_ms()));
    Some(OfflineCachePdf {
        mime: "application/pdf".to_owned(),
        filename,
        bytes: bytes.len(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub(super) fn browser_voice_command_body(
    mode: VoiceCommandMode,
    tab_index: usize,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    address: &str,
    page_focused: bool,
) -> String {
    serde_json::json!({
        "op": "browser_voice_command",
        "source": "browser",
        "host": local_hostname(),
        "mode": mode.wire(),
        "tab_index": tab_index,
        "engine": engine.wire(),
        "url": url,
        "title": title.trim(),
        "address": address.trim(),
        "focus": if page_focused { "page" } else { "chrome" },
        "max_transcript_chars": 4096,
    })
    .to_string()
}

pub(super) fn browser_voice_command_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_VOICE_COMMAND_PREFIX}{host}")
}

pub(super) fn browser_media_control_topic(host: &str) -> String {
    format!("{ACTION_BROWSER_MEDIA_CONTROL_PREFIX}{host}")
}

pub(super) fn browser_read_aloud_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_READ_ALOUD_PREFIX}{host}")
}

pub(super) fn browser_voice_command_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_VOICE_COMMAND_PREFIX}{host}")
}

pub(super) fn browser_passkey_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_PASSKEYS_PREFIX}{host}")
}

pub(super) fn browser_passkey_event_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_PASSKEYS_PREFIX}{host}")
}

pub(super) fn browser_translation_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_TRANSLATE_PREFIX}{host}")
}

pub(super) fn browser_share_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_SHARE_PREFIX}{host}")
}

pub(super) fn browser_offline_cache_result_topic(host: &str) -> String {
    format!("{EVENT_BROWSER_OFFLINE_CACHE_PREFIX}{host}")
}

pub(super) fn browser_security_update_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_SECURITY_UPDATE_PREFIX}{host}")
}

pub(super) fn browser_media_status_topic(host: &str) -> String {
    format!("{STATE_BROWSER_MEDIA_PREFIX}{host}")
}

pub(super) fn browser_safe_browsing_source_topic(host: &str) -> String {
    format!("{STATE_BROWSER_SAFE_BROWSING_SOURCE_PREFIX}{host}")
}

pub(super) fn browser_managed_policy_source_topic(host: &str) -> String {
    format!("{STATE_BROWSER_MANAGED_POLICY_SOURCE_PREFIX}{host}")
}

pub(super) fn browser_custom_filter_rules_source_topic(host: &str) -> String {
    format!("{STATE_BROWSER_CUSTOM_FILTER_RULES_SOURCE_PREFIX}{host}")
}

pub(super) fn browser_filter_list_source_topic(host: &str) -> String {
    format!("{STATE_BROWSER_FILTER_LIST_SOURCE_PREFIX}{host}")
}

pub(super) fn cache_url_keys(url: &str) -> Vec<String> {
    let url = url.trim();
    if url.is_empty() {
        return Vec::new();
    }
    let mut keys = vec![url.to_owned()];
    if let Some(canonical) = canonical_http_cache_url(url) {
        if !keys.iter().any(|key| key == &canonical) {
            keys.push(canonical);
        }
    }
    keys
}

pub(super) fn canonical_http_cache_url(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (before_query, query) = rest
        .split_once('?')
        .map_or((rest, None), |(before, query)| (before, Some(query)));
    let (authority, path) = before_query
        .split_once('/')
        .map_or((before_query, ""), |(authority, path)| (authority, path));
    let authority = canonical_http_authority(&scheme, authority)?;
    let query = canonical_query(query);
    Some(match query {
        Some(query) => format!("{scheme}://{authority}/{path}?{query}"),
        None => format!("{scheme}://{authority}/{path}"),
    })
}

fn canonical_http_authority(scheme: &str, authority: &str) -> Option<String> {
    let authority = authority.trim();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after_host) = rest.split_once(']')?;
        let host = host.to_ascii_lowercase();
        let port = after_host.strip_prefix(':');
        return match port {
            Some(port) if is_default_http_port(scheme, port) => Some(format!("[{host}]")),
            Some(port) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
                Some(format!("[{host}]:{port}"))
            }
            Some(_) => None,
            None if after_host.is_empty() => Some(format!("[{host}]")),
            None => None,
        };
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                (host, Some(port))
            } else {
                (authority, None)
            }
        });
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    match port {
        Some(port) if is_default_http_port(scheme, port) => Some(host),
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host),
    }
}

fn is_default_http_port(scheme: &str, port: &str) -> bool {
    matches!((scheme, port), ("http", "80") | ("https", "443"))
}

fn canonical_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    if query.is_empty() {
        return None;
    }
    let mut pairs = query
        .split('&')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_unstable();
    Some(pairs.join("&"))
}

pub(super) fn parse_translation_result(body: &str) -> Result<BrowserTranslationResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("translation result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_translation") {
        return Err("translation result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_translate") {
        return Err("translation result has the wrong source".to_owned());
    }
    let host = result_required_str(&v, "host")?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "translation result is missing tab_index".to_owned())?;
    let engine_wire = result_required_str(&v, "engine")?;
    let engine = BrowserEngine::from_wire(&engine_wire)
        .ok_or_else(|| "translation result has an unsupported engine".to_owned())?;
    let translation = clamp_chars(
        &result_required_str(&v, "translation")?,
        TRANSLATION_RESULT_MAX_CHARS,
    );
    Ok(BrowserTranslationResult {
        host,
        tab_index,
        engine,
        url: result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        source_lang: result_required_str(&v, "source_lang")?,
        target_lang: result_required_str(&v, "target_lang")?,
        translation,
    })
}

pub(super) fn parse_share_route_result(body: &str) -> Result<BrowserShareRouteResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("share result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_share_routed") {
        return Err("share result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_share") {
        return Err("share result has the wrong source".to_owned());
    }
    let target_wire = result_required_str(&v, "target")?;
    let target = BrowserShareTarget::from_wire(&target_wire)
        .ok_or_else(|| "share result has an unsupported target".to_owned())?;
    Ok(BrowserShareRouteResult {
        host: result_required_str(&v, "host")?,
        target,
        url: result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        preview: result_required_str(&v, "preview")?,
        request_id: result_required_str(&v, "request_id")?,
    })
}

pub(super) fn qr_share_result(
    route: BrowserShareRouteResult,
) -> Result<BrowserQrShareResult, String> {
    if route.target != BrowserShareTarget::Qr {
        return Err("share result is not a QR route".to_owned());
    }
    let modules = qr_modules(&route.url)?;
    Ok(BrowserQrShareResult {
        host: route.host,
        url: route.url,
        title: route.title,
        preview: route.preview,
        request_id: route.request_id,
        modules,
    })
}

fn qr_modules(url: &str) -> Result<Vec<Vec<bool>>, String> {
    let code = QrCode::new(url.as_bytes()).map_err(|err| format!("QR encode failed: {err}"))?;
    let width = code.width();
    let mut modules = Vec::with_capacity(width);
    for y in 0..width {
        let mut row = Vec::with_capacity(width);
        for x in 0..width {
            row.push(code[(x, y)] == qrcode::Color::Dark);
        }
        modules.push(row);
    }
    Ok(modules)
}

pub(super) fn parse_offline_cache_result(body: &str) -> Result<BrowserOfflineCacheResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("offline-cache result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_offline_cache_record") {
        return Err("offline-cache result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_offline_cache") {
        return Err("offline-cache result has the wrong source".to_owned());
    }
    if v.get("privacy").and_then(serde_json::Value::as_str) != Some("offline_or_mesh_only") {
        return Err("offline-cache result is not private".to_owned());
    }
    let host = cache_result_required_str(&v, "host")?;
    let cache_id = cache_result_required_str(&v, "cache_id")?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "offline-cache result is missing tab_index".to_owned())?;
    let engine_wire = cache_result_required_str(&v, "engine")?;
    let engine = BrowserEngine::from_wire(&engine_wire)
        .ok_or_else(|| "offline-cache result has an unsupported engine".to_owned())?;
    let text = clamp_chars(
        &cache_result_required_str(&v, "text")?,
        OFFLINE_CACHE_TEXT_MAX_CHARS,
    );
    let viewport = v
        .get("viewport_image")
        .map(parse_offline_cache_viewport_image)
        .transpose()?;
    let resources = v
        .get("resource_manifest")
        .map(parse_offline_cache_resource_manifest)
        .transpose()?
        .unwrap_or_default();
    let archive_mhtml = v
        .get("archive_mhtml")
        .map(parse_offline_cache_mhtml_archive)
        .transpose()?;
    let pdf_snapshot = v
        .get("pdf_snapshot")
        .map(parse_offline_cache_pdf_snapshot)
        .transpose()?;
    Ok(BrowserOfflineCacheResult {
        host,
        cache_id,
        tab_index,
        engine,
        url: cache_result_required_str(&v, "url")?,
        title: v
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        text,
        viewport,
        resources,
        archive_mhtml,
        pdf_snapshot,
        cached_ms: v.get("cached_ms").and_then(serde_json::Value::as_u64),
    })
}

fn parse_offline_cache_viewport_image(
    v: &serde_json::Value,
) -> Result<OfflineCacheViewportImage, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "image/png")
        .ok_or_else(|| "offline-cache viewport image must be image/png".to_owned())?;
    let width = v
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "offline-cache viewport image is missing width".to_owned())?;
    let height = v
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| "offline-cache viewport image is missing height".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache viewport image is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache viewport image base64: {err}"))?;
    if bytes.len() > OFFLINE_CACHE_VIEWPORT_MAX_BYTES {
        return Err("offline-cache viewport image is too large".to_owned());
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("offline-cache viewport image is not a PNG".to_owned());
    }
    Ok(OfflineCacheViewportImage {
        mime: mime.to_owned(),
        width,
        height,
        data_base64: data_base64.to_owned(),
    })
}

fn parse_offline_cache_mhtml_archive(v: &serde_json::Value) -> Result<OfflineCacheArchive, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "multipart/related")
        .ok_or_else(|| "offline-cache archive must be multipart/related".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_offline_archive_filename(name))
        .ok_or_else(|| "offline-cache archive filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= OFFLINE_CACHE_MHTML_MAX_BYTES)
        .ok_or_else(|| "offline-cache archive has invalid byte count".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache archive is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache archive base64: {err}"))?;
    validate_offline_cache_archive_bytes(&bytes, declared_bytes)?;
    Ok(OfflineCacheArchive {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data_base64: data_base64.to_owned(),
    })
}

fn parse_offline_cache_resource_manifest(
    v: &serde_json::Value,
) -> Result<Vec<OfflineCacheResource>, String> {
    let items = v
        .as_array()
        .ok_or_else(|| "offline-cache resource manifest must be an array".to_owned())?;
    if items.len() > OFFLINE_CACHE_RESOURCE_MAX_COUNT {
        return Err("offline-cache resource manifest has too many entries".to_owned());
    }
    items
        .iter()
        .map(parse_offline_cache_resource)
        .collect::<Result<Vec<_>, _>>()
}

fn parse_offline_cache_resource(v: &serde_json::Value) -> Result<OfflineCacheResource, String> {
    let url = v
        .get("url")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| {
            !url.is_empty() && url.chars().count() <= OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS
        })
        .ok_or_else(|| "offline-cache resource URL is invalid".to_owned())?;
    let resource = v
        .get("resource")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|resource| valid_offline_resource_type(resource))
        .ok_or_else(|| "offline-cache resource type is invalid".to_owned())?;
    let allowed = v
        .get("allowed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "offline-cache resource allowed flag is missing".to_owned())?;
    let blocked_by = v
        .get("blocked_by")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|rule| !rule.is_empty())
        .map(|rule| clamp_chars(rule, OFFLINE_CACHE_RESOURCE_URL_MAX_CHARS));
    Ok(OfflineCacheResource {
        url: url.to_owned(),
        resource: resource.to_owned(),
        allowed,
        blocked_by,
    })
}

pub(super) fn offline_cache_archive_bytes(
    archive: &OfflineCacheArchive,
) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(archive.data_base64.as_str())
        .map_err(|err| format!("offline-cache archive base64: {err}"))?;
    validate_offline_cache_archive_bytes(&bytes, archive.bytes)?;
    Ok(bytes)
}

fn parse_offline_cache_pdf_snapshot(v: &serde_json::Value) -> Result<OfflineCachePdf, String> {
    let mime = v
        .get("mime")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|mime| *mime == "application/pdf")
        .ok_or_else(|| "offline-cache PDF must be application/pdf".to_owned())?;
    let filename = v
        .get("filename")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| valid_offline_pdf_filename(name))
        .ok_or_else(|| "offline-cache PDF filename is invalid".to_owned())?;
    let declared_bytes = v
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| *n > 0 && *n <= OFFLINE_CACHE_PDF_MAX_BYTES)
        .ok_or_else(|| "offline-cache PDF has invalid byte count".to_owned())?;
    let data_base64 = v
        .get("data")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "offline-cache PDF is missing data".to_owned())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|err| format!("offline-cache PDF base64: {err}"))?;
    validate_offline_cache_pdf_bytes(&bytes, declared_bytes)?;
    Ok(OfflineCachePdf {
        mime: mime.to_owned(),
        filename: filename.to_owned(),
        bytes: declared_bytes,
        data_base64: data_base64.to_owned(),
    })
}

pub(super) fn offline_cache_pdf_bytes(pdf: &OfflineCachePdf) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(pdf.data_base64.as_str())
        .map_err(|err| format!("offline-cache PDF base64: {err}"))?;
    validate_offline_cache_pdf_bytes(&bytes, pdf.bytes)?;
    Ok(bytes)
}

fn validate_offline_cache_pdf_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("offline-cache PDF byte count mismatch".to_owned());
    }
    if bytes.len() > OFFLINE_CACHE_PDF_MAX_BYTES {
        return Err("offline-cache PDF is too large".to_owned());
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err("offline-cache PDF is not a PDF".to_owned());
    }
    Ok(())
}

fn validate_offline_cache_archive_bytes(bytes: &[u8], declared_bytes: usize) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() != declared_bytes {
        return Err("offline-cache archive byte count mismatch".to_owned());
    }
    if bytes.len() > OFFLINE_CACHE_MHTML_MAX_BYTES {
        return Err("offline-cache archive is too large".to_owned());
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "offline-cache archive is not UTF-8 MHTML".to_owned())?;
    if !text.starts_with("MIME-Version: 1.0\r\n") || !text.contains("multipart/related") {
        return Err("offline-cache archive is not MHTML".to_owned());
    }
    Ok(())
}

fn valid_offline_archive_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".mhtml")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_offline_pdf_filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && name.ends_with(".pdf")
        && !name.contains('/')
        && !name.contains('\\')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn valid_offline_resource_type(resource: &str) -> bool {
    matches!(
        resource,
        "document"
            | "subdocument"
            | "stylesheet"
            | "script"
            | "image"
            | "font"
            | "media"
            | "object"
            | "xhr"
            | "ping"
            | "websocket"
            | "other"
    )
}

pub(super) fn parse_read_aloud_status(body: &str) -> Result<BrowserReadAloudStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("read-aloud status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "read-aloud status")?;
    if !matches!(
        state.as_str(),
        "idle" | "speaking" | "spoken" | "unavailable" | "error"
    ) {
        return Err("read-aloud status has an unsupported state".to_owned());
    }
    Ok(BrowserReadAloudStatus {
        node: status_required_str(&v, "node", "read-aloud status")?,
        last_title: optional_trimmed_str(&v, "last_title"),
        last_url: optional_trimmed_str(&v, "last_url"),
        state,
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        spoken: v
            .get("spoken")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_request_ms: v.get("last_request_ms").and_then(serde_json::Value::as_u64),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

pub(super) fn parse_voice_command_status(body: &str) -> Result<BrowserVoiceCommandStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("voice status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "voice status")?;
    if !matches!(
        state.as_str(),
        "idle" | "listening" | "transcribed" | "unavailable" | "error"
    ) {
        return Err("voice status has an unsupported state".to_owned());
    }
    if let Some(mode) = optional_trimmed_str(&v, "last_mode") {
        if !matches!(mode.as_str(), "command" | "dictation") {
            return Err("voice status has an unsupported mode".to_owned());
        }
    }
    Ok(BrowserVoiceCommandStatus {
        node: status_required_str(&v, "node", "voice status")?,
        last_url: optional_trimmed_str(&v, "last_url"),
        last_mode: optional_trimmed_str(&v, "last_mode"),
        state,
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        transcribed: v
            .get("transcribed")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_transcript_chars: v
            .get("last_transcript_chars")
            .and_then(serde_json::Value::as_u64),
        last_request_ms: v.get("last_request_ms").and_then(serde_json::Value::as_u64),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

pub(super) fn parse_passkey_status(body: &str) -> Result<BrowserPasskeyStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("passkey status JSON: {err}"))?;
    let state = status_required_str(&v, "state", "passkey status")?;
    if !matches!(
        state.as_str(),
        "idle" | "pending" | "created" | "asserted" | "error"
    ) {
        return Err("passkey status has an unsupported state".to_owned());
    }
    if let Some(ceremony) = optional_trimmed_str(&v, "last_ceremony") {
        if !matches!(ceremony.as_str(), "create" | "get") {
            return Err("passkey status has an unsupported ceremony".to_owned());
        }
    }
    let hardware_state =
        optional_trimmed_str(&v, "hardware_state").unwrap_or_else(|| "unknown".to_owned());
    if !matches!(
        hardware_state.as_str(),
        "unknown" | "unavailable" | "present_permission_denied" | "ready"
    ) {
        return Err("passkey status has an unsupported hardware state".to_owned());
    }
    let hardware_ctaphid_state =
        optional_trimmed_str(&v, "hardware_ctaphid_state").unwrap_or_else(|| "unknown".to_owned());
    if !matches!(
        hardware_ctaphid_state.as_str(),
        "unknown" | "unavailable" | "init_request_ready"
    ) {
        return Err("passkey status has an unsupported CTAP HID state".to_owned());
    }
    let hardware_ctaphid_init_frame_count = v
        .get("hardware_ctaphid_init_frame_count")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if hardware_ctaphid_state == "init_request_ready" && hardware_ctaphid_init_frame_count == 0 {
        return Err("passkey status CTAP HID INIT diagnostic has no frames".to_owned());
    }
    if hardware_ctaphid_state != "init_request_ready" && hardware_ctaphid_init_frame_count > 0 {
        return Err("passkey status CTAP HID frame count contradicts the CTAP state".to_owned());
    }
    Ok(BrowserPasskeyStatus {
        node: status_required_str(&v, "node", "passkey status")?,
        last_request_id: optional_trimmed_str(&v, "last_request_id"),
        last_host: optional_trimmed_str(&v, "last_host"),
        last_ceremony: optional_trimmed_str(&v, "last_ceremony"),
        last_rp_id: optional_trimmed_str(&v, "last_rp_id"),
        state,
        mirrored: v
            .get("mirrored")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_default(),
        last_error: optional_trimmed_str(&v, "last_error"),
        accepted: v
            .get("accepted")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        rejected: v
            .get("rejected")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        last_pending_ms: v.get("last_pending_ms").and_then(serde_json::Value::as_u64),
        hardware_state,
        hardware_key_count: v
            .get("hardware_key_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        hardware_readable_count: v
            .get("hardware_readable_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        hardware_ctaphid_state,
        hardware_ctaphid_init_frame_count,
        hardware_probe_ms: v
            .get("hardware_probe_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

pub(super) fn parse_passkey_completion(body: &str) -> Result<BrowserPasskeyCompletion, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("passkey event JSON: {err}"))?;
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_passkeys") {
        return Err("passkey event has an unsupported source".to_owned());
    }
    let op = status_required_str(&v, "op", "passkey event")?;
    if !matches!(
        op.as_str(),
        "browser_passkey_created" | "browser_passkey_assertion"
    ) {
        return Err("passkey event is not a completion".to_owned());
    }
    let client_request_id = status_required_str(&v, "client_request_id", "passkey event")?;
    if client_request_id.len() > 128 {
        return Err("passkey event client_request_id is too long".to_owned());
    }
    let body = serde_json::to_string(&v).map_err(|err| format!("passkey event encode: {err}"))?;
    Ok(BrowserPasskeyCompletion {
        client_request_id,
        body,
    })
}

pub(super) fn parse_security_update_status(
    body: &str,
) -> Result<BrowserSecurityUpdateStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("security update JSON: {err}"))?;
    let state = security_status_required_str(&v, "state")?;
    if !matches!(
        state.as_str(),
        "current" | "missing" | "mismatch" | "manifest_missing"
    ) {
        return Err("security update has an unsupported state".to_owned());
    }
    let updater_state = v
        .get("updater_state")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("idle")
        .to_owned();
    Ok(BrowserSecurityUpdateStatus {
        node: security_status_required_str(&v, "node")?,
        state,
        expected_cef_version: optional_trimmed_str(&v, "expected_cef_version"),
        expected_chromium_version: optional_trimmed_str(&v, "expected_chromium_version"),
        expected_channel: optional_trimmed_str(&v, "expected_channel"),
        active_runtime: optional_trimmed_str(&v, "active_runtime"),
        installed_version: optional_trimmed_str(&v, "installed_version"),
        installed_chromium: optional_trimmed_str(&v, "installed_chromium"),
        libcef_present: v
            .get("libcef_present")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        updater_state,
        last_update_ms: v.get("last_update_ms").and_then(serde_json::Value::as_u64),
        last_update_exit_code: v
            .get("last_update_exit_code")
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok()),
        last_update_error: optional_trimmed_str(&v, "last_update_error"),
        last_error: optional_trimmed_str(&v, "last_error"),
        updated_ms: v
            .get("updated_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default(),
    })
}

fn security_status_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("security update is missing {key}"))
}

fn optional_trimmed_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn status_required_str(v: &serde_json::Value, key: &str, context: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{context} is missing {key}"))
}

fn result_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("translation result is missing {key}"))
}

fn cache_result_required_str(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("offline-cache result is missing {key}"))
}

pub(super) fn parse_voice_transcript_result(body: &str) -> Result<VoiceTranscriptResult, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("voice result JSON: {err}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_voice_transcript") {
        return Err("voice result has the wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser_voice_command") {
        return Err("voice result has the wrong source".to_owned());
    }
    let host = v
        .get("host")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| "voice result is missing host".to_owned())?;
    let mode = v
        .get("mode")
        .and_then(serde_json::Value::as_str)
        .and_then(VoiceCommandMode::from_wire)
        .ok_or_else(|| "voice result has an unsupported mode".to_owned())?;
    let tab_index = v
        .get("tab_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| "voice result is missing tab_index".to_owned())?;
    let focus = v
        .get("focus")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|focus| matches!(*focus, "page" | "chrome"))
        .ok_or_else(|| "voice result has an unsupported focus".to_owned())?;
    let transcript = v
        .get("transcript")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|transcript| !transcript.is_empty())
        .ok_or_else(|| "voice result is missing transcript".to_owned())?;
    Ok(VoiceTranscriptResult {
        host: host.to_owned(),
        mode,
        tab_index,
        focus: focus.to_owned(),
        transcript: clamp_chars(transcript, 4096),
    })
}

pub(super) fn voice_command_action(transcript: &str) -> Option<BrowserVoiceAction> {
    let command = normalize_voice_command(transcript);
    match command.as_str() {
        "new tab" | "open new tab" | "open a new tab" => Some(BrowserVoiceAction::NewTab),
        "close tab" | "close current tab" => Some(BrowserVoiceAction::CloseTab),
        "back" | "go back" => Some(BrowserVoiceAction::Back),
        "forward" | "go forward" => Some(BrowserVoiceAction::Forward),
        "reload" | "refresh" | "reload page" | "refresh page" => Some(BrowserVoiceAction::Reload),
        "read aloud" | "read page aloud" | "read this page aloud" => {
            Some(BrowserVoiceAction::ReadAloud)
        }
        _ => voice_find_query(&command).map(BrowserVoiceAction::Find),
    }
}

fn voice_find_query(command: &str) -> Option<String> {
    for prefix in [
        "find in page ",
        "find on page ",
        "search page for ",
        "search this page for ",
        "search for ",
        "find ",
    ] {
        if let Some(query) = command.strip_prefix(prefix).map(str::trim) {
            if !query.is_empty() {
                return Some(query.to_owned());
            }
        }
    }
    None
}

fn normalize_voice_command(transcript: &str) -> String {
    let mut out = String::new();
    let mut last_was_space = true;
    for ch in transcript.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_space = false;
        } else if !last_was_space {
            out.push(' ');
            last_was_space = true;
        }
    }
    out.trim().to_owned()
}

pub(super) fn clamp_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn session_state_wire(state: &SessionState) -> &'static str {
    match state {
        SessionState::Loading => "loading",
        SessionState::Live => "live",
        SessionState::Crashed { .. } => "crashed",
    }
}

const BROWSER_MEDIA_TEXT_MAX_CHARS: usize = 160;
const BROWSER_MEDIA_URL_MAX_CHARS: usize = 2048;

fn tab_has_media_metadata(tab: &Tab) -> bool {
    tab.session.media_metadata().is_some()
}

fn browser_media_status_tab(state: &WebState) -> Option<(usize, &Tab)> {
    if let Some(tab) = state
        .tabs
        .get(state.active)
        .filter(|tab| tab_has_media_metadata(tab))
    {
        return Some((state.active, tab));
    }
    state
        .tabs
        .iter()
        .enumerate()
        .find(|(_, tab)| tab_has_media_metadata(tab) && tab.session.audible())
        .or_else(|| {
            state
                .tabs
                .iter()
                .enumerate()
                .find(|(_, tab)| tab_has_media_metadata(tab))
        })
}

pub(super) fn browser_media_status_tab_index(state: &WebState) -> Option<usize> {
    browser_media_status_tab(state).map(|(index, _)| index)
}

fn media_metadata_string(value: &serde_json::Value, key: &str, max_chars: usize) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| clamp_chars(s, max_chars))
}

fn media_metadata_u64(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
}

fn media_metadata_optional_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}

fn browser_media_metadata_object(body: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    Some(serde_json::json!({
        "title": media_metadata_string(&value, "title", BROWSER_MEDIA_TEXT_MAX_CHARS).unwrap_or_default(),
        "artist": media_metadata_string(&value, "artist", BROWSER_MEDIA_TEXT_MAX_CHARS).unwrap_or_default(),
        "album": media_metadata_string(&value, "album", BROWSER_MEDIA_TEXT_MAX_CHARS).unwrap_or_default(),
        "artwork_url": media_metadata_string(&value, "artwork_url", BROWSER_MEDIA_URL_MAX_CHARS).unwrap_or_default(),
        "source_url": media_metadata_string(&value, "source_url", BROWSER_MEDIA_URL_MAX_CHARS).unwrap_or_default(),
        "paused": value
            .get("paused")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true),
        "duration_ms": media_metadata_u64(&value, "duration_ms"),
        "position_ms": media_metadata_u64(&value, "position_ms"),
        "volume_percent": media_metadata_optional_u64(&value, "volume_percent"),
    }))
}

pub(super) fn browser_media_status_signature(state: &WebState) -> String {
    let Some((tab_index, tab)) = browser_media_status_tab(state) else {
        return "idle".to_owned();
    };
    let nav = tab.session.nav();
    let media_body = tab
        .session
        .media_metadata()
        .map(|metadata| metadata.body.trim())
        .unwrap_or_default();
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        tab_index,
        tab.id,
        tab.engine.wire(),
        nav.url,
        tab.session.title(),
        tab.session.audible(),
        tab.muted,
        media_body
    )
}

pub(super) fn browser_media_status_body(state: &WebState, updated_ms: u64) -> String {
    let host = local_hostname();
    let Some((tab_index, tab)) = browser_media_status_tab(state) else {
        return serde_json::json!({
            "op": "browser_media_status",
            "source": "browser",
            "node": host,
            "host": host,
            "state": "idle",
            "tab_index": serde_json::Value::Null,
            "tab_id": serde_json::Value::Null,
            "engine": serde_json::Value::Null,
            "active_tab": false,
            "url": "",
            "page_title": "",
            "label": serde_json::Value::Null,
            "audible": false,
            "muted": false,
            "metadata": serde_json::Value::Null,
            "updated_ms": updated_ms,
        })
        .to_string();
    };
    let media_body = tab
        .session
        .media_metadata()
        .map(|metadata| metadata.body.as_str())
        .unwrap_or_default();
    let metadata = browser_media_metadata_object(media_body);
    let paused = metadata
        .as_ref()
        .and_then(|value| value.get("paused"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| !tab.session.audible());
    let playback_state = if tab.session.audible() || !paused {
        "playing"
    } else {
        "paused"
    };
    let nav = tab.session.nav();
    serde_json::json!({
        "op": "browser_media_status",
        "source": "browser",
        "node": host,
        "host": host,
        "state": playback_state,
        "tab_index": tab_index,
        "tab_id": tab.id,
        "engine": tab.engine.wire(),
        "active_tab": tab_index == state.active,
        "url": clamp_chars(nav.url.trim(), BROWSER_MEDIA_URL_MAX_CHARS),
        "page_title": clamp_chars(tab.session.title().trim(), BROWSER_MEDIA_TEXT_MAX_CHARS),
        "label": media_metadata_chip_label(media_body),
        "audible": tab.session.audible(),
        "muted": tab.muted,
        "metadata": metadata.unwrap_or(serde_json::Value::Null),
        "updated_ms": updated_ms,
    })
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BrowserMediaControlRequest {
    pub(super) action: mde_web_preview_client::MediaTransportAction,
    pub(super) tab_id: Option<u64>,
}

fn media_transport_action_from_str(
    action: &str,
) -> Option<mde_web_preview_client::MediaTransportAction> {
    match action.trim().to_ascii_lowercase().as_str() {
        "playpause" | "play-pause" | "play_pause" | "toggle" => {
            Some(mde_web_preview_client::MediaTransportAction::PlayPause)
        }
        "play" => Some(mde_web_preview_client::MediaTransportAction::Play),
        "pause" => Some(mde_web_preview_client::MediaTransportAction::Pause),
        "stop" => Some(mde_web_preview_client::MediaTransportAction::Stop),
        "next" => Some(mde_web_preview_client::MediaTransportAction::Next),
        "previous" | "prev" => Some(mde_web_preview_client::MediaTransportAction::Previous),
        "volumeup" | "volume-up" | "volume_up" | "volup" | "raisevolume" | "raise-volume" => {
            Some(mde_web_preview_client::MediaTransportAction::VolumeUp)
        }
        "volumedown" | "volume-down" | "volume_down" | "voldown" | "lowervolume"
        | "lower-volume" => Some(mde_web_preview_client::MediaTransportAction::VolumeDown),
        _ => None,
    }
}

pub(super) fn browser_media_control_body(
    action: mde_web_preview_client::MediaTransportAction,
    tab_id: Option<u64>,
    source: &str,
    updated_ms: u64,
) -> String {
    let action = match action {
        mde_web_preview_client::MediaTransportAction::PlayPause => "play-pause",
        mde_web_preview_client::MediaTransportAction::Play => "play",
        mde_web_preview_client::MediaTransportAction::Pause => "pause",
        mde_web_preview_client::MediaTransportAction::Stop => "stop",
        mde_web_preview_client::MediaTransportAction::Next => "next",
        mde_web_preview_client::MediaTransportAction::Previous => "previous",
        mde_web_preview_client::MediaTransportAction::VolumeUp => "volume-up",
        mde_web_preview_client::MediaTransportAction::VolumeDown => "volume-down",
    };
    serde_json::json!({
        "op": "browser_media_control",
        "source": source.trim(),
        "action": action,
        "tab_id": tab_id,
        "updated_ms": updated_ms,
    })
    .to_string()
}

pub(super) fn parse_browser_media_control_request(
    body: &str,
) -> Result<BrowserMediaControlRequest, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("media control JSON: {err}"))?;
    if value
        .get("op")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|op| op != "browser_media_control")
    {
        return Err("unexpected media control op".to_owned());
    }
    let action = value
        .get("action")
        .and_then(serde_json::Value::as_str)
        .and_then(media_transport_action_from_str)
        .ok_or_else(|| "missing/unsupported media control action".to_owned())?;
    Ok(BrowserMediaControlRequest {
        action,
        tab_id: value.get("tab_id").and_then(serde_json::Value::as_u64),
    })
}

pub(super) fn browser_session_sync_body(state: &WebState) -> String {
    let tabs = state
        .tabs
        .iter()
        .enumerate()
        .filter(|(_, tab)| tab.internal_page.is_none())
        .map(|(index, tab)| {
            let nav = tab.session.nav();
            serde_json::json!({
                "index": index,
                "engine": tab.engine.wire(),
                "container": tab.container.wire(),
                "display_target": tab.display_target.wire(),
                "url": nav.url.as_str(),
                "title": tab.session.title(),
                "state": session_state_wire(&tab.session.state()),
                "loading": nav.loading,
                "can_back": nav.can_back,
                "can_forward": nav.can_forward,
                "muted": tab.muted,
                "autoplay_blocked": tab.autoplay_blocked,
                "force_dark": tab.force_dark,
                "reader_mode": tab.reader_mode,
                "user_scripts": tab.user_scripts,
                "user_agent": tab.user_agent.wire(),
                "device_profile": tab.device_profile.wire(),
                "idle_suspended": tab.idle_suspended,
            })
        })
        .collect::<Vec<_>>();
    let downloads = state
        .download_jobs
        .iter()
        .map(|job| {
            serde_json::json!({
                "id": job.id.as_str(),
                "source": job.source.as_str(),
                "dest": job.dest.as_str(),
                "method": job.method,
                "state": job.state,
                "progress": job.progress,
                "updated_ms": job.updated_ms,
            })
        })
        .collect::<Vec<_>>();
    let speed_dial = state
        .speed_dial
        .iter()
        .map(|entry| {
            serde_json::json!({
                "label": entry.label.as_str(),
                "url": entry.url.as_str(),
                "hint": entry.hint.as_str(),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "op": "browser_session_sync",
        "source": "browser",
        "host": local_hostname(),
        "active_index": if state.tabs.is_empty()
            || state
                .tabs
                .get(state.active)
                .is_some_and(|tab| tab.internal_page.is_some())
        {
            serde_json::Value::Null
        } else {
            serde_json::json!(state.active.min(state.tabs.len().saturating_sub(1)))
        },
        "settings": {
            "future_engine": state.engine.wire(),
            "vertical_tabs": state.vertical_tabs,
            "page_zoom_percent": state.page_zoom_percent,
            "find_open": state.find_open,
            "downloads_open": state.downloads_open,
            "power_mode": state.power_mode,
            "speed_dial": speed_dial,
        },
        "tabs": tabs,
        "downloads": downloads,
    })
    .to_string()
}

pub(super) fn browser_tab_suspend_body(
    tab_index: usize,
    engine: BrowserEngine,
    url: &str,
    title: &str,
    idle_after: Duration,
) -> String {
    let idle_after_ms = u64::try_from(idle_after.as_millis()).unwrap_or(u64::MAX);
    serde_json::json!({
        "op": "browser_tab_suspend",
        "tab_index": tab_index,
        "engine": engine.wire(),
        "url": url,
        "title": title,
        "idle_after_ms": idle_after_ms,
        "source": "browser",
        "host": local_hostname(),
    })
    .to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExternalProtocol {
    Mailto,
    Tel,
    Magnet,
}

impl ExternalProtocol {
    pub(super) fn from_url(url: &str) -> Option<Self> {
        let (scheme, _) = url.split_once(':')?;
        match scheme.to_ascii_lowercase().as_str() {
            "mailto" => Some(Self::Mailto),
            "tel" => Some(Self::Tel),
            "magnet" => Some(Self::Magnet),
            _ => None,
        }
    }

    pub(super) const fn scheme(self) -> &'static str {
        match self {
            Self::Mailto => "mailto",
            Self::Tel => "tel",
            Self::Magnet => "magnet",
        }
    }

    const fn target(self) -> &'static str {
        match self {
            Self::Mailto => "email",
            Self::Tel => "voice",
            Self::Magnet => "transfers",
        }
    }

    pub(super) const fn target_label(self) -> &'static str {
        match self {
            Self::Mailto => "Email",
            Self::Tel => "Voice",
            Self::Magnet => "Transfers",
        }
    }
}

pub(super) fn browser_protocol_handoff_body(protocol: ExternalProtocol, url: &str) -> String {
    serde_json::json!({
        "op": "browser_protocol_handoff",
        "source": "browser",
        "host": local_hostname(),
        "scheme": protocol.scheme(),
        "target": protocol.target(),
        "url": url,
    })
    .to_string()
}

pub(super) fn voice_dial_body(url: &str) -> String {
    let number = url.split_once(':').map_or(url, |(_, rest)| rest).trim();
    serde_json::json!({
        "peer": number,
        "host": local_hostname(),
        "source": "browser",
        "url": url,
    })
    .to_string()
}

pub(super) fn browser_notify_body(
    severity: Severity,
    summary: &str,
    detail: Option<&str>,
) -> String {
    let mut body = serde_json::json!({
        "severity": severity.tag(),
        "host": local_hostname(),
        "source": "browser",
        "summary": summary,
        "action": "action/shell/goto/browser",
    });
    if let Some(detail) = detail.filter(|s| !s.trim().is_empty()) {
        body["detail"] = serde_json::Value::String(detail.to_owned());
    }
    body.to_string()
}

/// Resolve an address-bar draft into the URL sent to the helper.
///
/// BROWSER-DD-2 asks for a real omnibox, not a strict URL field: explicit schemes
/// pass through, likely hostnames become HTTPS URLs, and free text searches the
/// mesh-hosted SearXNG instance. Empty/whitespace drafts stay inert.
pub(super) fn omnibox_target(draft: &str) -> Option<String> {
    let draft = draft.trim();
    if draft.is_empty() {
        return None;
    }
    if has_url_scheme(draft) {
        return Some(draft.to_owned());
    }
    if looks_like_host(draft) {
        return Some(format!("https://{draft}"));
    }
    Some(format!(
        "{DEFAULT_SEARCH_URL}?q={}",
        percent_encode_query(draft)
    ))
}

pub(super) fn should_fetch_suggestions(draft: &str) -> bool {
    let draft = draft.trim();
    !draft.is_empty() && !has_url_scheme(draft) && !looks_like_host(draft)
}

pub(super) fn suggestions_url(query: &str) -> String {
    format!(
        "{DEFAULT_SUGGEST_URL}?q={}",
        percent_encode_query(query.trim())
    )
}

pub(super) fn fetch_suggestions(query: &str) -> Result<Vec<String>, String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(SUGGEST_TIMEOUT)
        .build()
        .map_err(|e| format!("Suggestions unavailable: {e}"))?
        .get(suggestions_url(query))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| format!("Suggestions unavailable: {e}"))?
        .text()
        .map_err(|e| format!("Suggestions unavailable: {e}"))?;
    parse_suggestions_json(query, &body)
}

pub(super) fn chromium_devtools_frontend_for_active_url(
    active_url: &str,
) -> Result<Option<String>, String> {
    let body = reqwest::blocking::Client::builder()
        .timeout(CEF_DEVTOOLS_TIMEOUT)
        .build()
        .map_err(|e| format!("target discovery unavailable: {e}"))?
        .get(CEF_DEVTOOLS_LIST_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|e| format!("target discovery unavailable: {e}"))?
        .text()
        .map_err(|e| format!("target discovery unavailable: {e}"))?;
    chromium_devtools_frontend_from_list(active_url, &body)
}

pub(super) fn chromium_devtools_frontend_from_list(
    active_url: &str,
    body: &str,
) -> Result<Option<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid DevTools target JSON: {e}"))?;
    let Some(targets) = value.as_array() else {
        return Err("DevTools target JSON is not an array".to_owned());
    };
    let active_url = active_url.trim();
    let mut fallback = None;
    for target in targets {
        let target_url = target
            .get("url")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        let target_type = target
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("page");
        if target_type != "page" || target_url.starts_with("devtools://") {
            continue;
        }
        let Some(frontend) = chromium_devtools_frontend_url(target) else {
            continue;
        };
        if target_url == active_url {
            return Ok(Some(frontend));
        }
        fallback.get_or_insert(frontend);
    }
    Ok(fallback)
}

fn chromium_devtools_frontend_url(target: &serde_json::Value) -> Option<String> {
    if let Some(frontend) = target
        .get("devtoolsFrontendUrl")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        if frontend.starts_with("http://127.0.0.1:9222/") {
            return Some(frontend.to_owned());
        }
        if frontend.starts_with('/') {
            return Some(format!("http://127.0.0.1:9222{frontend}"));
        }
    }
    let ws = target
        .get("webSocketDebuggerUrl")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())?;
    let ws = ws
        .strip_prefix("ws://")
        .or_else(|| ws.strip_prefix("wss://"))
        .unwrap_or(ws);
    Some(format!(
        "http://127.0.0.1:9222/devtools/inspector.html?ws={ws}"
    ))
}

pub(super) fn parse_suggestions_json(query: &str, body: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("Invalid suggestions JSON: {e}"))?;
    let mut out = Vec::new();
    collect_suggestion_values(query.trim(), &value, &mut out);
    out.truncate(8);
    Ok(out)
}

fn collect_suggestion_values(query: &str, value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => push_suggestion(query, s, out),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_suggestion_values(query, item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for key in [
                "suggestions",
                "results",
                "completions",
                "value",
                "phrase",
                "text",
            ] {
                if let Some(v) = map.get(key) {
                    collect_suggestion_values(query, v, out);
                }
            }
        }
        _ => {}
    }
}

fn push_suggestion(query: &str, value: &str, out: &mut Vec<String>) {
    let value = value.trim();
    if value.is_empty() || value == query || out.iter().any(|s| s == value) {
        return;
    }
    out.push(value.to_owned());
}

fn has_url_scheme(s: &str) -> bool {
    if let Some((scheme, _rest)) = s.split_once("://") {
        return valid_scheme(scheme);
    }
    let Some((scheme, _rest)) = s.split_once(':') else {
        return false;
    };
    matches!(
        scheme,
        "about" | "data" | "file" | "mailto" | "tel" | "magnet" | "view-source"
    )
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn looks_like_host(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    let host = s.split('/').next().unwrap_or(s);
    host == "localhost"
        || host.contains('.')
        || host.contains(':')
        || host.chars().all(|c| c.is_ascii_digit() || c == '.')
}

pub(super) fn percent_encode_query(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(b));
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub(super) fn is_plain_http(url: &str) -> bool {
    url.trim_start()
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http://"))
}

pub(super) fn is_new_tab_url(url: &str) -> bool {
    matches!(url.trim(), "" | "about:blank")
}

pub(super) fn https_upgrade(url: &str) -> String {
    let trimmed = url.trim();
    trimmed
        .strip_prefix("http://")
        .map_or_else(|| trimmed.to_owned(), |rest| format!("https://{rest}"))
}

/// WL-ARCH-003 — shared **wire-contract fixtures** for the mirrored daemon
/// bodies the shell's Bus readers decode (now off the [`BusReader`] seam). Each
/// fixture is a *pinned canonical JSON body* in the exact shape the daemon
/// worker publishes; the test decodes it through the SAME `parse_*` the poller
/// runs and asserts the projection. If a field is renamed / retyped / dropped on
/// either side of the wire, the matching fixture stops decoding and the test goes
/// red — a wire-shape drift is caught here, not on a live seat.
///
/// [`BusReader`]: crate::bus_reader::BusReader
#[cfg(test)]
mod wire_contract {
    use super::*;

    /// `state/browser/<node>/passkey/status` — the latest-wins mirror
    /// `poll_passkey_status` reads via `BusReader::latest`.
    const PASSKEY_STATUS: &str = r#"{
        "node": "alpha",
        "state": "asserted",
        "last_request_id": "req-7",
        "last_host": "example.test",
        "last_ceremony": "get",
        "last_rp_id": "example.test",
        "mirrored": true,
        "accepted": 3,
        "rejected": 1,
        "hardware_state": "ready",
        "hardware_key_count": 1,
        "hardware_ctaphid_state": "unknown",
        "updated_ms": 1000
    }"#;

    /// `state/browser/<node>/security/update` — the mirror
    /// `refresh_security_update_status` reads via `BusReader::latest`.
    const SECURITY_UPDATE_STATUS: &str = r#"{
        "node": "alpha",
        "state": "current",
        "expected_cef_version": "127.0.0",
        "installed_version": "127.0.0",
        "libcef_present": true,
        "updater_state": "idle",
        "updated_ms": 1000
    }"#;

    /// `state/browser/<node>/read_aloud/status` — read by `poll_speech_statuses`.
    const READ_ALOUD_STATUS: &str = r#"{
        "node": "alpha",
        "state": "speaking",
        "last_title": "A Document",
        "last_url": "https://page.example/a",
        "accepted": 2,
        "spoken": 1,
        "rejected": 0,
        "updated_ms": 1000
    }"#;

    /// `state/browser/<node>/voice/status` — read by `poll_speech_statuses`.
    const VOICE_COMMAND_STATUS: &str = r#"{
        "node": "alpha",
        "state": "listening",
        "last_mode": "command",
        "accepted": 1,
        "transcribed": 0,
        "rejected": 0,
        "updated_ms": 1000
    }"#;

    /// `event/browser/<node>/passkey` — the completion `poll_passkey_results`
    /// drains via `BusReader::open` + `list_since`.
    const PASSKEY_COMPLETION: &str = r#"{
        "source": "browser_passkeys",
        "op": "browser_passkey_created",
        "client_request_id": "req-7"
    }"#;

    #[test]
    fn passkey_status_wire_shape_decodes() {
        let status = parse_passkey_status(PASSKEY_STATUS).expect("passkey status decodes");
        assert_eq!(status.node, "alpha");
        assert_eq!(status.state, "asserted");
        assert_eq!(status.accepted, 3);
        assert!(status.mirrored);
    }

    #[test]
    fn security_update_status_wire_shape_decodes() {
        let status =
            parse_security_update_status(SECURITY_UPDATE_STATUS).expect("security status decodes");
        assert_eq!(status.node, "alpha");
        assert_eq!(status.state, "current");
        assert!(status.libcef_present);
    }

    #[test]
    fn read_aloud_status_wire_shape_decodes() {
        let status = parse_read_aloud_status(READ_ALOUD_STATUS).expect("read-aloud status decodes");
        assert_eq!(status.node, "alpha");
        assert_eq!(status.state, "speaking");
        assert_eq!(status.accepted, 2);
    }

    #[test]
    fn voice_command_status_wire_shape_decodes() {
        let status =
            parse_voice_command_status(VOICE_COMMAND_STATUS).expect("voice status decodes");
        assert_eq!(status.node, "alpha");
        assert_eq!(status.state, "listening");
        assert_eq!(status.last_mode.as_deref(), Some("command"));
    }

    #[test]
    fn passkey_completion_wire_shape_decodes() {
        let completion =
            parse_passkey_completion(PASSKEY_COMPLETION).expect("passkey completion decodes");
        assert_eq!(completion.client_request_id, "req-7");
    }

    #[test]
    fn media_control_body_round_trips_through_its_parser() {
        // The one mirror with an in-crate producer: build with the daemon-facing
        // `*_body` and decode with the poller's parser — the two ends must agree.
        let body = browser_media_control_body(
            mde_web_preview_client::MediaTransportAction::Pause,
            Some(7),
            "mpris",
            5,
        );
        let request = parse_browser_media_control_request(&body).expect("media control decodes");
        assert_eq!(
            request.action,
            mde_web_preview_client::MediaTransportAction::Pause
        );
        assert_eq!(request.tab_id, Some(7));
    }

    #[test]
    fn a_dropped_required_field_is_caught_as_drift() {
        // The contract guard: a body missing a REQUIRED field (here `node`) must
        // fail the parser rather than decode to a silent default — so a producer
        // that drops the field is caught by this test.
        let drifted = r#"{ "state": "asserted", "updated_ms": 1 }"#;
        assert!(parse_passkey_status(drifted).is_err());
    }
}
