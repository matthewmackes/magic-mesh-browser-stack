//! BUS-1.5 — topic registry + slash-hierarchy topic names.
//!
//! Topic naming is locked to MQTT-style slash hierarchy (Round 3 of
//! the 104-Q poll): `fleet/sec`, `peer/laptop/alerts`, `mon/cpu`.
//! Wildcards `+` (single level) and `#` (multi-level) match the MQTT
//! 3.1.1 specification and are implemented in
//! [`crate::wildcard`].
//!
//! Self-serve creation per the open-mesh / flat-trust directive — any
//! peer with the mesh passcode publishes to a new name and the topic
//! exists. The registry is in-memory here; persistence lands in
//! BUS-1.4 (SQLite + file-tree).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One topic in the registry.
///
/// The `priority_default` and `retention` fields are seeded by
/// [`crate::seed`] for the 12 curated defaults; ad-hoc topics created
/// via [`Registry::create`] take the bus-wide fallback (`default`
/// priority, 7-day retention per Round 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Topic {
    /// Canonical slash-hierarchy name, e.g. `fleet/sec`.
    pub name: String,
    /// Human-readable purpose, surfaced in `mde-bus topic list` and
    /// in the Workbench Mesh > Bus > Topics tab (BUS-7.3).
    pub description: String,
    /// Default priority for publishes that do not specify one.
    pub priority_default: Priority,
    /// Retention TTL in seconds. `None` means "follow the bus-wide
    /// per-priority default" (Round 4):
    /// urgent = forever, high = 30d, default = 7d, min = 24h.
    pub retention_s: Option<u64>,
}

/// Bus priority ladder (Round 5).
///
/// `Min` is silent log only and produces no Dock-Breadcrumb segment
/// (Round 19 tension resolution). `Default` lands in tray + dock
/// badge. `High` opens the status-zone slide-up strip with sound.
/// `Urgent` triggers the Theater takeover + wallpaper stripe + phone
/// push.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Min,
    Default,
    High,
    Urgent,
}

impl Default for Priority {
    fn default() -> Self {
        Self::Default
    }
}

/// Errors a [`Registry`] operation can produce.
#[derive(Debug, thiserror::Error)]
pub enum TopicError {
    #[error("topic name is empty")]
    Empty,
    #[error(
        "topic name `{0}` contains a wildcard character (`+` or `#`); wildcards are subscription-side only"
    )]
    WildcardInName(String),
    #[error("topic name `{0}` contains an empty segment (double slash or leading/trailing slash)")]
    EmptySegment(String),
    #[error(
        "topic name `{0}` contains an invalid character (allowed: `[A-Za-z0-9_.-]` and `/` between segments)"
    )]
    InvalidChar(String),
    #[error("topic name `{0}` contains a path traversal segment")]
    PathTraversal(String),
    #[error("topic name `{0}` is longer than {1} bytes")]
    TooLong(String, usize),
}

/// Maximum byte length of a topic name. Mirrors the upstream ntfy
/// soft cap and keeps SQLite indexes (BUS-1.4) bounded.
pub const MAX_NAME_LEN: usize = 256;

// Note: the workspace-level `thiserror` would normally come from the
// root Cargo.toml's `[workspace.dependencies]`, but at the time of
// writing it's not declared there. The crate-local dep is added in
// the Cargo.toml above to keep changes scoped to BUS-1.

/// In-memory topic registry. Provides creation, lookup, listing, and
/// wildcard-driven subscription matching.
///
/// Persistence is intentionally out of scope here — BUS-1.4 layers
/// the SQLite + file-tree store on top via a save/load hook that
/// drains the registry into the index on shutdown and rehydrates it
/// on startup.
#[derive(Debug, Default)]
pub struct Registry {
    topics: BTreeMap<String, Topic>,
}

impl Registry {
    /// Create an empty registry. Call [`crate::seed::seed_defaults`]
    /// to populate the 12 curated topics.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate a topic name. Returns `Ok(())` if the name is a legal
    /// publish target (no wildcards, no empty segments, allowed
    /// chars, under the length cap). Subscriptions go through
    /// [`crate::wildcard::validate_pattern`] instead.
    pub fn validate_publish_name(name: &str) -> Result<(), TopicError> {
        if name.is_empty() {
            return Err(TopicError::Empty);
        }
        if name.len() > MAX_NAME_LEN {
            return Err(TopicError::TooLong(name.to_string(), MAX_NAME_LEN));
        }
        if name.contains('+') || name.contains('#') {
            return Err(TopicError::WildcardInName(name.to_string()));
        }
        if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
            return Err(TopicError::EmptySegment(name.to_string()));
        }
        if name.split('/').any(|seg| seg == "." || seg == "..") || name.contains("..") {
            return Err(TopicError::PathTraversal(name.to_string()));
        }
        let ok_char =
            |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/';
        if !name.chars().all(ok_char) {
            return Err(TopicError::InvalidChar(name.to_string()));
        }
        Ok(())
    }

    /// Create a new topic. Returns `Ok(true)` if the topic was added,
    /// `Ok(false)` if a topic with the same name already existed
    /// (idempotent — matches the BUS-1.6 second-launch-is-a-no-op
    /// exit criterion for the seed step).
    pub fn create(&mut self, topic: Topic) -> Result<bool, TopicError> {
        Self::validate_publish_name(&topic.name)?;
        if self.topics.contains_key(&topic.name) {
            return Ok(false);
        }
        self.topics.insert(topic.name.clone(), topic);
        Ok(true)
    }

    /// Auto-create a topic on first publish — matches the Round 3
    /// self-serve creation lock. Returns a reference to the (possibly
    /// freshly-created) topic. Equivalent to calling
    /// [`Self::create`] with a bare default if the topic did not
    /// exist.
    // perf-12: the untrusted `name` is validated by `validate_publish_name(name)?`
    // above; the trailing `.expect()` is a post-insert map invariant (we just inserted
    // the key when vacant, so `get` cannot be `None`) — a static invariant, not a
    // remote-reachable failure. Kept over an `entry()` rewrite to avoid allocating the
    // key string on the hot already-present path.
    #[allow(clippy::expect_used)]
    pub fn ensure(&mut self, name: &str) -> Result<&Topic, TopicError> {
        Self::validate_publish_name(name)?;
        // `entry().or_insert_with` would give us a `&mut Topic`, but
        // we want a `&Topic` for callers + we want the default body
        // only if the entry is vacant. Build it inline.
        if !self.topics.contains_key(name) {
            self.topics.insert(
                name.to_string(),
                Topic {
                    name: name.to_string(),
                    description: String::from("(auto-created on first publish)"),
                    priority_default: Priority::Default,
                    retention_s: None,
                },
            );
        }
        Ok(self
            .topics
            .get(name)
            .expect("just inserted or already present"))
    }

    /// Look up a topic by exact name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Topic> {
        self.topics.get(name)
    }

    /// Number of topics in the registry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.topics.len()
    }

    /// `true` when no topics are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.topics.is_empty()
    }

    /// Iterate every topic in lexicographic name order. Stable order
    /// is load-bearing for `mde-bus topic list` output + Workbench UI
    /// + idempotency tests on [`crate::seed::seed_defaults`].
    pub fn iter(&self) -> impl Iterator<Item = &Topic> {
        self.topics.values()
    }

    /// Subscribe a wildcard pattern and return the set of topics
    /// that match right now. The returned slice is sorted by topic
    /// name so test snapshots are deterministic.
    pub fn match_pattern(&self, pattern: &str) -> Vec<&Topic> {
        let mut out: Vec<&Topic> = self
            .topics
            .values()
            .filter(|t| crate::wildcard::matches(pattern, &t.name))
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_wildcards_in_publish_name() {
        assert!(matches!(
            Registry::validate_publish_name("fleet/+/alerts"),
            Err(TopicError::WildcardInName(_))
        ));
        assert!(matches!(
            Registry::validate_publish_name("fleet/#"),
            Err(TopicError::WildcardInName(_))
        ));
    }

    #[test]
    fn validate_rejects_empty_and_double_slash() {
        assert!(matches!(
            Registry::validate_publish_name(""),
            Err(TopicError::Empty)
        ));
        assert!(matches!(
            Registry::validate_publish_name("/fleet/sec"),
            Err(TopicError::EmptySegment(_))
        ));
        assert!(matches!(
            Registry::validate_publish_name("fleet//sec"),
            Err(TopicError::EmptySegment(_))
        ));
        assert!(matches!(
            Registry::validate_publish_name("fleet/sec/"),
            Err(TopicError::EmptySegment(_))
        ));
    }

    #[test]
    fn validate_rejects_invalid_chars() {
        assert!(matches!(
            Registry::validate_publish_name("fleet/sec!"),
            Err(TopicError::InvalidChar(_))
        ));
        // Underscore + dash + digits + DNS hostname dots are allowed.
        assert!(Registry::validate_publish_name("fleet/sec-1_a").is_ok());
        assert!(Registry::validate_publish_name("audit/localhost.localdomain").is_ok());
        assert!(Registry::validate_publish_name("peer/localhost.localdomain/alerts").is_ok());
    }

    #[test]
    fn validate_rejects_path_traversal_dots() {
        assert!(matches!(
            Registry::validate_publish_name("audit/../escape"),
            Err(TopicError::PathTraversal(_))
        ));
        assert!(matches!(
            Registry::validate_publish_name("audit/localhost..localdomain"),
            Err(TopicError::PathTraversal(_))
        ));
    }

    #[test]
    fn validate_rejects_too_long() {
        let long = "a/".repeat(200);
        assert!(matches!(
            Registry::validate_publish_name(&long),
            Err(TopicError::TooLong(_, MAX_NAME_LEN))
        ));
    }

    #[test]
    fn create_is_idempotent() {
        let mut r = Registry::new();
        let t = Topic {
            name: "fleet/sec".into(),
            description: "security".into(),
            priority_default: Priority::High,
            retention_s: Some(86_400),
        };
        assert_eq!(r.create(t.clone()).unwrap(), true);
        assert_eq!(r.create(t).unwrap(), false);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn ensure_creates_on_first_publish() {
        let mut r = Registry::new();
        let t = r.ensure("never/seen").unwrap();
        assert_eq!(t.name, "never/seen");
        assert_eq!(t.priority_default, Priority::Default);
        // Second call returns the existing topic without changes.
        let t2 = r.ensure("never/seen").unwrap();
        assert_eq!(t2.name, "never/seen");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn match_pattern_obeys_mqtt_wildcards() {
        let mut r = Registry::new();
        for name in [
            "fleet/sec",
            "fleet/info",
            "peer/laptop/alerts",
            "peer/kitchen/alerts",
            "mon/cpu",
        ] {
            r.ensure(name).unwrap();
        }
        // `+` matches exactly one level.
        let m: Vec<_> = r
            .match_pattern("peer/+/alerts")
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(m, vec!["peer/kitchen/alerts", "peer/laptop/alerts"]);
        // `#` matches all descendants.
        let m: Vec<_> = r
            .match_pattern("fleet/#")
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(m, vec!["fleet/info", "fleet/sec"]);
        // Exact match still works.
        let m: Vec<_> = r
            .match_pattern("mon/cpu")
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(m, vec!["mon/cpu"]);
    }

    #[test]
    fn iter_is_lexicographic() {
        let mut r = Registry::new();
        for n in ["zzz/a", "aaa/b", "mmm/c"] {
            r.ensure(n).unwrap();
        }
        let names: Vec<_> = r.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["aaa/b", "mmm/c", "zzz/a"]);
    }
}
