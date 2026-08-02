//! Typed wire contract for the keyless MBTA GTFS-Realtime transit overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node transit snapshots.
pub const TRANSIT_STATE_PREFIX: &str = "state/overlay/gtfs-transit/";
/// Release-audit tier for the attribution-bearing MassDOT license.
pub const LICENSE_TIER: &str = "open-data-attribution";
/// Attribution required by the MassDOT Developers License Agreement.
pub const ATTRIBUTION: &str = "MassDOT · MBTA";

/// Latest-wins transit topic for one workstation adapter.
#[must_use]
pub fn transit_state_topic(node: &str) -> String {
    format!("{TRANSIT_STATE_PREFIX}{node}")
}

/// Exact GTFS-Realtime passenger occupancy state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitOccupancy {
    /// Few or no passengers.
    Empty,
    /// Many seats remain.
    ManySeatsAvailable,
    /// Few seats remain.
    FewSeatsAvailable,
    /// Standing capacity remains.
    StandingRoomOnly,
    /// Limited standing capacity remains.
    CrushedStandingRoomOnly,
    /// Vehicle is full but may permit boarding.
    Full,
    /// Vehicle is temporarily not accepting passengers.
    NotAcceptingPassengers,
    /// Producer explicitly supplied no occupancy data.
    NoDataAvailable,
    /// Vehicle never accepts passengers.
    NotBoardable,
}

/// Vehicle relationship to its current stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitStopStatus {
    /// Vehicle is arriving at the referenced stop.
    IncomingAt,
    /// Vehicle is stopped at the referenced stop.
    StoppedAt,
    /// Vehicle is travelling toward the referenced stop.
    InTransitTo,
}

/// One normalized, nearby GTFS-Realtime vehicle position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitVehicle {
    /// Feed-stable entity/vehicle identifier.
    pub id: String,
    /// Bounded rider-visible vehicle label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Static-GTFS route identifier, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_id: Option<String>,
    /// Position observation time, Unix milliseconds.
    pub observed_at_ms: i64,
    /// WGS-84 latitude.
    pub latitude: f64,
    /// WGS-84 longitude.
    pub longitude: f64,
    /// Clockwise degrees from north, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearing_deg: Option<f32>,
    /// Momentary speed in metres per second, when plausible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed_mps: Option<f32>,
    /// Producer's exact non-linear occupancy category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupancy: Option<TransitOccupancy>,
    /// Producer occupancy percentage. Values above 100 are valid crush loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupancy_percentage: Option<u32>,
    /// Referenced current/next stop id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_id: Option<String>,
    /// Relationship to the referenced stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_status: Option<TransitStopStatus>,
}

/// Complete vehicle-scoped MBTA feed fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitSnapshot {
    /// Adapter node.
    pub host: String,
    /// Agency/provider identifier.
    pub agency: String,
    /// Successful fetch/validation time, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Feed header generation time, Unix milliseconds.
    pub feed_generated_at_ms: i64,
    /// GTFS-Realtime schema version (`1.0` or `2.0`).
    pub feed_version: String,
    /// Vehicle latitude used for relevance filtering.
    pub query_latitude: f64,
    /// Vehicle longitude used for relevance filtering.
    pub query_longitude: f64,
    /// Client-side relevance radius in nautical miles.
    pub radius_nm: f32,
    /// Feed entity count before bounded traversal/filtering.
    pub feed_total: u32,
    /// Nearby, fresh, normalized vehicles.
    #[serde(default)]
    pub vehicles: Vec<TransitVehicle>,
    /// Valid vehicles outside the relevance radius.
    pub relevance_filtered: u32,
    /// Missing/stale/invalid vehicle positions omitted.
    pub quality_filtered: u32,
    /// Honest transport/schema/normalization gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit license tier.
    pub license_tier: String,
    /// Required active-layer attribution.
    pub attribution: String,
}

impl TransitSnapshot {
    /// Empty successful full-dataset shell.
    #[must_use]
    pub fn empty(
        host: &str,
        fetched_at_ms: i64,
        feed_generated_at_ms: i64,
        feed_version: &str,
        latitude: f64,
        longitude: f64,
    ) -> Self {
        Self {
            host: host.to_string(),
            agency: "mbta".to_string(),
            fetched_at_ms,
            feed_generated_at_ms,
            feed_version: feed_version.to_string(),
            query_latitude: latitude,
            query_longitude: longitude,
            radius_nm: 15.0,
            feed_total: 0,
            vehicles: Vec::new(),
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
    fn topic_and_massdot_attribution_are_stable() {
        assert_eq!(
            transit_state_topic("rig-1"),
            "state/overlay/gtfs-transit/rig-1"
        );
        let snapshot = TransitSnapshot::empty("rig-1", 2, 1, "2.0", 42.3, -71.1);
        assert_eq!(snapshot.license_tier, "open-data-attribution");
        assert!(snapshot.attribution.contains("MassDOT"));
    }

    #[test]
    fn crush_load_percentage_above_one_hundred_round_trips() {
        let mut snapshot = TransitSnapshot::empty("rig-1", 2, 1, "2.0", 42.3, -71.1);
        snapshot.vehicles.push(TransitVehicle {
            id: "y1891".to_string(),
            label: Some("1891".to_string()),
            route_id: Some("22".to_string()),
            observed_at_ms: 1,
            latitude: 42.3,
            longitude: -71.1,
            bearing_deg: Some(45.0),
            speed_mps: None,
            occupancy: Some(TransitOccupancy::Full),
            occupancy_percentage: Some(160),
            stop_id: Some("334".to_string()),
            stop_status: Some(TransitStopStatus::StoppedAt),
        });
        let body = serde_json::to_string(&snapshot).expect("encode");
        let decoded: TransitSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.vehicles[0].occupancy_percentage, Some(160));
    }
}
