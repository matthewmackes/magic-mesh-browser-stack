//! LIGHTHOUSE-8 — deep-probe result for one lighthouse.
//!
//! The replicated peer directory ([`crate::lighthouse`]) carries a lighthouse's
//! *binary* health (online / overlay-up / master-service ok). It deliberately
//! does NOT carry the live operational facts an operator wants when a beacon
//! goes red: is the Nebula tunnel actually handshaken, what public/underlay
//! endpoint is it reachable at, how many overlay peers does it have, how long
//! has it been up, and how close is the mesh CA cert to expiry.
//!
//! The `lighthouse_probe` worker (in `mackesd`) measures these every ~15 s
//! against EACH lighthouse and publishes one [`LighthouseProbe`] to the mde-bus
//! topic `compute/lighthouse-probe/<name>`. The Workbench Lighthouses tab
//! subscribes to that topic and renders the five fields in each card.
//!
//! **Honest degradation (§7).** Every measurable field is an `Option`: a field
//! the probe could not measure this tick (debug-SSH down, no overlay IP yet,
//! `nebula-cert` absent) is `None`, which the card renders as `—` — never a
//! guessed or stubbed value.

use serde::{Deserialize, Serialize};

/// One deep-probe pass against a single lighthouse, published to
/// `compute/lighthouse-probe/<name>`.
///
/// Each operational field is optional: `None` means "not measurable this tick"
/// (rendered `—`), distinct from a measured zero/false.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LighthouseProbe {
    /// The lighthouse hostname this probe is about (matches the directory row
    /// key + the bus topic suffix).
    pub name: String,

    /// Overlay (Nebula) IP the probe targeted, when the directory carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,

    /// Nebula handshake state with this lighthouse: `Some(true)` when the local
    /// node holds an active tunnel to it (a live hostmap entry), `Some(false)`
    /// when the overlay is reachable but no tunnel is established, `None` when
    /// the state could not be determined (no overlay IP, no debug SSH).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handshake: Option<bool>,

    /// The lighthouse's public / external underlay endpoint (`ip` or `ip:port`)
    /// — the directory's `external_addr`, or the chosen remote endpoint from the
    /// live hostmap when present. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,

    /// Count of overlay peers in the mesh the lighthouse set anchors, derived
    /// from the replicated directory (the size of the overlay membership).
    /// Mesh-wide, not per-lighthouse — the directory does not attribute peers to
    /// a specific lighthouse. `None` when the directory could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_count: Option<u32>,

    /// Lighthouse uptime in seconds, derived from how long its directory row has
    /// been continuously fresh (first-seen → now). `None` when no presence data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,

    /// Days until the mesh CA cert (the shared trust anchor every lighthouse
    /// cert is signed under) expires. Negative when already past `notAfter`.
    /// `None` when `nebula-cert` / the CA cert is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_expiry_days: Option<i64>,

    /// Unix-ms wall-clock when this probe pass ran (the publisher's "now").
    pub probed_at_ms: u64,
}

impl LighthouseProbe {
    /// A fresh probe for `name` with every operational field unmeasured.
    /// The worker fills the fields it can measure this tick and leaves the rest
    /// `None` (honest degradation — the card renders `—`).
    #[must_use]
    pub fn unmeasured(name: impl Into<String>, probed_at_ms: u64) -> Self {
        Self {
            name: name.into(),
            overlay_ip: None,
            handshake: None,
            public_ip: None,
            peer_count: None,
            uptime_s: None,
            cert_expiry_days: None,
            probed_at_ms,
        }
    }

    /// The bus topic a probe for `name` is published to / read from.
    #[must_use]
    pub fn topic(name: &str) -> String {
        format!("compute/lighthouse-probe/{name}")
    }

    /// Human-readable handshake label for the card (`—` when unknown).
    #[must_use]
    pub const fn handshake_word(&self) -> &'static str {
        match self.handshake {
            Some(true) => "established",
            Some(false) => "no tunnel",
            None => "—",
        }
    }

    /// Format uptime as a compact `Nd Nh Nm` / `Nh Nm` / `Nm` string, or `—`
    /// when unknown.
    #[must_use]
    pub fn uptime_human(&self) -> String {
        let Some(total) = self.uptime_s else {
            return "—".to_string();
        };
        let days = total / 86_400;
        let hours = (total % 86_400) / 3_600;
        let mins = (total % 3_600) / 60;
        if days > 0 {
            format!("{days}d {hours}h {mins}m")
        } else if hours > 0 {
            format!("{hours}h {mins}m")
        } else {
            format!("{mins}m")
        }
    }

    /// Format the CA cert-expiry field for the card: `in N days`, `expired N
    /// days ago`, `expires today`, or `—` when unknown.
    #[must_use]
    pub fn cert_expiry_human(&self) -> String {
        match self.cert_expiry_days {
            None => "—".to_string(),
            Some(0) => "expires today".to_string(),
            Some(d) if d > 0 => format!("in {d} days"),
            Some(d) => format!("expired {} days ago", -d),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_targets_the_compute_lane() {
        assert_eq!(
            LighthouseProbe::topic("anvil"),
            "compute/lighthouse-probe/anvil"
        );
    }

    #[test]
    fn unmeasured_leaves_every_field_none() {
        let p = LighthouseProbe::unmeasured("anvil", 1_700_000_000_000);
        assert_eq!(p.name, "anvil");
        assert_eq!(p.probed_at_ms, 1_700_000_000_000);
        assert!(p.handshake.is_none());
        assert!(p.public_ip.is_none());
        assert!(p.peer_count.is_none());
        assert!(p.uptime_s.is_none());
        assert!(p.cert_expiry_days.is_none());
    }

    #[test]
    fn handshake_word_covers_every_state() {
        let mut p = LighthouseProbe::unmeasured("a", 0);
        assert_eq!(p.handshake_word(), "—");
        p.handshake = Some(false);
        assert_eq!(p.handshake_word(), "no tunnel");
        p.handshake = Some(true);
        assert_eq!(p.handshake_word(), "established");
    }

    #[test]
    fn uptime_human_buckets_days_hours_minutes() {
        let mut p = LighthouseProbe::unmeasured("a", 0);
        assert_eq!(p.uptime_human(), "—");
        p.uptime_s = Some(45); // < 1 minute
        assert_eq!(p.uptime_human(), "0m");
        p.uptime_s = Some(5 * 60);
        assert_eq!(p.uptime_human(), "5m");
        p.uptime_s = Some(2 * 3600 + 5 * 60);
        assert_eq!(p.uptime_human(), "2h 5m");
        p.uptime_s = Some(3 * 86_400 + 4 * 3600 + 7 * 60);
        assert_eq!(p.uptime_human(), "3d 4h 7m");
    }

    #[test]
    fn cert_expiry_human_covers_future_today_and_past() {
        let mut p = LighthouseProbe::unmeasured("a", 0);
        assert_eq!(p.cert_expiry_human(), "—");
        p.cert_expiry_days = Some(30);
        assert_eq!(p.cert_expiry_human(), "in 30 days");
        p.cert_expiry_days = Some(0);
        assert_eq!(p.cert_expiry_human(), "expires today");
        p.cert_expiry_days = Some(-3);
        assert_eq!(p.cert_expiry_human(), "expired 3 days ago");
    }

    #[test]
    fn round_trips_through_json_with_partial_fields() {
        let p = LighthouseProbe {
            name: "anvil".into(),
            overlay_ip: Some("10.42.0.5".into()),
            handshake: Some(true),
            public_ip: Some("203.0.113.5:4242".into()),
            peer_count: Some(4),
            uptime_s: Some(7_200),
            cert_expiry_days: Some(180),
            probed_at_ms: 1_700_000_000_000,
        };
        let s = serde_json::to_string(&p).expect("serialize");
        let back: LighthouseProbe = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, p);
    }

    #[test]
    fn unmeasured_fields_are_omitted_from_json() {
        let p = LighthouseProbe::unmeasured("anvil", 42);
        let s = serde_json::to_string(&p).expect("serialize");
        // Only `name` + `probed_at_ms` are unconditional; the Option fields
        // skip when None so a degraded probe is a small document.
        assert!(s.contains("\"name\":\"anvil\""));
        assert!(s.contains("\"probed_at_ms\":42"));
        assert!(!s.contains("handshake"));
        assert!(!s.contains("cert_expiry_days"));
        // And it still round-trips back to the all-None shape.
        let back: LighthouseProbe = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, p);
    }
}
