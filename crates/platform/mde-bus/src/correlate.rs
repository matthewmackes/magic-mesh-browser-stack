//! BUS-6.5 — cross-topic correlation engine.
//!
//! Synthesizes new topics when multiple source topics fire within a
//! window. Example rule:
//!
//! ```yaml
//! rules:
//!   - name: likely-power-outage
//!     sources: [power/ups/grid-loss, network/wan-down]
//!     window_seconds: 60
//!     emits: incident/likely-power-outage
//!     priority: high
//! ```
//!
//! When BOTH `power/ups/grid-loss` AND `network/wan-down` publish
//! within 60 s of each other, the engine fires a synthesized
//! publish on `incident/likely-power-outage` at high priority.
//!
//! Operator config lives at `~/.config/mde/bus-correlate.yaml`
//! (per the BUS-6.5 design lock — distinct from bus_root which is
//! GFS-mesh-synced state).
//!
//! ## Ships in BUS-6.5.parser
//!
//! - [`CorrelateRule`] schema (deny-unknown-fields YAML)
//! - [`CorrelateConfig`] top-level container
//! - [`SlidingWindow`] per-topic recent-observation tracker
//! - [`evaluate_rule`] pure-fn — given a rule + window + now,
//!   returns `Some(emission)` when every source has fired inside
//!   the window, `None` otherwise.
//! - [`load_default`] reads `~/.config/mde/bus-correlate.yaml`.
//!
//! ## Future
//!
//! BUS-6.5.evaluator wires this into the publish flow: every
//! publish updates the per-topic SlidingWindow, then evaluates
//! every rule whose source-set contains the published topic;
//! firing rules synthesize a fresh `mde-bus publish <emits>`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::hooks::config::Priority;

/// Top-level `bus-correlate.yaml` shape.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrelateConfig {
    /// Rules evaluated in declaration order. Each rule is
    /// independent — multiple rules can fire on a single publish
    /// if their predicates overlap.
    #[serde(default)]
    pub rules: Vec<CorrelateRule>,
}

/// One correlation rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorrelateRule {
    /// Human-readable name for logs + audit.
    pub name: String,
    /// Source topics — ALL must have fired within `window_seconds`
    /// for the rule to emit.
    pub sources: Vec<String>,
    /// Window length, in seconds. A source's last-seen timestamp
    /// older than `now - window_seconds` is treated as not-fired.
    pub window_seconds: u32,
    /// Topic the synthesized publish lands on.
    pub emits: String,
    /// Priority of the synthesized publish.
    #[serde(default)]
    pub priority: Priority,
}

/// Per-topic last-observed timestamp tracker. Operator-process-
/// lifetime in-memory state; doesn't survive a daemon restart
/// (intentional — synthesized incidents over a fleet restart
/// would be noise).
#[derive(Debug, Default, Clone)]
pub struct SlidingWindow {
    /// Topic → wall-clock timestamp (milliseconds since Unix
    /// epoch) of the most recent observation.
    last_seen: BTreeMap<String, i64>,
}

impl SlidingWindow {
    /// Record that `topic` was just observed at `now_unix_ms`.
    pub fn observe(&mut self, topic: &str, now_unix_ms: i64) {
        self.last_seen.insert(topic.to_string(), now_unix_ms);
    }

    /// Return the last-observed timestamp for `topic`, or `None`
    /// when the topic has never been observed (or has aged out).
    /// This helper doesn't age-out by itself — that's the
    /// `evaluate_rule` caller's job to compare against
    /// `window_seconds`.
    #[must_use]
    pub fn last_seen(&self, topic: &str) -> Option<i64> {
        self.last_seen.get(topic).copied()
    }

    /// Count of distinct topics tracked. Used in tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.last_seen.len()
    }

    /// True when no topic has been observed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last_seen.is_empty()
    }
}

/// One synthesized emission from a fired rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedEmission {
    /// The rule that fired — surfaced in audit.
    pub rule_name: String,
    /// Synthesized topic to publish on.
    pub topic: String,
    /// Priority of the synthesized publish.
    pub priority: Priority,
}

/// Pure-fn — evaluate one rule against the live sliding window.
/// Returns `Some(emission)` when every `source` topic has fired
/// at or after `now_unix_ms - window_seconds * 1000`. Empty
/// `sources` returns `None` (an always-firing rule is a config
/// error). Missing observations for any source return `None`.
#[must_use]
pub fn evaluate_rule(
    rule: &CorrelateRule,
    window: &SlidingWindow,
    now_unix_ms: i64,
) -> Option<SynthesizedEmission> {
    if rule.sources.is_empty() {
        return None;
    }
    let cutoff_ms = now_unix_ms - i64::from(rule.window_seconds) * 1000;
    for src in &rule.sources {
        match window.last_seen(src) {
            Some(ts) if ts >= cutoff_ms => continue,
            _ => return None,
        }
    }
    Some(SynthesizedEmission {
        rule_name: rule.name.clone(),
        topic: rule.emits.clone(),
        priority: rule.priority,
    })
}

/// Default operator-config path:
/// `$XDG_CONFIG_HOME/mde/bus-correlate.yaml` (falls back to
/// `$HOME/.config/mde/bus-correlate.yaml`).
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("mde").join("bus-correlate.yaml"))
}

/// Load the correlation config from `path`. Returns:
/// - `Ok(config)` on successful parse.
/// - `Ok(default)` when the file is missing (operators may not
///   have configured correlation rules; that's not an error).
/// - `Err(CorrelateLoadError)` on read or parse failure.
///
/// # Errors
/// Returns [`CorrelateLoadError::Read`] when the file exists but
/// cannot be read; [`CorrelateLoadError::Parse`] when the YAML is
/// malformed.
pub fn load_default(path: &std::path::Path) -> Result<CorrelateConfig, CorrelateLoadError> {
    if !path.exists() {
        return Ok(CorrelateConfig::default());
    }
    let body = std::fs::read_to_string(path)
        .map_err(|e| CorrelateLoadError::Read(format!("{}: {e}", path.display())))?;
    serde_yaml::from_str(&body).map_err(|e| CorrelateLoadError::Parse(e.to_string()))
}

/// One validation finding produced by [`validate_config`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// Zero-based rule index in the config's `rules:` list. `None`
    /// for cross-rule issues (e.g. duplicate rule names where the
    /// finding spans multiple entries).
    pub rule_index: Option<usize>,
    /// Rule name when the issue is per-rule; empty string when
    /// `rule_index` is None.
    pub rule_name: String,
    /// Human-readable problem description, surfaced verbatim to
    /// the operator via the CLI's `validate` verb.
    pub message: String,
}

/// Pure-fn — walk every rule + flag common configuration problems.
/// Returns an empty Vec when the config is clean. Issues are
/// returned in declaration order so the operator sees them
/// surface-by-surface.
///
/// Caught classes:
///   - Empty `name` (rule headers in templates / audit need a non-
///     empty identifier).
///   - Empty `sources` list (a rule with no sources can never
///     fire; almost always a YAML typo).
///   - Empty `emits` (synthesized publish would land on the empty
///     topic).
///   - `window_seconds == 0` (zero-window rules require all
///     sources to fire in the same millisecond — usable as an edge
///     case via [`evaluate_rule_zero_window_requires_exact_now`]
///     test fixture, but in operator config this is almost always
///     a typo).
///   - Duplicate rule names (audit + log lines key on
///     `rule_name` — two rules with the same name make the audit
///     trail ambiguous).
#[must_use]
pub fn validate_config(cfg: &CorrelateConfig) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    // Per-rule checks.
    for (i, rule) in cfg.rules.iter().enumerate() {
        if rule.name.is_empty() {
            issues.push(ValidationIssue {
                rule_index: Some(i),
                rule_name: rule.name.clone(),
                message: "rule.name is empty".to_string(),
            });
        }
        if rule.sources.is_empty() {
            issues.push(ValidationIssue {
                rule_index: Some(i),
                rule_name: rule.name.clone(),
                message: "rule.sources is empty (rule can never fire)".to_string(),
            });
        }
        if rule.emits.is_empty() {
            issues.push(ValidationIssue {
                rule_index: Some(i),
                rule_name: rule.name.clone(),
                message: "rule.emits is empty (synthesized topic would be the empty string)"
                    .to_string(),
            });
        }
        if rule.window_seconds == 0 {
            issues.push(ValidationIssue {
                rule_index: Some(i),
                rule_name: rule.name.clone(),
                message: "rule.window_seconds is 0 (requires all sources in the same millisecond)"
                    .to_string(),
            });
        }
    }
    // Cross-rule: duplicate names.
    let mut seen: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, rule) in cfg.rules.iter().enumerate() {
        if !rule.name.is_empty() {
            seen.entry(rule.name.clone()).or_default().push(i);
        }
    }
    for (name, indices) in seen {
        if indices.len() > 1 {
            issues.push(ValidationIssue {
                rule_index: None,
                rule_name: name.clone(),
                message: format!(
                    "duplicate rule name {name:?} at indices {:?} — audit trail would be ambiguous",
                    indices,
                ),
            });
        }
    }
    issues
}

/// Errors loading the correlation config.
#[derive(Debug)]
pub enum CorrelateLoadError {
    /// Filesystem read failed (permission, encoding, etc.).
    Read(String),
    /// YAML parse failed.
    Parse(String),
}

impl std::fmt::Display for CorrelateLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "correlate config read: {e}"),
            Self::Parse(e) => write!(f, "correlate config parse: {e}"),
        }
    }
}

impl std::error::Error for CorrelateLoadError {}

// ─────────────────────────────────────────────────────────────────────
// BUS-6.5.evaluator — daemon-side wiring.
// ─────────────────────────────────────────────────────────────────────

/// Default poll cadence for the evaluator loop. The evaluator
/// reads the per-peer SQLite index incrementally (cheap index-range
/// scan per source topic), so a 2 s tick keeps synthesized incidents
/// near-real-time without hammering the index. Well under the
/// shortest meaningful `window_seconds` an operator would configure.
pub const DEFAULT_EVAL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Title prefix stamped on every synthesized publish. Acts as the
/// `correlate=<rule-name>` cycle-protection marker per the BUS-6.5
/// design lock: the evaluator skips observing any message whose
/// title carries this prefix, so a synth-emit landing on a topic
/// that is also a rule source can't feed back into the window and
/// loop. Operator-visible + informative ("this is a correlated
/// incident") rather than hidden machine junk.
pub const SYNTH_TITLE_PREFIX: &str = "[correlate] ";

/// Lowercase CLI-arg form of a priority (matches the `mde-bus
/// publish --priority` accepted values).
const fn priority_arg(p: Priority) -> &'static str {
    match p {
        Priority::Min => "min",
        Priority::Default => "default",
        Priority::High => "high",
        Priority::Urgent => "urgent",
    }
}

/// True when `title` carries the synth-publish marker — i.e. the
/// message was emitted by the correlation engine itself, not by an
/// operator / adapter. Such messages are skipped from window
/// observation so they can't re-trigger a rule (cycle protection).
#[must_use]
pub fn is_synth_marker_title(title: Option<&str>) -> bool {
    title.is_some_and(|t| t.starts_with(SYNTH_TITLE_PREFIX))
}

/// Build the synth-publish title for `rule_name` — the marker
/// prefix plus the rule name, so the notification reads
/// `[correlate] likely-power-outage`.
#[must_use]
pub fn synth_marker_title(rule_name: &str) -> String {
    format!("{SYNTH_TITLE_PREFIX}{rule_name}")
}

/// Stateful correlation evaluator. Holds the rule set, the live
/// per-topic [`SlidingWindow`], a per-source-topic ULID cursor for
/// incremental polling, and a per-rule last-fire timestamp for
/// cooldown gating.
///
/// Process-lifetime in-memory state — a daemon restart resets the
/// window + cursors (intentional: synthesized incidents spanning a
/// restart would be noise, per the [`SlidingWindow`] doc lock).
#[derive(Debug)]
pub struct CorrelateEvaluator {
    config: CorrelateConfig,
    window: SlidingWindow,
    /// `rule.name` → last-fire wall-clock ms. Gates re-fire within
    /// the rule's `window_seconds` cooldown.
    last_fired: BTreeMap<String, i64>,
    /// source topic → last-consumed ULID. `list_since` resumes from
    /// here so each message is observed exactly once.
    cursors: BTreeMap<String, String>,
}

impl CorrelateEvaluator {
    /// Build an evaluator from a loaded config. The window + cursors
    /// start empty; the first `poll_once` primes them.
    #[must_use]
    pub fn new(config: CorrelateConfig) -> Self {
        Self {
            config,
            window: SlidingWindow::default(),
            last_fired: BTreeMap::new(),
            cursors: BTreeMap::new(),
        }
    }

    /// `true` when no rules are configured — the daemon can idle the
    /// evaluator loop entirely rather than poll the index for nothing.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.config.rules.is_empty()
    }

    /// Distinct set of source topics across every rule. Drives which
    /// topics `poll_once` scans the index for.
    fn source_topics(&self) -> std::collections::BTreeSet<String> {
        self.config
            .rules
            .iter()
            .flat_map(|r| r.sources.iter().cloned())
            .collect()
    }

    /// One evaluation pass. For each rule source topic, reads new
    /// messages from the persist index since the last cursor, observes
    /// each non-synth message into the window, then evaluates every
    /// rule against the freshened window. Returns the emissions whose
    /// cooldown has elapsed (the caller performs the actual publish).
    ///
    /// Synth-emits (messages carrying [`SYNTH_TITLE_PREFIX`]) advance
    /// the cursor but are NOT observed — that's the cycle break.
    pub fn poll_once(
        &mut self,
        persist: &crate::persist::Persist,
        now_unix_ms: i64,
    ) -> Vec<SynthesizedEmission> {
        for topic in self.source_topics() {
            let cursor = self.cursors.get(&topic).cloned();
            let new_msgs = match persist.list_since(&topic, cursor.as_deref()) {
                Ok(m) => m,
                Err(e) => {
                    tracing::debug!(
                        target: "mde_bus::correlate",
                        %topic,
                        error = %e,
                        "list_since failed; skipping topic this tick"
                    );
                    continue;
                }
            };
            for msg in new_msgs {
                // Advance the cursor unconditionally so we never
                // re-read this message — even when we skip observing
                // it below.
                self.cursors.insert(topic.clone(), msg.ulid.clone());
                if is_synth_marker_title(msg.title.as_deref()) {
                    // Cycle protection: a synth-emit on a source topic
                    // must not feed the window.
                    continue;
                }
                self.window.observe(&msg.topic, msg.ts_unix_ms);
            }
        }

        let mut emissions = Vec::new();
        for rule in &self.config.rules {
            let Some(emission) = evaluate_rule(rule, &self.window, now_unix_ms) else {
                continue;
            };
            // Cooldown: window_seconds doubles as the re-fire gate so
            // a sustained alignment emits once per window, not once
            // per tick.
            let cooldown_ms = i64::from(rule.window_seconds) * 1000;
            if let Some(&last) = self.last_fired.get(&rule.name) {
                if now_unix_ms - last < cooldown_ms {
                    continue;
                }
            }
            self.last_fired.insert(rule.name.clone(), now_unix_ms);
            emissions.push(emission);
        }
        emissions
    }
}

/// Shell out to `mde-bus publish` for one synthesized emission.
/// Mirrors the BUS-4.3 dual-write pattern: the synth-publish goes
/// through the same persist + audit + ntfy path as an operator
/// publish, so the incident shows up in `mde-bus audit list` and on
/// every peer's file-tree subscription. Best-effort — a publish
/// failure logs + continues (the daemon must not crash because a
/// child `mde-bus` invocation hiccuped).
fn synth_publish(emission: &SynthesizedEmission) {
    // Re-invoke our own binary so the child is the same `mde-bus`
    // build the daemon is running, without a PATH lookup.
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("mde-bus"));
    let title = synth_marker_title(&emission.rule_name);
    let body = format!(
        "Correlation rule '{}' fired — all source topics observed within the window.",
        emission.rule_name
    );
    let result = std::process::Command::new(exe)
        .arg("publish")
        .arg(&emission.topic)
        .arg("--priority")
        .arg(priority_arg(emission.priority))
        .arg("--title")
        .arg(&title)
        .arg("--body-flag")
        .arg(&body)
        .status();
    match result {
        Ok(s) if s.success() => tracing::info!(
            target: "mde_bus::correlate",
            rule = %emission.rule_name,
            topic = %emission.topic,
            "synthesized correlation publish"
        ),
        Ok(s) => tracing::warn!(
            target: "mde_bus::correlate",
            rule = %emission.rule_name,
            topic = %emission.topic,
            code = ?s.code(),
            "synth publish exited non-zero — incident not surfaced"
        ),
        Err(e) => tracing::warn!(
            target: "mde_bus::correlate",
            rule = %emission.rule_name,
            error = %e,
            "mde-bus binary not invocable — synth publish skipped"
        ),
    }
}

/// Daemon evaluator loop. Polls the persist index every `interval`,
/// fires synth-publishes for rules whose cooldown has elapsed, and
/// exits cleanly on the shutdown signal. Idles (no index reads) when
/// the config carries zero rules.
pub async fn run_evaluator_loop(
    config: CorrelateConfig,
    bus_root: PathBuf,
    interval: std::time::Duration,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut evaluator = CorrelateEvaluator::new(config);
    if evaluator.is_idle() {
        // No rules — park until shutdown rather than spin the index.
        let _ = shutdown_rx.changed().await;
        return;
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Skip the immediate first tick so the daemon finishes startup
    // before the first index read.
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => return,
            _ = ticker.tick() => {
                let persist = match crate::persist::Persist::open(bus_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::debug!(
                            target: "mde_bus::correlate",
                            error = %e,
                            "persist open failed; skipping evaluator tick"
                        );
                        continue;
                    }
                };
                for emission in evaluator.poll_once(&persist, current_unix_ms()) {
                    synth_publish(&emission);
                }
            }
        }
    }
}

/// Wall-clock milliseconds since the Unix epoch.
fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_parses() {
        let cfg: CorrelateConfig = serde_yaml::from_str("rules: []").unwrap();
        assert_eq!(cfg.rules.len(), 0);
    }

    #[test]
    fn sample_rule_round_trips() {
        let yaml = r#"
rules:
  - name: likely-power-outage
    sources:
      - power/ups/grid-loss
      - network/wan-down
    window_seconds: 60
    emits: incident/likely-power-outage
    priority: high
"#;
        let cfg: CorrelateConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        let rule = &cfg.rules[0];
        assert_eq!(rule.name, "likely-power-outage");
        assert_eq!(rule.sources.len(), 2);
        assert_eq!(rule.window_seconds, 60);
        assert_eq!(rule.emits, "incident/likely-power-outage");
    }

    #[test]
    fn rejects_unknown_fields() {
        let yaml = r#"
rules:
  - name: r
    sources: [a]
    window_seconds: 60
    emits: x
    unexpected_field: oops
"#;
        let err = serde_yaml::from_str::<CorrelateConfig>(yaml).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown field") || msg.contains("unexpected_field"));
    }

    #[test]
    fn sliding_window_observe_and_lookup() {
        let mut w = SlidingWindow::default();
        assert!(w.is_empty());
        w.observe("a", 100);
        w.observe("b", 200);
        assert_eq!(w.last_seen("a"), Some(100));
        assert_eq!(w.last_seen("b"), Some(200));
        assert!(w.last_seen("c").is_none());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn sliding_window_observe_updates_existing_entry() {
        let mut w = SlidingWindow::default();
        w.observe("a", 100);
        w.observe("a", 250);
        // Updated value wins.
        assert_eq!(w.last_seen("a"), Some(250));
        assert_eq!(w.len(), 1);
    }

    fn sample_rule() -> CorrelateRule {
        CorrelateRule {
            name: "likely-power-outage".to_string(),
            sources: vec![
                "power/ups/grid-loss".to_string(),
                "network/wan-down".to_string(),
            ],
            window_seconds: 60,
            emits: "incident/likely-power-outage".to_string(),
            priority: Priority::High,
        }
    }

    #[test]
    fn evaluate_rule_fires_when_both_sources_within_window() {
        let rule = sample_rule();
        let mut w = SlidingWindow::default();
        let now = 1_700_000_000_000;
        // Both sources fired 30 s ago — inside the 60 s window.
        w.observe("power/ups/grid-loss", now - 30_000);
        w.observe("network/wan-down", now - 15_000);
        let emission = evaluate_rule(&rule, &w, now).expect("fires");
        assert_eq!(emission.rule_name, "likely-power-outage");
        assert_eq!(emission.topic, "incident/likely-power-outage");
        assert_eq!(emission.priority, Priority::High);
    }

    #[test]
    fn evaluate_rule_no_fire_when_source_outside_window() {
        let rule = sample_rule();
        let mut w = SlidingWindow::default();
        let now = 1_700_000_000_000;
        w.observe("power/ups/grid-loss", now - 30_000);
        // network/wan-down fired 90 s ago — beyond the 60 s window.
        w.observe("network/wan-down", now - 90_000);
        assert!(evaluate_rule(&rule, &w, now).is_none());
    }

    #[test]
    fn evaluate_rule_no_fire_when_source_missing() {
        let rule = sample_rule();
        let mut w = SlidingWindow::default();
        let now = 1_700_000_000_000;
        // Only one of the two sources observed.
        w.observe("power/ups/grid-loss", now - 30_000);
        assert!(evaluate_rule(&rule, &w, now).is_none());
    }

    #[test]
    fn evaluate_rule_no_fire_on_empty_sources() {
        let rule = CorrelateRule {
            name: "always-fire-bug".to_string(),
            sources: vec![],
            window_seconds: 60,
            emits: "x".to_string(),
            priority: Priority::Default,
        };
        let w = SlidingWindow::default();
        // Empty source set returns None — an always-firing rule
        // is a config error, not a feature.
        assert!(evaluate_rule(&rule, &w, 0).is_none());
    }

    #[test]
    fn evaluate_rule_zero_window_requires_exact_now() {
        let rule = CorrelateRule {
            name: "instant".to_string(),
            sources: vec!["a".to_string()],
            window_seconds: 0,
            emits: "x".to_string(),
            priority: Priority::Default,
        };
        let mut w = SlidingWindow::default();
        let now = 1_700_000_000_000;
        // Observation at exactly `now` → cutoff = now - 0 = now;
        // ts >= cutoff → fires.
        w.observe("a", now);
        assert!(evaluate_rule(&rule, &w, now).is_some());
        // Observation 1 ms ago → cutoff still now; ts < cutoff
        // → no fire.
        let mut w2 = SlidingWindow::default();
        w2.observe("a", now - 1);
        assert!(evaluate_rule(&rule, &w2, now).is_none());
    }

    #[test]
    fn load_default_missing_file_returns_default() {
        let p = std::path::Path::new("/nonexistent/path/bus-correlate.yaml");
        let cfg = load_default(p).unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn validate_clean_config_returns_empty() {
        let cfg = CorrelateConfig {
            rules: vec![sample_rule()],
        };
        assert!(validate_config(&cfg).is_empty());
    }

    #[test]
    fn validate_empty_sources_flags_issue() {
        let cfg = CorrelateConfig {
            rules: vec![CorrelateRule {
                name: "bad".to_string(),
                sources: vec![],
                window_seconds: 60,
                emits: "x".to_string(),
                priority: Priority::Default,
            }],
        };
        let issues = validate_config(&cfg);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("rule.sources is empty"));
        assert_eq!(issues[0].rule_index, Some(0));
        assert_eq!(issues[0].rule_name, "bad");
    }

    #[test]
    fn validate_empty_emits_flags_issue() {
        let cfg = CorrelateConfig {
            rules: vec![CorrelateRule {
                name: "bad".to_string(),
                sources: vec!["a".to_string()],
                window_seconds: 60,
                emits: String::new(),
                priority: Priority::Default,
            }],
        };
        let issues = validate_config(&cfg);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("rule.emits is empty")));
    }

    #[test]
    fn validate_zero_window_flags_issue() {
        let cfg = CorrelateConfig {
            rules: vec![CorrelateRule {
                name: "bad".to_string(),
                sources: vec!["a".to_string()],
                window_seconds: 0,
                emits: "x".to_string(),
                priority: Priority::Default,
            }],
        };
        let issues = validate_config(&cfg);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("window_seconds is 0")));
    }

    #[test]
    fn validate_duplicate_names_flags_issue() {
        let cfg = CorrelateConfig {
            rules: vec![
                sample_rule(),
                sample_rule(), // same name as the first
            ],
        };
        let issues = validate_config(&cfg);
        // 1 issue: duplicate-name (the rules themselves are valid).
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("duplicate rule name"));
        assert!(issues[0].rule_index.is_none());
    }

    #[test]
    fn validate_empty_name_flags_issue_but_doesnt_dup_track() {
        let cfg = CorrelateConfig {
            rules: vec![
                CorrelateRule {
                    name: String::new(),
                    sources: vec!["a".to_string()],
                    window_seconds: 60,
                    emits: "x".to_string(),
                    priority: Priority::Default,
                },
                CorrelateRule {
                    name: String::new(),
                    sources: vec!["b".to_string()],
                    window_seconds: 60,
                    emits: "y".to_string(),
                    priority: Priority::Default,
                },
            ],
        };
        let issues = validate_config(&cfg);
        // 2 issues (one per empty-name rule); empty names are
        // explicitly excluded from the duplicate-name check so
        // operators see the "name is empty" finding, not a
        // confusing "duplicate empty name" finding.
        assert_eq!(issues.len(), 2);
        assert!(issues
            .iter()
            .all(|i| i.message.contains("rule.name is empty")));
    }

    #[test]
    fn load_default_round_trips() {
        let tmp =
            std::env::temp_dir().join(format!("mde-bus-correlate-load-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("bus-correlate.yaml");
        std::fs::write(
            &path,
            "rules:\n  - name: r\n    sources: [a, b]\n    window_seconds: 60\n    emits: x\n",
        )
        .unwrap();
        let cfg = load_default(&path).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].sources, vec!["a".to_string(), "b".to_string()]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── BUS-6.5.evaluator tests ────────────────────────────────────────

    fn eval_rule(name: &str, sources: &[&str], window: u32, emits: &str) -> CorrelateRule {
        CorrelateRule {
            name: name.to_string(),
            sources: sources.iter().map(|s| s.to_string()).collect(),
            window_seconds: window,
            emits: emits.to_string(),
            priority: Priority::High,
        }
    }

    fn cfg_with(rules: Vec<CorrelateRule>) -> CorrelateConfig {
        CorrelateConfig { rules }
    }

    /// Build a Persist over a tmpdir + write each `(topic, title, ts_ms)`
    /// message, back-dating its `ts_unix_ms` via a direct SQLite UPDATE
    /// so window membership is deterministic against the test clock.
    /// Returns the keep-alive tempdir + the Persist handle.
    fn persist_with(
        msgs: &[(&str, Option<&str>, i64)],
    ) -> (tempfile::TempDir, crate::persist::Persist) {
        let tmp = tempfile::tempdir().unwrap();
        let p = crate::persist::Persist::open(tmp.path().to_path_buf()).unwrap();
        for (topic, title, ts) in msgs {
            let m = p
                .write(topic, Priority::Default, *title, Some("body"))
                .unwrap();
            let conn = rusqlite::Connection::open(tmp.path().join("index.sqlite")).unwrap();
            conn.execute(
                "UPDATE messages SET ts_unix_ms = ?1 WHERE ulid = ?2",
                rusqlite::params![ts, m.ulid],
            )
            .unwrap();
        }
        (tmp, p)
    }

    #[test]
    fn marker_round_trips() {
        let t = synth_marker_title("likely-power-outage");
        assert_eq!(t, "[correlate] likely-power-outage");
        assert!(is_synth_marker_title(Some(&t)));
        assert!(!is_synth_marker_title(Some("Grid loss detected")));
        assert!(!is_synth_marker_title(None));
    }

    #[test]
    fn evaluator_is_idle_with_no_rules() {
        let e = CorrelateEvaluator::new(CorrelateConfig::default());
        assert!(e.is_idle());
        let e2 = CorrelateEvaluator::new(cfg_with(vec![eval_rule("r", &["a"], 60, "x")]));
        assert!(!e2.is_idle());
    }

    #[test]
    fn evaluator_fires_when_both_sources_within_window() {
        // Bench-acceptance mirror: both source topics publish within
        // the 60 s window → the rule emits.
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[
            ("power/ups/grid-loss", None, now - 5_000),
            ("network/wan-down", None, now - 3_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        let out = e.poll_once(&p, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, "incident/likely-power-outage");
        assert_eq!(out[0].rule_name, "likely-power-outage");
        assert_eq!(out[0].priority, Priority::High);
    }

    #[test]
    fn evaluator_no_fire_with_only_one_source() {
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[("power/ups/grid-loss", None, now - 5_000)]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        assert!(e.poll_once(&p, now).is_empty());
    }

    #[test]
    fn evaluator_no_fire_when_source_outside_window() {
        let now = 1_000_000_000_000_i64;
        // grid-loss is 90 s old — outside the 60 s window.
        let (_tmp, p) = persist_with(&[
            ("power/ups/grid-loss", None, now - 90_000),
            ("network/wan-down", None, now - 3_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        assert!(e.poll_once(&p, now).is_empty());
    }

    #[test]
    fn evaluator_cooldown_blocks_rapid_refire() {
        // Spec: second fire on rapid re-trigger only emits one
        // synthesized publish per cooldown window.
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[
            ("power/ups/grid-loss", None, now - 5_000),
            ("network/wan-down", None, now - 3_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        // First poll fires.
        assert_eq!(e.poll_once(&p, now).len(), 1);
        // Immediate re-poll (1 s later, well inside the 60 s
        // cooldown) — sources still in-window, but cooldown blocks.
        assert!(e.poll_once(&p, now + 1_000).is_empty());
    }

    #[test]
    fn evaluator_synth_emit_marker_skipped() {
        // A synth-marked message on a source topic must NOT be
        // observed — otherwise a chained rule could loop. Here the
        // ONLY message on grid-loss carries the marker, so the rule
        // never sees two live sources.
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[
            (
                "power/ups/grid-loss",
                Some("[correlate] upstream-rule"),
                now - 5_000,
            ),
            ("network/wan-down", None, now - 3_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        assert!(
            e.poll_once(&p, now).is_empty(),
            "synth-marked source message must not contribute to firing"
        );
    }

    #[test]
    fn evaluator_real_publish_on_marked_topic_still_observed() {
        // Two messages on grid-loss: one synth-marked (skipped) + one
        // real (observed). The real one keeps the source live, so the
        // rule fires alongside a live wan-down.
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[
            (
                "power/ups/grid-loss",
                Some("[correlate] noise"),
                now - 50_000,
            ),
            ("power/ups/grid-loss", Some("Grid loss"), now - 4_000),
            ("network/wan-down", None, now - 3_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "likely-power-outage",
            &["power/ups/grid-loss", "network/wan-down"],
            60,
            "incident/likely-power-outage",
        )]));
        assert_eq!(e.poll_once(&p, now).len(), 1);
    }

    #[test]
    fn evaluator_independent_rules_both_fire() {
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[
            ("a", None, now - 1_000),
            ("b", None, now - 1_000),
            ("c", None, now - 1_000),
            ("d", None, now - 1_000),
        ]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![
            eval_rule("ab", &["a", "b"], 60, "incident/ab"),
            eval_rule("cd", &["c", "d"], 60, "incident/cd"),
        ]));
        let out = e.poll_once(&p, now);
        assert_eq!(out.len(), 2);
        let topics: std::collections::BTreeSet<_> = out.iter().map(|e| e.topic.as_str()).collect();
        assert!(topics.contains("incident/ab"));
        assert!(topics.contains("incident/cd"));
    }

    #[test]
    fn evaluator_cursor_advances_no_double_observe() {
        // Two polls with no new messages between them: the cursor
        // means the second poll reads nothing new. The window state
        // persists, so cooldown (not re-observation) is what governs
        // the second poll's outcome.
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[("a", None, now - 1_000), ("b", None, now - 1_000)]);
        let mut e = CorrelateEvaluator::new(cfg_with(vec![eval_rule(
            "ab",
            &["a", "b"],
            60,
            "incident/ab",
        )]));
        assert_eq!(e.poll_once(&p, now).len(), 1);
        // Cursors now point past both messages; nothing new to read.
        assert!(e.poll_once(&p, now + 500).is_empty());
    }

    #[test]
    fn evaluator_empty_sources_rule_never_fires() {
        let now = 1_000_000_000_000_i64;
        let (_tmp, p) = persist_with(&[("a", None, now - 1_000)]);
        let mut e =
            CorrelateEvaluator::new(cfg_with(vec![eval_rule("bad", &[], 60, "incident/bad")]));
        assert!(e.poll_once(&p, now).is_empty());
    }

    // ── EPIC-BUS-EXT-CORRELATION-5 — shipped sample-rules template ──────

    /// The 5-rule reference template shipped to
    /// `/usr/share/mde/bus/correlate.yaml.tmpl`. Embedded at compile
    /// time so a broken example fails the build, not just a bench run.
    const SAMPLE_TEMPLATE: &str = include_str!("../../../../data/bus/correlate.yaml.tmpl");

    #[test]
    fn shipped_template_parses_to_five_rules() {
        let cfg: CorrelateConfig = serde_yaml::from_str(SAMPLE_TEMPLATE).unwrap();
        assert_eq!(cfg.rules.len(), 5, "template must ship exactly 5 rules");
        let names: std::collections::BTreeSet<_> =
            cfg.rules.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "power-outage",
            "disk-pressure",
            "mesh-degraded",
            "vpn-flap",
            "meshfs-quota-trending",
        ] {
            assert!(names.contains(expected), "missing rule: {expected}");
        }
    }

    #[test]
    fn shipped_template_passes_validation() {
        // The examples operators copy must be clean — no empty
        // names/sources/emits, no zero windows, no duplicate names.
        let cfg: CorrelateConfig = serde_yaml::from_str(SAMPLE_TEMPLATE).unwrap();
        let issues = validate_config(&cfg);
        assert!(
            issues.is_empty(),
            "shipped template has validation issues: {issues:?}"
        );
    }

    #[test]
    fn shipped_template_rules_are_well_formed() {
        let cfg: CorrelateConfig = serde_yaml::from_str(SAMPLE_TEMPLATE).unwrap();
        for r in &cfg.rules {
            // Every example must be a real ≥2-source correlation (a
            // 1-source "correlation" is just a passthrough) with a
            // non-zero window + an `incident/` synth topic.
            assert!(
                r.sources.len() >= 2,
                "rule {} should correlate ≥2 sources",
                r.name
            );
            assert!(r.window_seconds > 0, "rule {} has zero window", r.name);
            assert!(
                r.emits.starts_with("incident/"),
                "rule {} should emit under incident/",
                r.name
            );
        }
    }
}
