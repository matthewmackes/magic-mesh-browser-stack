//! Typed wire contract for the keyless USGS earthquake overlay.
//!
//! The workstation-side `mackesd` adapter publishes one latest-wins snapshot on
//! `state/overlay/usgs-earthquakes/<node>`. Desktop consumers replace their
//! prior snapshot wholesale, so USGS event revisions and deletions converge by
//! event `id` + `updated_at_ms` without a second local database.

use serde::{Deserialize, Serialize};

/// Topic prefix for the per-node USGS earthquake overlay mirror.
pub const EARTHQUAKE_STATE_PREFIX: &str = "state/overlay/usgs-earthquakes/";

/// Release-audit tag: USGS earthquake data is United States government public
/// domain data and requires no paid or non-commercial-only license tier.
pub const LICENSE_TIER: &str = "public-domain";

/// Attribution shown whenever the earthquake layer is enabled.
pub const ATTRIBUTION: &str = "USGS";

/// The `state/overlay/usgs-earthquakes/<node>` mirror topic for one adapter.
#[must_use]
pub fn earthquake_state_topic(node: &str) -> String {
    format!("{EARTHQUAKE_STATE_PREFIX}{node}")
}

/// PAGER impact alert carried by a USGS event, when USGS has assigned one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PagerAlert {
    /// Little or no expected impact.
    Green,
    /// Some impact is possible.
    Yellow,
    /// Significant impact is likely.
    Orange,
    /// Severe impact is likely.
    Red,
}

/// One normalized USGS GeoJSON feature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EarthquakeEvent {
    /// Stable USGS event id (for example `us7000abcd`).
    pub id: String,
    /// Origin time, Unix milliseconds.
    pub occurred_at_ms: i64,
    /// Last USGS revision time, Unix milliseconds.
    pub updated_at_ms: i64,
    /// Epicentre latitude in decimal degrees.
    pub latitude: f64,
    /// Epicentre longitude in decimal degrees.
    pub longitude: f64,
    /// Hypocentre depth in kilometres. This is the third GeoJSON coordinate,
    /// not altitude.
    pub depth_km: f32,
    /// Reported magnitude. `None` preserves USGS `null`; it is never changed to
    /// zero merely to make the marker renderable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magnitude: Option<f32>,
    /// Human-readable place description supplied by USGS.
    #[serde(default)]
    pub place: String,
    /// PAGER impact alert, when assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pager_alert: Option<PagerAlert>,
    /// Canonical USGS event detail URL, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_url: Option<String>,
}

/// Latest-wins snapshot published by one workstation adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EarthquakeSnapshot {
    /// Adapter node whose worker fetched this snapshot.
    pub host: String,
    /// Successful fetch or conditional-validation time, Unix milliseconds.
    /// This remains unchanged across a failed refresh so consumers see honest
    /// staleness instead of a synthetic new timestamp.
    pub fetched_at_ms: i64,
    /// USGS feed generation time, Unix milliseconds, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_generated_at_ms: Option<i64>,
    /// Complete current feature set. Consumers replace, rather than append,
    /// this list so revised/deleted events converge.
    #[serde(default)]
    pub events: Vec<EarthquakeEvent>,
    /// Partial/fetch gaps. A retained last-good snapshot may carry a new gap
    /// while preserving its original `fetched_at_ms`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// License tier pinned into every snapshot for release-audit grepability.
    pub license_tier: String,
    /// Human attribution pinned into the wire record.
    pub attribution: String,
}

impl EarthquakeSnapshot {
    /// Build an empty, honest snapshot shell for a successful feed response.
    #[must_use]
    pub fn empty(host: &str, fetched_at_ms: i64) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            feed_generated_at_ms: None,
            events: Vec::new(),
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
            earthquake_state_topic("eagle"),
            "state/overlay/usgs-earthquakes/eagle"
        );
        let snapshot = EarthquakeSnapshot::empty("eagle", 123);
        assert_eq!(snapshot.license_tier, "public-domain");
        assert_eq!(snapshot.attribution, "USGS");
    }

    #[test]
    fn snapshot_round_trips_without_turning_null_magnitude_into_zero() {
        let mut snapshot = EarthquakeSnapshot::empty("rig-1", 123);
        snapshot.events.push(EarthquakeEvent {
            id: "us-test".to_string(),
            occurred_at_ms: 100,
            updated_at_ms: 110,
            latitude: 34.1,
            longitude: -118.2,
            depth_km: 8.4,
            magnitude: None,
            place: "test event".to_string(),
            pager_alert: Some(PagerAlert::Yellow),
            detail_url: None,
        });
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: EarthquakeSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.events[0].magnitude, None);
    }
}
