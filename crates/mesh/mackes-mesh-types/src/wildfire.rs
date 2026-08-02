//! Typed wire contract for the keyless NIFC WFIGS wildfire-perimeter overlay.
//!
//! The workstation adapter queries the official current-perimeters feature
//! service around a fresh vehicle fix and publishes one complete latest-wins
//! snapshot. Consumers replace the prior perimeter set wholesale so upstream
//! revisions and removals converge without a second local database.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node NIFC WFIGS snapshots.
pub const WILDFIRE_STATE_PREFIX: &str = "state/overlay/nifc-wildfire/";
/// Release-audit tag for United States government open data.
pub const LICENSE_TIER: &str = "public-domain";
/// Attribution shown whenever the perimeter layer is enabled.
pub const ATTRIBUTION: &str = "NIFC WFIGS";

/// Retained NIFC wildfire topic for one workstation adapter.
#[must_use]
pub fn wildfire_state_topic(node: &str) -> String {
    format!("{WILDFIRE_STATE_PREFIX}{node}")
}

/// One finite WGS-84 point in a perimeter ring.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WildfirePoint {
    /// Latitude, north positive.
    pub latitude: f64,
    /// Longitude, east positive.
    pub longitude: f64,
}

/// One GeoJSON polygon. The first ring is the exterior and later rings are
/// holes. MultiPolygon features become multiple values in a perimeter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WildfirePolygon {
    /// Exterior and optional interior rings.
    pub rings: Vec<Vec<WildfirePoint>>,
}

/// One normalized current WFIGS wildfire perimeter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WildfirePerimeter {
    /// Stable ArcGIS object id, namespaced as a string on the wire.
    pub id: String,
    /// WFIGS incident name.
    pub incident_name: String,
    /// Unique fire identifier when WFIGS supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unique_fire_id: Option<String>,
    /// GIS area in acres, when finite and non-negative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acres: Option<f64>,
    /// Reported containment percentage clamped by validation to 0 through 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_contained: Option<f32>,
    /// Producer perimeter timestamp, Unix milliseconds, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perimeter_updated_at_ms: Option<i64>,
    /// Polygon or MultiPolygon geometry normalized to polygons with rings.
    #[serde(default)]
    pub polygons: Vec<WildfirePolygon>,
}

/// Complete current WFIGS perimeter set for one vehicle-centred query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WildfireSnapshot {
    /// Adapter node that performed the query.
    pub host: String,
    /// Last successful fetch time, Unix milliseconds. Failed refreshes retain
    /// this value so the cockpit sees growing age rather than synthetic fresh data.
    pub fetched_at_ms: i64,
    /// Vehicle latitude used to build the server-side bounding envelope.
    pub query_latitude: f64,
    /// Vehicle longitude used to build the server-side bounding envelope.
    pub query_longitude: f64,
    /// Approximate query radius in kilometres.
    pub query_radius_km: u16,
    /// Complete normalized wildfire perimeter set in the response.
    #[serde(default)]
    pub perimeters: Vec<WildfirePerimeter>,
    /// Upstream features omitted by server or adapter bounds.
    #[serde(default)]
    pub omitted_features: u32,
    /// Honest parse, truncation, or paused-fetch notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit tag carried in every snapshot.
    pub license_tier: String,
    /// Human map attribution carried in every snapshot.
    pub attribution: String,
}

impl WildfireSnapshot {
    /// Empty successful snapshot shell for a validated query point.
    #[must_use]
    pub fn empty(
        host: &str,
        fetched_at_ms: i64,
        query_latitude: f64,
        query_longitude: f64,
        query_radius_km: u16,
    ) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            query_latitude,
            query_longitude,
            query_radius_km,
            perimeters: Vec::new(),
            omitted_features: 0,
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
    fn topic_and_release_audit_fields_are_stable() {
        assert_eq!(
            wildfire_state_topic("eagle"),
            "state/overlay/nifc-wildfire/eagle"
        );
        let snapshot = WildfireSnapshot::empty("eagle", 123, 44.0, -120.0, 200);
        assert_eq!(snapshot.license_tier, "public-domain");
        assert_eq!(snapshot.attribution, "NIFC WFIGS");
    }

    #[test]
    fn multipolygon_and_null_metrics_round_trip_without_fabrication() {
        let mut snapshot = WildfireSnapshot::empty("rig-1", 123, 44.0, -120.0, 200);
        snapshot.perimeters.push(WildfirePerimeter {
            id: "42".to_string(),
            incident_name: "Morrill".to_string(),
            unique_fire_id: Some("2024-NE-NESUP-000123".to_string()),
            acres: Some(642_029.0),
            percent_contained: None,
            perimeter_updated_at_ms: Some(120),
            polygons: vec![WildfirePolygon {
                rings: vec![vec![
                    WildfirePoint {
                        latitude: 44.0,
                        longitude: -120.0,
                    },
                    WildfirePoint {
                        latitude: 44.1,
                        longitude: -120.0,
                    },
                    WildfirePoint {
                        latitude: 44.0,
                        longitude: -119.9,
                    },
                ]],
            }],
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: WildfireSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.perimeters[0].percent_contained, None);
    }
}
