//! Typed wire contract for the keyless adsb.lol aircraft overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node aircraft snapshots.
pub const AIRCRAFT_STATE_PREFIX: &str = "state/overlay/adsb-aircraft/";
/// Release-audit tag for ODbL data requiring attribution.
pub const LICENSE_TIER: &str = "open-data-attribution";
/// Attribution shown whenever the aircraft layer is enabled.
pub const ATTRIBUTION: &str = "adsb.lol · ODbL";

/// Latest-wins aircraft topic for one workstation adapter.
#[must_use]
pub fn aircraft_state_topic(node: &str) -> String {
    format!("{AIRCRAFT_STATE_PREFIX}{node}")
}

/// Trusted position source retained by the driver-relevant layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AircraftPositionSource {
    /// Direct 1090 MHz ADS-B position.
    Adsb,
    /// Direct 978 MHz UAT position.
    Uat,
    /// ADS-R rebroadcast of an ADS-B/UAT position.
    Adsr,
}

/// One normalized, position-qualified low-altitude aircraft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AircraftTrack {
    /// ICAO/non-ICAO transponder identifier.
    pub id: String,
    /// Trimmed callsign, when broadcast and syntactically safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsign: Option<String>,
    /// Last qualified position observation time, Unix milliseconds.
    pub observed_at_ms: i64,
    /// Latitude in decimal degrees.
    pub latitude: f64,
    /// Longitude in decimal degrees.
    pub longitude: f64,
    /// Reported geometric altitude when present, otherwise barometric altitude.
    pub altitude_msl_ft: f32,
    /// Estimated height above the vehicle's local ground altitude.
    pub estimated_agl_ft: f32,
    /// Ground speed in knots, when finite and within the dead-reckoning guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ground_speed_kt: Option<f32>,
    /// Ground track in degrees true, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_deg: Option<f32>,
    /// Qualified source class.
    pub position_source: AircraftPositionSource,
}

/// Complete point-scoped aircraft set from one successful fetch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AircraftSnapshot {
    /// Adapter node.
    pub host: String,
    /// Successful fetch or conditional validation time, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Server generation time, Unix milliseconds, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_generated_at_ms: Option<i64>,
    /// Vehicle latitude used for the point query.
    pub query_latitude: f64,
    /// Vehicle longitude used for the point query.
    pub query_longitude: f64,
    /// Vehicle GNSS altitude used as the local AGL reference.
    pub query_altitude_msl_ft: f32,
    /// Point-query radius in nautical miles.
    pub radius_nm: f32,
    /// Feed-reported total before driver-relevance filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_total: Option<u32>,
    /// Complete normalized low-altitude set.
    #[serde(default)]
    pub aircraft: Vec<AircraftTrack>,
    /// Ordinary on-ground/high-altitude/out-of-radius records filtered out.
    pub relevance_filtered: u32,
    /// MLAT, TIS-B, coarse, stale, or otherwise unqualified positions filtered.
    pub quality_filtered: u32,
    /// Honest payload/fetch/normalization gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit license tag.
    pub license_tier: String,
    /// Required attribution.
    pub attribution: String,
}

impl AircraftSnapshot {
    /// Empty successful point-query shell.
    #[must_use]
    pub fn empty(
        host: &str,
        fetched_at_ms: i64,
        latitude: f64,
        longitude: f64,
        altitude_msl_ft: f32,
    ) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            feed_generated_at_ms: None,
            query_latitude: latitude,
            query_longitude: longitude,
            query_altitude_msl_ft: altitude_msl_ft,
            radius_nm: 10.0,
            feed_total: None,
            aircraft: Vec::new(),
            relevance_filtered: 0,
            quality_filtered: 0,
            gaps: Vec::new(),
            license_tier: LICENSE_TIER.to_string(),
            attribution: ATTRIBUTION.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_and_odbl_audit_fields_are_stable() {
        assert_eq!(
            aircraft_state_topic("rig-1"),
            "state/overlay/adsb-aircraft/rig-1"
        );
        let snapshot = AircraftSnapshot::empty("rig-1", 10, 32.2, -95.0, 400.0);
        assert_eq!(snapshot.license_tier, "open-data-attribution");
        assert!(snapshot.attribution.contains("ODbL"));
    }

    #[test]
    fn optional_callsign_and_source_round_trip() {
        let mut snapshot = AircraftSnapshot::empty("rig-1", 10, 32.2, -95.0, 400.0);
        snapshot.aircraft.push(AircraftTrack {
            id: "aaacc3".to_string(),
            callsign: None,
            observed_at_ms: 9,
            latitude: 32.21,
            longitude: -95.01,
            altitude_msl_ft: 625.0,
            estimated_agl_ft: 425.0,
            ground_speed_kt: Some(157.9),
            track_deg: Some(206.73),
            position_source: AircraftPositionSource::Adsb,
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: AircraftSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.aircraft[0].callsign, None);
    }
}
