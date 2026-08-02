//! KDC2-5.1+5.2 — peer kind + KDC connect facts.
//!
//! Shared types consumed by `mde-peer-card`, the GUI shell,
//! and applet crates so all surfaces render the same view of a
//! KDC-paired peer. Pure data — no I/O, no D-Bus, no protocol
//! knowledge.

use serde::{Deserialize, Serialize};

/// Coarse classification of a mesh peer. Drives conditional UI
/// (phone-only sections in `mde-peer-card`, dock-icon glyph,
/// device-row sorting in the GUI shell).
///
/// Token table (`#[serde(rename_all = "snake_case")]`) — stays
/// stable across releases since it's persisted in devices.toml
/// + emitted on the D-Bus surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeerKind {
    /// Linux desktop / laptop running MDE.
    Desktop,
    /// Always-on server peer (no display, no operator at console).
    Server,
    /// Headless embedded device (Pi, NAS, `IoT`).
    Embedded,
    /// Android handset paired via KDC.
    Phone,
    /// Tablet (Android, iOS) paired via KDC.
    Tablet,
    /// Anything else — fallback when classification fails.
    Unknown,
}

impl PeerKind {
    /// Every variant in stable order — used by tests + the
    /// future operator-facing "filter by kind" dropdown.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::Desktop,
            Self::Server,
            Self::Embedded,
            Self::Phone,
            Self::Tablet,
            Self::Unknown,
        ]
    }

    /// True when this peer-kind is a hand-held device.
    /// `mde-peer-card` uses this gate to show the phone-only
    /// sections (battery / ring / find).
    #[must_use]
    pub const fn is_handheld(self) -> bool {
        matches!(self, Self::Phone | Self::Tablet)
    }

    /// Stable display token. Matches the serde rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Server => "server",
            Self::Embedded => "embedded",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for PeerKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Pairing state of a peer w.r.t. KDC. Drives the
/// `mde-peer-card` action buttons (Pair / Unpair / Re-pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    /// Never paired.
    Unpaired,
    /// Pairing handshake in progress.
    Pairing,
    /// Paired + reachable.
    Paired,
    /// Was paired but the public-key handshake fails now —
    /// device rotated its key. Operator needs to re-pair.
    KeyMismatch,
}

impl PairingState {
    /// Stable audit-log token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unpaired => "unpaired",
            Self::Pairing => "pairing",
            Self::Paired => "paired",
            Self::KeyMismatch => "key_mismatch",
        }
    }
}

/// Battery state mirrored from a phone or laptop peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatterySnapshot {
    /// Charge percentage (0..=100). Negative values from
    /// upstream get sanitized to `None` in the constructor.
    pub charge_pct: Option<u8>,
    /// True when the device is plugged in / actively charging.
    pub is_charging: bool,
    /// Threshold event token (`"low"`, `"critical"`, or empty
    /// for no threshold).
    #[serde(default)]
    pub threshold_event: String,
    /// Unix epoch seconds of the most recent battery report.
    pub reported_at: i64,
}

impl BatterySnapshot {
    /// Construct from raw upstream charge (signed because
    /// upstream KDE Connect sometimes emits -1 for "unknown").
    /// Negative or > 100 maps to `None`.
    #[must_use]
    pub fn from_raw(
        charge: i32,
        is_charging: bool,
        threshold_event: String,
        reported_at: i64,
    ) -> Self {
        let charge_pct = if (0..=100).contains(&charge) {
            Some(charge as u8)
        } else {
            None
        };
        Self {
            charge_pct,
            is_charging,
            threshold_event,
            reported_at,
        }
    }
}

/// KDC-specific facts about a paired peer. Populated by the
/// daemon-API layer (`mde-kdc`'s D-Bus host); consumed by
/// `mde-peer-card`'s conditional sections + the workbench's
/// device-row badges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectFacts {
    /// Kind of device. Drives the conditional phone-only
    /// section rendering.
    pub kind: PeerKind,
    /// Pairing state.
    pub pairing: PairingState,
    /// Most-recent battery snapshot, if any.
    #[serde(default)]
    pub battery: Option<BatterySnapshot>,
    /// Plugin tokens the peer's KDC announce advertised under
    /// `incomingCapabilities`.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Unix epoch seconds of the most recent KDC packet from
    /// this peer. 0 = never.
    #[serde(default)]
    pub last_seen_at: i64,
}

impl ConnectFacts {
    /// True when the peer is currently paired AND reachable.
    /// Used by the workbench to render the device-row green
    /// vs. grey.
    #[must_use]
    pub const fn is_online(&self, now_epoch_s: i64) -> bool {
        matches!(self.pairing, PairingState::Paired) && {
            // "Recent" = within the last 90s (matches the KDC
            // identity-broadcast cadence — every peer
            // re-announces ~60s).
            now_epoch_s.saturating_sub(self.last_seen_at) <= 90
        }
    }

    /// True when this peer should surface the phone-only UI
    /// sections (battery / ring / find / SMS / share).
    #[must_use]
    pub const fn shows_phone_sections(&self) -> bool {
        self.kind.is_handheld()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_kind_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&PeerKind::Desktop).unwrap(),
            r#""desktop""#
        );
        assert_eq!(
            serde_json::to_string(&PeerKind::Phone).unwrap(),
            r#""phone""#
        );
        assert_eq!(
            serde_json::to_string(&PeerKind::Tablet).unwrap(),
            r#""tablet""#
        );
        assert_eq!(
            serde_json::to_string(&PeerKind::Unknown).unwrap(),
            r#""unknown""#
        );
    }

    #[test]
    fn peer_kind_display_matches_serde_token() {
        for k in PeerKind::all() {
            let display = format!("{k}");
            let serde_token = serde_json::to_string(&k)
                .unwrap()
                .trim_matches('"')
                .to_string();
            assert_eq!(display, serde_token, "Display drift for {k:?}");
        }
    }

    #[test]
    fn is_handheld_covers_phone_and_tablet_only() {
        assert!(PeerKind::Phone.is_handheld());
        assert!(PeerKind::Tablet.is_handheld());
        assert!(!PeerKind::Desktop.is_handheld());
        assert!(!PeerKind::Server.is_handheld());
        assert!(!PeerKind::Embedded.is_handheld());
        assert!(!PeerKind::Unknown.is_handheld());
    }

    #[test]
    fn peer_kind_all_returns_six_distinct_variants() {
        let v = PeerKind::all();
        assert_eq!(v.len(), 6);
        let mut tokens: Vec<&str> = v.iter().map(|k| k.as_str()).collect();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), 6);
    }

    #[test]
    fn pairing_state_round_trips_through_json() {
        for s in [
            PairingState::Unpaired,
            PairingState::Pairing,
            PairingState::Paired,
            PairingState::KeyMismatch,
        ] {
            let raw = serde_json::to_string(&s).unwrap();
            let back: PairingState = serde_json::from_str(&raw).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn battery_snapshot_from_raw_sanitizes_negative_to_none() {
        let s = BatterySnapshot::from_raw(-1, false, String::new(), 0);
        assert_eq!(s.charge_pct, None);
    }

    #[test]
    fn battery_snapshot_from_raw_accepts_valid_range() {
        let s = BatterySnapshot::from_raw(73, true, "low".into(), 1_700_000_000);
        assert_eq!(s.charge_pct, Some(73));
        assert!(s.is_charging);
        assert_eq!(s.threshold_event, "low");
    }

    #[test]
    fn battery_snapshot_from_raw_rejects_over_100() {
        let s = BatterySnapshot::from_raw(150, false, String::new(), 0);
        assert_eq!(s.charge_pct, None);
    }

    #[test]
    fn connect_facts_is_online_requires_paired_and_recent() {
        let now = 1_700_000_000;
        let facts = ConnectFacts {
            kind: PeerKind::Phone,
            pairing: PairingState::Paired,
            battery: None,
            capabilities: vec![],
            last_seen_at: now,
        };
        assert!(facts.is_online(now));
        // 30s ago — still online.
        assert!(facts.is_online(now + 30));
        // 90s ago — still online (boundary inclusive).
        assert!(facts.is_online(now + 90));
        // 91s ago — offline.
        assert!(!facts.is_online(now + 91));
    }

    #[test]
    fn connect_facts_not_online_when_unpaired() {
        let now = 1_700_000_000;
        let facts = ConnectFacts {
            kind: PeerKind::Phone,
            pairing: PairingState::Unpaired,
            battery: None,
            capabilities: vec![],
            last_seen_at: now,
        };
        // Even with a fresh last_seen, unpaired = not online.
        assert!(!facts.is_online(now));
    }

    #[test]
    fn shows_phone_sections_only_for_handheld_kinds() {
        for kind in PeerKind::all() {
            let facts = ConnectFacts {
                kind,
                pairing: PairingState::Paired,
                battery: None,
                capabilities: vec![],
                last_seen_at: 0,
            };
            assert_eq!(facts.shows_phone_sections(), kind.is_handheld());
        }
    }

    #[test]
    fn connect_facts_round_trips_through_json() {
        let facts = ConnectFacts {
            kind: PeerKind::Phone,
            pairing: PairingState::Paired,
            battery: Some(BatterySnapshot::from_raw(
                85,
                true,
                String::new(),
                1_700_000_000,
            )),
            capabilities: vec!["kdeconnect.clipboard".into(), "kdeconnect.battery".into()],
            last_seen_at: 1_700_000_500,
        };
        let raw = serde_json::to_string(&facts).unwrap();
        let back: ConnectFacts = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, facts);
    }
}
