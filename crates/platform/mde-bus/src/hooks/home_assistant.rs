//! Home Assistant adapter — extracts template fields from HA
//! webhook payloads (BUS-3.7).
//!
//! Home Assistant ships webhooks via its "Webhooks" automation
//! action and a configurable per-webhook endpoint
//! (`POST /api/webhook/<id>`). The body is operator-defined in
//! the automation YAML; for the Bus this adapter, the canonical
//! shape is:
//!
//! ```json
//! {
//!   "event": "automation_triggered",
//!   "automation": "Front door open",
//!   "detail": "Front door opened at 12:34",
//!   "entity": "binary_sensor.front_door",
//!   "state": "on",
//!   "area": "Foyer"
//! }
//! ```
//!
//! Templates can reference: `automation_name`, `automation_detail`,
//! `entity`, `state`, `area`. Optional `severity` lets HA tag
//! payloads as `info`/`warning`/`critical`.

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The Home Assistant adapter — stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct HomeAssistantAdapter;

impl Adapter for HomeAssistantAdapter {
    fn extract(
        &self,
        _headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let event = body.get("event").and_then(Value::as_str)?.to_string();
        let mut fields: BTreeMap<String, String> = BTreeMap::new();

        if let Some(name) = body.get("automation").and_then(Value::as_str) {
            fields.insert("automation_name".to_string(), name.to_string());
        }
        if let Some(detail) = body.get("detail").and_then(Value::as_str) {
            fields.insert("automation_detail".to_string(), detail.to_string());
        }
        if let Some(entity) = body.get("entity").and_then(Value::as_str) {
            fields.insert("entity".to_string(), entity.to_string());
        }
        if let Some(state) = body.get("state").and_then(Value::as_str) {
            fields.insert("state".to_string(), state.to_string());
        }
        if let Some(area) = body.get("area").and_then(Value::as_str) {
            fields.insert("area".to_string(), area.to_string());
        }
        if let Some(sev) = body.get("severity").and_then(Value::as_str) {
            fields.insert("severity".to_string(), sev.to_string());
        }

        Some((event, fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn automation_triggered_extracts_canonical_fields() {
        let body = json!({
            "event": "automation_triggered",
            "automation": "Front door open",
            "detail": "Front door opened at 12:34",
            "entity": "binary_sensor.front_door",
            "state": "on",
            "area": "Foyer",
        });
        let (event, fields) = HomeAssistantAdapter
            .extract(&BTreeMap::new(), &body)
            .unwrap();
        assert_eq!(event, "automation_triggered");
        assert_eq!(
            fields.get("automation_name").map(String::as_str),
            Some("Front door open")
        );
        assert_eq!(
            fields.get("entity").map(String::as_str),
            Some("binary_sensor.front_door")
        );
        assert_eq!(fields.get("state").map(String::as_str), Some("on"));
        assert_eq!(fields.get("area").map(String::as_str), Some("Foyer"));
    }

    #[test]
    fn severity_field_lifts_through_when_present() {
        let body = json!({
            "event": "alert",
            "automation": "Smoke detected",
            "severity": "critical",
        });
        let (_, fields) = HomeAssistantAdapter
            .extract(&BTreeMap::new(), &body)
            .unwrap();
        assert_eq!(fields.get("severity").map(String::as_str), Some("critical"));
    }

    #[test]
    fn missing_event_returns_none() {
        let body = json!({"automation": "x"});
        assert!(HomeAssistantAdapter
            .extract(&BTreeMap::new(), &body)
            .is_none());
    }

    #[test]
    fn missing_optional_fields_yields_partial_map() {
        let body = json!({"event": "ping"});
        let (event, fields) = HomeAssistantAdapter
            .extract(&BTreeMap::new(), &body)
            .unwrap();
        assert_eq!(event, "ping");
        assert!(fields.is_empty());
    }
}
