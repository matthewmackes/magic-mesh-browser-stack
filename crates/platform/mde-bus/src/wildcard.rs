//! BUS-1.5 — MQTT-style wildcard matcher (`+` single, `#` multi).
//!
//! Locked Round 4 of the 104-Q poll. `+` matches exactly one level
//! (one segment between slashes); `#` matches all remaining levels
//! and is legal only at the end of a pattern. Both are
//! subscription-side only — [`crate::topic::Registry::validate_publish_name`]
//! rejects them in publish-target names.
//!
//! The matcher walks the pattern and the topic in parallel,
//! segment-by-segment, with O(N) cost where N is the number of
//! segments. No allocations on the hot path. The exhaustive table
//! at the bottom is the contract — any change to wildcard semantics
//! must add a row there first.

use thiserror::Error;

/// Errors when validating a wildcard subscription pattern.
#[derive(Debug, Error)]
pub enum PatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("`#` wildcard must be the last segment of the pattern (got `{0}`)")]
    HashNotLast(String),
    #[error("pattern `{0}` contains an empty segment (double slash or leading/trailing slash)")]
    EmptySegment(String),
    #[error("pattern `{0}` mixes a wildcard with other characters inside a segment (e.g. `fle+/sec`); wildcards stand alone in their segment")]
    WildcardMixedWithLiteral(String),
}

/// Validate a subscription pattern. Returns `Ok(())` for well-formed
/// MQTT-style patterns; returns the structural error otherwise.
pub fn validate_pattern(pattern: &str) -> Result<(), PatternError> {
    if pattern.is_empty() {
        return Err(PatternError::Empty);
    }
    if pattern.starts_with('/') || pattern.ends_with('/') || pattern.contains("//") {
        return Err(PatternError::EmptySegment(pattern.to_string()));
    }
    let segments: Vec<&str> = pattern.split('/').collect();
    for (idx, seg) in segments.iter().enumerate() {
        let is_last = idx + 1 == segments.len();
        if *seg == "#" {
            if !is_last {
                return Err(PatternError::HashNotLast(pattern.to_string()));
            }
            continue;
        }
        if *seg == "+" {
            continue;
        }
        if seg.contains('+') || seg.contains('#') {
            return Err(PatternError::WildcardMixedWithLiteral(pattern.to_string()));
        }
    }
    Ok(())
}

/// `true` when `pattern` (an MQTT-style subscription) matches the
/// fully-qualified `topic`. Invalid patterns return `false` silently
/// — callers should validate via [`validate_pattern`] before storing
/// a pattern in the subscription manifest.
#[must_use]
pub fn matches(pattern: &str, topic: &str) -> bool {
    if pattern.is_empty() || topic.is_empty() {
        return false;
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = topic.split('/').collect();
    let mut pi = 0;
    let mut ti = 0;
    while pi < p.len() {
        match p[pi] {
            "#" => {
                // `#` must be last (per validate_pattern). It absorbs
                // every remaining segment, including zero — a
                // subscription `fleet/#` matches `fleet` too in MQTT
                // 3.1.1. Round 4 of the poll inherits that.
                return pi + 1 == p.len();
            }
            "+" => {
                // Match exactly one topic segment.
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            literal => {
                if ti >= t.len() || t[ti] != literal {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    // Pattern fully consumed — topic must also be fully consumed
    // (no trailing topic segments).
    ti == t.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive contract table. New wildcard semantics must
    /// add a row here.
    #[test]
    fn match_table() {
        let cases: &[(&str, &str, bool)] = &[
            // Exact match.
            ("fleet/sec", "fleet/sec", true),
            ("fleet/sec", "fleet/info", false),
            ("fleet/sec", "fleet/sec/extra", false),
            ("fleet/sec/extra", "fleet/sec", false),
            // `+` matches exactly one segment.
            ("peer/+/alerts", "peer/laptop/alerts", true),
            ("peer/+/alerts", "peer/kitchen/alerts", true),
            ("peer/+/alerts", "peer/laptop/system", false),
            ("peer/+/alerts", "peer/alerts", false),
            ("peer/+/alerts", "peer/a/b/alerts", false),
            // `+` at top.
            ("+/sec", "fleet/sec", true),
            ("+/sec", "mon/sec", true),
            ("+/sec", "fleet/info", false),
            // `#` matches zero-or-more remaining levels.
            ("fleet/#", "fleet", true),
            ("fleet/#", "fleet/sec", true),
            ("fleet/#", "fleet/sec/extra", true),
            ("fleet/#", "mon/sec", false),
            // `#` at root matches everything.
            ("#", "fleet/sec", true),
            ("#", "a", true),
            ("#", "", false),
            // Combinations.
            ("peer/+/#", "peer/laptop/alerts", true),
            ("peer/+/#", "peer/laptop/alerts/extra", true),
            ("peer/+/#", "peer/laptop", true),
            ("peer/+/#", "peer", false),
            // Empty inputs always false.
            ("", "fleet/sec", false),
            ("fleet/sec", "", false),
        ];
        for (p, t, want) in cases {
            assert_eq!(
                matches(p, t),
                *want,
                "matches({p:?}, {t:?}) expected {want}"
            );
        }
    }

    #[test]
    fn validate_accepts_well_formed_patterns() {
        for p in [
            "fleet/sec",
            "fleet/+/alerts",
            "+",
            "#",
            "peer/+/#",
            "mon/cpu",
        ] {
            assert!(validate_pattern(p).is_ok(), "expected {p} to validate");
        }
    }

    #[test]
    fn validate_rejects_malformed() {
        assert!(matches!(validate_pattern(""), Err(PatternError::Empty)));
        assert!(matches!(
            validate_pattern("fleet/#/extra"),
            Err(PatternError::HashNotLast(_))
        ));
        assert!(matches!(
            validate_pattern("/fleet/sec"),
            Err(PatternError::EmptySegment(_))
        ));
        assert!(matches!(
            validate_pattern("fleet//sec"),
            Err(PatternError::EmptySegment(_))
        ));
        assert!(matches!(
            validate_pattern("fle+/sec"),
            Err(PatternError::WildcardMixedWithLiteral(_))
        ));
        assert!(matches!(
            validate_pattern("fleet/sec#"),
            Err(PatternError::WildcardMixedWithLiteral(_))
        ));
    }
}
