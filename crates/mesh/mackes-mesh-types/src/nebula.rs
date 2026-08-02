//! NF-11.1 (v2.5) — Nebula facts surface for the peer
//! card.
//!
//! Mirrors the JSON shape `dev.mackes.MDE.Nebula.Status`'s
//! `Status() + ListPeers()` calls return. Defined here so
//! every consumer (mde-peer-card, the mesh panels of the
//! GUI shell, the v2.5 wizard preview page) reads from the
//! same canonical struct rather than re-deriving fields
//! from the raw JSON.
//!
//! Open-mesh directive (2026-05-23): the only role split is
//! `Host` (lighthouse-eligible) vs `Peer`. No per-service
//! ACLs surface here — the cert groups are flat per the
//! NF-2.3 sign module.

use serde::{Deserialize, Serialize};

/// Nebula-specific facts about one peer. Optional on
/// [`mde-peer-card::PeerCardData`] — `None` for peers that
/// haven't been signed under the active CA yet (e.g. a
/// freshly enrolled peer before the supervisor's reconcile
/// tick lands the bundle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NebulaFacts {
    /// Overlay IP this peer was allocated (e.g.
    /// `10.42.0.5`).
    pub overlay_ip: String,
    /// First 8 chars of the peer cert's fingerprint.
    /// Empty when no cert is on file.
    pub fingerprint: String,
    /// Unix-epoch seconds when the cert expires.
    pub cert_expires_at: i64,
    /// CA epoch this cert was signed under. Bumps on every
    /// NF-2.5 rotation; a peer whose epoch < the active
    /// epoch is silently re-signed by the supervisor.
    pub ca_epoch: i64,
    /// `Host` when this peer is lighthouse-eligible; `Peer`
    /// otherwise. Mirrors `mackesd::ca::sign::PeerRole`.
    pub role: NebulaRole,
}

/// Lighthouse-eligibility split. Mirrors the
/// `mackesd::ca::sign::PeerRole` enum but lives here so
/// renderer crates don't take a mackesd-core dep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NebulaRole {
    /// Lighthouse-eligible host.
    Host,
    /// Regular mesh peer.
    Peer,
}

impl NebulaRole {
    /// Display label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Peer => "Peer",
        }
    }
}

impl NebulaFacts {
    /// True when the section UI should render the
    /// lighthouse-pictogram badge next to the role label.
    /// Mirrors `NF-10.4`'s `show_lighthouse_badge`.
    #[must_use]
    pub const fn is_lighthouse(&self) -> bool {
        matches!(self.role, NebulaRole::Host)
    }

    /// Human-readable cert-expiry hint suitable for the
    /// peer-card tooltip. Returns "expired N days ago" for
    /// past dates, "expires in N days" for future dates,
    /// "expires today" within ±1 day.
    #[must_use]
    pub fn cert_expiry_hint(&self, now_unix: i64) -> String {
        let days = (self.cert_expires_at - now_unix) / 86_400;
        if days.abs() <= 1 {
            "expires today".to_string()
        } else if days < 0 {
            format!("expired {} days ago", -days)
        } else {
            format!("expires in {days} days")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> NebulaFacts {
        NebulaFacts {
            overlay_ip: "10.42.0.5".into(),
            fingerprint: "abcd1234".into(),
            cert_expires_at: 1_716_499_200,
            ca_epoch: 0,
            role: NebulaRole::Peer,
        }
    }

    #[test]
    fn role_labels_lock() {
        assert_eq!(NebulaRole::Host.label(), "Host");
        assert_eq!(NebulaRole::Peer.label(), "Peer");
    }

    #[test]
    fn is_lighthouse_true_only_for_host() {
        let host = NebulaFacts {
            role: NebulaRole::Host,
            ..sample()
        };
        assert!(host.is_lighthouse());
        let peer = sample();
        assert!(!peer.is_lighthouse());
    }

    #[test]
    fn cert_expiry_hint_past_present_future() {
        let f = NebulaFacts {
            cert_expires_at: 1_000_000,
            ..sample()
        };
        // Today (±1 day)
        assert_eq!(f.cert_expiry_hint(1_000_000), "expires today");
        // 7 days ago
        let past = f.cert_expiry_hint(1_000_000 + 7 * 86_400);
        assert_eq!(past, "expired 7 days ago");
        // 30 days from now
        let future = f.cert_expiry_hint(1_000_000 - 30 * 86_400);
        assert_eq!(future, "expires in 30 days");
    }

    #[test]
    fn round_trip_through_json() {
        let f = sample();
        let raw = serde_json::to_string(&f).expect("serialize");
        let parsed: NebulaFacts = serde_json::from_str(&raw).expect("parse");
        assert_eq!(parsed, f);
    }

    #[test]
    fn role_serializes_as_snake_case_lowercase() {
        let h = serde_json::to_string(&NebulaRole::Host).unwrap();
        let p = serde_json::to_string(&NebulaRole::Peer).unwrap();
        assert_eq!(h, "\"host\"");
        assert_eq!(p, "\"peer\"");
    }
}
