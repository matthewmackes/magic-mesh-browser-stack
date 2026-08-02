//! Typed wire contract for the keyless Caltrans CWWP2 traffic-camera overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node Caltrans camera snapshots.
pub const CALTRANS_CAMERA_STATE_PREFIX: &str = "state/overlay/caltrans-cameras/";
/// Release-audit tier for Caltrans public traveler-information data.
pub const LICENSE_TIER: &str = "public-data-attribution";
/// Attribution shown whenever the camera layer is active.
pub const ATTRIBUTION: &str = "Caltrans CWWP2";

/// Latest-wins Caltrans camera topic for one workstation adapter.
#[must_use]
pub fn caltrans_camera_state_topic(node: &str) -> String {
    format!("{CALTRANS_CAMERA_STATE_PREFIX}{node}")
}

/// One bounded current still from an official Caltrans camera URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraThumbnail {
    /// Time the still was last modified, or fetched when the server omitted it.
    pub observed_at_ms: i64,
    /// Base64-encoded JPEG bytes, bounded by the adapter before publication.
    pub jpeg_base64: String,
}

/// One nearby Caltrans highway camera.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaltransCamera {
    /// District-local stable camera index.
    pub id: String,
    /// Human location name.
    pub name: String,
    /// Nearby city/place, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nearby_place: Option<String>,
    /// County, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub county: Option<String>,
    /// Highway route label, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Camera/roadway direction text, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// WGS-84 latitude.
    pub latitude: f64,
    /// WGS-84 longitude.
    pub longitude: f64,
    /// Great-circle distance from the qualifying vehicle fix.
    pub distance_nm: f32,
    /// Whether CWWP2 reports the camera in service.
    pub in_service: bool,
    /// Camera metadata record time, Unix milliseconds, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_at_ms: Option<i64>,
    /// Validated official still-image URL.
    pub image_url: String,
    /// Current-image update frequency advertised by the feed, in minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_update_minutes: Option<u16>,
    /// Bounded current still for a nearest route-ahead camera.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<CameraThumbnail>,
}

/// Complete vehicle-scoped Caltrans camera fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaltransCameraSnapshot {
    /// Adapter node.
    pub host: String,
    /// Configured Caltrans district (1–12).
    pub district: u8,
    /// Successful catalog/thumbnail validation time, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Catalog HTTP `Last-Modified`, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_modified_at_ms: Option<i64>,
    /// Fresh vehicle latitude used for relevance filtering.
    pub query_latitude: f64,
    /// Fresh vehicle longitude used for relevance filtering.
    pub query_longitude: f64,
    /// Heading used to select route-ahead stills, when trustworthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_heading_deg: Option<f32>,
    /// Client-side relevance radius in nautical miles.
    pub radius_nm: f32,
    /// Catalog record count before bounded traversal/filtering.
    pub feed_total: u32,
    /// Nearby normalized camera records, nearest first.
    #[serde(default)]
    pub cameras: Vec<CaltransCamera>,
    /// Valid records outside the relevance radius.
    pub relevance_filtered: u32,
    /// Malformed/invalid records omitted.
    pub quality_filtered: u32,
    /// Honest transport/schema/normalization gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit license tier.
    pub license_tier: String,
    /// Required active-layer attribution.
    pub attribution: String,
}

impl CaltransCameraSnapshot {
    /// Empty successful district-catalog shell.
    #[must_use]
    pub fn empty(
        host: &str,
        district: u8,
        fetched_at_ms: i64,
        latitude: f64,
        longitude: f64,
    ) -> Self {
        Self {
            host: host.to_string(),
            district,
            fetched_at_ms,
            catalog_modified_at_ms: None,
            query_latitude: latitude,
            query_longitude: longitude,
            query_heading_deg: None,
            radius_nm: 30.0,
            feed_total: 0,
            cameras: Vec::new(),
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
    fn topic_license_and_attribution_are_stable() {
        assert_eq!(
            caltrans_camera_state_topic("rig-1"),
            "state/overlay/caltrans-cameras/rig-1"
        );
        let snapshot = CaltransCameraSnapshot::empty("rig-1", 3, 1, 38.5, -121.5);
        assert_eq!(snapshot.license_tier, "public-data-attribution");
        assert!(snapshot.attribution.contains("Caltrans"));
    }

    #[test]
    fn bounded_jpeg_thumbnail_round_trips_exactly() {
        let mut snapshot = CaltransCameraSnapshot::empty("rig-1", 3, 2, 38.5, -121.5);
        snapshot.cameras.push(CaltransCamera {
            id: "1".to_string(),
            name: "Hwy 5 at Pocket".to_string(),
            nearby_place: Some("Sacramento".to_string()),
            county: Some("Sacramento".to_string()),
            route: Some("I-5".to_string()),
            direction: Some("Median".to_string()),
            latitude: 38.481128,
            longitude: -121.510528,
            distance_nm: 1.0,
            in_service: true,
            record_at_ms: Some(1),
            image_url: "https://cwwp2.dot.ca.gov/data/d3/cctv/image/test/test.jpg".to_string(),
            image_update_minutes: Some(1),
            thumbnail: Some(CameraThumbnail {
                observed_at_ms: 2,
                jpeg_base64: "/9j/2Q==".to_string(),
            }),
        });
        let body = serde_json::to_string(&snapshot).expect("encode");
        let decoded: CaltransCameraSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
    }
}
