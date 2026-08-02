//! WL-FUNC-008 — the unified **service provenance/health** record.
//!
//! Three service sources exist on the mesh and were never unified:
//! 1. **Published** services — each node's `kdc-services/<host>.json` directory
//!    (`mde_kdc_host::service_directory::NodeServices`), a bare list of advertised
//!    tokens with no health or endpoint.
//! 2. **Probe**-discovered services — the nmap inventory (`probe-inventory.json`)
//!    each peer writes, with real `ip:port` endpoints + a `service_kind`.
//! 3. **Enrichment** labels — the Explorer's offline `service → openable-action`
//!    map (`unit_aggregator::enrich`), which turns a service kind into a launch verb.
//!
//! A [`ServiceRecord`] is the ONE unified fact: which host offers which service,
//! at what endpoint, with what [`ServiceHealth`], attested by which
//! [`ServiceProvenance`] source(s), and which openable action it owns. The mackesd
//! `service_aggregator` worker merges the three sources into a
//! [`ServicesState`] set (with stale-entry TTL age-out) and publishes it on
//! `state/services/<node>`; the shell's Phones-hub Services view renders it.
//!
//! Lives here (like `mesh_storage` / `device_control` / `vdi_session`) so BOTH the
//! daemon (producer) and the desktop shell (consumer) share one type instead of
//! maintaining byte-compatible copies.

use serde::{Deserialize, Serialize};

/// Which of the three unified sources attests a service fact.
///
/// The variant order is the merge-display order (`Published` < `Probe` <
/// `Enrichment`), so a record attested by every source lists them deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceProvenance {
    /// A node's own published service directory (`kdc-services/<host>.json`).
    Published,
    /// An nmap probe of the host (`probe-inventory.json`).
    Probe,
    /// A host-enrichment label (the Explorer's `service → openable-action` map).
    Enrichment,
}

impl ServiceProvenance {
    /// A stable lowercase token for the wire / a UI badge.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Probe => "probe",
            Self::Enrichment => "enrichment",
        }
    }
}

/// A service's coarse health in the unified view.
///
/// Deliberately coarse + honest (§7): a **published-only** service is advertised
/// but its reachability was never confirmed ([`Unknown`](Self::Unknown)); only a
/// **probe**-attested service is [`Up`](Self::Up). A record last seen beyond the
/// freshness window is [`Stale`](Self::Stale) — and a *probe-only* stale record is
/// aged out of the set entirely (it can't be re-confirmed off a directory row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceHealth {
    /// Probe-confirmed reachable this cycle.
    Up,
    /// Known but unconfirmed this cycle (e.g. published-only, never probed).
    #[default]
    Unknown,
    /// Last seen beyond the freshness window but not yet aged out.
    Stale,
    /// Explicitly reported down.
    Down,
}

impl ServiceHealth {
    /// A stable lowercase token for the wire / a UI badge.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Unknown => "unknown",
            Self::Stale => "stale",
            Self::Down => "down",
        }
    }
}

/// One unified service fact merged across the three sources.
///
/// The merge key is `(host, kind)`: two sources naming the same service on the
/// same host fold into one record whose [`provenance`](Self::provenance) is the
/// union of both. The richest endpoint wins (a probe's `ip:port` over a bare
/// published overlay IP), and the freshest `last_seen_ms` is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceRecord {
    /// The host offering the service (hostname when known, else the address).
    pub host: String,
    /// The coarse service kind / token (`ssh`, `http`, `files`, `openstack`, …).
    pub kind: String,
    /// The dial endpoint (`ip:port`), when a source knows one. `None` for a bare
    /// published token whose node has no resolved overlay IP yet (honest, §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The source(s) that attest this service, sorted + deduped.
    #[serde(default)]
    pub provenance: Vec<ServiceProvenance>,
    /// The coarse health.
    #[serde(default)]
    pub health: ServiceHealth,
    /// The openable-action verb this service owns (`open-ssh`, `open-http`, …),
    /// when the enrichment map knows one; `None` otherwise (never fabricated, §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// Unix-ms this service was last seen by any source (freshness / age-out key).
    #[serde(default)]
    pub last_seen_ms: i64,
}

impl ServiceRecord {
    /// A fresh record for `host`/`kind` with no provenance yet.
    #[must_use]
    pub fn new(host: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            kind: kind.into(),
            endpoint: None,
            provenance: Vec::new(),
            health: ServiceHealth::Unknown,
            action: None,
            last_seen_ms: 0,
        }
    }

    /// The `(host, kind)` merge key.
    #[must_use]
    pub fn key(&self) -> (String, String) {
        (self.host.clone(), self.kind.clone())
    }

    /// Record that `source` attests this service, keeping the provenance list
    /// sorted + deduped (idempotent — a second attestation is a no-op).
    pub fn attest(&mut self, source: ServiceProvenance) {
        if let Err(idx) = self.provenance.binary_search(&source) {
            self.provenance.insert(idx, source);
        }
    }

    /// Whether `source` attests this service.
    #[must_use]
    pub fn attested_by(&self, source: ServiceProvenance) -> bool {
        self.provenance.binary_search(&source).is_ok()
    }

    /// Structural equality IGNORING `last_seen_ms` — the publish-on-change gate.
    /// A freshness bump that changes nothing material must NOT trigger a republish;
    /// a health/endpoint/provenance/action change must (mirrors the sibling
    /// `UnitsState::same_ignoring_time` idiom).
    #[must_use]
    pub fn same_ignoring_time(&self, other: &Self) -> bool {
        self.host == other.host
            && self.kind == other.kind
            && self.endpoint == other.endpoint
            && self.provenance == other.provenance
            && self.health == other.health
            && self.action == other.action
    }
}

/// The `state/services/<node>` mirror body — the aggregated unified set one node
/// publishes. Every node folds + publishes its OWN merge of the (replicated)
/// sources, so any node's mirror carries the mesh-wide picture (no center).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServicesState {
    /// The publishing node's id (the Bus `host` stamp + topic namespace).
    pub host: String,
    /// The merged unified records, sorted by `(host, kind)`.
    #[serde(default)]
    pub records: Vec<ServiceRecord>,
    /// Unix-ms this mirror was published (latest-wins fold key on the reader side).
    #[serde(default)]
    pub published_at_ms: i64,
}

impl ServicesState {
    /// Publish-on-change equality: the record sets match ignoring per-record
    /// freshness (`last_seen_ms`) and the mirror publish time.
    #[must_use]
    pub fn same_ignoring_time(&self, other: &Self) -> bool {
        self.host == other.host
            && self.records.len() == other.records.len()
            && self
                .records
                .iter()
                .zip(&other.records)
                .all(|(a, b)| a.same_ignoring_time(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attest_keeps_provenance_sorted_and_deduped() {
        let mut r = ServiceRecord::new("alpha", "ssh");
        r.attest(ServiceProvenance::Probe);
        r.attest(ServiceProvenance::Published);
        r.attest(ServiceProvenance::Probe); // idempotent
        r.attest(ServiceProvenance::Enrichment);
        assert_eq!(
            r.provenance,
            vec![
                ServiceProvenance::Published,
                ServiceProvenance::Probe,
                ServiceProvenance::Enrichment,
            ]
        );
        assert!(r.attested_by(ServiceProvenance::Published));
        assert!(!ServiceRecord::new("b", "http").attested_by(ServiceProvenance::Probe));
    }

    #[test]
    fn same_ignoring_time_tolerates_a_freshness_bump_only() {
        let mut a = ServiceRecord::new("alpha", "ssh");
        a.attest(ServiceProvenance::Probe);
        a.last_seen_ms = 100;
        let mut b = a.clone();
        b.last_seen_ms = 999; // only freshness moved
        assert!(a.same_ignoring_time(&b));
        b.health = ServiceHealth::Stale; // a material change
        assert!(!a.same_ignoring_time(&b));
    }

    #[test]
    fn record_round_trips_through_json() {
        let mut r = ServiceRecord::new("alpha", "ssh");
        r.attest(ServiceProvenance::Published);
        r.attest(ServiceProvenance::Probe);
        r.endpoint = Some("10.42.0.5:22".into());
        r.health = ServiceHealth::Up;
        r.action = Some("open-ssh".into());
        r.last_seen_ms = 1700;
        let json = serde_json::to_string(&r).unwrap();
        let back: ServiceRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        // The wire tokens are the stable snake_case forms.
        assert!(json.contains("\"published\""));
        assert!(json.contains("\"up\""));
    }

    #[test]
    fn state_same_ignoring_time_is_a_heartbeat_no_op() {
        let mut rec = ServiceRecord::new("alpha", "ssh");
        rec.attest(ServiceProvenance::Probe);
        rec.last_seen_ms = 1;
        let a = ServicesState {
            host: "me".into(),
            records: vec![rec.clone()],
            published_at_ms: 10,
        };
        let mut b = a.clone();
        b.published_at_ms = 99;
        b.records[0].last_seen_ms = 88;
        assert!(
            a.same_ignoring_time(&b),
            "heartbeat republish is not a change"
        );
        b.records[0].health = ServiceHealth::Down;
        assert!(!a.same_ignoring_time(&b), "a health flip is a change");
    }
}
