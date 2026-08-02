//! Rule-matching engine.
//!
//! Given an inbound webhook request (adapter name + HTTP headers
//! + JSON body) and a parsed [`HooksConfig`], produce a
//! [`RenderedPublish`] when any rule matches, or `None` when no
//! rule fires.
//!
//! The flow per request:
//!
//! 1. Look up the adapter in the config.
//! 2. Call the adapter's [`Adapter::extract`] to get the
//!    event-name string + a `BTreeMap<String, String>` of
//!    template fields.
//! 3. Walk the rule list in declaration order. For each rule:
//!    - If `match.event` is set, it must equal the extracted
//!      event name.
//!    - Every `match.field.<k> = v` predicate must equal the
//!      extracted field value at `k`.
//! 4. On the first match, render the rule's `topic` + `title` +
//!    `body` templates via Tera against the extracted fields and
//!    return the result.
//!
//! Note: templates are rendered *strict* — referencing a field
//! the adapter didn't populate produces a render error which
//! becomes a 422 in the server layer. This makes operator typos
//! surface immediately at request time rather than silently
//! publishing an empty body.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

use super::config::{HooksConfig, Priority, Rule};

/// A built-in adapter's per-request extractor. The implementation
/// lives next to the source (e.g. [`super::github::GitHubAdapter`]).
///
/// `headers` is delivered with all-lowercase header names so each
/// implementation can pattern-match against a consistent shape.
pub trait Adapter: Send + Sync {
    /// Returns `(event_name, fields)` when this adapter can
    /// handle the request, or `None` to signal "not for me" (the
    /// server returns 400 to the caller).
    fn extract(
        &self,
        headers: &BTreeMap<String, String>,
        body: &Value,
    ) -> Option<(String, BTreeMap<String, String>)>;
}

/// The result of a successful match — ready to ship to the
/// publisher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPublish {
    /// Rule that produced this publish — surfaced in logs +
    /// audit.
    pub rule_name: String,
    /// Final topic path (template rendered).
    pub topic: String,
    /// Final priority (verbatim from the rule).
    pub priority: Priority,
    /// Title (template rendered).
    pub title: String,
    /// Body (template rendered).
    pub body: String,
    /// BUS-2.8.topic-hours — the resolved quiet-hours window for
    /// the rule's topic. Default (no window) when the rule's
    /// PublishSpec didn't set `quiet_after` + `quiet_until`.
    pub quiet_hours: crate::dnd::TopicQuietHours,
}

/// Match errors that arise during evaluation.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MatchError {
    /// The adapter named in the request doesn't appear in the
    /// loaded config. Server returns 404.
    #[error("unknown adapter: {0}")]
    UnknownAdapter(String),
    /// The adapter's extractor returned `None` — the request
    /// doesn't fit its expected shape (e.g. missing required
    /// header). Server returns 400.
    #[error("adapter rejected request shape")]
    AdapterRejected,
    /// A template failed to render — referenced variable missing,
    /// syntax error, etc. Server returns 422.
    #[error("template render: {0}")]
    Render(String),
}

/// Run the full match flow against the request. Returns:
///
/// - `Ok(Some(rendered))` — a rule matched + rendered.
/// - `Ok(None)` — adapter accepted the shape but no rule fired
///   (server returns 204).
/// - `Err(MatchError::*)` — the request failed earlier.
///
/// # Errors
/// See [`MatchError`] for the per-case mapping.
pub fn match_request(
    adapter_name: &str,
    headers: &BTreeMap<String, String>,
    body: &Value,
    config: &HooksConfig,
    adapter: &dyn Adapter,
) -> Result<Option<RenderedPublish>, MatchError> {
    let adapter_cfg = config
        .adapters
        .get(adapter_name)
        .ok_or_else(|| MatchError::UnknownAdapter(adapter_name.to_string()))?;

    let (event, fields) = adapter
        .extract(headers, body)
        .ok_or(MatchError::AdapterRejected)?;

    for rule in &adapter_cfg.rules {
        if rule_matches(rule, &event, &fields) {
            return Ok(Some(render_rule(rule, &fields)?));
        }
    }
    Ok(None)
}

fn rule_matches(rule: &Rule, event: &str, fields: &BTreeMap<String, String>) -> bool {
    if let Some(expected) = &rule.r#match.event {
        if expected != event {
            return false;
        }
    }
    for (key, expected) in &rule.r#match.field {
        match fields.get(key) {
            Some(actual) if actual == expected => {}
            _ => return false,
        }
    }
    true
}

fn render_rule(
    rule: &Rule,
    fields: &BTreeMap<String, String>,
) -> Result<RenderedPublish, MatchError> {
    let topic = render(&rule.publish.topic, fields)?;
    let title = render(&rule.publish.title, fields)?;
    let body = render(&rule.publish.body, fields)?;
    Ok(RenderedPublish {
        rule_name: rule.name.clone(),
        topic,
        priority: rule.publish.priority,
        title,
        body,
        quiet_hours: rule.publish.quiet_hours(),
    })
}

/// Render a Tera template body against the extracted field map.
/// Pure helper — exposed for adapter-specific tests.
///
/// # Errors
/// Returns [`MatchError::Render`] when the template references a
/// missing variable or has a syntax error.
pub fn render(template: &str, fields: &BTreeMap<String, String>) -> Result<String, MatchError> {
    let mut ctx = tera::Context::new();
    for (k, v) in fields {
        ctx.insert(k, v);
    }
    tera::Tera::one_off(template, &ctx, false).map_err(|e| MatchError::Render(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::super::config::{AdapterConfig, Match, PublishSpec, Rule};
    use super::*;
    use serde_json::json;

    /// Test-only adapter — returns the headers as the event name +
    /// a single `key: value` field carried in the body.
    struct StubAdapter;
    impl Adapter for StubAdapter {
        fn extract(
            &self,
            headers: &BTreeMap<String, String>,
            body: &Value,
        ) -> Option<(String, BTreeMap<String, String>)> {
            let event = headers.get("x-event")?.clone();
            let mut fields = BTreeMap::new();
            if let Some(v) = body.get("key").and_then(Value::as_str) {
                fields.insert("key".to_string(), v.to_string());
            }
            Some((event, fields))
        }
    }

    fn cfg_with_one_rule(rule: Rule) -> HooksConfig {
        let mut cfg = HooksConfig::default();
        cfg.adapters
            .insert("stub".to_string(), AdapterConfig { rules: vec![rule] });
        cfg
    }

    fn simple_rule(event: &str) -> Rule {
        Rule {
            name: "r".to_string(),
            r#match: Match {
                event: Some(event.to_string()),
                field: BTreeMap::new(),
            },
            publish: PublishSpec {
                topic: "t/{{ key }}".to_string(),
                priority: Priority::Default,
                title: "title-{{ key }}".to_string(),
                body: "body-{{ key }}".to_string(),
                quiet_after: None,
                quiet_until: None,
            },
        }
    }

    #[test]
    fn unknown_adapter_returns_error() {
        let cfg = HooksConfig::default();
        let headers = BTreeMap::new();
        let body = json!({});
        let err = match_request("github", &headers, &body, &cfg, &StubAdapter).unwrap_err();
        assert_eq!(err, MatchError::UnknownAdapter("github".to_string()));
    }

    #[test]
    fn adapter_reject_returns_error() {
        let cfg = cfg_with_one_rule(simple_rule("push"));
        let headers = BTreeMap::new(); // no x-event
        let body = json!({});
        let err = match_request("stub", &headers, &body, &cfg, &StubAdapter).unwrap_err();
        assert_eq!(err, MatchError::AdapterRejected);
    }

    #[test]
    fn event_mismatch_returns_no_rule() {
        let cfg = cfg_with_one_rule(simple_rule("push"));
        let headers = BTreeMap::from([("x-event".to_string(), "ping".to_string())]);
        let body = json!({"key": "v"});
        let result = match_request("stub", &headers, &body, &cfg, &StubAdapter).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn event_match_renders_templates() {
        let cfg = cfg_with_one_rule(simple_rule("push"));
        let headers = BTreeMap::from([("x-event".to_string(), "push".to_string())]);
        let body = json!({"key": "abc"});
        let result = match_request("stub", &headers, &body, &cfg, &StubAdapter).unwrap();
        let r = result.expect("should match");
        assert_eq!(r.rule_name, "r");
        assert_eq!(r.topic, "t/abc");
        assert_eq!(r.title, "title-abc");
        assert_eq!(r.body, "body-abc");
    }

    #[test]
    fn first_matching_rule_wins() {
        let mut cfg = HooksConfig::default();
        let mut a = simple_rule("push");
        a.name = "first".to_string();
        a.publish.topic = "topic-first".to_string();
        let mut b = simple_rule("push");
        b.name = "second".to_string();
        b.publish.topic = "topic-second".to_string();
        cfg.adapters
            .insert("stub".to_string(), AdapterConfig { rules: vec![a, b] });
        let headers = BTreeMap::from([("x-event".to_string(), "push".to_string())]);
        let body = json!({"key": "x"});
        let r = match_request("stub", &headers, &body, &cfg, &StubAdapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.rule_name, "first");
        assert_eq!(r.topic, "topic-first");
    }

    #[test]
    fn field_predicate_narrows_match() {
        let mut rule = simple_rule("push");
        rule.r#match
            .field
            .insert("key".to_string(), "expected".to_string());
        let cfg = cfg_with_one_rule(rule);
        let headers = BTreeMap::from([("x-event".to_string(), "push".to_string())]);
        // Mismatch on the field
        let body = json!({"key": "other"});
        assert!(match_request("stub", &headers, &body, &cfg, &StubAdapter)
            .unwrap()
            .is_none());
        // Match on the field
        let body = json!({"key": "expected"});
        let r = match_request("stub", &headers, &body, &cfg, &StubAdapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.topic, "t/expected");
    }

    #[test]
    fn render_error_on_missing_field_propagates() {
        let mut rule = simple_rule("push");
        rule.publish.body = "{{ undeclared_var }}".to_string();
        let cfg = cfg_with_one_rule(rule);
        let headers = BTreeMap::from([("x-event".to_string(), "push".to_string())]);
        let body = json!({"key": "x"});
        let err = match_request("stub", &headers, &body, &cfg, &StubAdapter).unwrap_err();
        assert!(matches!(err, MatchError::Render(_)));
    }

    #[test]
    fn rule_with_no_event_predicate_matches_any_event() {
        let mut rule = simple_rule("push");
        rule.r#match.event = None;
        let cfg = cfg_with_one_rule(rule);
        let headers = BTreeMap::from([("x-event".to_string(), "ping".to_string())]);
        let body = json!({"key": "v"});
        let r = match_request("stub", &headers, &body, &cfg, &StubAdapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.topic, "t/v");
    }
}
