//! Typed wire contract for the keyless IEM/NWS NEXRAD radar-tile overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node IEM NEXRAD snapshots.
pub const IEM_RADAR_STATE_PREFIX: &str = "state/overlay/iem-nexrad/";
/// Release-audit tier: public NEXRAD data served by the IEM courtesy service.
pub const LICENSE_TIER: &str = "public-domain-courtesy-attribution";
/// Attribution shown whenever the radar layer is active.
pub const ATTRIBUTION: &str = "IEM / NOAA NWS NEXRAD";

/// Latest-wins IEM radar topic for one workstation adapter.
#[must_use]
pub fn iem_radar_state_topic(node: &str) -> String {
    format!("{IEM_RADAR_STATE_PREFIX}{node}")
}

/// One bounded Web-Mercator PNG tile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IemRadarTile {
    /// Slippy-map zoom level.
    pub z: u8,
    /// XYZ tile column.
    pub x: u32,
    /// XYZ tile row.
    pub y: u32,
    /// Base64-encoded 256×256 PNG bytes.
    pub png_base64: String,
}

/// One exact IEM composite frame and its local tile subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IemRadarFrame {
    /// NEXRAD mosaic valid time from IEM metadata, Unix milliseconds.
    pub valid_at_ms: i64,
    /// Relative history lane (`0`, `5`, … `55` minutes).
    pub nominal_minutes_ago: u8,
    /// IEM contributing-radar quorum, when supplied (for example `143/147`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radar_quorum: Option<String>,
    /// Bounded local tile subset for this frame.
    #[serde(default)]
    pub tiles: Vec<IemRadarTile>,
}

/// Complete vehicle-scoped animated radar snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IemRadarSnapshot {
    /// Adapter node.
    pub host: String,
    /// Successful metadata/tile validation time, Unix milliseconds.
    pub fetched_at_ms: i64,
    /// Fresh vehicle latitude used to select the local tile.
    pub query_latitude: f64,
    /// Fresh vehicle longitude used to select the local tile.
    pub query_longitude: f64,
    /// Fixed slippy-map tile zoom used by this snapshot.
    pub tile_zoom: u8,
    /// Newest-to-oldest exact producer frames.
    #[serde(default)]
    pub frames: Vec<IemRadarFrame>,
    /// Honest transport/schema/coverage gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit license tier.
    pub license_tier: String,
    /// Required active-layer attribution.
    pub attribution: String,
}

impl IemRadarSnapshot {
    /// Empty successful local-tile shell.
    #[must_use]
    pub fn empty(host: &str, fetched_at_ms: i64, latitude: f64, longitude: f64) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            query_latitude: latitude,
            query_longitude: longitude,
            tile_zoom: 6,
            frames: Vec::new(),
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
            iem_radar_state_topic("rig-1"),
            "state/overlay/iem-nexrad/rig-1"
        );
        let snapshot = IemRadarSnapshot::empty("rig-1", 1, 42.36, -71.06);
        assert!(snapshot.license_tier.contains("public-domain"));
        assert!(snapshot.attribution.contains("NEXRAD"));
    }

    #[test]
    fn bounded_frame_round_trips_exactly() {
        let mut snapshot = IemRadarSnapshot::empty("rig-1", 2, 42.36, -71.06);
        snapshot.frames.push(IemRadarFrame {
            valid_at_ms: 1,
            nominal_minutes_ago: 0,
            radar_quorum: Some("143/147".to_string()),
            tiles: vec![IemRadarTile {
                z: 6,
                x: 19,
                y: 23,
                png_base64: "iVBORw0KGgo=".to_string(),
            }],
        });
        let body = serde_json::to_string(&snapshot).expect("encode");
        let decoded: IemRadarSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
    }
}
