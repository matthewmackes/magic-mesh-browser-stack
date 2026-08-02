//! Typed wire contract for the credential-gated NASA FIRMS hotspot overlay.
//!
//! The workstation adapter queries the official FIRMS area CSV around a fresh
//! vehicle fix and publishes one complete latest-wins snapshot.  Missing
//! credentials, missing fixes, and failed refreshes remain explicit on the
//! wire; a consumer must never mistake a retained hotspot set for a fresh
//! safety-of-life signal.

use serde::{Deserialize, Serialize};

/// Per-node NASA FIRMS hotspot snapshot topic prefix.
pub const FIRMS_STATE_PREFIX: &str = "state/overlay/firms-hotspots/";
/// NASA open data with a free MAP_KEY required by the official API.
pub const LICENSE_TIER: &str = "free-key-gov";
/// Attribution and the provider's near-real-time safety disclaimer.
pub const ATTRIBUTION: &str = "NASA FIRMS (NRT; not for safety-of-life decisions)";

/// Retained FIRMS hotspot topic for one workstation adapter.
#[must_use]
pub fn firms_state_topic(node: &str) -> String {
    format!("{FIRMS_STATE_PREFIX}{node}")
}

/// Whether the FIRMS adapter has a usable sealed credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirmsAvailability {
    /// The operator has not sealed the free FIRMS MAP_KEY.
    Unconfigured,
    /// A sealed key is present and the adapter is ready or has fetched data.
    Ready,
    /// The sealed-secret backend could not be read or contained invalid data.
    SecretStoreError,
}

/// One normalized finite FIRMS thermal anomaly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirmsHotspot {
    /// Stable row-derived identity for latest-wins consumers.
    pub id: String,
    /// WGS-84 latitude, north positive.
    pub latitude: f64,
    /// WGS-84 longitude, east positive.
    pub longitude: f64,
    /// FIRMS brightness temperature in Kelvin, when supplied and valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness_k: Option<f32>,
    /// Fire radiative power in megawatts, when supplied and valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frp_mw: Option<f32>,
    /// FIRMS confidence label or percentage, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Satellite/instrument source, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellite: Option<String>,
    /// Acquisition timestamp in UTC, Unix milliseconds.
    pub observed_at_ms: i64,
    /// Great-circle distance from the vehicle query point.
    pub distance_km: f32,
}

/// Complete vehicle-centred FIRMS hotspot state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirmsSnapshot {
    /// Adapter node that performed the query.
    pub host: String,
    /// Time this status/snapshot was published, Unix milliseconds.
    pub published_at_ms: i64,
    /// Last successful FIRMS fetch, absent when no keyed request succeeded.
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
    /// FIRMS source selected for the area CSV query.
    pub source: String,
    /// Credential/backend availability; never implies a successful fetch.
    pub availability: FirmsAvailability,
    /// Latest bounded hotspot set from the complete response.
    #[serde(default)]
    pub hotspots: Vec<FirmsHotspot>,
    /// Source rows omitted by validation, confidence filtering, or retention caps.
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

impl FirmsSnapshot {
    /// Explicit no-credential state. No fetch time or query point is invented.
    #[must_use]
    pub fn unconfigured(host: &str, published_at_ms: i64, source: &str) -> Self {
        Self {
            host: host.to_string(),
            published_at_ms,
            fetched_at_ms: None,
            query_latitude: None,
            query_longitude: None,
            query_radius_km: 200,
            source: source.to_string(),
            availability: FirmsAvailability::Unconfigured,
            hotspots: Vec::new(),
            omitted_records: 0,
            gaps: vec!["FIRMS API key is not sealed (secret:firms-api-key)".to_string()],
            license_tier: LICENSE_TIER.to_string(),
            attribution: ATTRIBUTION.to_string(),
        }
    }

    /// Empty configured/status shell for a vehicle-centred query point.
    #[must_use]
    pub fn status(
        host: &str,
        published_at_ms: i64,
        source: &str,
        availability: FirmsAvailability,
        query: Option<(f64, f64)>,
        gap: impl Into<String>,
    ) -> Self {
        Self {
            host: host.to_string(),
            published_at_ms,
            fetched_at_ms: None,
            query_latitude: query.map(|(latitude, _)| latitude),
            query_longitude: query.map(|(_, longitude)| longitude),
            query_radius_km: 200,
            source: source.to_string(),
            availability,
            hotspots: Vec::new(),
            omitted_records: 0,
            gaps: vec![gap.into()],
            license_tier: LICENSE_TIER.to_string(),
            attribution: ATTRIBUTION.to_string(),
        }
    }

    /// Empty successful snapshot shell for a validated vehicle query point.
    #[must_use]
    pub fn empty(
        host: &str,
        published_at_ms: i64,
        fetched_at_ms: i64,
        source: &str,
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
            source: source.to_string(),
            availability: FirmsAvailability::Ready,
            hotspots: Vec::new(),
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
            firms_state_topic("eagle"),
            "state/overlay/firms-hotspots/eagle"
        );
        let snapshot = FirmsSnapshot::unconfigured("eagle", 123, "VIIRS_NOAA20_NRT");
        assert_eq!(snapshot.availability, FirmsAvailability::Unconfigured);
        assert_eq!(snapshot.fetched_at_ms, None);
        assert!(snapshot.hotspots.is_empty());
        assert_eq!(snapshot.license_tier, "free-key-gov");
        assert!(snapshot.attribution.contains("NASA FIRMS"));
        assert!(snapshot.gaps[0].contains("firms-api-key"));
    }

    #[test]
    fn configured_hotspot_round_trips_without_inventing_optional_fields() {
        let mut snapshot =
            FirmsSnapshot::empty("rig-1", 200, 190, "VIIRS_NOAA20_NRT", 35.78, -78.64, 200);
        snapshot.hotspots.push(FirmsHotspot {
            id: "VIIRS_NOAA20_NRT:190:35.78000:-78.64000".to_string(),
            latitude: 35.78,
            longitude: -78.64,
            brightness_k: Some(331.2),
            frp_mw: Some(18.4),
            confidence: Some("nominal".to_string()),
            satellite: Some("N20".to_string()),
            observed_at_ms: 190,
            distance_km: 0.0,
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: FirmsSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.hotspots[0].confidence.as_deref(), Some("nominal"));
    }
}
