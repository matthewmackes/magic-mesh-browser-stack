//! Typed wire contract for the keyless NCDOT TIMS traffic-event overlay.

use serde::{Deserialize, Serialize};

/// Per-node NCDOT traffic snapshot topic prefix.
pub const TRAFFIC_STATE_PREFIX: &str = "state/overlay/ncdot-traffic/";
/// NCDOT open-data release tier; attribution is required in the cockpit.
pub const LICENSE_TIER: &str = "open-data-attribution";
/// Attribution shown whenever the traffic-event layer is enabled.
pub const ATTRIBUTION: &str = "NCDOT DriveNC / TIMS";

/// Retained NCDOT traffic topic for one workstation adapter.
#[must_use]
pub fn traffic_state_topic(node: &str) -> String {
    format!("{TRAFFIC_STATE_PREFIX}{node}")
}

/// One normalized current NCDOT TIMS incident point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrafficEvent {
    /// Stable TIMS event identifier.
    pub id: String,
    /// Road or route name.
    #[serde(default)]
    pub road: String,
    /// Driver-facing event/location summary.
    #[serde(default)]
    pub summary: String,
    /// Normalized source event type, for example `roadwork`.
    #[serde(default)]
    pub event_type: String,
    /// More specific source subtype, for example `construction`.
    #[serde(default)]
    pub event_subtype: String,
    /// Source road condition, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// Lane impact text, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lanes_affected: Option<String>,
    /// Travel direction, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// County name, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub county: Option<String>,
    /// Whether TIMS marks the road fully closed.
    pub full_closure: bool,
    /// WGS-84 latitude.
    pub latitude: f64,
    /// WGS-84 longitude.
    pub longitude: f64,
    /// Great-circle distance from the query point.
    pub distance_km: f32,
    /// Event start, Unix milliseconds, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_at_ms: Option<i64>,
    /// Planned event end, Unix milliseconds, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at_ms: Option<i64>,
    /// Producer revision time, Unix milliseconds, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<i64>,
}

/// Complete vehicle-centred NCDOT current-event snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrafficSnapshot {
    /// Adapter node.
    pub host: String,
    /// Last successful fetch/conditional validation, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Vehicle latitude used for the server-side envelope.
    pub query_latitude: f64,
    /// Vehicle longitude used for the server-side envelope.
    pub query_longitude: f64,
    /// Relevance radius in kilometres.
    pub query_radius_km: u16,
    /// Complete normalized nearby current event set.
    #[serde(default)]
    pub events: Vec<TrafficEvent>,
    /// Features omitted by validation or retention caps.
    #[serde(default)]
    pub omitted_features: u32,
    /// Honest parse, truncation, or paused-fetch notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit tag carried in every snapshot.
    pub license_tier: String,
    /// Map attribution carried in every snapshot.
    pub attribution: String,
}

impl TrafficSnapshot {
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
            events: Vec::new(),
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
            traffic_state_topic("eagle"),
            "state/overlay/ncdot-traffic/eagle"
        );
        let snapshot = TrafficSnapshot::empty("eagle", 1, 35.7, -78.6, 100);
        assert_eq!(snapshot.license_tier, "open-data-attribution");
        assert_eq!(snapshot.attribution, "NCDOT DriveNC / TIMS");
    }

    #[test]
    fn null_optional_fields_round_trip_without_fabrication() {
        let mut snapshot = TrafficSnapshot::empty("rig-1", 1, 35.7, -78.6, 100);
        snapshot.events.push(TrafficEvent {
            id: "2188564".to_string(),
            road: "NC 55".to_string(),
            summary: "Construction on NC 55".to_string(),
            event_type: "roadwork".to_string(),
            event_subtype: "construction".to_string(),
            condition: Some("Open".to_string()),
            lanes_affected: Some("Lane Affected".to_string()),
            direction: Some("Both Directions".to_string()),
            county: Some("Wake".to_string()),
            full_closure: false,
            latitude: 35.5574,
            longitude: -78.7545,
            distance_km: 20.0,
            starts_at_ms: Some(1),
            ends_at_ms: None,
            updated_at_ms: Some(2),
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: TrafficSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.events[0].ends_at_ms, None);
    }
}
