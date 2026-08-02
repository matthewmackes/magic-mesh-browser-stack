//! Typed wire contract for the keyless NWS hourly drive-ahead forecast overlay.

use serde::{Deserialize, Serialize};

/// Topic prefix for per-node hourly forecast snapshots.
pub const NWS_FORECAST_STATE_PREFIX: &str = "state/overlay/nws-hourly/";
/// Release-audit tier for US-government public-domain data.
pub const LICENSE_TIER: &str = "us-government-public-domain";
/// Attribution shown whenever the layer is active.
pub const ATTRIBUTION: &str = "NOAA · National Weather Service";

/// Latest-wins hourly forecast topic for one workstation adapter.
#[must_use]
pub fn nws_forecast_state_topic(node: &str) -> String {
    format!("{NWS_FORECAST_STATE_PREFIX}{node}")
}

/// Normalized rider/driver-facing weather category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForecastKind {
    /// Thunder or convective storms.
    Thunderstorm,
    /// Rain, showers, or drizzle.
    Rain,
    /// Snow, sleet, freezing rain, or other wintry precipitation.
    Wintry,
    /// Fog, smoke, haze, or reduced visibility.
    LowVisibility,
    /// Strong or gusty wind.
    Wind,
    /// Clear or mostly clear conditions.
    Clear,
    /// Cloud cover without a stronger hazard category.
    Cloudy,
    /// Producer text did not map to a known category.
    Unknown,
}

/// One bounded NWS hourly period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForecastPeriod {
    /// Producer period number.
    pub number: u16,
    /// Inclusive start time, Unix milliseconds.
    pub start_at_ms: i64,
    /// Exclusive end time, Unix milliseconds.
    pub end_at_ms: i64,
    /// Day/night flag supplied by NWS.
    pub is_daytime: bool,
    /// Integer temperature as supplied by NWS.
    pub temperature: i16,
    /// `F` or `C`.
    pub temperature_unit: String,
    /// Precipitation probability, percent, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precipitation_percent: Option<u8>,
    /// Relative humidity, percent, when supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub humidity_percent: Option<u8>,
    /// Bounded NWS wind-speed text (for example `9 mph`).
    pub wind_speed: String,
    /// Bounded compass direction.
    pub wind_direction: String,
    /// Bounded NWS short forecast.
    pub short_forecast: String,
    /// Normalized visual category derived from the short forecast.
    pub kind: ForecastKind,
}

/// Forecast periods for one current/drive-ahead sample point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastSample {
    /// Distance along current vehicle heading; zero is the current fix.
    pub distance_ahead_km: f32,
    /// Estimated arrival time at the sample, Unix milliseconds.
    pub eta_at_ms: i64,
    /// WGS-84 sample latitude.
    pub latitude: f64,
    /// WGS-84 sample longitude.
    pub longitude: f64,
    /// NWS grid office identifier.
    pub grid_id: String,
    /// NWS grid X coordinate.
    pub grid_x: i32,
    /// NWS grid Y coordinate.
    pub grid_y: i32,
    /// Upcoming periods, bounded to the first 24 hours.
    pub periods: Vec<ForecastPeriod>,
}

/// Complete vehicle-scoped hourly forecast fold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NwsForecastSnapshot {
    /// Adapter node.
    pub host: String,
    /// Successful fetch/validation time, Unix milliseconds; zero before data.
    pub fetched_at_ms: i64,
    /// Oldest `generatedAt` among retained NWS samples, Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_generated_at_ms: Option<i64>,
    /// Fresh vehicle-fix latitude used for sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_latitude: Option<f64>,
    /// Fresh vehicle-fix longitude used for sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_longitude: Option<f64>,
    /// Heading used for drive-ahead points, when motion made it trustworthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading_deg: Option<f32>,
    /// Current and drive-ahead forecast samples.
    #[serde(default)]
    pub samples: Vec<ForecastSample>,
    /// Honest no-fix/transport/schema/normalization gaps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Release-audit license tier.
    pub license_tier: String,
    /// Required active-layer attribution.
    pub attribution: String,
}

impl NwsForecastSnapshot {
    /// Honest no-data shell, used before a fresh same-host MG90 fix exists.
    #[must_use]
    pub fn unavailable(host: &str, gap: impl Into<String>) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms: 0,
            feed_generated_at_ms: None,
            query_latitude: None,
            query_longitude: None,
            heading_deg: None,
            samples: Vec::new(),
            gaps: vec![gap.into()],
            license_tier: LICENSE_TIER.to_string(),
            attribution: ATTRIBUTION.to_string(),
        }
    }

    /// Empty successful point-scoped shell.
    #[must_use]
    pub fn empty(host: &str, fetched_at_ms: i64, latitude: f64, longitude: f64) -> Self {
        Self {
            host: host.to_string(),
            fetched_at_ms,
            feed_generated_at_ms: None,
            query_latitude: Some(latitude),
            query_longitude: Some(longitude),
            heading_deg: None,
            samples: Vec::new(),
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
    fn topic_license_and_unavailable_state_are_stable() {
        assert_eq!(
            nws_forecast_state_topic("rig-1"),
            "state/overlay/nws-hourly/rig-1"
        );
        let snapshot = NwsForecastSnapshot::unavailable("rig-1", "no fresh fix");
        assert_eq!(snapshot.license_tier, "us-government-public-domain");
        assert!(snapshot.attribution.contains("National Weather Service"));
        assert_eq!(snapshot.fetched_at_ms, 0);
        assert!(snapshot.samples.is_empty());
    }

    #[test]
    fn typed_period_round_trips_without_inventing_values() {
        let mut snapshot = NwsForecastSnapshot::empty("rig-1", 2, 42.3, -71.1);
        snapshot.samples.push(ForecastSample {
            distance_ahead_km: 0.0,
            eta_at_ms: 2,
            latitude: 42.3,
            longitude: -71.1,
            grid_id: "BOX".to_string(),
            grid_x: 71,
            grid_y: 101,
            periods: vec![ForecastPeriod {
                number: 1,
                start_at_ms: 1,
                end_at_ms: 3,
                is_daytime: true,
                temperature: 83,
                temperature_unit: "F".to_string(),
                precipitation_percent: Some(27),
                humidity_percent: Some(65),
                wind_speed: "9 mph".to_string(),
                wind_direction: "W".to_string(),
                short_forecast: "Chance Showers".to_string(),
                kind: ForecastKind::Rain,
            }],
        });
        let body = serde_json::to_string(&snapshot).expect("encode");
        let decoded: NwsForecastSnapshot = serde_json::from_str(&body).expect("decode");
        assert_eq!(decoded, snapshot);
    }
}
