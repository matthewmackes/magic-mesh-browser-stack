//! Generic-JSON adapter — passes through any JSON object as
//! template fields (BUS-3.7).
//!
//! Useful when the operator wants to wire a one-off poster that
//! doesn't fit the github/gitea/sonarr/nut/HA shapes. The shim's
//! contract is: the request body MUST be a JSON object; every
//! top-level string / number / bool field becomes a template
//! variable. Nested objects and arrays are skipped (the
//! supported adapters above are the right place to handle
//! per-source nesting).
//!
//! `event` is taken from the body's `event` key when present,
//! falling back to the literal string `"event"` so an empty
//! `match.event` rule still fires. Operators can narrow with
//! `match.field.<key>: <value>` predicates.
//!
//! Example body:
//!
//! ```json
//! { "event": "ping", "source": "my-script", "message": "hi" }
//! ```
//!
//! Templates can reference `{{ event }}`, `{{ source }}`,
//! `{{ message }}`.

use std::collections::BTreeMap;

use serde_json::Value;

use super::matcher::Adapter;

/// The generic-JSON adapter — stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct GenericAdapter;

impl Adapter for GenericAdapter {
    fn extract(
        &self,
        _headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)> {
        let obj = body.as_object()?;
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in obj {
            match v {
                Value::String(s) => {
                    fields.insert(k.clone(), s.clone());
                }
                Value::Number(n) => {
                    fields.insert(k.clone(), n.to_string());
                }
                Value::Bool(b) => {
                    fields.insert(k.clone(), b.to_string());
                }
                // Skip null / object / array — operators who need
                // structure should use a typed adapter.
                _ => {}
            }
        }
        let event = obj
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("event")
            .to_string();
        Some((event, fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flat_object_extracts_string_number_bool() {
        let body = json!({
            "event": "ping",
            "source": "my-script",
            "message": "hi",
            "count": 7,
            "ok": true,
        });
        let (event, fields) = GenericAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "ping");
        assert_eq!(fields.get("source").map(String::as_str), Some("my-script"));
        assert_eq!(fields.get("message").map(String::as_str), Some("hi"));
        assert_eq!(fields.get("count").map(String::as_str), Some("7"));
        assert_eq!(fields.get("ok").map(String::as_str), Some("true"));
    }

    #[test]
    fn nested_objects_and_arrays_are_skipped() {
        let body = json!({
            "event": "x",
            "obj": {"k": "v"},
            "arr": [1, 2, 3],
            "kept": "value",
        });
        let (_, fields) = GenericAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert!(!fields.contains_key("obj"));
        assert!(!fields.contains_key("arr"));
        assert_eq!(fields.get("kept").map(String::as_str), Some("value"));
    }

    #[test]
    fn no_event_field_defaults_to_literal_event() {
        let body = json!({"k": "v"});
        let (event, _) = GenericAdapter.extract(&BTreeMap::new(), &body).unwrap();
        assert_eq!(event, "event");
    }

    #[test]
    fn non_object_body_returns_none() {
        let body = json!([1, 2, 3]);
        assert!(GenericAdapter.extract(&BTreeMap::new(), &body).is_none());

        let body = json!("just a string");
        assert!(GenericAdapter.extract(&BTreeMap::new(), &body).is_none());
    }
}
