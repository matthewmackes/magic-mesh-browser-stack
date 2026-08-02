//! WL-RUN-006 — the **router firewall-edit** request/result + tamper-evident
//! audit schema (the "mutations fast-follow" stage of the router-control read
//! slice, `docs/design/router-control.md`).
//!
//! **This JSON is the §6 contract** between the desktop-side requester (the
//! Device-Manager surface, `mde-shell-egui`, where a `HostKind::Router` already
//! renders) and the mesh-side executor (`mackesd`'s `router_action` worker).
//! Neither crate may depend on the other, so — exactly like
//! [`crate::device_control`] — the shape + the on-disk seam live here in the
//! mesh-neutral shared crate, and both sides `use
//! mackes_mesh_types::router_action::*`.
//!
//! ## The remote-exec seam (the proven PD-11 / device-control pattern)
//!
//! A privileged firewall mutation runs **on the node that sits behind the router**
//! (the box that already holds the sealed `router/<mac>` cred + discovered the
//! appliance — `router_registry`), never pushed from a random seat (§9 — typed
//! verbs, the target runs locally). The shell writes a typed
//! [`RouterActionRequest`] into the **replicated**
//! `<workgroup_root>/action/router/<target-host>/<id>.json` (the literal
//! `action/router/*` plane); Syncthing carries it to that node; its
//! `router_action` worker drains its own dir and — for each request:
//!
//! 1. **Typed-confirm gate** (SAFETY 1): the request must echo the appliance id
//!    (the gateway MAC) as its [`RouterActionRequest::confirm_token`]; a mismatch
//!    is refused before any mutation (mirrors the release-rollback `--confirm
//!    ROLLBACK` typed token, but bound to the *specific* appliance so a fat-finger
//!    can never edit the wrong router).
//! 2. **Vyatta `commit-confirm`** (SAFETY 2): the typed [`FirewallEdit`] is
//!    synthesized into `set/delete firewall name …` lines
//!    ([`FirewallEdit::to_vyatta_commands`], pure, §9 — no command string) and
//!    applied inside a `commit-confirm <min>` window, so an edit that locks the
//!    operator out **auto-reverts** if the node cannot re-reach the router to
//!    `confirm` it.
//! 3. **Hash-chain audit** (SAFETY 3): every edit — applied, refused, reverted, or
//!    staged — appends an [`AuditRecord`] to the tamper-evident
//!    `<workgroup_root>/<host>/router-audit.jsonl` chain (the shell reads it back
//!    to render the audit trail; `mackesd` hashes + verifies it).
//!
//! The live mutation itself is **operator-gated** (a real network-firewall change
//! against the farm gateway) — the worker stages the edit and records it, but only
//! shells out to the router when the operator has armed the live seam.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The kind of firewall mutation an edit performs (design: additive, §6 — an edit
/// touches ONE named rule in ONE named ruleset, never a wholesale rewrite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirewallEditOp {
    /// Create/replace the addressed rule's leaves (and optionally the ruleset's
    /// default-action) — `set firewall name <rs> rule <n> <attr> '<val>'`.
    Set,
    /// Remove the addressed rule — `delete firewall name <rs> rule <n>`.
    Delete,
}

impl FirewallEditOp {
    /// The stable wire/audit token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Delete => "delete",
        }
    }
}

/// One typed Vyatta firewall-rule leaf (`<attr> <val>`), e.g. `action accept`,
/// `protocol tcp`, `destination port 443`. A space in `attr` is a nested node
/// (mirrors `scripts/apply-firewall.sh`). §9: a typed pair, never a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallLeaf {
    /// The leaf attribute path (`action`, `protocol`, `destination port`, …).
    pub attr: String,
    /// The leaf value (`accept`, `tcp`, `443`, `10.42.0.0/17`, …).
    pub val: String,
}

impl FirewallLeaf {
    /// A leaf from `&str` parts.
    #[must_use]
    pub fn new(attr: impl Into<String>, val: impl Into<String>) -> Self {
        Self {
            attr: attr.into(),
            val: val.into(),
        }
    }
}

/// A typed, bounded firewall edit — ONE rule in ONE named ruleset (§9 — typed
/// params, never a shell/command string). The executor maps it onto FIXED
/// `set/delete firewall name …` Vyatta lines or refuses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallEdit {
    /// The `firewall name` ruleset (`WAN_IN`, `LAN_LOCAL`, …).
    pub ruleset: String,
    /// The rule number within the ruleset (a decimal string, `10`..`9999`).
    pub rule: String,
    /// Set (create/replace) or Delete (remove) the addressed rule.
    pub op: FirewallEditOp,
    /// Optional ruleset-level default-action to set alongside a `Set` (a common
    /// first-touch: `set firewall name <rs> default-action drop`). Ignored on
    /// `Delete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_action: Option<String>,
    /// The rule's typed leaves for a `Set` (empty on `Delete`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<FirewallLeaf>,
}

/// The safe charset for a ruleset name / rule number / leaf attribute — the tokens
/// that land unquoted in a Vyatta config line. Spaces are allowed in `attr` (a
/// nested node) but validated separately; ruleset/rule are strict.
fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

/// A leaf attribute path may additionally contain single interior spaces (a
/// nested Vyatta node like `destination port`), but no quotes/newlines/metachars.
fn is_safe_attr(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.' || b == b' ')
}

/// A leaf VALUE is single-quoted in the emitted line, so the ONLY escape risks are
/// a single-quote, a newline, or a control byte — reject those. Everything else
/// (dots, slashes, colons for IPs/CIDRs/ports) is safe inside `'…'`.
fn is_safe_value(s: &str) -> bool {
    !s.is_empty()
        && !s
            .bytes()
            .any(|b| b == b'\'' || b == b'"' || b == b'`' || b == b'\n' || b == b'\r' || b < 0x20)
}

impl FirewallEdit {
    /// Whether this edit is well-formed + injection-safe (every token passes the
    /// charset guards). The executor refuses an invalid edit BEFORE any mutation,
    /// and the shell disables submit until valid — so a malformed/hostile edit can
    /// never reach the router config line.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if !is_safe_name(&self.ruleset) || !is_safe_name(&self.rule) {
            return false;
        }
        // The rule number must be a bare decimal in Vyatta's 1..=9999 range.
        if !matches!(self.rule.parse::<u32>(), Ok(1..=9999)) {
            return false;
        }
        if let Some(da) = self.default_action.as_deref() {
            if !matches!(da, "drop" | "accept" | "reject") {
                return false;
            }
        }
        match self.op {
            FirewallEditOp::Set => {
                // A Set must change something: at least one leaf or a default-action.
                if self.attrs.is_empty() && self.default_action.is_none() {
                    return false;
                }
                self.attrs
                    .iter()
                    .all(|l| is_safe_attr(&l.attr) && is_safe_value(&l.val))
            }
            // A Delete carries no leaves (a whole-rule removal).
            FirewallEditOp::Delete => true,
        }
    }

    /// Synthesize the FIXED Vyatta config lines this edit applies (§9 — pure, no
    /// command string). Mirrors `scripts/apply-firewall.sh`: single-quoted values,
    /// one `set …` per leaf, a `delete …` for a removal. Returns an empty vec for
    /// an invalid edit (the executor refuses it upstream).
    #[must_use]
    pub fn to_vyatta_commands(&self) -> Vec<String> {
        if !self.is_valid() {
            return Vec::new();
        }
        let base = format!("firewall name {}", self.ruleset);
        match self.op {
            FirewallEditOp::Set => {
                let mut out = Vec::new();
                if let Some(da) = self.default_action.as_deref() {
                    out.push(format!("set {base} default-action {da}"));
                }
                for leaf in &self.attrs {
                    out.push(format!(
                        "set {base} rule {} {} '{}'",
                        self.rule, leaf.attr, leaf.val
                    ));
                }
                out
            }
            FirewallEditOp::Delete => {
                vec![format!("delete {base} rule {}", self.rule)]
            }
        }
    }

    /// A one-line human summary for the audit trail + the dispatch toast.
    #[must_use]
    pub fn summary(&self) -> String {
        match self.op {
            FirewallEditOp::Set => format!(
                "set firewall {} rule {} ({} leaf{})",
                self.ruleset,
                self.rule,
                self.attrs.len(),
                if self.attrs.len() == 1 { "" } else { "es" }
            ),
            FirewallEditOp::Delete => {
                format!("delete firewall {} rule {}", self.ruleset, self.rule)
            }
        }
    }
}

/// The default commit-confirm auto-revert window (minutes) — matches
/// `scripts/apply-firewall.sh` (`EDGEOS_FW_CONFIRM_MIN` default 2).
pub const DEFAULT_COMMIT_CONFIRM_MIN: u32 = 2;

/// Clamp a requested commit-confirm window into a sane 1..=60 minute range (a
/// too-short window can revert before the reachability check; a too-long one keeps
/// a bad edit live). Applied by the executor, so a hostile request can't disable
/// the auto-revert with a huge window.
#[must_use]
pub fn clamp_confirm_min(requested: u32) -> u32 {
    requested.clamp(1, 60)
}

/// A typed router firewall-edit request (§9 — typed params, never a command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterActionRequest {
    /// Correlation id (the requester matches the eventual result on it).
    pub id: String,
    /// The appliance's stable id — the gateway MAC (the `router/<mac>` cred key +
    /// the typed-confirm token).
    pub appliance_id: String,
    /// The mesh node the appliance sits behind (the replicated dir it lands in +
    /// the node whose `router_action` worker drains it).
    pub target_host: String,
    /// The requesting seat/node id (the audit actor).
    pub from: String,
    /// The typed firewall edit to apply.
    pub edit: FirewallEdit,
    /// The typed-confirm token the operator echoed — MUST equal the appliance id
    /// (SAFETY 1); a mismatch is refused before any mutation.
    pub confirm_token: String,
    /// The commit-confirm auto-revert window (minutes); the executor clamps it.
    #[serde(default = "default_commit_min")]
    pub commit_confirm_min: u32,
}

fn default_commit_min() -> u32 {
    DEFAULT_COMMIT_CONFIRM_MIN
}

impl RouterActionRequest {
    /// SAFETY 1 — does the request's typed-confirm token authorize a mutation of
    /// THIS appliance? True only when the echoed token equals the appliance id
    /// (case-insensitive, trimmed — a MAC is hex). An empty/wrong token is refused.
    #[must_use]
    pub fn typed_confirm_ok(&self) -> bool {
        typed_confirm_matches(&self.confirm_token, &self.appliance_id)
    }
}

/// The token the operator must type to arm an edit on `appliance_id` — the
/// appliance id itself (the gateway MAC). Surfaced by the shell as the echo hint.
#[must_use]
pub fn expected_confirm_token(appliance_id: &str) -> String {
    appliance_id.trim().to_ascii_lowercase()
}

/// SAFETY 1 predicate — the typed-confirm token authorizes a mutation of
/// `appliance_id` (case-insensitive, trimmed, non-empty).
#[must_use]
pub fn typed_confirm_matches(token: &str, appliance_id: &str) -> bool {
    let expected = expected_confirm_token(appliance_id);
    !expected.is_empty() && token.trim().to_ascii_lowercase() == expected
}

/// The typed result the executor writes back for the requester to poll.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RouterActionResult {
    /// The request id this answers.
    pub id: String,
    /// True once the edit applied for real AND was confirmed (made permanent).
    pub ok: bool,
    /// True when a live apply auto-reverted (lost reachability after the change) —
    /// distinct from a plain failure so the shell can say "reverted", not "failed".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reverted: bool,
    /// True when the edit was recorded but the live seam is operator-gated (parked)
    /// — a staged edit, not a fabricated success (§7).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub staged: bool,
    /// Human-readable note (`applied WAN_IN rule 40; confirmed`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The honest failure reason on a degrade path (empty on success).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl RouterActionResult {
    /// A confirmed-success result.
    #[must_use]
    pub fn ok(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            detail: detail.into(),
            ..Self::default()
        }
    }

    /// A staged (recorded, live-apply parked/operator-gated) result — honest, not
    /// a fabricated apply.
    #[must_use]
    pub fn staged(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            staged: true,
            detail: detail.into(),
            ..Self::default()
        }
    }

    /// An auto-reverted result (the commit-confirm window elapsed without a
    /// confirm because reachability was lost).
    #[must_use]
    pub fn reverted(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reverted: true,
            detail: detail.into(),
            ..Self::default()
        }
    }

    /// A typed failure result carrying the honest reason (never a fake success).
    #[must_use]
    pub fn failed(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            error: error.into(),
            ..Self::default()
        }
    }
}

/// The per-target request directory — `<workgroup_root>/action/router/<host>/`
/// (the literal `action/router/*` plane; same replicated `<target>` idiom the
/// device-control verb uses).
#[must_use]
pub fn action_dir(workgroup_root: &Path, target_host: &str) -> PathBuf {
    workgroup_root
        .join("action")
        .join("router")
        .join(target_host)
}

// Replicated action leaves are peer-controlled input. Keep the request/result
// documents and the audit chain bounded before serde can materialize them.
const MAX_ROUTER_ACTION_JSON_BYTES: usize = 256 * 1024;
const MAX_ROUTER_AUDIT_BYTES: usize = 4 * 1024 * 1024;

/// Read a replicated text leaf through the descriptor that will be consumed.
///
/// The final path component is opened without following symlinks, and known
/// Unix targets use non-blocking open semantics so a FIFO or other special file
/// cannot stall a worker. The descriptor must identify a regular file whose
/// size stays within the bound and does not change while it is read. UTF-8 is
/// validated before a caller invokes serde.
fn read_bounded_router_record(path: &Path, max_bytes: usize) -> Option<String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0o400000 | 0o4000 | 0o2000000); // O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        options.custom_flags(0x100 | 0x4); // O_NOFOLLOW | O_NONBLOCK

        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
            return None;
        }
    }
    #[cfg(not(unix))]
    if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
        return None;
    }

    let file = options.open(path).ok()?;
    let before = file.metadata().ok()?;
    let max_bytes_u64 = u64::try_from(max_bytes).ok()?;
    if !before.file_type().is_file() || before.len() > max_bytes_u64 {
        return None;
    }

    let capacity = usize::try_from(before.len())
        .ok()?
        .min(max_bytes)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    (&file)
        .take(max_bytes_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let after = file.metadata().ok()?;
    // A peer can extend or truncate a regular file after the first metadata
    // snapshot. Do not parse a row that changed while it was being consumed.
    if !after.file_type().is_file()
        || after.len() != before.len()
        || bytes.len() > max_bytes
        || bytes.len() as u64 != before.len()
    {
        return None;
    }

    String::from_utf8(bytes).ok()
}

fn read_bounded_router_json<T>(path: &Path, max_bytes: usize) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    let raw = read_bounded_router_record(path, max_bytes)?;
    serde_json::from_str(&raw).ok()
}

/// Write a request into `target_host`'s replicated dir (atomic temp + rename).
///
/// # Errors
/// IO / serialization failures (surfaced to the shell as an honest error toast).
pub fn write_request(workgroup_root: &Path, req: &RouterActionRequest) -> std::io::Result<PathBuf> {
    let dir = action_dir(workgroup_root, &req.target_host);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", req.id));
    let tmp = dir.join(format!(".{}.json.tmp", req.id));
    std::fs::write(&tmp, serde_json::to_string_pretty(req)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Consume (read + delete) every pending request addressed to `self_host`. Result
/// files (`*.result.json`) and half-written dotfiles are skipped.
#[must_use]
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "request/result filenames are lowercase-`.json` by construction"
)]
pub fn take_requests(workgroup_root: &Path, self_host: &str) -> Vec<RouterActionRequest> {
    let dir = action_dir(workgroup_root, self_host);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.ends_with(".json") || name.ends_with(".result.json") || name.starts_with('.') {
            continue;
        }
        if let Some(req) =
            read_bounded_router_json::<RouterActionRequest>(&p, MAX_ROUTER_ACTION_JSON_BYTES)
        {
            let _ = std::fs::remove_file(&p);
            out.push(req);
        }
    }
    out
}

/// Write the result for a request back into `target_host`'s dir (atomic).
///
/// # Errors
/// IO / serialization failures.
pub fn write_result(
    workgroup_root: &Path,
    target_host: &str,
    result: &RouterActionResult,
) -> std::io::Result<PathBuf> {
    let dir = action_dir(workgroup_root, target_host);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.result.json", result.id));
    let tmp = dir.join(format!(".{}.result.tmp", result.id));
    std::fs::write(&tmp, serde_json::to_string_pretty(result)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Read (and consume) the result for `id`, if the executor has answered yet.
#[must_use]
pub fn take_result(
    workgroup_root: &Path,
    target_host: &str,
    id: &str,
) -> Option<RouterActionResult> {
    let path = action_dir(workgroup_root, target_host).join(format!("{id}.result.json"));
    let result =
        read_bounded_router_json::<RouterActionResult>(&path, MAX_ROUTER_ACTION_JSON_BYTES)?;
    let _ = std::fs::remove_file(&path);
    Some(result)
}

// ───────────────────────────────────────────────────────────────────────────
// Tamper-evident audit trail (SAFETY 3).
//
// The schema + reader live here (shared) so the shell can render the audit
// trail; `mackesd` owns the hashing + append + verify (it has the SHA-256
// primitive — `crate::audit`) so the chain algorithm is single-sourced there.
// ───────────────────────────────────────────────────────────────────────────

/// The replicated audit-chain file for a node's router edits —
/// `<workgroup_root>/<host>/router-audit.jsonl` (one JSON [`AuditRecord`] per
/// line, hash-chained). Co-located with `<host>/router-registry.json`.
pub const ROUTER_AUDIT_FILE: &str = "router-audit.jsonl";

/// The audit-chain path for `host` under the replicated root.
#[must_use]
pub fn audit_log_path(workgroup_root: &Path, host: &str) -> PathBuf {
    workgroup_root.join(host).join(ROUTER_AUDIT_FILE)
}

/// One tamper-evident audit row for a router firewall edit. The hash covers every
/// field except `prev_hash`/`hash` themselves (see [`Self::hash_payload`]); the
/// writer chains each row on the previous row's `hash` (genesis = 32 zero bytes),
/// so a rewritten row breaks the chain from that point on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuditRecord {
    /// Monotonic sequence (1-based) — the row's position in the chain.
    pub seq: u64,
    /// Unix epoch milliseconds (little-endian in the hash preimage).
    pub timestamp_ms: i64,
    /// The appliance the edit targeted (gateway MAC).
    pub appliance_id: String,
    /// The edit verb (`set` / `delete`).
    pub action: String,
    /// The human summary of the edit ([`FirewallEdit::summary`]).
    pub summary: String,
    /// The outcome token: `applied` · `refused-typed-confirm` · `refused-invalid`
    /// · `auto-reverted` · `staged-parked` · `error`.
    pub outcome: String,
    /// The requesting seat/node id.
    pub from: String,
    /// Hex of the previous row's hash (genesis = 64 zeros).
    pub prev_hash: String,
    /// Hex of this row's hash (`SHA-256(prev_hash || payload || ts_le)`).
    pub hash: String,
}

impl AuditRecord {
    /// The canonical hash-preimage payload bytes — every field EXCEPT the chain
    /// fields (`prev_hash`/`hash`). Deterministic (serde field order is stable), so
    /// the writer + the verifier recompute the same bytes.
    ///
    /// # Errors
    /// `serde_json::Error` only on OOM.
    pub fn hash_payload(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&AuditPayload {
            seq: self.seq,
            timestamp_ms: self.timestamp_ms,
            appliance_id: &self.appliance_id,
            action: &self.action,
            summary: &self.summary,
            outcome: &self.outcome,
            from: &self.from,
        })
    }
}

/// The hash-preimage view of an [`AuditRecord`] (the chain fields excluded).
#[derive(Serialize)]
struct AuditPayload<'a> {
    seq: u64,
    timestamp_ms: i64,
    appliance_id: &'a str,
    action: &'a str,
    summary: &'a str,
    outcome: &'a str,
    from: &'a str,
}

/// Read every [`AuditRecord`] from a chain file, in file (chain) order. Malformed
/// lines are skipped. Empty/absent file → empty vec. The shell uses this to render
/// the audit trail; `mackesd` uses it to walk + verify the chain.
#[must_use]
pub fn read_audit(path: &Path) -> Vec<AuditRecord> {
    let Some(raw) = read_bounded_router_record(path, MAX_ROUTER_AUDIT_BYTES) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<AuditRecord>(line).ok())
        .collect()
}

/// The canonical per-appliance tofu var-file (`infra/tofu/edgeos/appliances/`) an
/// appliance id selects — `appliances/<sanitized-id>.tfvars`. The wrapper
/// `scripts/tofu-appliance.sh` mirrors this mapping. `gateway` is the reserved
/// alias for the auto-loaded default (`terraform.tfvars`).
#[must_use]
pub fn appliance_var_file(appliance_id: &str) -> String {
    let id = appliance_id.trim();
    if id.eq_ignore_ascii_case("gateway") || id.is_empty() {
        return "terraform.tfvars".to_string();
    }
    // A MAC keys the file; colons are legal in a Linux filename, but normalise to
    // `-` so the file is shell/glob-friendly across the tooling.
    let sanitized: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("appliances/{}.tfvars", sanitized.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edit() -> FirewallEdit {
        FirewallEdit {
            ruleset: "WAN_IN".into(),
            rule: "40".into(),
            op: FirewallEditOp::Set,
            default_action: None,
            attrs: vec![
                FirewallLeaf::new("action", "accept"),
                FirewallLeaf::new("protocol", "tcp"),
                FirewallLeaf::new("destination port", "443"),
            ],
        }
    }

    fn sample_request() -> RouterActionRequest {
        RouterActionRequest {
            id: "01HZX".into(),
            appliance_id: "46:6a:7c:96:e8:aa".into(),
            target_host: "eagle".into(),
            from: "peer:laptop-mm".into(),
            edit: sample_edit(),
            confirm_token: "46:6a:7c:96:e8:aa".into(),
            commit_confirm_min: 2,
        }
    }

    #[test]
    fn to_vyatta_commands_mirror_the_apply_script() {
        let cmds = sample_edit().to_vyatta_commands();
        assert_eq!(
            cmds,
            vec![
                "set firewall name WAN_IN rule 40 action 'accept'",
                "set firewall name WAN_IN rule 40 protocol 'tcp'",
                "set firewall name WAN_IN rule 40 destination port '443'",
            ]
        );
        // A default-action prefixes the set lines.
        let mut e = sample_edit();
        e.default_action = Some("drop".into());
        assert_eq!(
            e.to_vyatta_commands()[0],
            "set firewall name WAN_IN default-action drop"
        );
        // A delete is a single line, no leaves.
        let del = FirewallEdit {
            op: FirewallEditOp::Delete,
            attrs: vec![],
            ..sample_edit()
        };
        assert_eq!(
            del.to_vyatta_commands(),
            vec!["delete firewall name WAN_IN rule 40"]
        );
    }

    #[test]
    fn invalid_edits_are_refused_and_emit_no_commands() {
        // Injection attempt in a value → rejected (no commands).
        let mut e = sample_edit();
        e.attrs = vec![FirewallLeaf::new("description", "x'; reboot #")];
        assert!(!e.is_valid());
        assert!(e.to_vyatta_commands().is_empty());
        // Hostile ruleset name.
        let mut e = sample_edit();
        e.ruleset = "WAN IN; rm -rf".into();
        assert!(!e.is_valid());
        // Non-numeric / out-of-range rule.
        let mut e = sample_edit();
        e.rule = "0".into();
        assert!(!e.is_valid());
        let mut e = sample_edit();
        e.rule = "notanum".into();
        assert!(!e.is_valid());
        // A Set that changes nothing.
        let e = FirewallEdit {
            op: FirewallEditOp::Set,
            attrs: vec![],
            default_action: None,
            ..sample_edit()
        };
        assert!(!e.is_valid());
        // Bad default-action.
        let mut e = sample_edit();
        e.default_action = Some("yolo".into());
        assert!(!e.is_valid());
    }

    #[test]
    fn typed_confirm_gate_binds_to_the_appliance() {
        let req = sample_request();
        assert!(req.typed_confirm_ok());
        // Case-insensitive, trimmed.
        let mut r = sample_request();
        r.confirm_token = "  46:6A:7C:96:E8:AA  ".into();
        assert!(r.typed_confirm_ok());
        // A different appliance's MAC does NOT authorize this one.
        let mut r = sample_request();
        r.confirm_token = "aa:bb:cc:dd:ee:ff".into();
        assert!(!r.typed_confirm_ok());
        // Empty token never arms.
        let mut r = sample_request();
        r.confirm_token = String::new();
        assert!(!r.typed_confirm_ok());
        assert_eq!(
            expected_confirm_token("46:6A:7C:96:E8:AA"),
            "46:6a:7c:96:e8:aa"
        );
    }

    #[test]
    fn commit_min_is_clamped() {
        assert_eq!(clamp_confirm_min(0), 1);
        assert_eq!(clamp_confirm_min(2), 2);
        assert_eq!(clamp_confirm_min(9999), 60);
    }

    #[test]
    fn per_appliance_var_file_selection() {
        assert_eq!(appliance_var_file("gateway"), "terraform.tfvars");
        assert_eq!(appliance_var_file(""), "terraform.tfvars");
        assert_eq!(
            appliance_var_file("46:6a:7c:96:e8:aa"),
            "appliances/46-6a-7c-96-e8-aa.tfvars"
        );
        // Case-normalised + separator-sanitised.
        assert_eq!(
            appliance_var_file("AA-BB-CC-DD-EE-FF"),
            "appliances/aa-bb-cc-dd-ee-ff.tfvars"
        );
    }

    #[test]
    fn request_and_result_round_trip_through_the_replicated_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let req = sample_request();

        let path = write_request(root, &req).unwrap();
        assert_eq!(
            path,
            action_dir(root, "eagle").join("01HZX.json"),
            "the request lands under action/router/<target>/"
        );
        // A non-target drains nothing.
        assert!(take_requests(root, "other").is_empty());
        // The target drains exactly its request (consumed once).
        let got = take_requests(root, "eagle");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], req);
        assert!(take_requests(root, "eagle").is_empty());

        write_result(root, "eagle", &RouterActionResult::ok("01HZX", "applied")).unwrap();
        let res = take_result(root, "eagle", "01HZX").expect("result present");
        assert!(res.ok);
        assert!(
            take_result(root, "eagle", "01HZX").is_none(),
            "consumed once"
        );
    }

    #[test]
    fn request_reader_rejects_hostile_leaves_without_consuming_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let invalid = action_dir(root, "invalid-utf8");
        std::fs::create_dir_all(&invalid).unwrap();
        let invalid_path = invalid.join("bad.json");
        std::fs::write(&invalid_path, [b'{', 0xff, b'}']).unwrap();
        assert!(take_requests(root, "invalid-utf8").is_empty());
        assert!(invalid_path.exists(), "rejected input is not consumed");

        let oversized = action_dir(root, "oversized");
        std::fs::create_dir_all(&oversized).unwrap();
        let oversized_path = oversized.join("bad.json");
        std::fs::write(
            &oversized_path,
            vec![b' '; MAX_ROUTER_ACTION_JSON_BYTES + 1],
        )
        .unwrap();
        assert!(take_requests(root, "oversized").is_empty());
        assert!(oversized_path.exists(), "rejected input is not consumed");

        let directory = action_dir(root, "directory");
        std::fs::create_dir_all(directory.join("directory.json")).unwrap();
        assert!(take_requests(root, "directory").is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::os::unix::net::UnixListener;

            let linked = action_dir(root, "symlink");
            std::fs::create_dir_all(&linked).unwrap();
            let target = tmp.path().join("request-target.json");
            std::fs::write(&target, serde_json::to_vec(&sample_request()).unwrap()).unwrap();
            let linked_path = linked.join("linked.json");
            symlink(&target, &linked_path).unwrap();
            assert!(take_requests(root, "symlink").is_empty());
            assert!(std::fs::symlink_metadata(&linked_path)
                .unwrap()
                .file_type()
                .is_symlink());

            let socket = action_dir(root, "socket");
            std::fs::create_dir_all(&socket).unwrap();
            let socket_path = socket.join("socket.json");
            let _listener = UnixListener::bind(&socket_path).unwrap();
            assert!(take_requests(root, "socket").is_empty());
        }
    }

    #[test]
    fn result_reader_rejects_hostile_leaves_without_consuming_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let invalid = action_dir(root, "invalid-utf8");
        std::fs::create_dir_all(&invalid).unwrap();
        let invalid_path = invalid.join("bad.result.json");
        std::fs::write(&invalid_path, [b'{', 0xff, b'}']).unwrap();
        assert!(take_result(root, "invalid-utf8", "bad").is_none());
        assert!(invalid_path.exists(), "rejected input is not consumed");

        let oversized = action_dir(root, "oversized");
        std::fs::create_dir_all(&oversized).unwrap();
        let oversized_path = oversized.join("bad.result.json");
        std::fs::write(
            &oversized_path,
            vec![b' '; MAX_ROUTER_ACTION_JSON_BYTES + 1],
        )
        .unwrap();
        assert!(take_result(root, "oversized", "bad").is_none());
        assert!(oversized_path.exists(), "rejected input is not consumed");

        let directory = action_dir(root, "directory");
        std::fs::create_dir_all(directory.join("bad.result.json")).unwrap();
        assert!(take_result(root, "directory", "bad").is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::os::unix::net::UnixListener;

            let linked = action_dir(root, "symlink");
            std::fs::create_dir_all(&linked).unwrap();
            let target = tmp.path().join("result-target.json");
            std::fs::write(
                &target,
                serde_json::to_vec(&RouterActionResult::ok("bad", "applied")).unwrap(),
            )
            .unwrap();
            let linked_path = linked.join("bad.result.json");
            symlink(&target, &linked_path).unwrap();
            assert!(take_result(root, "symlink", "bad").is_none());
            assert!(std::fs::symlink_metadata(&linked_path)
                .unwrap()
                .file_type()
                .is_symlink());

            let socket = action_dir(root, "socket");
            std::fs::create_dir_all(&socket).unwrap();
            let socket_path = socket.join("bad.result.json");
            let _listener = UnixListener::bind(&socket_path).unwrap();
            assert!(take_result(root, "socket", "bad").is_none());
        }
    }

    #[test]
    fn result_shapes_stay_honest() {
        let staged = RouterActionResult::staged("a", "recorded; live-apply parked");
        assert!(!staged.ok && staged.staged && !staged.reverted);
        let reverted = RouterActionResult::reverted("b", "lost reach; auto-reverted");
        assert!(!reverted.ok && reverted.reverted);
        let failed = RouterActionResult::failed("c", "no cred");
        assert!(!failed.ok && !failed.staged && failed.error == "no cred");
    }

    #[test]
    fn audit_record_hash_payload_excludes_chain_fields() {
        let mut a = AuditRecord {
            seq: 1,
            timestamp_ms: 1000,
            appliance_id: "mac".into(),
            action: "set".into(),
            summary: "s".into(),
            outcome: "applied".into(),
            from: "peer:x".into(),
            prev_hash: "deadbeef".into(),
            hash: "cafef00d".into(),
        };
        let p1 = a.hash_payload().unwrap();
        // Mutating the chain fields must NOT change the preimage.
        a.prev_hash = "0000".into();
        a.hash = "1111".into();
        assert_eq!(p1, a.hash_payload().unwrap());
        // Mutating a real field MUST change it.
        a.outcome = "auto-reverted".into();
        assert_ne!(p1, a.hash_payload().unwrap());
    }

    #[test]
    fn read_audit_skips_blank_and_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.jsonl");
        let rec = AuditRecord {
            seq: 1,
            outcome: "applied".into(),
            ..AuditRecord::default()
        };
        let body = format!("{}\n\nnot-json\n", serde_json::to_string(&rec).unwrap());
        std::fs::write(&path, body).unwrap();
        let got = read_audit(&path);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].outcome, "applied");
        assert!(read_audit(&tmp.path().join("missing.jsonl")).is_empty());
    }

    #[test]
    fn audit_reader_rejects_hostile_leaves_fail_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let valid = AuditRecord {
            seq: 1,
            outcome: "applied".into(),
            ..AuditRecord::default()
        };

        let invalid_path = tmp.path().join("invalid.jsonl");
        std::fs::write(&invalid_path, [b'{', 0xff, b'}']).unwrap();
        assert!(read_audit(&invalid_path).is_empty());

        let oversized_path = tmp.path().join("oversized.jsonl");
        std::fs::write(&oversized_path, vec![b' '; MAX_ROUTER_AUDIT_BYTES + 1]).unwrap();
        assert!(read_audit(&oversized_path).is_empty());

        let directory_path = tmp.path().join("directory.jsonl");
        std::fs::create_dir(&directory_path).unwrap();
        assert!(read_audit(&directory_path).is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::os::unix::net::UnixListener;

            let target = tmp.path().join("audit-target.jsonl");
            std::fs::write(
                &target,
                format!("{}\n", serde_json::to_string(&valid).unwrap()),
            )
            .unwrap();
            let linked_path = tmp.path().join("linked.jsonl");
            symlink(&target, &linked_path).unwrap();
            assert!(read_audit(&linked_path).is_empty());

            let socket_path = tmp.path().join("socket.jsonl");
            let _listener = UnixListener::bind(&socket_path).unwrap();
            assert!(read_audit(&socket_path).is_empty());
        }
    }
}
