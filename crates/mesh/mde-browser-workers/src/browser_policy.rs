//! BOOKMARKS-8 — the mackesd **browser-policy worker** (fleet-wide governance of
//! the browser + ad-blocker, ENFORCED mesh-side — not just in the UI).
//!
//! Builds on the landed `adfilter` worker (the ad-block stats the fleet
//! view surfaces) and the pure [`mde_adblock`] model, and adds the *governance*
//! plane BOOKMARKS-7 deliberately left to a follow-on: an operator-authored
//! [`BrowserPolicyDoc`] replicated over Syncthing, folded for **this node's role**,
//! and ENFORCED at the browser launch/spawn seam.
//!
//! ## What this worker owns
//!
//! * **Policy replication** (the same substrate the `adfilter` +
//!   `bookmarks` workers use). Every node writes ONLY its own
//!   `<share>/browser-policy/<node>/doc.json` (single-writer → Syncthing never
//!   sees a write conflict) and *reads* every peer's doc, converging on the
//!   **newest-authored** doc mesh-wide (a policy is a coherent singleton the
//!   operator edits atomically, so whole-doc last-writer-wins is the merge).
//! * **Per-role fold** ([`BrowserPolicyDoc::enforced_for`]). The converged doc is
//!   folded for this box's `worker_role::role_name` into an
//!   [`EnforcedPolicy`]: is the browser allowed on this role, is the ad-blocker
//!   forced on (a one-way ratchet — the base or the role can force it, neither can
//!   un-force it), the merged URL navigation allowlist, and the merged custom
//!   filter lists to inject.
//! * **Enforcement at the launch/spawn seam** (§6 — the acceptance's "NOT just
//!   UI"). The worker drains `action/browser/{launch,navigate,set-adblock}` and
//!   evaluates each against the enforced policy: a `launch` on a disallowed role is
//!   REFUSED (never a spawn); an allowed `launch` yields a [`LaunchDecision::Granted`]
//!   carrying the forced ad-blocker + allowlist + custom lists to INJECT; a
//!   `navigate` to an out-of-allowlist URL is REJECTED; a `set-adblock off` under a
//!   force-on policy is REJECTED. The published state carries the enforced policy so
//!   the (gated, follow-on) desktop launcher consults it before it opens the surface.
//! * **Policy authoring**. Drains `action/browser-policy/set` (the operator's
//!   governance verb) — parses the desired [`BrowserPolicyDoc`], stamps it
//!   `now`/`this node`, and replicates it as this node's own doc (it then converges
//!   mesh-wide by the newest stamp).
//! * **Disable = stop-sync + hide, retain data**. When the policy disables the
//!   browser for this role, the worker stops mirroring the node's browser-data
//!   manifest to the share (sync off) + publishes `surface_hidden`, but NEVER
//!   deletes the node-local browser data dir ([`resolve_browser_data_root`]) — a
//!   re-enable resumes the sync with the data intact (no destructive wipe).
//! * **State publish**. Publishes `state/browser-policy/<node>` (the enforced
//!   policy + service state + enforcement counters) via the existing mackesd Bus
//!   [`Persist`] mechanism, for the Workbench fleet view (alongside the adfilter
//!   worker's `state/adfilter/*` ad-block stats).
//!
//! ## §6 / §7 posture — nothing faked
//!
//! Like `adfilter`, this worker has no external transport to fake:
//! Syncthing does the replication out of band and the worker's job is real file
//! I/O against the shared dir — it runs unchanged on a headless farm box. The one
//! environmental condition is whether the canonical shared mount is present, the
//! existing `shared_root_writable` guard (AUDIT-MESH-15): when it is not,
//! the worker keeps its node-local doc + data and publishes an honest offline
//! status, never a faked converge nor a write into a bare unprovisioned mount.
//! Timestamps are injected (`now_fn`) so the model stays deterministic under test.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;

use mde_worker_core::{ShutdownToken, Worker};

/// Retained-latest topic prefix carrying this node's [`BrowserPolicyStatus`]
/// (`state/browser-policy/<node>`).
pub const STATE_PREFIX: &str = "state/browser-policy/";

/// The `action/browser-policy/` RPC domain prefix this worker drains — the
/// operator's fleet-policy authoring verb (`set`).
pub const POLICY_ACTION_PREFIX: &str = "action/browser-policy/";

/// The `action/browser/` RPC domain prefix this worker drains — the enforcement
/// seam the launcher/UI drives (`launch` / `navigate` / `set-adblock`).
pub const BROWSER_ACTION_PREFIX: &str = "action/browser/";

/// The share subdirectory the per-node policy docs live under
/// (`<root>/browser-policy/…`).
pub const POLICY_SUBDIR: &str = "browser-policy";

/// Each node's replicated policy-doc file name (single-writer per node).
pub const DOC_FILE: &str = "doc.json";

/// The per-node browser-data manifest mirrored to the share while the browser is
/// enabled (removed while disabled — the "stop sync" half of a disable).
pub const DATA_MANIFEST_FILE: &str = "browser-data.manifest";

/// Default poll/flush cadence. A fleet policy changes rarely (an operator edit);
/// a 30 s tick keeps convergence prompt without polling storms — same as adfilter.
pub const DEFAULT_TICK: Duration = Duration::from_secs(30);

/// A wall-clock source (ms since the Unix epoch). Injected so the model stays pure
/// and tests drive a deterministic fake clock.
type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

// ── the policy model (pure, testable folds) ──────────────────────────────────

/// One custom filter list the fleet policy mandates the browser inject on launch.
///
/// It layers on top of the adfilter-compiled shared engine — name + optional
/// upstream URL; the raw body rides the adfilter mirror/refresh path, so the
/// policy only names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CustomFilterList {
    /// A stable, human name (the merge key across the default + role lists).
    pub name: String,
    /// The upstream URL the operator sources it from, if any (advisory).
    #[serde(default)]
    pub url: Option<String>,
}

/// The policy for one deployment role (or the baseline applied to unlisted roles).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RolePolicy {
    /// Whether the browser may run on this role. `false` → the launcher must not
    /// spawn it (and the surface is hidden), local data retained.
    pub browser_enabled: bool,
    /// Whether the ad-blocker is FORCED on (the local user cannot turn it off).
    #[serde(default)]
    pub force_adblock: bool,
    /// The URL navigation allowlist. EMPTY = navigate anywhere; non-empty = the
    /// browser may only navigate to hosts at (or under) one of these domains.
    #[serde(default)]
    pub url_allowlist: Vec<String>,
    /// Custom filter lists the browser must inject on launch (beyond the shared
    /// adfilter engine).
    #[serde(default)]
    pub custom_filter_lists: Vec<CustomFilterList>,
}

impl RolePolicy {
    /// The permissive baseline (the "no policy configured" default): the browser is
    /// enabled, the ad-blocker is not forced, navigation is unrestricted, and no
    /// custom lists are mandated. A fresh fleet with no authored policy runs this.
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            browser_enabled: true,
            force_adblock: false,
            url_allowlist: Vec::new(),
            custom_filter_lists: Vec::new(),
        }
    }
}

impl Default for RolePolicy {
    fn default() -> Self {
        Self::permissive()
    }
}

/// The mesh-distributable browser/ad-blocker fleet policy — the operator-authored,
/// Syncthing-replicated singleton this worker governs by.
///
/// `Default` is the un-authored baseline: a permissive `default` role policy, no
/// per-role overrides, and `updated_ms` 0 so any authored doc wins the LWW merge.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct BrowserPolicyDoc {
    /// The baseline applied to any role not explicitly listed in [`Self::roles`].
    #[serde(default = "RolePolicy::permissive")]
    pub default: RolePolicy,
    /// Per-role overrides, keyed by the canonical role name
    /// (`worker_role::role_name`: `lighthouse` / `workstation`).
    #[serde(default)]
    pub roles: BTreeMap<String, RolePolicy>,
    /// When this doc was authored (unix ms) — the whole-doc LWW key the mesh
    /// converges on. `0` = the un-authored default (any authored doc wins).
    #[serde(default)]
    pub updated_ms: u64,
    /// The node that authored it (attribution / the fleet-view "policy source").
    #[serde(default)]
    pub updated_by: String,
}

impl BrowserPolicyDoc {
    /// Fold the policy for `role` into the [`EnforcedPolicy`] the launch seam
    /// enforces. The per-role fold (the acceptance's tested cases):
    ///
    /// * **role allow/deny** — `browser_enabled` is the role's override if the role
    ///   is listed, else the default's.
    /// * **force-on override** — `force_adblock` is a one-way ratchet: forced on
    ///   when EITHER the default OR the role forces it (neither can un-force it).
    /// * **allowlist merge** — the enforced URL allowlist is the UNION of the
    ///   default's and the role's (normalized hosts, deduped).
    /// * custom lists likewise merge by name (a role entry overrides the default's).
    #[must_use]
    pub fn enforced_for(&self, role: &str) -> EnforcedPolicy {
        let role_pol = self.roles.get(role);
        let browser_enabled = role_pol.map_or(self.default.browser_enabled, |r| r.browser_enabled);
        let force_adblock = self.default.force_adblock || role_pol.is_some_and(|r| r.force_adblock);

        // Allowlist merge: union of default ∪ role, host-normalized + deduped.
        let mut allow: BTreeSet<String> = self
            .default
            .url_allowlist
            .iter()
            .filter_map(|p| normalize_host_pattern(p))
            .collect();
        if let Some(r) = role_pol {
            allow.extend(
                r.url_allowlist
                    .iter()
                    .filter_map(|p| normalize_host_pattern(p)),
            );
        }

        // Custom lists merge by name (role entry wins on a name clash).
        let mut lists: BTreeMap<String, CustomFilterList> = self
            .default
            .custom_filter_lists
            .iter()
            .map(|l| (l.name.clone(), l.clone()))
            .collect();
        if let Some(r) = role_pol {
            for l in &r.custom_filter_lists {
                lists.insert(l.name.clone(), l.clone());
            }
        }

        EnforcedPolicy {
            role: role.to_string(),
            browser_enabled,
            force_adblock,
            url_allowlist: allow.into_iter().collect(),
            custom_filter_lists: lists.into_values().collect(),
        }
    }

    /// Serialize the doc to the JSON blob replicated over Syncthing.
    ///
    /// # Errors
    /// Propagates any [`serde_json`] serialization error (unreachable for this
    /// plain-data model, but returned rather than panicking).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse a doc from the replicated JSON blob.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] if `json` is not a valid serialized doc.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// The policy folded for THIS node's role — what the launch seam enforces.
///
/// The launcher injects it: it is published inside [`BrowserPolicyStatus`] so the
/// (gated, follow-on) desktop launcher reads the exact config to spawn with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EnforcedPolicy {
    /// The role this was folded for.
    pub role: String,
    /// Whether the browser may run on this role.
    pub browser_enabled: bool,
    /// Whether the ad-blocker is forced on (cannot be toggled off locally).
    pub force_adblock: bool,
    /// The merged URL navigation allowlist (empty = unrestricted).
    pub url_allowlist: Vec<String>,
    /// The merged custom filter lists to inject on launch.
    pub custom_filter_lists: Vec<CustomFilterList>,
}

impl EnforcedPolicy {
    /// Evaluate a browser LAUNCH against the policy: refuse on a disallowed role,
    /// else grant with the ad-blocker + allowlist + custom lists to INJECT.
    #[must_use]
    pub fn evaluate_launch(&self) -> LaunchDecision {
        if self.browser_enabled {
            LaunchDecision::Granted {
                force_adblock: self.force_adblock,
                url_allowlist: self.url_allowlist.clone(),
                custom_filter_lists: self.custom_filter_lists.clone(),
            }
        } else {
            LaunchDecision::Refused {
                reason: format!(
                    "the browser is disabled by fleet policy for role `{}`",
                    self.role
                ),
            }
        }
    }

    /// Whether `url` is navigable under the policy. An empty allowlist permits any
    /// URL; otherwise the URL's host must equal — or sit under — an allowlist
    /// domain. A URL with no host (e.g. `about:blank`) is permitted (no navigation
    /// target to gate); an unparseable-but-http URL with an empty host is rejected.
    #[must_use]
    pub fn allows_navigation(&self, url: &str) -> bool {
        if self.url_allowlist.is_empty() {
            return true;
        }
        let Some(host) = mde_adblock::host_of(url) else {
            // No `://authority` host (about:/data:/blank) — nothing to gate.
            return true;
        };
        self.url_allowlist
            .iter()
            .any(|domain| host_under_domain(&host, domain))
    }

    /// Whether the local user may toggle the ad-blocker to `on`. Turning it ON is
    /// always allowed; turning it OFF is rejected while `force_adblock` holds.
    #[must_use]
    pub const fn allows_adblock_toggle(&self, on: bool) -> bool {
        on || !self.force_adblock
    }
}

/// The outcome of evaluating a browser launch against the policy — the grant
/// carries the config the launcher must INJECT (the "not just UI" enforcement).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum LaunchDecision {
    /// The browser may launch — spawn it with this injected ad-blocker config.
    Granted {
        /// Force the ad-blocker on in the spawned browser.
        force_adblock: bool,
        /// Restrict navigation to these domains (empty = unrestricted).
        url_allowlist: Vec<String>,
        /// Inject these custom filter lists on top of the shared engine.
        custom_filter_lists: Vec<CustomFilterList>,
    },
    /// The browser is refused on this role — DO NOT spawn it.
    Refused {
        /// A human-readable reason (surfaced to the operator).
        reason: String,
    },
}

impl LaunchDecision {
    /// Whether this decision granted the launch.
    #[must_use]
    pub const fn is_granted(&self) -> bool {
        matches!(self, Self::Granted { .. })
    }
}

/// Normalize a policy allowlist pattern to a bare lowercase host, or `None` when it
/// carries nothing usable. Accepts a bare domain (`example.com`) or a full URL
/// (`https://example.com/x`) — both reduce to the host.
fn normalize_host_pattern(pattern: &str) -> Option<String> {
    let p = pattern.trim();
    if p.is_empty() {
        return None;
    }
    // A full URL → its host; a bare token → itself (lowercased, no leading dot).
    let host = mde_adblock::host_of(p).unwrap_or_else(|| p.to_ascii_lowercase());
    let host = host.trim_matches('.').to_string();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Does `host` equal `domain` or sit under it as a subdomain (label-boundary
/// enforced, so `notexample.com` is NOT under `example.com`)?
fn host_under_domain(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|p| p.ends_with('.'))
}

// ── the published status ──────────────────────────────────────────────────────

/// The per-node browser-policy status published to `state/browser-policy/<node>`.
///
/// The operator's fleet-view row (BOOKMARKS-8 §6): it carries the enforced policy
/// (so the launcher injects) + the honest service state + the enforcement counters.
// A published status DTO is legitimately bool-heavy (enabled/hidden/forced/…); each
// bool is an independent honest flag the fleet view renders, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowserPolicyStatus {
    /// This node's id.
    pub node: String,
    /// The role the policy was folded for.
    pub role: String,
    /// Whether the browser is enabled on this node's role.
    pub browser_enabled: bool,
    /// Whether the surface is hidden (== the browser being disabled).
    pub surface_hidden: bool,
    /// Whether the ad-blocker is forced on.
    pub force_adblock: bool,
    /// The enforced URL navigation allowlist (empty = unrestricted).
    pub url_allowlist: Vec<String>,
    /// The custom filter lists injected on launch.
    pub custom_filter_lists: Vec<CustomFilterList>,
    /// When the converged policy was authored (unix ms; `0` = the default baseline).
    pub policy_updated_ms: u64,
    /// The node that authored the converged policy (empty = the default baseline).
    pub policy_source: String,
    /// Whether the most recent `launch` decision was a refusal.
    pub last_launch_refused: bool,
    /// How many `launch` requests this node has GRANTED.
    pub launches_granted: u64,
    /// How many `launch` requests this node has REFUSED (disallowed role).
    pub launches_refused: u64,
    /// How many `navigate` requests this node has REJECTED (out of allowlist).
    pub navigations_rejected: u64,
    /// How many `set-adblock off` requests this node has REJECTED (force-on).
    pub adblock_toggles_rejected: u64,
    /// How many *other* nodes' policy docs this node is merging.
    pub peers: usize,
    /// Whether the shared Syncthing folder was present + writable this tick.
    pub share_reachable: bool,
    /// Whether the node-local browser data survives a disable (never wiped) — the
    /// disable-retains-data invariant, `true` whenever a local data dir is present.
    pub local_data_retained: bool,
    /// Wall-clock ms of the last flush.
    pub last_flush_ms: u64,
}

// ── the typed actions ─────────────────────────────────────────────────────────

/// A typed `action/browser/<verb>` enforcement request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAction {
    /// Request to launch/spawn the browser (evaluated → grant or refuse).
    Launch,
    /// Request to navigate to a URL (evaluated against the allowlist).
    Navigate {
        /// The target URL.
        url: String,
    },
    /// Request to toggle the ad-blocker (rejected when turning off under force-on).
    SetAdblock {
        /// `true` = turn the ad-blocker on; `false` = turn it off.
        on: bool,
    },
}

#[derive(serde::Deserialize)]
struct NavigateReq {
    url: String,
}

#[derive(serde::Deserialize)]
struct SetAdblockReq {
    on: bool,
}

/// Parse a typed [`BrowserAction`] from the topic's `<verb>` slot + JSON body.
///
/// # Errors
/// An unknown verb or a body missing its required field returns a readable message.
pub fn parse_browser_action(verb: &str, body: &str) -> Result<BrowserAction, String> {
    let body = body.trim();
    let json = if body.is_empty() { "{}" } else { body };
    let malformed = |e: serde_json::Error| format!("malformed `{verb}` browser request: {e}");
    match verb {
        "launch" => Ok(BrowserAction::Launch),
        "navigate" => {
            let r: NavigateReq = serde_json::from_str(json).map_err(malformed)?;
            let url = r.url.trim().to_string();
            if url.is_empty() {
                Err("empty `url` in `navigate` browser request".to_string())
            } else {
                Ok(BrowserAction::Navigate { url })
            }
        }
        "set-adblock" => {
            let r: SetAdblockReq = serde_json::from_str(json).map_err(malformed)?;
            Ok(BrowserAction::SetAdblock { on: r.on })
        }
        other => Err(format!("unknown browser action verb `{other}`")),
    }
}

/// Parse a `action/browser-policy/set` body into the desired [`BrowserPolicyDoc`]
/// (its `updated_ms`/`updated_by` are re-stamped by the worker, so a client-set
/// stamp is ignored).
///
/// # Errors
/// An empty or malformed body returns a readable message.
pub fn parse_policy_set(body: &str) -> Result<BrowserPolicyDoc, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("empty browser-policy `set` request".to_string());
    }
    BrowserPolicyDoc::from_json(body).map_err(|e| format!("malformed browser-policy doc: {e}"))
}

// ── path helpers ─────────────────────────────────────────────────────────────

fn policy_dir(root: &Path) -> PathBuf {
    root.join(POLICY_SUBDIR)
}
fn node_dir(root: &Path, node: &str) -> PathBuf {
    policy_dir(root).join(node)
}
fn doc_path(root: &Path, node: &str) -> PathBuf {
    node_dir(root, node).join(DOC_FILE)
}
fn data_manifest_path(root: &Path, node: &str) -> PathBuf {
    node_dir(root, node).join(DATA_MANIFEST_FILE)
}

/// Load a policy doc from `path`, or `None` when absent / corrupt (a peer-supplied
/// file never panics the reader).
fn load_doc(path: &Path) -> Option<BrowserPolicyDoc> {
    let text = std::fs::read_to_string(path).ok()?;
    BrowserPolicyDoc::from_json(&text).ok()
}

// ── the worker ───────────────────────────────────────────────────────────────

/// BOOKMARKS-8 — the mesh-wide browser-policy worker.
pub struct BrowserPolicyWorker {
    /// This node's id (the doc owner + status key).
    node: String,
    /// This node's deployment role name (the policy is folded for this).
    role: String,
    /// Node-local durable root (offline-first + restart durability).
    local_root: PathBuf,
    /// The shared Syncthing root: this node mirrors its own doc here + reads peers.
    share_root: PathBuf,
    /// The node-local browser data dir the disable path must RETAIN (never wipe).
    browser_data_root: PathBuf,
    /// This node's authoritative own policy doc (authored via the `set` verb).
    own: BrowserPolicyDoc,
    /// The converged policy (newest-authored across own ⊕ peers) — folded + published.
    converged: BrowserPolicyDoc,
    /// Peer count observed on the last rebuild.
    peer_count: usize,
    /// Enforcement counters.
    launches_granted: u64,
    launches_refused: u64,
    navigations_rejected: u64,
    adblock_toggles_rejected: u64,
    /// Whether the most recent `launch` decision was a refusal.
    last_launch_refused: bool,
    /// Wall-clock ms of the last flush.
    last_flush_ms: u64,
    /// Poll/flush cadence.
    tick: Duration,
    /// Per-topic action cursors (`action/browser*/<verb>` → last ULID).
    cursors: HashMap<String, String>,
    /// Injected wall clock.
    now_fn: NowFn,
    /// Test seam forcing the share up/down; `None` → the real writable guard.
    share_gate: Option<Arc<AtomicBool>>,
    /// Bus spool root override (tests point this at a tempdir).
    bus_root_override: Option<PathBuf>,
}

impl BrowserPolicyWorker {
    /// Construct with production defaults. `role` is the box's deployment role name
    /// (`worker_role::role_name`); `local_root` is a node-local durable dir
    /// ([`resolve_local_root`]); `share_root` is the mesh workgroup root.
    #[must_use]
    pub fn new(node: String, role: String, local_root: PathBuf, share_root: PathBuf) -> Self {
        let browser_data_root = resolve_browser_data_root();
        Self {
            node,
            role,
            local_root,
            share_root,
            browser_data_root,
            own: BrowserPolicyDoc::default(),
            converged: BrowserPolicyDoc::default(),
            peer_count: 0,
            launches_granted: 0,
            launches_refused: 0,
            navigations_rejected: 0,
            adblock_toggles_rejected: 0,
            last_launch_refused: false,
            last_flush_ms: 0,
            tick: DEFAULT_TICK,
            cursors: HashMap::new(),
            now_fn: Arc::new(default_now),
            share_gate: None,
            bus_root_override: None,
        }
    }

    /// Inject a deterministic wall clock (tests).
    #[must_use]
    pub fn with_now_fn(mut self, now: NowFn) -> Self {
        self.now_fn = now;
        self
    }

    /// Inject a share-availability gate (offline-first tests).
    #[must_use]
    pub fn with_share_gate(mut self, gate: Arc<AtomicBool>) -> Self {
        self.share_gate = Some(gate);
        self
    }

    /// Override the poll/flush cadence (tests use a short value).
    #[must_use]
    pub const fn with_tick(mut self, d: Duration) -> Self {
        self.tick = d;
        self
    }

    /// Override the Bus spool root (tests).
    #[must_use]
    pub fn with_bus_root(mut self, root: PathBuf) -> Self {
        self.bus_root_override = Some(root);
        self
    }

    /// Override the node-local browser data dir (tests point this at a tempdir).
    #[must_use]
    pub fn with_browser_data_root(mut self, root: PathBuf) -> Self {
        self.browser_data_root = root;
        self
    }

    fn now_ms(&self) -> u64 {
        (self.now_fn)()
    }

    /// Whether the shared folder is present + writable this tick. The test gate
    /// wins when set; otherwise the AUDIT-MESH-15 canonical-mount guard.
    fn share_writable(&self) -> bool {
        self.share_gate.as_ref().map_or_else(
            || mackes_mesh_types::mesh_storage::shared_root_writable(&self.share_root),
            |g| g.load(Ordering::SeqCst),
        )
    }

    /// The policy folded for THIS node's role — the enforcement seam's input.
    #[must_use]
    pub fn enforced(&self) -> EnforcedPolicy {
        self.converged.enforced_for(&self.role)
    }

    /// Restore this node's authoritative own doc from `local_root` (offline-proof),
    /// else the default baseline, then rebuild the converged view.
    fn load(&mut self) {
        self.own = load_doc(&doc_path(&self.local_root, &self.node)).unwrap_or_default();
        self.rebuild_converged();
    }

    /// Rebuild the converged doc: newest-authored (whole-doc LWW) across own ⊕ every
    /// peer's doc. Also counts the peers merged (for the status).
    fn rebuild_converged(&mut self) {
        let mut best = self.own.clone();
        let mut peers = 0usize;
        if let Ok(rd) = std::fs::read_dir(policy_dir(&self.share_root)) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let Some(node) = name.to_str() else {
                    continue;
                };
                if node == self.node {
                    continue;
                }
                if let Some(peer) = load_doc(&path.join(DOC_FILE)) {
                    peers += 1;
                    if peer.updated_ms > best.updated_ms {
                        best = peer;
                    }
                }
            }
        }
        self.peer_count = peers;
        self.converged = best;
    }

    /// Author this node's own policy doc (the `set` verb), stamped now + this node.
    fn author_policy(&mut self, mut doc: BrowserPolicyDoc) {
        doc.updated_ms = self.now_ms();
        doc.updated_by.clone_from(&self.node);
        self.own = doc;
    }

    /// Persist this node's authoritative own doc to `local_root` (restart-proof).
    fn persist_own_local(&self) {
        let dir = node_dir(&self.local_root, &self.node);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        if let Ok(json) = self.own.to_json() {
            let _ = std::fs::write(doc_path(&self.local_root, &self.node), json);
        }
    }

    /// Mirror this node's own doc into the shared Syncthing folder so peers can
    /// converge it. A no-op while the share is down (offline). NEVER writes into a
    /// bare unprovisioned canonical mount (AUDIT-MESH-15). Returns whether it
    /// mirrored.
    fn mirror_doc_to_share(&self) -> bool {
        if !self.share_writable() {
            return false;
        }
        let dir = node_dir(&self.share_root, &self.node);
        if std::fs::create_dir_all(&dir).is_err() {
            return false;
        }
        let Ok(json) = self.own.to_json() else {
            return false;
        };
        std::fs::write(doc_path(&self.share_root, &self.node), json).is_ok()
    }

    /// The disable-aware browser-data sync. When the browser is ENABLED (and the
    /// share is up) the node's browser-data manifest is mirrored to the share so the
    /// data syncs mesh-wide; when it is DISABLED the shared manifest is removed (the
    /// "stop sync" half). In BOTH cases the node-local browser data dir is left
    /// untouched — the "retain local data, no destructive wipe" invariant. Returns
    /// whether the browser-data sync is currently active.
    fn sync_browser_data(&self, enabled: bool) -> bool {
        let manifest = data_manifest_path(&self.share_root, &self.node);
        if enabled && self.share_writable() {
            if let Some(parent) = manifest.parent() {
                if std::fs::create_dir_all(parent).is_err() {
                    return false;
                }
            }
            let listing = self.browser_data_listing();
            std::fs::write(&manifest, listing).is_ok()
        } else {
            // Disabled (or offline): stop syncing — drop the shared manifest. The
            // node-local data dir is deliberately NOT touched.
            let _ = std::fs::remove_file(&manifest);
            false
        }
    }

    /// A newline-joined listing of the node-local browser data files (the manifest
    /// body). Reads names only — never the browser's data contents.
    fn browser_data_listing(&self) -> String {
        let mut names: Vec<String> = std::fs::read_dir(&self.browser_data_root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        names.join("\n")
    }

    /// Whether the node-local browser data survives (the disable-retains-data
    /// invariant): `true` when the local data dir exists with retained contents.
    fn local_data_retained(&self) -> bool {
        self.browser_data_root.is_dir()
    }

    /// One convergence pass (no Bus): persist + mirror own doc, merge peers in, then
    /// run the disable-aware browser-data sync off the freshly-enforced policy. Split
    /// from [`Self::flush`] so tests drive convergence without a Bus.
    fn sync(&mut self) {
        self.persist_own_local();
        let _ = self.mirror_doc_to_share();
        self.rebuild_converged();
        let enabled = self.enforced().browser_enabled;
        let _ = self.sync_browser_data(enabled);
        self.last_flush_ms = self.now_ms();
    }

    /// The current published status derived from the converged + enforced policy.
    #[must_use]
    pub fn status(&self) -> BrowserPolicyStatus {
        let enforced = self.enforced();
        BrowserPolicyStatus {
            node: self.node.clone(),
            role: self.role.clone(),
            surface_hidden: !enforced.browser_enabled,
            browser_enabled: enforced.browser_enabled,
            force_adblock: enforced.force_adblock,
            url_allowlist: enforced.url_allowlist,
            custom_filter_lists: enforced.custom_filter_lists,
            policy_updated_ms: self.converged.updated_ms,
            policy_source: self.converged.updated_by.clone(),
            last_launch_refused: self.last_launch_refused,
            launches_granted: self.launches_granted,
            launches_refused: self.launches_refused,
            navigations_rejected: self.navigations_rejected,
            adblock_toggles_rejected: self.adblock_toggles_rejected,
            peers: self.peer_count,
            share_reachable: self.share_writable(),
            local_data_retained: self.local_data_retained(),
            last_flush_ms: self.last_flush_ms,
        }
    }

    /// Publish `state/browser-policy/<node>`.
    fn publish_state(&self, persist: &Persist) {
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&self.status()) {
            if let Err(e) = persist.write(&topic, Priority::Default, None, Some(&body)) {
                tracing::warn!(target: "mackesd::browser_policy", error = %e, "state publish failed");
            }
        }
    }

    /// A sync pass + publish (the tick body's convergence half).
    fn flush(&mut self, persist: &Persist) {
        self.sync();
        self.publish_state(persist);
    }

    /// Evaluate one enforcement action against the enforced policy, updating the
    /// counters + the last-launch flag. Returns the launch decision for a `launch`
    /// (so the caller can publish it), `None` for navigate / set-adblock.
    fn enforce(&mut self, action: BrowserAction) -> Option<LaunchDecision> {
        let enforced = self.enforced();
        match action {
            BrowserAction::Launch => {
                let decision = enforced.evaluate_launch();
                if decision.is_granted() {
                    self.launches_granted += 1;
                    self.last_launch_refused = false;
                } else {
                    self.launches_refused += 1;
                    self.last_launch_refused = true;
                }
                Some(decision)
            }
            BrowserAction::Navigate { url } => {
                if !enforced.allows_navigation(&url) {
                    self.navigations_rejected += 1;
                    tracing::info!(
                        target: "mackesd::browser_policy",
                        url = %url,
                        "navigation rejected — out of the fleet-policy URL allowlist"
                    );
                }
                None
            }
            BrowserAction::SetAdblock { on } => {
                if !enforced.allows_adblock_toggle(on) {
                    self.adblock_toggles_rejected += 1;
                    tracing::info!(
                        target: "mackesd::browser_policy",
                        "ad-blocker toggle-off rejected — the fleet policy forces it on"
                    );
                }
                None
            }
        }
    }

    /// Drain net-new `action/browser-policy/set` (authoring) + `action/browser/*`
    /// (enforcement) requests. Republishes immediately when any landed so the
    /// surface reflects the change without waiting for the flush.
    fn drain_requests(&mut self, persist: &Persist) {
        let topics = match persist.list_topics() {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_policy", error = %e, "list_topics failed");
                return;
            }
        };
        let mut policy_changed = false;
        let mut enforced_any = false;
        for topic in topics {
            if let Some(verb) = topic.strip_prefix(POLICY_ACTION_PREFIX) {
                if verb.is_empty() {
                    continue;
                }
                for body in self.drain_topic(persist, &topic) {
                    if verb == "set" {
                        match parse_policy_set(&body) {
                            Ok(doc) => {
                                self.author_policy(doc);
                                policy_changed = true;
                            }
                            Err(e) => {
                                tracing::warn!(target: "mackesd::browser_policy", error = %e, "bad policy set");
                            }
                        }
                    } else {
                        tracing::warn!(target: "mackesd::browser_policy", verb, "unknown browser-policy verb");
                    }
                }
            } else if let Some(verb) = topic.strip_prefix(BROWSER_ACTION_PREFIX) {
                if verb.is_empty() {
                    continue;
                }
                let verb = verb.to_string();
                for body in self.drain_topic(persist, &topic) {
                    match parse_browser_action(&verb, &body) {
                        Ok(action) => {
                            let _ = self.enforce(action);
                            enforced_any = true;
                        }
                        Err(e) => {
                            tracing::warn!(target: "mackesd::browser_policy", verb = %verb, error = %e, "bad browser action");
                        }
                    }
                }
            }
        }
        if policy_changed {
            // Persist + mirror the new policy right away, then republish.
            self.persist_own_local();
            let _ = self.mirror_doc_to_share();
            self.rebuild_converged();
            let enabled = self.enforced().browser_enabled;
            let _ = self.sync_browser_data(enabled);
        }
        if policy_changed || enforced_any {
            self.publish_state(persist);
        }
    }

    /// Drain a single topic's net-new bodies (advancing the cursor), oldest first.
    fn drain_topic(&mut self, persist: &Persist, topic: &str) -> Vec<String> {
        let cursor = self.cursors.get(topic).cloned();
        let msgs = match persist.list_since(topic, cursor.as_deref()) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_policy", topic, error = %e, "list_since failed");
                return Vec::new();
            }
        };
        let mut bodies = Vec::new();
        for msg in msgs {
            self.cursors.insert(topic.to_string(), msg.ulid.clone());
            bodies.push(msg.body.unwrap_or_default());
        }
        bodies
    }

    /// Seed each action topic's cursor at its tail so a restart doesn't replay +
    /// re-apply already-processed requests (the policy is already in the doc; the
    /// enforcement counters are advisory + reset per restart by design).
    fn seed_cursors(&mut self, persist: &Persist) {
        if let Ok(topics) = persist.list_topics() {
            for topic in topics.into_iter().filter(|t| {
                (t.starts_with(POLICY_ACTION_PREFIX) && t.len() > POLICY_ACTION_PREFIX.len())
                    || (t.starts_with(BROWSER_ACTION_PREFIX)
                        && t.len() > BROWSER_ACTION_PREFIX.len())
            }) {
                if let Ok(Some(ulid)) = persist.latest_ulid(&topic) {
                    self.cursors.insert(topic, ulid);
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Worker for BrowserPolicyWorker {
    fn name(&self) -> &'static str {
        "browser_policy"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_policy", "no bus root; worker idle");
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_policy", error = %e, "persist open failed; worker idle");
                return Ok(());
            }
        };
        self.load();
        self.seed_cursors(&persist);
        self.flush(&persist); // publish the initial converged + enforced state
        let mut tick = tokio::time::interval(self.tick);
        tick.tick().await; // burn the immediate first tick
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.drain_requests(&persist);
                    self.flush(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        // Clean shutdown: persist + a final mirror so a restart resumes exactly.
        self.persist_own_local();
        let _ = self.mirror_doc_to_share();
        Ok(())
    }
}

/// Resolve the node-local durable browser-policy root
/// (`<XDG_DATA_HOME>/mde/browser-policy`, or `/var/lib/mde/browser-policy` headless).
#[must_use]
pub fn resolve_local_root() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("/var/lib/mde/browser-policy"),
        |d| d.join("mde").join("browser-policy"),
    )
}

/// Resolve the node-local browser data dir the disable path must RETAIN.
///
/// `<XDG_DATA_HOME>/mde/browser` (or `/var/lib/mde/browser` headless) — the
/// browser's local profile/bookmarks/cache. A disable never wipes this.
#[must_use]
pub fn resolve_browser_data_root() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("/var/lib/mde/browser"),
        |d| d.join("mde").join("browser"),
    )
}

/// Wall-clock epoch millis (the production [`NowFn`]).
fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn fake_clock(start: u64) -> (Arc<AtomicU64>, NowFn) {
        let cell = Arc::new(AtomicU64::new(start));
        let reader = cell.clone();
        let now: NowFn = Arc::new(move || reader.load(Ordering::SeqCst));
        (cell, now)
    }

    fn worker(
        node: &str,
        role: &str,
        local: &Path,
        share: &Path,
        now: NowFn,
    ) -> BrowserPolicyWorker {
        // Point the browser-data dir at the local root by default so tests that
        // don't care about it don't touch the real XDG dir.
        BrowserPolicyWorker::new(
            node.to_string(),
            role.to_string(),
            local.to_path_buf(),
            share.to_path_buf(),
        )
        .with_now_fn(now)
        .with_browser_data_root(local.join("browser-data"))
    }

    /// A doc that disables the browser for `role` and forces the ad-blocker on for
    /// the baseline, with an allowlist + a custom list — the "governed" shape.
    fn governed_doc() -> BrowserPolicyDoc {
        let mut roles = BTreeMap::new();
        roles.insert(
            "lighthouse".to_string(),
            RolePolicy {
                browser_enabled: false,
                force_adblock: true,
                url_allowlist: vec![],
                custom_filter_lists: vec![],
            },
        );
        roles.insert(
            "workstation".to_string(),
            RolePolicy {
                browser_enabled: true,
                force_adblock: false, // the baseline force-on ratchets this to true
                url_allowlist: vec!["docs.example.com".into()],
                custom_filter_lists: vec![CustomFilterList {
                    name: "CorpBlocklist".into(),
                    url: Some("https://corp.example/list.txt".into()),
                }],
            },
        );
        BrowserPolicyDoc {
            default: RolePolicy {
                browser_enabled: true,
                force_adblock: true,
                url_allowlist: vec!["example.com".into()],
                custom_filter_lists: vec![],
            },
            roles,
            updated_ms: 1_000,
            updated_by: "operator@eagle".into(),
        }
    }

    // ── the per-role fold (role allow/deny · force-on override · allowlist merge) ──

    #[test]
    fn fold_denies_the_browser_on_a_disallowed_role() {
        let doc = governed_doc();
        let lh = doc.enforced_for("lighthouse");
        assert!(
            !lh.browser_enabled,
            "the policy disables the browser on lighthouse"
        );
        // ...while the workstation role keeps it enabled.
        let ws = doc.enforced_for("workstation");
        assert!(
            ws.browser_enabled,
            "the browser stays enabled on workstation"
        );
    }

    #[test]
    fn fold_ratchets_force_adblock_on_via_the_baseline() {
        let doc = governed_doc();
        // The workstation role sets force_adblock=false, but the baseline forces it
        // on → the ratchet keeps it on (force-on override).
        let ws = doc.enforced_for("workstation");
        assert!(
            ws.force_adblock,
            "the baseline force-on overrides the role's off"
        );
        // A role can also force it on independently of the baseline.
        let mut doc2 = BrowserPolicyDoc::default(); // baseline force=false
        doc2.roles.insert(
            "workstation".into(),
            RolePolicy {
                force_adblock: true,
                ..RolePolicy::permissive()
            },
        );
        assert!(doc2.enforced_for("workstation").force_adblock);
        assert!(
            !doc2.enforced_for("lighthouse").force_adblock,
            "the other role is unaffected"
        );
    }

    #[test]
    fn fold_merges_the_default_and_role_allowlists() {
        let doc = governed_doc();
        let ws = doc.enforced_for("workstation");
        // The enforced allowlist is the UNION of the baseline (example.com) and the
        // role (docs.example.com), host-normalized + deduped.
        assert!(ws.url_allowlist.contains(&"example.com".to_string()));
        assert!(ws.url_allowlist.contains(&"docs.example.com".to_string()));
        assert_eq!(ws.url_allowlist.len(), 2);
        // The custom lists merge too (the role adds CorpBlocklist).
        assert_eq!(ws.custom_filter_lists.len(), 1);
        assert_eq!(ws.custom_filter_lists[0].name, "CorpBlocklist");
    }

    #[test]
    fn fold_of_an_unlisted_role_uses_the_permissive_default() {
        // The empty default doc permits everything for any role.
        let doc = BrowserPolicyDoc::default();
        let e = doc.enforced_for("workstation");
        assert!(e.browser_enabled);
        assert!(!e.force_adblock);
        assert!(e.url_allowlist.is_empty());
        assert!(e.custom_filter_lists.is_empty());
    }

    // ── refuse-to-spawn + inject-on-launch enforcement ──

    #[test]
    fn launch_is_refused_on_a_disallowed_role_and_granted_with_injection_otherwise() {
        let doc = governed_doc();
        // Lighthouse: refused — the launcher must NOT spawn.
        let refused = doc.enforced_for("lighthouse").evaluate_launch();
        let LaunchDecision::Refused { reason } = &refused else {
            unreachable!("expected a refusal on the disallowed role, got {refused:?}");
        };
        assert!(reason.contains("lighthouse"));
        // Workstation: granted, and the grant INJECTS the forced ad-blocker + the
        // merged allowlist + the custom list (the "not just UI" enforcement).
        let granted = doc.enforced_for("workstation").evaluate_launch();
        let LaunchDecision::Granted {
            force_adblock,
            url_allowlist,
            custom_filter_lists,
        } = &granted
        else {
            unreachable!("expected a grant on the allowed role, got {granted:?}");
        };
        assert!(*force_adblock, "the grant injects the forced ad-blocker");
        assert_eq!(
            url_allowlist.len(),
            2,
            "the grant injects the merged allowlist"
        );
        assert_eq!(
            custom_filter_lists.len(),
            1,
            "the grant injects the custom list"
        );
    }

    #[test]
    fn navigation_is_rejected_outside_the_allowlist() {
        let e = governed_doc().enforced_for("workstation"); // allowlist: example.com + docs.example.com
        assert!(
            e.allows_navigation("https://example.com/page"),
            "the base domain"
        );
        assert!(
            e.allows_navigation("https://sub.example.com/x"),
            "a subdomain of the base"
        );
        assert!(
            e.allows_navigation("https://docs.example.com/api"),
            "the role domain"
        );
        assert!(
            !e.allows_navigation("https://evil.test/malware"),
            "an out-of-policy host"
        );
        assert!(
            !e.allows_navigation("https://notexample.com/x"),
            "label-boundary enforced"
        );
        // A hostless URL (about:/data:) has no target to gate → permitted.
        assert!(e.allows_navigation("about:blank"));
        // An empty allowlist permits any URL.
        let open = BrowserPolicyDoc::default().enforced_for("workstation");
        assert!(open.allows_navigation("https://anything.test/"));
    }

    #[test]
    fn adblock_toggle_off_is_rejected_under_force_on() {
        let forced = governed_doc().enforced_for("workstation"); // force_adblock == true
        assert!(
            forced.allows_adblock_toggle(true),
            "turning it ON is always allowed"
        );
        assert!(
            !forced.allows_adblock_toggle(false),
            "turning it OFF is rejected under force-on"
        );
        // Without force-on, the user may toggle it off.
        let free = BrowserPolicyDoc::default().enforced_for("workstation");
        assert!(free.allows_adblock_toggle(false));
    }

    // ── the drain wires the enforcement counters + the launch decision ──

    #[test]
    fn enforce_counts_grants_refusals_and_rejections() {
        let (_c, now) = fake_clock(1_000);
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let mut w = worker("peer:lh", "lighthouse", local.path(), share.path(), now);
        w.author_policy(governed_doc());
        w.rebuild_converged();
        // A launch on the (disallowed) lighthouse role is refused + counted.
        let d = w
            .enforce(BrowserAction::Launch)
            .expect("launch yields a decision");
        assert!(!d.is_granted());
        assert_eq!(w.launches_refused, 1);
        assert!(w.last_launch_refused);

        // Now a workstation node grants + injects, and rejects an out-of-policy nav.
        let mut ws = worker(
            "peer:ws",
            "workstation",
            local.path(),
            share.path(),
            fake_clock(2_000).1,
        );
        ws.author_policy(governed_doc());
        ws.rebuild_converged();
        let g = ws.enforce(BrowserAction::Launch).expect("decision");
        assert!(g.is_granted());
        assert_eq!(ws.launches_granted, 1);
        assert!(!ws.last_launch_refused);
        ws.enforce(BrowserAction::Navigate {
            url: "https://evil.test/x".into(),
        });
        assert_eq!(ws.navigations_rejected, 1);
        ws.enforce(BrowserAction::SetAdblock { on: false });
        assert_eq!(ws.adblock_toggles_rejected, 1);
    }

    // ── two nodes converge on the newest-authored policy ──

    #[test]
    fn two_nodes_converge_on_the_newest_authored_policy() {
        let share = tempfile::tempdir().unwrap();
        let la = tempfile::tempdir().unwrap();
        let lb = tempfile::tempdir().unwrap();
        let mut a = worker(
            "A",
            "workstation",
            la.path(),
            share.path(),
            fake_clock(1_000).1,
        );
        let mut b = worker(
            "B",
            "workstation",
            lb.path(),
            share.path(),
            fake_clock(5_000).1,
        );
        a.load();
        b.load();
        // B authors a governed policy at t=5000; A never authors (default, ms=0).
        b.author_policy(governed_doc()); // re-stamped to 5000 by author_policy
        a.sync();
        b.sync();
        a.sync();
        // Both nodes converge on B's authored policy (the newest stamp wins).
        assert_eq!(
            a.converged.updated_by, "B",
            "A converged on B's authored doc"
        );
        assert_eq!(b.converged.updated_by, "B");
        assert_eq!(a.status().peers, 1, "A merged B's doc");
        // A's enforced workstation policy now carries the forced ad-blocker.
        assert!(a.enforced().force_adblock);
    }

    // ── disable = stop-sync + hide, retain local data (no destructive wipe) ──

    #[test]
    fn disable_stops_sync_and_hides_but_retains_local_data() {
        let (_c, now) = fake_clock(1_000);
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        // A lighthouse node — the governed policy DISABLES the browser on it.
        let mut w = worker("peer:lh", "lighthouse", local.path(), share.path(), now);
        // Seed some node-local browser data that must survive the disable.
        let data_dir = local.path().join("browser-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let profile = data_dir.join("profile.json");
        std::fs::write(&profile, br#"{"bookmarks":3}"#).unwrap();

        // First, ENABLE the browser (default doc) and sync → the manifest mirrors.
        w.load();
        w.sync();
        let manifest = data_manifest_path(share.path(), "peer:lh");
        assert!(
            manifest.exists(),
            "an enabled browser mirrors its data manifest (sync on)"
        );
        assert!(w.status().browser_enabled);
        assert!(!w.status().surface_hidden);

        // Now DISABLE it (author the governed policy) + sync.
        w.author_policy(governed_doc());
        w.sync();
        let st = w.status();
        assert!(!st.browser_enabled, "the browser is disabled on lighthouse");
        assert!(st.surface_hidden, "the surface is hidden");
        // Sync STOPPED — the shared manifest is gone...
        assert!(!manifest.exists(), "disable stops the browser-data sync");
        // ...but the node-local data is RETAINED (no destructive wipe).
        assert!(
            profile.exists(),
            "the local browser data survives the disable"
        );
        assert!(st.local_data_retained, "the retain-data invariant holds");
        assert_eq!(std::fs::read(&profile).unwrap(), br#"{"bookmarks":3}"#);

        // Re-enable → the sync resumes (manifest reappears), data still intact.
        w.own = BrowserPolicyDoc::default(); // clears the disable
        w.author_policy(BrowserPolicyDoc::default());
        w.sync();
        assert!(manifest.exists(), "re-enable resumes the browser-data sync");
        assert!(profile.exists());
    }

    // ── offline-first: a down share never fakes a converge nor wipes data ──

    #[test]
    fn offline_share_is_never_written_and_data_stays_local() {
        let (_c, now) = fake_clock(1_000);
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let gate = Arc::new(AtomicBool::new(false)); // share DOWN
        let mut w = worker("solo", "workstation", local.path(), share.path(), now)
            .with_share_gate(gate.clone());
        let data_dir = local.path().join("browser-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("x"), b"local").unwrap();
        w.load();
        w.author_policy(governed_doc());
        w.sync();
        assert!(!w.status().share_reachable);
        // The authored policy is durable node-local...
        assert!(doc_path(local.path(), "solo").exists());
        // ...but nothing was mirrored into the down share.
        assert!(!doc_path(share.path(), "solo").exists());
        assert!(!data_manifest_path(share.path(), "solo").exists());
        // Local data untouched.
        assert!(data_dir.join("x").exists());

        // Share reappears → the next sync mirrors the backlog out.
        gate.store(true, Ordering::SeqCst);
        w.sync();
        assert!(doc_path(share.path(), "solo").exists());
    }

    // ── typed action parsing ──

    #[test]
    fn parse_browser_action_covers_the_verbs_and_rejects_bad_input() {
        assert_eq!(
            parse_browser_action("launch", "").unwrap(),
            BrowserAction::Launch
        );
        assert_eq!(
            parse_browser_action("navigate", r#"{"url":"https://x.com"}"#).unwrap(),
            BrowserAction::Navigate {
                url: "https://x.com".into()
            },
        );
        assert_eq!(
            parse_browser_action("set-adblock", r#"{"on":false}"#).unwrap(),
            BrowserAction::SetAdblock { on: false },
        );
        assert!(parse_browser_action("frobnicate", "{}").is_err());
        assert!(
            parse_browser_action("navigate", "{}").is_err(),
            "missing url is an error"
        );
        assert!(
            parse_browser_action("navigate", r#"{"url":"  "}"#).is_err(),
            "empty url rejected"
        );
    }

    #[test]
    fn parse_policy_set_round_trips_a_doc() {
        let doc = governed_doc();
        let json = doc.to_json().unwrap();
        let back = parse_policy_set(&json).unwrap();
        assert_eq!(back.roles.len(), 2);
        assert!(parse_policy_set("").is_err(), "empty body is an error");
        assert!(
            parse_policy_set("{ not json").is_err(),
            "malformed body is an error"
        );
    }

    // ── the published status shape ──

    #[test]
    fn status_shape_serializes_the_documented_fields() {
        let (_c, now) = fake_clock(1_000);
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let mut w = worker("peer:eagle", "workstation", local.path(), share.path(), now);
        w.author_policy(governed_doc());
        w.rebuild_converged();
        let status = w.status();
        let json = serde_json::to_string(&status).expect("serialize status");
        let back: BrowserPolicyStatus = serde_json::from_str(&json).expect("round-trip status");
        assert_eq!(back, status);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["node"], "peer:eagle");
        assert_eq!(v["role"], "workstation");
        assert_eq!(v["browser_enabled"], true);
        assert_eq!(v["force_adblock"], true);
        assert_eq!(v["policy_source"], "peer:eagle");
        assert!(v.get("url_allowlist").is_some());
        assert!(v.get("custom_filter_lists").is_some());
        assert!(v.get("surface_hidden").is_some());
        assert!(v.get("last_flush_ms").is_some());
    }

    #[test]
    fn worker_name_is_locked() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let (_c, now) = fake_clock(0);
        let w = worker("n1", "workstation", local.path(), share.path(), now);
        assert_eq!(w.name(), "browser_policy");
    }

    // ── BUG-BROWSER-5 — the tick path must not leak file descriptors ──
    //
    // Eagle logged a recurring `state publish failed … Too many open files
    // (os error 24)` from this worker's `publish_state`. This soak drives the REAL
    // per-tick body — `drain_requests` (list_topics + a net-new enforcement action
    // each iteration) then `flush` (persist + mirror own doc, rebuild the converged
    // view, read the browser-data dir, and publish `state/browser-policy/<node>`) —
    // N times against ONE long-lived `Persist`, exactly as `run()` reuses it, and
    // asserts the process fd count stays bounded. A handle opened each tick and never
    // dropped (a re-opened Bus connection, an un-consumed `ReadDir`, an unreaped
    // child) would climb the count until it trips EMFILE; a small slack absorbs the
    // one-time lazy fds (the sqlite wal/shm, the `/proc/self/fd` snapshot itself).

    /// Count this process's open file descriptors (Linux `/proc/self/fd`).
    #[cfg(target_os = "linux")]
    fn open_fd_count() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(std::iter::Iterator::count)
            .unwrap_or(0)
    }

    /// Publish one fresh `action/browser/launch` so each soak iteration has a net-new
    /// message to drain + `enforce` (the drain cursor advances past prior ones).
    #[cfg(target_os = "linux")]
    fn publish_launch(persist: &Persist) {
        let _ = persist.write("action/browser/launch", Priority::Default, None, Some("{}"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tick_does_not_leak_file_descriptors_over_a_soak() {
        let bus = tempfile::tempdir().unwrap();
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let (_c, now) = fake_clock(1_000);
        let persist =
            Persist::open(bus.path().to_path_buf()).expect("open the bus Persist for the soak");
        let mut w = worker("soak", "workstation", local.path(), share.path(), now)
            .with_share_gate(Arc::new(AtomicBool::new(true))); // share up: exercise the mirror path
        w.load();

        // Warm-up: let one-time lazy fds (sqlite wal/shm, the first topic dirs) settle
        // so they don't count as "leaked" against the measured window.
        for _ in 0..10 {
            publish_launch(&persist);
            w.drain_requests(&persist);
            w.flush(&persist);
        }

        let before = open_fd_count();
        assert!(
            before > 0,
            "/proc/self/fd must be readable to measure the soak"
        );
        for _ in 0..400 {
            publish_launch(&persist);
            w.drain_requests(&persist);
            w.flush(&persist);
        }
        let after = open_fd_count();

        assert!(
            after <= before + 8,
            "browser_policy tick leaked file descriptors over 400 ticks: {before} → {after}"
        );
    }
}
