//! Typed wire contract for the credential-gated US EPA AirNow AQI overlay.
//!
//! An unconfigured adapter publishes an explicit snapshot with no fetch time
//! and no observations. Once an operator seals the free AirNow key, successful
//! observations replace the prior set wholesale; failed refreshes retain the
//! original fetch time so consumers never mistake old AQI for live data.

use serde::{Deserialize, Serialize};

/// Per-node AirNow AQI snapshot topic prefix.
pub const AIR_QUALITY_STATE_PREFIX: &str = "state/overlay/airnow-aqi/";
/// AirNow requires a free government-issued API key.
pub const LICENSE_TIER: &str = "free-key-gov";
/// Attribution and preliminary-data warning shown with the active layer.
pub const ATTRIBUTION: &str = "US EPA AirNow (preliminary)";

/// Retained AirNow AQI topic for one workstation adapter.
#[must_use]
pub fn air_quality_state_topic(node: &str) -> String {
    format!("{AIR_QUALITY_STATE_PREFIX}{node}")
}

/// Whether the credential-gated adapter can contact AirNow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AirNowAvailability {
    /// The operator has not sealed the free deployment key.
    Unconfigured,
    /// A sealed key is present and the adapter is ready or has fetched data.
    Ready,
    /// The sealed-secret backend could not be read or contained invalid data.
    SecretStoreError,
}

/// One normalized hourly AQI observation at an AirNow monitoring site.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AirQualityStation {
    /// Stable AQS site identifier, or a bounded coordinate-derived fallback.
    pub id: String,
    /// Site name when AirNow's verbose response supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Normalized pollutant (`PM2.5` or `OZONE`).
    pub parameter: String,
    /// Producer-supplied AQI, validated to the official 0-500 range.
    pub aqi: u16,
    /// WGS-84 latitude.
    pub latitude: f64,
    /// WGS-84 longitude.
    pub longitude: f64,
    /// Great-circle distance from the vehicle query point.
    pub distance_km: f32,
    /// AirNow observation hour in Unix milliseconds.
    pub observed_at_ms: i64,
}

/// Complete vehicle-centred AirNow AQI state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AirQualitySnapshot {
    /// Adapter node.
    pub host: String,
    /// Time this status/snapshot was published, Unix milliseconds.
    pub published_at_ms: i64,
    /// Last successful AirNow fetch, absent when no keyed request succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_ms: Option<i64>,
    /// Vehicle latitude used for the bounding box, when a valid fix existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_latitude: Option<f64>,
    /// Vehicle longitude used for the bounding box, when a valid fix existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_longitude: Option<f64>,
    /// Relevance radius in kilometres.
    pub query_radius_km: u16,
    /// Credential/backend availability; never implies a successful fetch.
    pub availability: AirNowAvailability,
    /// Latest observation per monitoring-site/pollutant pair.
    #[serde(default)]
    pub stations: Vec<AirQualityStation>,
    /// Source records omitted by validation, de-duplication, or retention caps.
    #[serde(default)]
    pub omitted_records: u32,
    /// Honest configuration, parse, truncation, or paused-fetch notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit tag carried in every snapshot.
    pub license_tier: String,
    /// Map attribution carried in every snapshot.
    pub attribution: String,
}

impl AirQualitySnapshot {
    /// Explicit no-credential state. No fetch time or query point is invented.
    #[must_use]
    pub fn unconfigured(host: &str, published_at_ms: i64) -> Self {
        Self {
            host: host.to_string(),
            published_at_ms,
            fetched_at_ms: None,
            query_latitude: None,
            query_longitude: None,
            query_radius_km: 100,
            availability: AirNowAvailability::Unconfigured,
            stations: Vec::new(),
            omitted_records: 0,
            gaps: vec!["AirNow API key is not sealed (secret:airnow-api-key)".to_string()],
            license_tier: LICENSE_TIER.to_string(),
            attribution: ATTRIBUTION.to_string(),
        }
    }

    /// Empty configured snapshot shell for a validated vehicle query point.
    #[must_use]
    pub fn empty(
        host: &str,
        published_at_ms: i64,
        fetched_at_ms: i64,
        query_latitude: f64,
        query_longitude: f64,
        query_radius_km: u16,
    ) -> Self {
        Self {
            host: host.to_string(),
            published_at_ms,
            fetched_at_ms: Some(fetched_at_ms),
            query_latitude: Some(query_latitude),
            query_longitude: Some(query_longitude),
            query_radius_km,
            availability: AirNowAvailability::Ready,
            stations: Vec::new(),
            omitted_records: 0,
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
    fn topic_and_unconfigured_state_are_explicit() {
        assert_eq!(
            air_quality_state_topic("eagle"),
            "state/overlay/airnow-aqi/eagle"
        );
        let snapshot = AirQualitySnapshot::unconfigured("eagle", 123);
        assert_eq!(snapshot.availability, AirNowAvailability::Unconfigured);
        assert_eq!(snapshot.fetched_at_ms, None);
        assert!(snapshot.stations.is_empty());
        assert_eq!(snapshot.license_tier, "free-key-gov");
        assert_eq!(snapshot.attribution, "US EPA AirNow (preliminary)");
    }

    #[test]
    fn configured_observation_round_trips_without_inventing_fields() {
        let mut snapshot = AirQualitySnapshot::empty("rig-1", 200, 190, 35.78, -78.64, 100);
        snapshot.stations.push(AirQualityStation {
            id: "840371830014".to_string(),
            name: None,
            parameter: "PM2.5".to_string(),
            aqi: 156,
            latitude: 35.7829,
            longitude: -78.5742,
            distance_km: 6.0,
            observed_at_ms: 180,
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: AirQualitySnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.stations[0].name, None);
    }
}
