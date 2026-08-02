//! Lighthouse discovery + binary health (LIGHTHOUSE-2).
//!
//! The lighthouses are the relay/anchor nodes of the Nebula overlay. This
//! module derives, from the **replicated peer directory** ([`PeerRecord`]) that
//! every node already mirrors over the shared dir, the two things the hero
//! surfaces need:
//!
//!   1. **Discovery** — which peers are lighthouses ([`is_lighthouse`] /
//!      [`lighthouse_records`]); the set is `role == "lighthouse"` in the
//!      replicated directory JSON (design Q1).
//!   2. **Binary health** — a single green/red [`Beacon`] per lighthouse
//!      ([`beacon_for`]), so the Hub footer, the Workbench Lighthouses tab, and
//!      the panel applet all agree (design Q2/Q3/Q15).
//!
//! Health is **strictly binary** (Q15): a lighthouse is green iff it is online
//! AND its overlay is up AND — for the elected leader (Q3/Q22) — its core
//! services are healthy; anything else (offline, overlay down, leader service
//! degraded, or no data yet) folds to red. The classifier ([`classify`]) is a
//! pure function of explicit booleans so it is exhaustively unit-testable;
//! [`beacon_for`] is the thin adapter that reads those booleans off a
//! [`PeerRecord`] + the caller-supplied "is this the leader?" fact.

use crate::peers::PeerRecord;

/// Records older than this many milliseconds are treated as offline — three
/// missed 30 s heartbeats (the telemetry cadence). A lighthouse that has not
/// refreshed its directory row within this window is presumed down.
pub const DEFAULT_STALE_MS: u64 = 90_000;

/// The directory `role` value that marks a lighthouse (design Q1).
pub const LIGHTHOUSE_ROLE: &str = "lighthouse";

/// Retired class name retained for decoding historical directory snapshots.
/// Current lighthouses are always the thin plain class.
pub const LIGHTHOUSE_MEDIA_CLASS: &str = "lighthouse_media";

/// How long a leader lease is valid (mirrors `mackesd`'s `leader::LEASE_DURATION`).
/// A lease older than this is treated as no leader (failover in progress).
pub const LEASE_DURATION_S: u64 = 60;

/// Parse the current leader hostname from the leader-lease contents
/// (the etcd leader lease, or `<workgroup>/.mackesd-leader.lock` pre-etcd). The
/// lease line is `node_id\trenewed_at_s\tepoch`; the holder's `peer:<host>` node
/// id maps to `<host>`. Returns `None` when the lease is empty, malformed, or
/// expired (older than [`LEASE_DURATION_S`] at `now_s`) — then no lighthouse is
/// the leader and all use the lenient health check. Pure + testable; the file
/// read lives at the call site so this stays unit-testable.
#[must_use]
pub fn master_from_lease(lease_text: &str, now_s: u64) -> Option<String> {
    let line = lease_text.lines().next()?.trim();
    let mut parts = line.split('\t');
    let node_id = parts.next()?;
    let renewed_at_s: u64 = parts.next()?.parse().ok()?;
    if node_id.is_empty() || now_s.saturating_sub(renewed_at_s) >= LEASE_DURATION_S {
        return None;
    }
    Some(node_id.strip_prefix("peer:").unwrap_or(node_id).to_string())
}

/// The 8-position discrete beam, read as a beam of light circling the beacon
/// (Q9/Q10/Q12). Compass arrows ↑↗→↘↓↙←↖. Shared by the Hub footer and the
/// Workbench Lighthouses tab so both animate identically.
pub const BEAM_ARROWS: [&str; 8] = [
    "\u{2191}", "\u{2197}", "\u{2192}", "\u{2198}", "\u{2193}", "\u{2199}", "\u{2190}", "\u{2196}",
];

/// Healthy beacons advance one beam step per this many ticks (slow sweep);
/// unhealthy beacons advance every tick and strobe (Q11).
pub const BEAM_HEALTHY_DIVISOR: u16 = 4;

/// The current beam glyph for a beacon at animation phase `beam_step` (Q10/Q11/
/// Q12). Pure + testable. Healthy beacons sweep slowly through the 8 discrete
/// positions; unhealthy beacons spin a step every tick AND strobe (blank on the
/// even phase) for an at-a-glance alarm.
#[must_use]
pub const fn beam_frame(healthy: bool, beam_step: u16) -> &'static str {
    let n = BEAM_ARROWS.len() as u16;
    if healthy {
        BEAM_ARROWS[((beam_step / BEAM_HEALTHY_DIVISOR) % n) as usize]
    } else if beam_step % 2 == 0 {
        " " // strobe off-phase
    } else {
        BEAM_ARROWS[(beam_step % n) as usize]
    }
}

/// The binary beacon state, with the reason it is red (for the status word).
/// All non-`Healthy` variants render red (Q15); the variant only chooses the
/// label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeaconStatus {
    /// Online, overlay up, and (if master) core services healthy → green.
    Healthy,
    /// No directory row seen yet — folds to red until the first snapshot (Q15).
    NoData,
    /// Heartbeat stale or the row reports `unreachable` → red.
    Offline,
    /// Present but a core service is down: overlay missing, or the elected
    /// leader's core services are unhealthy (Q3) → red.
    ServiceDown,
}

impl BeaconStatus {
    /// Whether this state is the single healthy (green) state.
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }

    /// A short status word for the card detail line (Q16).
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Healthy => "Healthy",
            Self::NoData => "No data",
            Self::Offline => "Offline",
            Self::ServiceDown => "Service down",
        }
    }
}

/// A single lighthouse's beacon: identity + binary health, ready for the Hub
/// footer / tab / applet to render (name + overlay IP + status word, Q16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    /// The lighthouse hostname (the directory row key).
    pub hostname: String,
    /// Its Nebula overlay IP, if it has recorded one.
    pub overlay_ip: Option<String>,
    /// Whether this lighthouse is the elected leader vs a follower
    /// (Q22). Caller-supplied — see [`beacon_for`].
    pub is_master: bool,
    /// The binary health state.
    pub status: BeaconStatus,
}

impl Beacon {
    /// Green iff [`BeaconStatus::Healthy`].
    #[must_use]
    pub const fn healthy(&self) -> bool {
        self.status.is_healthy()
    }
}

/// Whether a directory row is a lighthouse (design Q1). Tolerant of the
/// pre-role writers that left `role` unset — those are treated as non-
/// lighthouse peers.
#[must_use]
pub fn is_lighthouse(peer: &PeerRecord) -> bool {
    peer.role.as_deref() == Some(LIGHTHOUSE_ROLE)
}

/// The lighthouse subset of a peer directory, sorted by hostname for a stable
/// render order (the Hub footer strip + the tab list).
#[must_use]
pub fn lighthouse_records(peers: &[PeerRecord]) -> Vec<PeerRecord> {
    let mut out: Vec<PeerRecord> = peers.iter().filter(|p| is_lighthouse(p)).cloned().collect();
    out.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    out
}

/// Whether a directory row is a supported media lighthouse. Always `false`:
/// the media/file-sharing lighthouse subclass is retired. The `media` field is
/// retained only for old replicated JSON compatibility.
#[must_use]
pub fn is_media_lighthouse(peer: &PeerRecord) -> bool {
    let _ = peer;
    false
}

/// Retired media-lighthouse query. It always returns an empty set so stale
/// media markers cannot revive service discovery or provisioning.
#[must_use]
pub fn media_lighthouse_records(peers: &[PeerRecord]) -> Vec<PeerRecord> {
    let mut out: Vec<PeerRecord> = peers
        .iter()
        .filter(|p| is_media_lighthouse(p))
        .cloned()
        .collect();
    out.sort_by(|a, b| a.hostname.cmp(&b.hostname));
    out
}

/// HA-4 — the minimum lighthouse count for a highly-available mesh. Below this
/// there is no failover headroom: a single lighthouse is a SPOF for relay/
/// discovery, the etcd quorum (SUBSTRATE-V2), and Mesh-Sync redundancy (§8 — up
/// to 3 lighthouses give exactly those). Two is the floor; the supported
/// envelope tops out at 3.
pub const HA_MIN_LIGHTHOUSES: usize = 2;

/// HA-4 — `true` when the mesh has too few lighthouses for HA (`< `[`HA_MIN_LIGHTHOUSES`]).
///
/// Drives the non-blocking `degraded: no HA` health flag + the founding warning;
/// it is a posture signal, never a hard gate (a 1-lighthouse mesh still works).
#[must_use]
pub const fn ha_degraded(lighthouse_count: usize) -> bool {
    lighthouse_count < HA_MIN_LIGHTHOUSES
}

/// HA-4 — the operator-facing reason string when HA is degraded, else `None`.
#[must_use]
pub fn ha_degraded_reason(lighthouse_count: usize) -> Option<&'static str> {
    ha_degraded(lighthouse_count).then_some("no HA: needs a 2nd lighthouse")
}

/// One lighthouse's dialable coordinates for the enroll roster (LIGHTHOUSE-10):
/// the bundle a joining node receives lists ALL of these so it can reach the
/// whole lighthouse set — losing any one leaves the rest reachable (the point of
/// "fully redundant"). Maps 1:1 to the `ca::bundle::LighthouseEntry` the daemon
/// emits; kept here (pure, in mesh-types) so the build is unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LighthouseAddr {
    /// The lighthouse's directory id (hostname) — informational in the bundle.
    pub node_id: String,
    /// Its Nebula overlay IP (the `static_host_map` key + a `lighthouse.hosts` entry).
    pub overlay_ip: String,
    /// Its PUBLIC `ip:port` underlay address (the `static_host_map` value peers dial).
    pub external_addr: String,
}

/// Build the enroll roster from the replicated peer directory (LIGHTHOUSE-10): every
/// `role=="lighthouse"` record that carries BOTH an `overlay_ip` and an
/// `external_addr` becomes a roster entry, so a joining node learns the FULL
/// lighthouse set rather than only the one it enrolled through. A lighthouse
/// missing either address is skipped — never advertised without a dialable
/// address (a half-known lighthouse would poison `static_host_map`). Sorted by
/// hostname for a stable bundle; deduped by hostname (the directory is
/// one-row-per-host, but a caller may prepend a self entry).
#[must_use]
pub fn roster_from_directory(peers: &[PeerRecord]) -> Vec<LighthouseAddr> {
    let mut out: Vec<LighthouseAddr> = Vec::new();
    for p in peers.iter().filter(|p| is_lighthouse(p)) {
        let (Some(overlay_ip), Some(external_addr)) = (&p.overlay_ip, &p.external_addr) else {
            continue;
        };
        if overlay_ip.is_empty() || external_addr.is_empty() {
            continue;
        }
        if out.iter().any(|e| e.node_id == p.hostname) {
            continue;
        }
        out.push(LighthouseAddr {
            node_id: p.hostname.clone(),
            overlay_ip: overlay_ip.clone(),
            external_addr: external_addr.clone(),
        });
    }
    out.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    out
}

/// LIGHTHOUSE-10 / HA — the **signer** roster: [`roster_from_directory`] plus a
/// guaranteed self entry. A founding lighthouse — or any lighthouse before its own
/// heartbeat has replicated into the directory — must still hand a joining peer at
/// least itself, so when no directory row already carries `self_external` we append
/// a self entry. Deduped by `external_addr` (the directory is one-row-per-host, but
/// self may already be present). `self_overlay` is the caller's LIVE overlay IP
/// (e.g. from `own_nebula_ip()`), never a hardcoded literal, so a geo lighthouse on
/// `.4`/`.5` self-advertises its real address. Used by every bundle-signing path
/// (the `/enroll` listener, the CSR auto-signer, the `ca sign-csr` CLI) so the
/// self-inclusion logic can't drift between them.
#[must_use]
pub fn roster_with_self(
    peers: &[PeerRecord],
    self_node_id: &str,
    self_overlay: &str,
    self_external: &str,
) -> Vec<LighthouseAddr> {
    let mut entries = roster_from_directory(peers);
    if !entries.iter().any(|e| e.external_addr == self_external) {
        entries.push(LighthouseAddr {
            node_id: self_node_id.to_string(),
            overlay_ip: self_overlay.to_string(),
            external_addr: self_external.to_string(),
        });
    }
    entries
}

/// The pure binary-health classifier (Q3/Q15). Green requires data AND
/// presence AND overlay AND — only when this is the master — a healthy master
/// service. The first failing condition (checked in escalation order) picks the
/// red variant.
#[must_use]
pub const fn classify(
    has_data: bool,
    online: bool,
    overlay_up: bool,
    is_master: bool,
    master_service_up: bool,
) -> BeaconStatus {
    if !has_data {
        BeaconStatus::NoData
    } else if !online {
        BeaconStatus::Offline
    } else if !overlay_up || (is_master && !master_service_up) {
        BeaconStatus::ServiceDown
    } else {
        BeaconStatus::Healthy
    }
}

/// Derive a [`Beacon`] from a replicated directory row + the caller's
/// "is this the master?" fact, evaluated against `now_ms` (passed explicitly so
/// the derivation is deterministic + testable).
///
/// The booleans handed to [`classify`] are read off the row:
/// - **`has_data`** — the row carries a real timestamp (`last_seen_ms > 0`).
/// - **online** — fresh within `stale_ms` AND not self-reported `unreachable`.
/// - **`overlay_up`** — the node recorded its own overlay IP this heartbeat.
/// - **`master_service_up`** — for the leader, the Netdata-derived health tier is
///   `healthy` (a degraded leader trips an alarm → degraded/critical/etc.).
#[must_use]
pub fn beacon_for(peer: &PeerRecord, is_master: bool, now_ms: u64, stale_ms: u64) -> Beacon {
    let has_data = peer.last_seen_ms > 0;
    let age = now_ms.saturating_sub(peer.last_seen_ms);
    let online = has_data && age <= stale_ms && peer.health != "unreachable";
    let overlay_up = peer.overlay_ip.is_some();
    let master_service_up = peer.health == "healthy";
    let status = classify(has_data, online, overlay_up, is_master, master_service_up);
    Beacon {
        hostname: peer.hostname.clone(),
        overlay_ip: peer.overlay_ip.clone(),
        is_master,
        status,
    }
}

/// Build the beacon list for a peer directory in one call: discover the
/// lighthouses, then derive each one's beacon. `master_hostname` names the
/// current elected leader if known — that lighthouse is flagged `is_master` and
/// held to the stricter service check (Q22).
#[must_use]
pub fn beacons(
    peers: &[PeerRecord],
    master_hostname: Option<&str>,
    now_ms: u64,
    stale_ms: u64,
) -> Vec<Beacon> {
    lighthouse_records(peers)
        .iter()
        .map(|p| {
            let is_master = master_hostname == Some(p.hostname.as_str());
            beacon_for(p, is_master, now_ms, stale_ms)
        })
        .collect()
}

/// `(healthy, total)` lighthouse counts for the Hub header `N/M` (Q8).
#[must_use]
pub fn health_counts(beacons: &[Beacon]) -> (usize, usize) {
    let healthy = beacons.iter().filter(|b| b.healthy()).count();
    (healthy, beacons.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lh(host: &str, health: &str, overlay: Option<&str>, last_seen_ms: u64) -> PeerRecord {
        let mut p = PeerRecord::now(host, Some("v10".into()), health);
        p.last_seen_ms = last_seen_ms;
        p.overlay_ip = overlay.map(str::to_string);
        p.role = Some(LIGHTHOUSE_ROLE.to_string());
        p
    }

    fn lh_full(host: &str, overlay: &str, external: &str) -> PeerRecord {
        let mut p = lh(host, "healthy", Some(overlay), 1_000_000);
        p.external_addr = Some(external.to_string());
        p
    }

    #[test]
    fn roster_with_self_appends_self_when_absent_from_directory() {
        // A founding LH whose own heartbeat hasn't replicated yet: the directory
        // has two OTHER lighthouses, and self must still be handed out.
        let peers = vec![
            lh_full("lh-02", "10.42.0.2", "203.0.113.2:4242"),
            lh_full("lh-03", "10.42.0.3", "203.0.113.3:4242"),
        ];
        let roster = roster_with_self(&peers, "self-lh", "10.42.0.1", "203.0.113.1:4242");
        assert_eq!(roster.len(), 3, "the two directory LHs + self");
        let me = roster
            .iter()
            .find(|e| e.external_addr == "203.0.113.1:4242")
            .expect("self present");
        assert_eq!(me.node_id, "self-lh");
        assert_eq!(me.overlay_ip, "10.42.0.1");
    }

    #[test]
    fn roster_with_self_dedups_self_when_already_in_directory() {
        // Self's own directory row is already present (heartbeat replicated) — it
        // must NOT be double-listed.
        let peers = vec![
            lh_full("self-lh", "10.42.0.1", "203.0.113.1:4242"),
            lh_full("lh-02", "10.42.0.2", "203.0.113.2:4242"),
        ];
        let roster = roster_with_self(&peers, "self-lh", "10.42.0.1", "203.0.113.1:4242");
        assert_eq!(roster.len(), 2, "self deduped by external_addr");
        assert_eq!(
            roster
                .iter()
                .filter(|e| e.external_addr == "203.0.113.1:4242")
                .count(),
            1
        );
    }

    #[test]
    fn roster_with_self_propagates_the_provided_overlay_not_a_literal() {
        // A geo lighthouse on .5 must self-advertise its REAL overlay, never the
        // conventional 10.42.0.1 founder literal.
        let roster = roster_with_self(&[], "geo-lh", "10.42.0.5", "198.51.100.5:4242");
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].overlay_ip, "10.42.0.5");
        assert_eq!(roster[0].external_addr, "198.51.100.5:4242");
    }

    #[test]
    fn roster_from_directory_collects_every_addressable_lighthouse() {
        // Three lighthouses + a server + a lighthouse missing its external addr.
        let mut half = lh("lh-half", "healthy", Some("10.42.0.9"), 1_000_000); // no external_addr
        half.external_addr = None;
        let mut srv = PeerRecord::now("server-1", None, "healthy");
        srv.role = Some("server".into());
        srv.overlay_ip = Some("10.42.0.50".into());
        srv.external_addr = Some("198.51.100.1:4242".into()); // a server, not a LH
        let peers = vec![
            lh_full("lh-03", "10.42.0.3", "203.0.113.3:4242"),
            lh_full("lh-01", "10.42.0.1", "203.0.113.1:4242"),
            lh_full("lh-02", "10.42.0.2", "203.0.113.2:4242"),
            half,
            srv,
        ];
        let roster = roster_from_directory(&peers);
        // Only the three fully-addressed lighthouses, sorted by hostname.
        assert_eq!(roster.len(), 3, "half-known LH + the server are excluded");
        assert_eq!(
            roster
                .iter()
                .map(|e| e.node_id.as_str())
                .collect::<Vec<_>>(),
            ["lh-01", "lh-02", "lh-03"]
        );
        assert_eq!(roster[0].overlay_ip, "10.42.0.1");
        assert_eq!(roster[2].external_addr, "203.0.113.3:4242");
    }

    #[test]
    fn roster_skips_lighthouse_with_blank_addresses() {
        let mut blank = lh_full("lh-blank", "", "");
        blank.overlay_ip = Some(String::new());
        blank.external_addr = Some(String::new());
        assert!(roster_from_directory(&[blank]).is_empty());
    }

    #[test]
    fn is_lighthouse_reads_the_role_field_tolerating_unset() {
        let mut p = PeerRecord::now("a", None, "healthy");
        assert!(!is_lighthouse(&p), "unset role is not a lighthouse");
        p.role = Some("server".into());
        assert!(!is_lighthouse(&p));
        p.role = Some("lighthouse".into());
        assert!(is_lighthouse(&p));
    }

    #[test]
    fn retired_media_marker_never_identifies_a_lighthouse() {
        let now = 1_000_000;
        // A plain lighthouse: a lighthouse, but NOT media.
        let mut plain = lh("lh-plain", "healthy", Some("10.42.0.1"), now);
        assert!(is_lighthouse(&plain));
        assert!(
            !is_media_lighthouse(&plain),
            "no media tag → not the subclass"
        );
        // A stale media tag is ignored by the retired-class predicate.
        plain.media = true;
        assert!(is_lighthouse(&plain));
        assert!(!is_media_lighthouse(&plain));
        // The media tag on a non-lighthouse is meaningless: a server flagged
        // media is NOT a media-lighthouse (the role gate dominates).
        let mut srv = PeerRecord::now("srv", None, "healthy");
        srv.role = Some("server".into());
        srv.media = true;
        assert!(
            !is_media_lighthouse(&srv),
            "media tag alone ≠ media-lighthouse"
        );
    }

    #[test]
    fn media_lighthouse_records_is_always_empty() {
        let now = 1_000_000;
        let mut media_a = lh("media-a", "healthy", Some("10.42.0.1"), now);
        media_a.media = true;
        let mut media_b = lh("media-b", "healthy", Some("10.42.0.2"), now);
        media_b.media = true;
        let plain_lh = lh("plain-lh", "healthy", Some("10.42.0.3"), now);
        let mut srv = PeerRecord::now("srv", None, "healthy");
        srv.role = Some("server".into());
        srv.media = true; // a mis-tagged server — excluded
        let peers = vec![media_b, plain_lh, media_a, srv];
        let media = media_lighthouse_records(&peers);
        assert!(
            media.is_empty(),
            "retired media lighthouses are never supported"
        );
        // It is a subset of the lighthouse set.
        assert_eq!(lighthouse_records(&peers).len(), 3);
        assert_eq!(LIGHTHOUSE_MEDIA_CLASS, "lighthouse_media");
    }

    #[test]
    fn lighthouse_records_filters_and_sorts() {
        let now = 1_000_000;
        let peers = vec![
            lh("zeta", "healthy", Some("10.42.0.2"), now),
            {
                let mut s = PeerRecord::now("server-1", None, "healthy");
                s.role = Some("server".into());
                s
            },
            lh("alpha", "healthy", Some("10.42.0.1"), now),
        ];
        let lhs = lighthouse_records(&peers);
        assert_eq!(
            lhs.iter().map(|p| p.hostname.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"],
            "only lighthouses, sorted by hostname"
        );
    }

    #[test]
    fn ha_degraded_below_the_two_lighthouse_floor() {
        // HA-4: 0 or 1 lighthouse = degraded (no failover headroom); 2+ = HA OK.
        assert!(ha_degraded(0));
        assert!(ha_degraded(1));
        assert!(!ha_degraded(2));
        assert!(!ha_degraded(3));
        assert_eq!(HA_MIN_LIGHTHOUSES, 2);
        // The reason string is present iff degraded.
        assert_eq!(ha_degraded_reason(1), Some("no HA: needs a 2nd lighthouse"));
        assert_eq!(ha_degraded_reason(2), None);
    }

    #[test]
    fn ha_degraded_counts_real_lighthouse_records() {
        // End-to-end with the directory counter: one lighthouse = degraded.
        let now = 1_000_000;
        let one = vec![lh("alpha", "healthy", Some("10.42.0.1"), now)];
        assert!(ha_degraded(lighthouse_records(&one).len()));
        let two = vec![
            lh("alpha", "healthy", Some("10.42.0.1"), now),
            lh("beta", "healthy", Some("10.42.0.2"), now),
        ];
        assert!(!ha_degraded(lighthouse_records(&two).len()));
    }

    #[test]
    fn classify_is_binary_over_all_inputs() {
        // No data → NoData regardless of the rest.
        assert_eq!(
            classify(false, true, true, false, true),
            BeaconStatus::NoData
        );
        // Has data but stale/unreachable → Offline.
        assert_eq!(
            classify(true, false, true, true, true),
            BeaconStatus::Offline
        );
        // Online but overlay down → ServiceDown.
        assert_eq!(
            classify(true, true, false, false, true),
            BeaconStatus::ServiceDown
        );
        // Master online + overlay up but master service down → ServiceDown.
        assert_eq!(
            classify(true, true, true, true, false),
            BeaconStatus::ServiceDown
        );
        // Shadow online + overlay up, master service irrelevant → Healthy.
        assert_eq!(
            classify(true, true, true, false, false),
            BeaconStatus::Healthy
        );
        // Master fully up → Healthy.
        assert_eq!(
            classify(true, true, true, true, true),
            BeaconStatus::Healthy
        );
    }

    #[test]
    fn beacon_healthy_shadow() {
        let now = 1_000_000;
        let p = lh("shadow", "healthy", Some("10.42.0.2"), now - 10_000);
        let b = beacon_for(&p, false, now, DEFAULT_STALE_MS);
        assert!(b.healthy());
        assert_eq!(b.status.word(), "Healthy");
        assert!(!b.is_master);
    }

    #[test]
    fn beacon_offline_when_stale() {
        let now = 1_000_000;
        // Last seen well beyond the stale window.
        let p = lh("master", "healthy", Some("10.42.0.1"), now - 200_000);
        let b = beacon_for(&p, true, now, DEFAULT_STALE_MS);
        assert!(!b.healthy());
        assert_eq!(b.status, BeaconStatus::Offline);
    }

    #[test]
    fn beacon_service_down_when_overlay_missing() {
        let now = 1_000_000;
        let p = lh("shadow", "healthy", None, now);
        let b = beacon_for(&p, false, now, DEFAULT_STALE_MS);
        assert_eq!(b.status, BeaconStatus::ServiceDown);
    }

    #[test]
    fn master_spof_red_when_core_service_degraded() {
        let now = 1_000_000;
        // Online, overlay up, but the leader's health tier is degraded (Q3).
        let p = lh("master", "degraded", Some("10.42.0.1"), now);
        let master = beacon_for(&p, true, now, DEFAULT_STALE_MS);
        assert_eq!(
            master.status,
            BeaconStatus::ServiceDown,
            "master held strict"
        );
        // The very same row, were it merely a shadow, would be green (the
        // strict master-service check only applies to the master).
        let shadow = beacon_for(&p, false, now, DEFAULT_STALE_MS);
        assert!(
            shadow.healthy(),
            "shadow not bound by the master service check"
        );
    }

    #[test]
    fn no_data_folds_to_red() {
        let now = 1_000_000;
        let mut p = lh("fresh", "unknown", None, 0);
        p.health = "unknown".into();
        let b = beacon_for(&p, false, now, DEFAULT_STALE_MS);
        assert!(!b.healthy());
        assert_eq!(b.status, BeaconStatus::NoData);
    }

    #[test]
    fn master_from_lease_parses_holder_and_honors_expiry() {
        let now = 1_000_000u64;
        // Fresh lease → holder with the peer: prefix stripped.
        let lease = format!("peer:lighthouse-01\t{}\t3\n", now - 10);
        assert_eq!(
            master_from_lease(&lease, now),
            Some("lighthouse-01".to_string())
        );
        // Expired lease (older than LEASE_DURATION_S) → no master.
        let stale = format!("peer:lighthouse-01\t{}\t3\n", now - LEASE_DURATION_S - 5);
        assert_eq!(master_from_lease(&stale, now), None);
        // Malformed / empty → None.
        assert_eq!(master_from_lease("", now), None);
        assert_eq!(master_from_lease("garbage-only\n", now), None);
    }

    #[test]
    fn beam_frame_sweeps_slow_when_healthy_and_strobes_when_not() {
        // Healthy: a position is held for BEAM_HEALTHY_DIVISOR ticks (slow), and
        // it never blanks.
        assert_eq!(beam_frame(true, 0), BEAM_ARROWS[0]);
        assert_eq!(beam_frame(true, BEAM_HEALTHY_DIVISOR - 1), BEAM_ARROWS[0]);
        assert_eq!(beam_frame(true, BEAM_HEALTHY_DIVISOR), BEAM_ARROWS[1]);
        for step in 0..64u16 {
            assert_ne!(beam_frame(true, step), " ", "healthy never strobes off");
        }
        // Unhealthy: strobes off on the even phase, on (rotating fast) on the odd.
        assert_eq!(beam_frame(false, 0), " ");
        assert_eq!(beam_frame(false, 1), BEAM_ARROWS[1]);
        assert_eq!(beam_frame(false, 2), " ");
        assert_eq!(beam_frame(false, 3), BEAM_ARROWS[3]);
    }

    #[test]
    fn beacons_and_counts() {
        let now = 1_000_000;
        let peers = vec![
            lh("master", "healthy", Some("10.42.0.1"), now),
            lh("shadow", "healthy", Some("10.42.0.2"), now - 200_000), // stale
            {
                let mut s = PeerRecord::now("ws-1", None, "healthy");
                s.role = Some("workstation".into());
                s
            },
        ];
        let bs = beacons(&peers, Some("master"), now, DEFAULT_STALE_MS);
        assert_eq!(bs.len(), 2, "only the two lighthouses");
        assert!(bs[0].is_master, "master flagged");
        assert_eq!(health_counts(&bs), (1, 2), "master healthy, shadow stale");
    }
}
