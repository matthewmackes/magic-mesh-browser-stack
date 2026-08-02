//! UPS / NUT (Network UPS Tools) adapter — extracts template
//! fields from UPS event webhooks (BUS-3.6).
//!
//! NUT itself doesn't ship a built-in webhook poster, but the
//! community recipe is `upssched.conf` → external script → `curl`
//! to a configured URL. The payload shape is operator-defined;
//! the canonical body this adapter expects is:
//!
//! ```json
//! {
//!   "event": "ONBATT",
//!   "ups": "myups@server",
//!   "ts": "2026-05-26T12:34:56Z",
//!   "status": "ONBATT LB",
//!   "battery_charge": 42,
//!   "runtime_seconds": 600
//! }
//! ```
//!
//! Event names mirror NUT's UPS-status flags + the
//! `apcupsd`-style canonical events: `ONBATT` (grid loss),
//! `LOWBATT` (battery low), `ONLINE` (grid restored),
//! `SHUTDOWN` (shutdown imminent), `COMMOK` / `COMMBAD` (comms
//! restored / lost).
//!
//! Exposed template fields:
//!
//! | event       | fields                                                          |
//! |------------:|-----------------------------------------------------------------|
//! | any         | `ups_name`, `ups_ts`, `ups_status`, `battery_charge`, `runtime`, `runtime_human` |

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The UPS/NUT adapter — stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct NutAdapter;

impl Adapter for NutAdapter {
    fn extract(
        &self,
        _headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let event = body.get("event").and_then(Value::as_str)?.to_string();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        if let Some(ups) = body.get("ups").and_then(Value::as_str) {
            fields.insert("ups_name".to_string(), ups.to_string());
        }
        if let Some(ts) = body.get("ts").and_then(Value::as_str) {
            fields.insert("ups_ts".to_string(), ts.to_string());
        }
        if let Some(status) = body.get("status").and_then(Value::as_str) {
            fields.insert("ups_status".to_string(), status.to_string());
        }
        if let Some(bc) = body.get("battery_charge").and_then(Value::as_i64) {
            fields.insert("battery_charge".to_string(), bc.to_string());
        }
        if let Some(rt) = body.get("runtime_seconds").and_then(Value::as_i64) {
            fields.insert("runtime".to_string(), rt.to_string());
            fields.insert("runtime_human".to_string(), humanize_seconds(rt));
        }

        Some((event, fields))
    }
}

/// Render a runtime in seconds as "Xh Ym" or "Ym Zs". Operators
/// usually want a glance-value in notification bodies, not raw
/// seconds.
fn humanize_seconds(s: i64) -> String {
    if s <= 0 {
        return "0s".to_string();
    }
    let hours = s / 3600;
    let minutes = (s % 3600) / 60;
    let secs = s % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn onbatt_extracts_full_field_set() {
        let body = json!({
            "event": "ONBATT",
            "ups": "myups@srv1",
            "ts": "2026-05-26T12:34:56Z",
            "status": "ONBATT LB",
            "battery_charge": 42,
            "runtime_seconds": 600,
        });
        let (event, fields) = NutAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "ONBATT");
        assert_eq!(
            fields.get("ups_name").map(String::as_str),
            Some("myups@srv1")
        );
        assert_eq!(
            fields.get("ups_status").map(String::as_str),
            Some("ONBATT LB")
        );
        assert_eq!(fields.get("battery_charge").map(String::as_str), Some("42"));
        assert_eq!(fields.get("runtime").map(String::as_str), Some("600"));
        assert_eq!(
            fields.get("runtime_human").map(String::as_str),
            Some("10m 0s")
        );
    }

    #[test]
    fn online_with_minimal_payload_extracts_event_only() {
        let body = json!({"event": "ONLINE"});
        let (event, fields) = NutAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "ONLINE");
        assert!(fields.is_empty());
    }

    #[test]
    fn missing_event_field_returns_none() {
        let body = json!({"ups": "x"});
        assert!(NutAdapter.extract(&BTreeMap::new(), &body).is_none());
    }

    #[test]
    fn humanize_seconds_scales_correctly() {
        assert_eq!(humanize_seconds(0), "0s");
        assert_eq!(humanize_seconds(45), "45s");
        assert_eq!(humanize_seconds(125), "2m 5s");
        assert_eq!(humanize_seconds(3600), "1h 0m");
        assert_eq!(humanize_seconds(5400), "1h 30m");
    }
}
