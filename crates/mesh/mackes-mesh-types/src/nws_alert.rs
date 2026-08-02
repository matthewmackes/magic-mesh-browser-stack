//! Typed wire contract for the keyless NWS active-weather-alert overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node NWS alert snapshots.
pub const NWS_ALERT_STATE_PREFIX: &str = "state/overlay/nws-alerts/";
/// Release-audit tag: NWS data is United States government public domain data.
pub const LICENSE_TIER: &str = "public-domain";
/// Required map attribution for this layer.
pub const ATTRIBUTION: &str = "NWS";

/// The retained snapshot topic for one workstation adapter.
#[must_use]
pub fn nws_alert_state_topic(node: &str) -> String {
    format!("{NWS_ALERT_STATE_PREFIX}{node}")
}

/// One geographic point in decimal degrees.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    /// Latitude, north positive.
    pub latitude: f64,
    /// Longitude, east positive.
    pub longitude: f64,
}

/// One GeoJSON polygon. The first ring is the exterior; later rings are holes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AlertPolygon {
    /// Closed or open coordinate rings; consumers close them while painting.
    pub rings: Vec<Vec<GeoPoint>>,
}

/// CAP severity normalized from NWS properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NwsSeverity {
    /// Extraordinary threat to life or property.
    Extreme,
    /// Significant threat to life or property.
    Severe,
    /// Possible threat to life or property.
    Moderate,
    /// Minimal or no known threat.
    Minor,
    /// NWS did not assign a known severity.
    Unknown,
}

/// Where the normalized polygon came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeometrySource {
    /// Inline geometry on the active-alert feature.
    Inline,
    /// Resolved from one of the alert's `affectedZones` URLs.
    AffectedZone,
}

/// One normalized active NWS alert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NwsAlert {
    /// Stable NWS alert identifier.
    pub id: String,
    /// CAP event type, for example `Tornado Warning`.
    pub event: String,
    /// Operator-facing headline.
    #[serde(default)]
    pub headline: String,
    /// Human area description.
    #[serde(default)]
    pub area_desc: String,
    /// CAP severity.
    pub severity: NwsSeverity,
    /// CAP urgency string (`Immediate`, `Expected`, ...).
    #[serde(default)]
    pub urgency: String,
    /// CAP certainty string (`Observed`, `Likely`, ...).
    #[serde(default)]
    pub certainty: String,
    /// Issue/sent time, Unix milliseconds, when parseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at_ms: Option<i64>,
    /// Expiration time, Unix milliseconds, when parseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    /// Resolved geographic polygons. Empty is honest when NWS provides neither
    /// inline geometry nor a resolvable affected zone.
    #[serde(default)]
    pub polygons: Vec<AlertPolygon>,
    /// Provenance of `polygons`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_source: Option<GeometrySource>,
}

/// Latest-wins NWS active-alert snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NwsAlertSnapshot {
    /// Adapter node.
    pub host: String,
    /// Last successful fetch/conditional validation time, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Feed update time, Unix milliseconds, when NWS supplies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_updated_at_ms: Option<i64>,
    /// Vehicle point used for this point-scoped active-alert query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_point: Option<GeoPoint>,
    /// Complete active-alert set for the queried vehicle point.
    #[serde(default)]
    pub alerts: Vec<NwsAlert>,
    /// Honest parse/resolution/fetch gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit tag carried in every snapshot.
    pub license_tier: String,
    /// Map attribution.
    pub attribution: String,
}

impl NwsAlertSnapshot {
    /// Empty successful snapshot shell.
    #[must_use]
    pub fn empty(host: &str, fetched_at_ms: i64) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            feed_updated_at_ms: None,
            query_point: None,
            alerts: Vec::new(),
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
    fn topic_and_audit_fields_are_stable() {
        assert_eq!(
            nws_alert_state_topic("eagle"),
            "state/overlay/nws-alerts/eagle"
        );
        let snapshot = NwsAlertSnapshot::empty("eagle", 10);
        assert_eq!(snapshot.license_tier, "public-domain");
        assert_eq!(snapshot.attribution, "NWS");
    }

    #[test]
    fn polygon_and_geometry_provenance_round_trip() {
        let mut snapshot = NwsAlertSnapshot::empty("rig-1", 10);
        snapshot.alerts.push(NwsAlert {
            id: "urn:test".to_string(),
            event: "Tornado Warning".to_string(),
            headline: "Tornado Warning issued".to_string(),
            area_desc: "Test County".to_string(),
            severity: NwsSeverity::Extreme,
            urgency: "Immediate".to_string(),
            certainty: "Observed".to_string(),
            sent_at_ms: Some(1),
            expires_at_ms: Some(2),
            polygons: vec![AlertPolygon {
                rings: vec![vec![GeoPoint {
                    latitude: 32.0,
                    longitude: -95.0,
                }]],
            }],
            geometry_source: Some(GeometrySource::AffectedZone),
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: NwsAlertSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
    }
}
