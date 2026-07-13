//! [`BlockTally`] — a per-session block-count breakdown (BOOKMARKS-7).
//!
//! A pure, in-memory tally the browser chrome feeds after every
//! [`crate::Engine::check`] (or [`crate::Engine::match_request`]) call, so it
//! can surface a "N trackers blocked" summary with a by-domain and by-filter
//! breakdown. Deliberately **session-only** — no persistence, no clock, no
//! Bus — consistent with the browser's private-by-default posture (no
//! per-request history is written to disk); the caller owns the tally and
//! drops it when the session ends.

use std::collections::BTreeMap;

use crate::engine::Decision;
use crate::url::{host_of, registrable_domain};

/// A stable bucket key for a request whose host can't be parsed out of its URL.
const UNKNOWN_DOMAIN: &str = "(unknown)";

/// A per-session tally of blocked requests, grouped by registrable domain and
/// by the matched filter line.
///
/// Feed it with [`Self::record`] after each [`crate::Engine::check`]; `Allow`
/// decisions are ignored. Pure and allocation-frugal: two `BTreeMap`s, no
/// interior mutability, no I/O.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockTally {
    total: u64,
    by_domain: BTreeMap<String, u64>,
    by_filter: BTreeMap<String, u64>,
}

impl BlockTally {
    /// An empty tally.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the outcome of one request. Only [`Decision::Block`] counts —
    /// increments the session total and attributes it to `request_url`'s
    /// registrable domain (via [`host_of`] + [`registrable_domain`]; a host
    /// that can't be parsed out of `request_url` buckets under `"(unknown)"`)
    /// and to the matched filter line. [`Decision::Allow`] is a no-op.
    pub fn record(&mut self, decision: &Decision, request_url: &str) {
        let Some(filter) = decision.blocked_by() else {
            return;
        };
        self.total += 1;
        let domain = host_of(request_url)
            .map(|h| registrable_domain(&h))
            .unwrap_or_else(|| UNKNOWN_DOMAIN.to_string());
        *self.by_domain.entry(domain).or_insert(0) += 1;
        *self.by_filter.entry(filter.to_string()).or_insert(0) += 1;
    }

    /// The total number of blocked requests this session.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Blocked-request counts by registrable domain, sorted count DESC then
    /// domain name ASC (a deterministic order for ties).
    #[must_use]
    pub fn by_domain(&self) -> Vec<(String, u64)> {
        sorted_desc(&self.by_domain)
    }

    /// Blocked-request counts by the matched filter line, sorted count DESC
    /// then filter text ASC.
    #[must_use]
    pub fn by_filter(&self) -> Vec<(String, u64)> {
        sorted_desc(&self.by_filter)
    }

    /// The top `k` domains by blocked-request count (see [`Self::by_domain`]
    /// for the ordering); fewer than `k` if the tally holds fewer domains.
    #[must_use]
    pub fn top_domains(&self, k: usize) -> Vec<(String, u64)> {
        self.by_domain().into_iter().take(k).collect()
    }

    /// Has nothing been blocked yet this session?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Clear the tally back to empty (e.g. on a new browsing session).
    pub fn reset(&mut self) {
        self.total = 0;
        self.by_domain.clear();
        self.by_filter.clear();
    }
}

/// `map`'s entries as `(key, count)` pairs sorted count DESC, then key ASC.
fn sorted_desc(map: &BTreeMap<String, u64>) -> Vec<(String, u64)> {
    let mut entries: Vec<(String, u64)> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AllowReason;

    fn block(filter: &str) -> Decision {
        Decision::Block {
            filter: filter.to_string(),
        }
    }

    #[test]
    fn records_blocks_and_groups_by_registrable_domain() {
        let mut tally = BlockTally::new();
        tally.record(
            &block("||doubleclick.net^"),
            "https://ads.doubleclick.net/x",
        );
        tally.record(
            &block("||doubleclick.net^"),
            "https://pixel.doubleclick.net/y",
        );
        tally.record(
            &block("||tracker.example.com^"),
            "https://tracker.example.com/z",
        );

        assert_eq!(tally.total(), 3);
        let by_domain = tally.by_domain();
        assert_eq!(
            by_domain,
            vec![
                ("doubleclick.net".to_string(), 2),
                ("example.com".to_string(), 1),
            ]
        );
    }

    #[test]
    fn allow_decisions_are_ignored() {
        let mut tally = BlockTally::new();
        tally.record(
            &Decision::Allow(AllowReason::Default),
            "https://example.com/x",
        );
        tally.record(
            &Decision::Allow(AllowReason::Exception("@@||example.com^".to_string())),
            "https://example.com/y",
        );
        assert_eq!(tally.total(), 0);
        assert!(tally.is_empty());
    }

    #[test]
    fn by_domain_sorts_count_desc_then_name_asc_with_ties() {
        let mut tally = BlockTally::new();
        // b.com and a.com tie at 1; c.com leads at 2.
        tally.record(&block("f1"), "https://c.com/1");
        tally.record(&block("f1"), "https://c.com/2");
        tally.record(&block("f2"), "https://b.com/1");
        tally.record(&block("f3"), "https://a.com/1");

        assert_eq!(
            tally.by_domain(),
            vec![
                ("c.com".to_string(), 2),
                ("a.com".to_string(), 1),
                ("b.com".to_string(), 1),
            ]
        );
    }

    #[test]
    fn top_domains_returns_exactly_the_top_k() {
        let mut tally = BlockTally::new();
        tally.record(&block("f1"), "https://c.com/1");
        tally.record(&block("f1"), "https://c.com/2");
        tally.record(&block("f2"), "https://b.com/1");
        tally.record(&block("f3"), "https://a.com/1");

        assert_eq!(
            tally.top_domains(2),
            vec![("c.com".to_string(), 2), ("a.com".to_string(), 1)]
        );
        // Asking for more than exist just returns what's there.
        assert_eq!(tally.top_domains(10).len(), 3);
    }

    #[test]
    fn by_filter_groups_and_sorts_like_by_domain() {
        let mut tally = BlockTally::new();
        tally.record(&block("||ads.example^"), "https://x.com/1");
        tally.record(&block("||ads.example^"), "https://y.com/1");
        tally.record(&block("||track.example^"), "https://z.com/1");

        assert_eq!(
            tally.by_filter(),
            vec![
                ("||ads.example^".to_string(), 2),
                ("||track.example^".to_string(), 1),
            ]
        );
    }

    #[test]
    fn unparseable_host_buckets_under_unknown_without_panicking() {
        let mut tally = BlockTally::new();
        // No `://` authority — `host_of` returns None.
        tally.record(&block("blocked-scheme-rule"), "data:text/plain,hi");
        tally.record(&block("blocked-scheme-rule"), "about:blank");

        assert_eq!(tally.total(), 2);
        assert_eq!(tally.by_domain(), vec![(UNKNOWN_DOMAIN.to_string(), 2)]);
    }

    #[test]
    fn reset_empties_the_tally() {
        let mut tally = BlockTally::new();
        tally.record(&block("f1"), "https://c.com/1");
        assert!(!tally.is_empty());

        tally.reset();
        assert!(tally.is_empty());
        assert_eq!(tally.total(), 0);
        assert!(tally.by_domain().is_empty());
        assert!(tally.by_filter().is_empty());
    }
}
