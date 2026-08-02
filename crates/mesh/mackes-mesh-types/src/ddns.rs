//! DDNS-EGRESS — the dynamic-DNS config model + pure hostname/change logic
//! (design: `docs/design/ddns-egress.md`).
//!
//! When a VPN exit (or WAN) IP changes, DDNS rewrites a stable hostname under
//! `services.matthewmackes.com` to the new IP via the `DigitalOcean` DNS API. This
//! crate holds the durable `[ddns]` config (TOML on the shared substrate), the
//! record-name templating (`{node}-{provider}` → a FQDN in the zone), and the
//! **change-detection predicate** (only a real diff from the last-published value
//! triggers a DNS write — no churn). The `mackesd` `ddns` worker subscribes to
//! VPN-GW exit-IP changes + runs the WAN check, and the `DnsWriter` (DO) adapter
//! lives daemon-side; here is the pure, unit-tested core.

use serde::{Deserialize, Serialize};

/// What to do with a record when its tunnel goes down (kill-switch policy).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnDown {
    /// Delete the record (no stale/leaking address).
    #[default]
    Remove,
    /// Point it at a sentinel address (a parked/unreachable IP).
    Sentinel,
    /// Leave the last value (identity record; reachability may be stale).
    Keep,
}

/// One managed DNS record definition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordDef {
    /// Name template, e.g. `{node}-{provider}` → `eagle-mullvad`. Placeholders:
    /// `{node}`, `{provider}`, `{n}` (multi-instance index).
    pub name: String,
    /// IP source: a VPN-GW tunnel id (`tunnel:<id>`) or `wan` for the node WAN.
    pub source: String,
    /// Kill-switch behavior when the source is down.
    #[serde(default)]
    pub on_down: OnDown,
}

impl RecordDef {
    /// Resolve the template into the FQDN under `zone`, substituting `{node}` /
    /// `{provider}` / `{n}`. An absent `{n}` placeholder is fine (single
    /// instance). Pure + stable. The label is lowercased + non-DNS chars → `-`.
    #[must_use]
    pub fn fqdn(&self, node: &str, provider: &str, n: u32, zone: &str) -> String {
        let label = self
            .name
            .replace("{node}", node)
            .replace("{provider}", provider)
            .replace("{n}", &n.to_string());
        let label: String = label
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let label = label.trim_matches('-');
        format!("{label}.{zone}")
    }
}

/// CONNECT-9 — derive the bare DDNS record label for an ingress `hostname`.
///
/// A fully-qualified ingress hostname (`grafana.services.matthewmackes.com` in the
/// `services.matthewmackes.com` zone) is reduced to its bare label (`grafana`); a
/// hostname that is already bare (no zone suffix) is passed through trimmed. Empty
/// when nothing is left after stripping (a hostname equal to the zone), so the
/// caller skips it. Pure.
#[must_use]
pub fn ingress_record_label(hostname: &str, zone: &str) -> String {
    let host = hostname.trim().trim_end_matches('.');
    let zone = zone.trim().trim_end_matches('.');
    // A hostname equal to the zone has no bare label — collapse to empty so the
    // caller skips it (the `.{zone}` suffix strip below can't match the apex,
    // which would otherwise leave the zone itself as a bogus label).
    if host == zone {
        return String::new();
    }
    let suffix = format!(".{zone}");
    host.strip_suffix(&suffix)
        .unwrap_or(host)
        .trim()
        .to_string()
}

/// CONNECT-9 — the [`RecordDef`] a CONNECT ingress hostname maps to.
///
/// The bare `label` is published as a `wan`-sourced A record (the public name must
/// resolve to the ingress lighthouse's own public WAN IP, which Caddy/the
/// stream-forward terminate on). `on_down = keep` so a transient WAN-probe miss
/// never drops a live public name — the firewall/Caddy layer governs real
/// reachability, not the DNS record. `None` when `label` is empty (a hostname that
/// collapsed to the bare zone). The `mackesd` `ddns` reconcile worker
/// (DDNS-EGRESS-3) publishes it; this is only the durable record definition, NOT a
/// second DNS path. Pure.
#[must_use]
pub fn ingress_record(label: &str) -> Option<RecordDef> {
    let label = label.trim();
    if label.is_empty() {
        return None;
    }
    Some(RecordDef {
        name: label.to_string(),
        source: "wan".to_string(),
        on_down: OnDown::Keep,
    })
}

/// The `[ddns]` durable config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DdnsConfig {
    /// Master enable.
    #[serde(default)]
    pub enabled: bool,
    /// `DnsWriter` adapter (`digitalocean` in v1).
    #[serde(default = "default_provider")]
    pub provider: String,
    /// The DNS zone records live under.
    #[serde(default = "default_zone")]
    pub zone: String,
    /// Reference to the age-encrypted API token in the mesh secret store.
    #[serde(default)]
    pub token_ref: String,
    /// Record TTL seconds (short, so a change propagates fast).
    #[serde(default = "default_ttl")]
    pub ttl: u32,
    /// Managed records.
    #[serde(default)]
    pub record: Vec<RecordDef>,
}

fn default_provider() -> String {
    "digitalocean".to_string()
}
fn default_zone() -> String {
    "services.matthewmackes.com".to_string()
}
const fn default_ttl() -> u32 {
    60
}

impl Default for DdnsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_provider(),
            zone: default_zone(),
            token_ref: String::new(),
            ttl: default_ttl(),
            record: Vec::new(),
        }
    }
}

impl DdnsConfig {
    /// Parse from TOML.
    ///
    /// # Errors
    /// A TOML parse error.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to TOML.
    ///
    /// # Errors
    /// A TOML serialize error.
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// CONNECT-9 — insert-or-replace a managed record keyed by its name label (the
    /// stable key the responder + worker already use). Returns `true` when the
    /// config changed (a new record, or an existing one whose source/policy
    /// differed) so the caller can skip a no-op save. Idempotent: re-adding an
    /// identical record is a no-op.
    pub fn upsert_record(&mut self, rec: RecordDef) -> bool {
        if let Some(existing) = self.record.iter_mut().find(|r| r.name == rec.name) {
            if *existing == rec {
                return false;
            }
            *existing = rec;
            true
        } else {
            self.record.push(rec);
            true
        }
    }

    /// CONNECT-9 — remove a managed record by its name label. Returns `true` when
    /// one was removed (so the caller skips a no-op save on an already-absent name).
    pub fn remove_record(&mut self, name: &str) -> bool {
        let before = self.record.len();
        self.record.retain(|r| r.name != name);
        self.record.len() != before
    }
}

/// The change-detection predicate: should DDNS write `current` for a record
/// whose last-published value was `last`? Only a real change triggers a write —
/// `None` last (never published) or a differing value ⇒ yes; an unchanged value
/// ⇒ no (the no-churn rule). An empty `current` (no IP resolved yet) ⇒ no.
#[must_use]
pub fn needs_update(last: Option<&str>, current: &str) -> bool {
    if current.trim().is_empty() {
        return false;
    }
    last != Some(current)
}

/// The A/AAAA record type for an IP literal — `AAAA` for IPv6 (contains `:`),
/// else `A`. Pure helper for the `DigitalOcean` writer.
#[must_use]
pub fn record_type(ip: &str) -> &'static str {
    if ip.contains(':') {
        "AAAA"
    } else {
        "A"
    }
}

/// DDNS-EGRESS-2 — the `DigitalOcean` DNS API request to **upsert** a record: a
/// `PUT …/records/{id}` when `existing_id` is known, else a `POST …/records` to
/// create. Returns `(method, path, json_body)`; the daemon adapter attaches the
/// bearer token + executes. `name` is the bare label (the part before the zone).
/// Pure + testable — no HTTP here (keeps this lightweight crate dep-free).
#[must_use]
pub fn do_upsert_request(
    domain: &str,
    name: &str,
    ip: &str,
    ttl: u32,
    existing_id: Option<&str>,
) -> (&'static str, String, String) {
    let rtype = record_type(ip);
    let body = serde_json::json!({
        "type": rtype, "name": name, "data": ip, "ttl": ttl
    })
    .to_string();
    match existing_id {
        Some(id) => ("PUT", format!("/v2/domains/{domain}/records/{id}"), body),
        None => ("POST", format!("/v2/domains/{domain}/records"), body),
    }
}

/// DDNS-EGRESS-2 — the `DigitalOcean` request to **delete** a record by id
/// (`on_down = remove`). Returns `(method, path)`. Pure.
#[must_use]
pub fn do_delete_request(domain: &str, id: &str) -> (&'static str, String) {
    ("DELETE", format!("/v2/domains/{domain}/records/{id}"))
}

/// DDNS-EGRESS-4 — the **sentinel address** an `on_down = sentinel` record is
/// parked at when its tunnel drops: RFC 5737 TEST-NET-1 (`192.0.2.1`), an address
/// guaranteed never to be globally routed. Pointing a name here on a down tunnel
/// keeps the record resolvable (so clients get a definite *unreachable* answer
/// instead of NXDOMAIN) without ever leaking to a live/leaking exit.
pub const SENTINEL_ADDR: &str = "192.0.2.1";

/// DDNS-EGRESS-4 — the live state of a record's IP source, fed from the VPN-GW
/// exit-IP verifier (VPN-GW-6) for a `tunnel:<id>` source or the WAN check for a
/// `wan` source. This is what the reconcile decision ([`plan_action`]) consumes;
/// the worker resolves it each tick and the responder accepts it as a status
/// query input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum SourceState {
    /// The source is **up** with a verified address — publish/rewrite to it.
    Up {
        /// The verified current exit/WAN IP (A or AAAA literal).
        ip: String,
        /// Whether the tunnel can accept **inbound** connections at this IP —
        /// `true` only when the provider exposes a forwarded port / dedicated IP.
        /// A shared exit with no port-forward is identity-only (see
        /// [`reachability`]).
        ///
        /// [`reachability`]: Self::reachability
        #[serde(default)]
        port_forward: bool,
    },
    /// The source is **down** (tunnel flapped / WAN check failed). The reconcile
    /// decision then follows the record's [`OnDown`] policy, tied to whether the
    /// VPN-GW kill-switch is actively blocking this tunnel's egress.
    Down {
        /// `true` when the VPN-GW kill-switch is blocking this source's egress —
        /// the leak-proof state. With the kill-switch engaged, an
        /// `on_down = keep` record is downgraded to a sentinel park so the name
        /// never points at an IP that is (or could resume) leaking (the
        /// leak-coupling rule, design Risks).
        #[serde(default)]
        kill_switch: bool,
    },
}

/// DDNS-EGRESS-4 — how reachable a published name is, for the UI flag the
/// acceptance calls out: a name whose tunnel can't accept inbound is **identity
/// only** ("port-forward only") while still publishing its A record; a tunnel
/// with a forwarded port / dedicated IP is fully `Inbound`-reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reachability {
    /// The exit accepts inbound — the reachability use works (port-forward / a
    /// dedicated IP).
    Inbound,
    /// A shared exit with no port-forward — the record is an **identity** record
    /// only; reachability is "port-forward only" / not inbound-reachable.
    IdentityOnly,
    /// The source is down — nothing is reachable right now.
    Down,
}

impl Reachability {
    /// The operator-facing label the UI shows for this reachability.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::IdentityOnly => "port-forward only",
            Self::Down => "down",
        }
    }
}

/// DDNS-EGRESS-4 — the reachability of a record given its source state: an up
/// source with a forwarded port is [`Reachability::Inbound`], an up source
/// without one is [`Reachability::IdentityOnly`] (still published, flagged
/// "port-forward only"), and a down source is [`Reachability::Down`].
#[must_use]
pub const fn reachability(state: &SourceState) -> Reachability {
    match state {
        SourceState::Up {
            port_forward: true, ..
        } => Reachability::Inbound,
        SourceState::Up {
            port_forward: false,
            ..
        } => Reachability::IdentityOnly,
        SourceState::Down { .. } => Reachability::Down,
    }
}

/// DDNS-EGRESS-4 — the reconcile **decision** for one record: what the DNS writer
/// should do given the record's last-published value and the live source state.
/// Pure value the [`plan_action`] predicate yields; the daemon adapter turns it
/// into the DO API call ([`do_upsert_request`] / [`do_delete_request`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum DdnsAction {
    /// Write (create-or-update) the A/AAAA record to `ip`. Emitted on
    /// first-publish and on a **reconnect-with-new-IP** (the rewrite the
    /// acceptance requires) — only when the value actually changed (no churn).
    Upsert {
        /// The address to publish.
        ip: String,
    },
    /// Delete the record (`on_down = remove`, or a kill-switched `keep`/`remove`)
    /// so no stale name points at a dead exit.
    Remove,
    /// Leave the record exactly as published — no DNS call. Either nothing
    /// changed (the no-churn case) or `on_down = keep` on a clean (non-kill-
    /// switched) down where the operator chose to retain the last value.
    Noop,
}

/// DDNS-EGRESS-4 — the pure reconcile decision tying together **reconnect
/// rewrite** and the **on-down policy**. Given a record, its `last`-published
/// value (`None` = never published), and the live [`SourceState`], decide the
/// [`DdnsAction`]:
///
/// - **Source up** → publish/rewrite to the verified IP, but only if it differs
///   from `last` ([`needs_update`] — no churn). An already-correct record is a
///   [`DdnsAction::Noop`]. This is the reconnect-with-new-IP rewrite: a new exit
///   IP differs from `last`, so it is rewritten (within ~TTL).
/// - **Source down** → follow [`OnDown`], **tied to the kill-switch**:
///     - `remove` → [`DdnsAction::Remove`] (never leave a dead/leaking record).
///     - `sentinel` → rewrite to [`SENTINEL_ADDR`] (a parked, unroutable address)
///       — resolvable but definitively unreachable, never the dead exit.
///     - `keep` → keep the last value **unless the kill-switch is engaged**, in
///       which case it is downgraded to a sentinel park (the leak-coupling rule:
///       a kill-switched tunnel must not keep a name pointed at an IP that is or
///       could resume leaking). A never-published `keep`/`sentinel` on a down
///       source has nothing to keep ⇒ [`DdnsAction::Noop`] / sentinel publish.
#[must_use]
pub fn plan_action(record: &RecordDef, last: Option<&str>, state: &SourceState) -> DdnsAction {
    match state {
        SourceState::Up { ip, .. } => {
            if needs_update(last, ip) {
                DdnsAction::Upsert { ip: ip.clone() }
            } else {
                DdnsAction::Noop
            }
        }
        SourceState::Down { kill_switch } => match record.on_down {
            OnDown::Remove => {
                // Already absent ⇒ nothing to do; else delete it.
                if last.is_none() {
                    DdnsAction::Noop
                } else {
                    DdnsAction::Remove
                }
            }
            OnDown::Sentinel => park_sentinel(last),
            OnDown::Keep => {
                if *kill_switch {
                    // Leak-coupling: a kill-switched keep is downgraded to a park
                    // so the name never resolves to the (blocked/leaking) exit.
                    park_sentinel(last)
                } else {
                    // Retain the last value as-is (identity record) — no call.
                    DdnsAction::Noop
                }
            }
        },
    }
}

/// Park a record at the [`SENTINEL_ADDR`] — an upsert only when it is not already
/// there (no churn); a never-published record has nothing to point and stays a
/// [`DdnsAction::Noop`] (don't create a record just to park it).
fn park_sentinel(last: Option<&str>) -> DdnsAction {
    match last {
        None => DdnsAction::Noop,
        Some(SENTINEL_ADDR) => DdnsAction::Noop,
        Some(_) => DdnsAction::Upsert {
            ip: SENTINEL_ADDR.to_string(),
        },
    }
}

/// Durable path for the DDNS config: `<workgroup_root>/ddns/config.toml`.
#[must_use]
pub fn config_path(workgroup_root: &std::path::Path) -> std::path::PathBuf {
    workgroup_root.join("ddns").join("config.toml")
}

/// Load the DDNS config (missing/malformed → default disabled).
#[must_use]
pub fn load(workgroup_root: &std::path::Path) -> DdnsConfig {
    std::fs::read_to_string(config_path(workgroup_root))
        .ok()
        .and_then(|raw| DdnsConfig::from_toml_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the DDNS config (atomic temp+rename).
///
/// # Errors
/// An I/O / serialize error.
pub fn save(
    workgroup_root: &std::path::Path,
    cfg: &DdnsConfig,
) -> Result<std::path::PathBuf, String> {
    let path = config_path(workgroup_root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let toml = cfg.to_toml_string().map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fqdn_templates_node_provider_and_sanitizes() {
        let r = RecordDef {
            name: "{node}-{provider}".into(),
            source: "tunnel:mullvad-1".into(),
            on_down: OnDown::Remove,
        };
        assert_eq!(
            r.fqdn("eagle", "mullvad", 1, "services.matthewmackes.com"),
            "eagle-mullvad.services.matthewmackes.com"
        );
        // {n} + uppercasing + non-DNS chars.
        let r2 = RecordDef {
            name: "{node}_{provider}{n}".into(),
            ..Default::default()
        };
        assert_eq!(
            r2.fqdn("Eagle", "Proton", 2, "z.example"),
            "eagle-proton2.z.example"
        );
    }

    #[test]
    fn ingress_record_label_strips_zone_suffix() {
        // CONNECT-9 — a fqdn ingress hostname → its bare label.
        assert_eq!(
            ingress_record_label(
                "grafana.services.matthewmackes.com",
                "services.matthewmackes.com"
            ),
            "grafana"
        );
        // A trailing-dot fqdn + zone are both tolerated.
        assert_eq!(ingress_record_label("app.z.example.", "z.example."), "app");
        // Already bare → passed through.
        assert_eq!(ingress_record_label("bare", "z.example"), "bare");
        // A hostname equal to the zone collapses to empty (caller skips it).
        assert_eq!(ingress_record_label("z.example", "z.example"), "");
    }

    #[test]
    fn ingress_record_is_wan_keep_or_none() {
        // CONNECT-9 — an ingress name maps to a wan-sourced keep record.
        let r = ingress_record("grafana").expect("non-empty label → a record");
        assert_eq!(r.name, "grafana");
        assert_eq!(r.source, "wan");
        assert_eq!(r.on_down, OnDown::Keep);
        // An empty label yields no record (nothing to publish).
        assert!(ingress_record("").is_none());
        assert!(ingress_record("   ").is_none());
    }

    #[test]
    fn upsert_remove_record_are_idempotent_and_report_change() {
        // CONNECT-9 — the config-level glue the expose/unexpose flow drives.
        let mut cfg = DdnsConfig::default();
        let rec = ingress_record("grafana").unwrap();
        // First add → changed.
        assert!(cfg.upsert_record(rec.clone()));
        assert_eq!(cfg.record.len(), 1);
        // Re-adding the identical record → no change (no churn).
        assert!(!cfg.upsert_record(rec.clone()));
        // A differing source for the same label → replace, changed.
        let mut moved = rec.clone();
        moved.source = "tunnel:m1".into();
        assert!(cfg.upsert_record(moved));
        assert_eq!(cfg.record.len(), 1);
        // Remove by label → changed; removing again → no change.
        assert!(cfg.remove_record("grafana"));
        assert!(!cfg.remove_record("grafana"));
        assert!(cfg.record.is_empty());
    }

    #[test]
    fn needs_update_only_on_real_change() {
        assert!(needs_update(None, "1.2.3.4")); // never published
        assert!(needs_update(Some("1.2.3.4"), "5.6.7.8")); // changed
        assert!(!needs_update(Some("1.2.3.4"), "1.2.3.4")); // unchanged → no churn
        assert!(!needs_update(None, "")); // no IP resolved yet
        assert!(!needs_update(Some("1.2.3.4"), "  ")); // blank current
    }

    #[test]
    fn config_defaults_and_round_trip() {
        let c = DdnsConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.provider, "digitalocean");
        assert_eq!(c.zone, "services.matthewmackes.com");
        assert_eq!(c.ttl, 60);
        let mut c2 = c.clone();
        c2.enabled = true;
        c2.record.push(RecordDef {
            name: "{node}-{provider}".into(),
            source: "wan".into(),
            on_down: OnDown::Keep,
        });
        let s = c2.to_toml_string().unwrap();
        assert_eq!(DdnsConfig::from_toml_str(&s).unwrap(), c2);
    }

    #[test]
    fn do_requests_create_update_delete() {
        // No existing id → POST create.
        let (m, p, b) =
            do_upsert_request("matthewmackes.com", "eagle-mullvad", "1.2.3.4", 60, None);
        assert_eq!(m, "POST");
        assert_eq!(p, "/v2/domains/matthewmackes.com/records");
        assert!(b.contains("\"type\":\"A\"") && b.contains("\"data\":\"1.2.3.4\""));
        // Existing id → PUT update.
        let (m, p, _) = do_upsert_request(
            "matthewmackes.com",
            "eagle-mullvad",
            "1.2.3.4",
            60,
            Some("99"),
        );
        assert_eq!(m, "PUT");
        assert_eq!(p, "/v2/domains/matthewmackes.com/records/99");
        // IPv6 → AAAA.
        let (_, _, b6) = do_upsert_request("z", "n", "2001:db8::1", 60, None);
        assert!(b6.contains("\"type\":\"AAAA\""));
        // Delete.
        assert_eq!(
            do_delete_request("matthewmackes.com", "99"),
            (
                "DELETE",
                "/v2/domains/matthewmackes.com/records/99".to_string()
            )
        );
    }

    #[test]
    fn record_type_picks_a_or_aaaa() {
        assert_eq!(record_type("1.2.3.4"), "A");
        assert_eq!(record_type("2001:db8::1"), "AAAA");
    }

    #[test]
    fn load_missing_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(load(tmp.path()), DdnsConfig::default());
    }

    // ── DDNS-EGRESS-4: reconnect rewrite + on-down policy ──────────────────

    fn rec(on_down: OnDown) -> RecordDef {
        RecordDef {
            name: "{node}-{provider}".into(),
            source: "tunnel:mullvad-1".into(),
            on_down,
        }
    }

    #[test]
    fn up_publishes_first_then_rewrites_on_new_ip_no_churn() {
        let r = rec(OnDown::Remove);
        let up = |ip: &str| SourceState::Up {
            ip: ip.into(),
            port_forward: false,
        };
        // Never published → first publish.
        assert_eq!(
            plan_action(&r, None, &up("1.2.3.4")),
            DdnsAction::Upsert {
                ip: "1.2.3.4".into()
            }
        );
        // Reconnect with a NEW IP → rewrite (within ~TTL).
        assert_eq!(
            plan_action(&r, Some("1.2.3.4"), &up("5.6.7.8")),
            DdnsAction::Upsert {
                ip: "5.6.7.8".into()
            }
        );
        // Same IP already published → no churn.
        assert_eq!(
            plan_action(&r, Some("5.6.7.8"), &up("5.6.7.8")),
            DdnsAction::Noop
        );
    }

    #[test]
    fn down_remove_deletes_a_published_record_but_noops_when_absent() {
        let r = rec(OnDown::Remove);
        let down = SourceState::Down { kill_switch: false };
        // A live record on a down tunnel → delete it (no stale/leaking record).
        assert_eq!(plan_action(&r, Some("1.2.3.4"), &down), DdnsAction::Remove);
        // Nothing was ever published → nothing to remove.
        assert_eq!(plan_action(&r, None, &down), DdnsAction::Noop);
    }

    #[test]
    fn down_sentinel_parks_at_test_net_and_does_not_re_churn() {
        let r = rec(OnDown::Sentinel);
        let down = SourceState::Down { kill_switch: false };
        // Park a live record at the sentinel (resolvable but unreachable).
        assert_eq!(
            plan_action(&r, Some("1.2.3.4"), &down),
            DdnsAction::Upsert {
                ip: SENTINEL_ADDR.into()
            }
        );
        // Already parked → no churn.
        assert_eq!(
            plan_action(&r, Some(SENTINEL_ADDR), &down),
            DdnsAction::Noop
        );
        // Never published → don't create a record just to park it.
        assert_eq!(plan_action(&r, None, &down), DdnsAction::Noop);
        // The sentinel is the RFC 5737 TEST-NET-1 address.
        assert_eq!(SENTINEL_ADDR, "192.0.2.1");
    }

    #[test]
    fn down_keep_retains_unless_kill_switch_engaged() {
        let r = rec(OnDown::Keep);
        // Clean down (kill-switch not engaged) → keep the last value (identity).
        assert_eq!(
            plan_action(
                &r,
                Some("1.2.3.4"),
                &SourceState::Down { kill_switch: false }
            ),
            DdnsAction::Noop
        );
        // Kill-switch engaged → leak-coupling downgrades keep to a sentinel park,
        // so the name never resolves to the blocked/leaking exit.
        assert_eq!(
            plan_action(
                &r,
                Some("1.2.3.4"),
                &SourceState::Down { kill_switch: true }
            ),
            DdnsAction::Upsert {
                ip: SENTINEL_ADDR.into()
            }
        );
        // Kill-switched but already parked → no churn.
        assert_eq!(
            plan_action(
                &r,
                Some(SENTINEL_ADDR),
                &SourceState::Down { kill_switch: true }
            ),
            DdnsAction::Noop
        );
    }

    #[test]
    fn reachability_flags_port_forward_only() {
        // Up with a forwarded port → fully inbound-reachable.
        assert_eq!(
            reachability(&SourceState::Up {
                ip: "1.2.3.4".into(),
                port_forward: true,
            }),
            Reachability::Inbound
        );
        // Up without one → identity only ("port-forward only").
        let id = reachability(&SourceState::Up {
            ip: "1.2.3.4".into(),
            port_forward: false,
        });
        assert_eq!(id, Reachability::IdentityOnly);
        assert_eq!(id.label(), "port-forward only");
        // Down → nothing reachable.
        assert_eq!(
            reachability(&SourceState::Down { kill_switch: false }),
            Reachability::Down
        );
    }

    #[test]
    fn identity_only_source_still_publishes_the_record() {
        // A shared exit with no port-forward is identity-only, but the A record
        // is still published (the acceptance: publish the identity record
        // regardless, flag reachability "port-forward only").
        let r = rec(OnDown::Keep);
        let state = SourceState::Up {
            ip: "9.9.9.9".into(),
            port_forward: false,
        };
        assert_eq!(reachability(&state), Reachability::IdentityOnly);
        assert_eq!(
            plan_action(&r, None, &state),
            DdnsAction::Upsert {
                ip: "9.9.9.9".into()
            }
        );
    }

    #[test]
    fn source_state_round_trips_through_json() {
        for state in [
            SourceState::Up {
                ip: "1.2.3.4".into(),
                port_forward: true,
            },
            SourceState::Down { kill_switch: true },
        ] {
            let s = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<SourceState>(&s).unwrap(), state);
        }
        // port_forward / kill_switch default to false when omitted.
        let up: SourceState = serde_json::from_str(r#"{"state":"up","ip":"1.2.3.4"}"#).unwrap();
        assert_eq!(
            up,
            SourceState::Up {
                ip: "1.2.3.4".into(),
                port_forward: false
            }
        );
        let down: SourceState = serde_json::from_str(r#"{"state":"down"}"#).unwrap();
        assert_eq!(down, SourceState::Down { kill_switch: false });
    }
}
