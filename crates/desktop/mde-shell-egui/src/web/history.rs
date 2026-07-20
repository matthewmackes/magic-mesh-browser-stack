//! Session-only browsing history for the Browser surface (B3).
//!
//! Operator-ruled **session-only, in-memory** (threat-model Q74/Q80): a
//! recent-visits list that lives ONLY in RAM, clears on exit (dropped with
//! `WebState`) and on an explicit Clear, and is NEVER written to disk or
//! published to the Bus. This is the deliberate inverse of bookmarks (an
//! explicit, persisted, mesh-synced user act) — history is passive, so it must
//! not persist ([[browser-privacy-locks]]).
//!
//! Populated from the B1 nav stream (`session.nav().url` + `session.title()`),
//! deduping the `loading:true→false` NavState churn and back-filling the title
//! when it arrives after the URL commit. A self-contained unit (no `WebState`
//! coupling); `use super::*` only pulls in std collections.

use super::*;

/// One visited page (most-recent visit wins for the title/time).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct VisitRecord {
    pub(super) url: String,
    pub(super) title: String,
    pub(super) first_visit_ms: u64,
    pub(super) last_visit_ms: u64,
}

/// The bounded, most-recent-first visit log. In-memory only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HistoryStore {
    /// Front = most recent. Deduped: the same URL is one record, its
    /// `last_visit_ms` + title refreshed on re-visit rather than duplicated.
    visits: VecDeque<VisitRecord>,
}

/// The most visits retained this session — bounds RAM; older visits drop off.
const HISTORY_CAP: usize = 500;

impl HistoryStore {
    /// Record a committed navigation to `url` (with whatever `title` is known so
    /// far) at `now_ms`. Consecutive re-commits of the SAME url — the NavState
    /// `loading:true→false` churn, reloads, in-page re-commits — refresh the
    /// existing front record instead of adding a duplicate; a different url
    /// promotes/creates its record at the front.
    pub(super) fn record(&mut self, url: &str, title: &str, now_ms: u64) {
        let url = url.trim();
        if url.is_empty() {
            return;
        }
        // Consecutive same-URL → refresh the front record (title back-fill + time).
        if let Some(front) = self.visits.front_mut() {
            if front.url == url {
                if !title.is_empty() {
                    front.title = title.to_owned();
                }
                front.last_visit_ms = now_ms;
                return;
            }
        }
        // A repeat of an OLDER url: reuse its first_visit_ms, move it to the front.
        let existing = self
            .visits
            .iter()
            .position(|v| v.url == url)
            .and_then(|idx| self.visits.remove(idx));
        let first_visit_ms = existing.as_ref().map_or(now_ms, |v| v.first_visit_ms);
        let title = if title.is_empty() {
            existing.map(|v| v.title).unwrap_or_default()
        } else {
            title.to_owned()
        };
        self.visits.push_front(VisitRecord {
            url: url.to_owned(),
            title,
            first_visit_ms,
            last_visit_ms: now_ms,
        });
        while self.visits.len() > HISTORY_CAP {
            self.visits.pop_back();
        }
    }

    /// Back-fill the most-recent visit's title once the engine reports it (Title
    /// and NavState are separate events, so the title usually lags the commit).
    #[cfg(test)]
    pub(super) fn set_latest_title(&mut self, url: &str, title: &str) {
        let url = url.trim();
        if url.is_empty() || title.is_empty() {
            return;
        }
        if let Some(front) = self.visits.front_mut() {
            if front.url == url {
                front.title = title.to_owned();
            }
        }
    }

    /// Forget everything (Clear History; also automatic on shell exit via Drop).
    pub(super) fn clear(&mut self) {
        self.visits.clear();
    }

    #[must_use]
    pub(super) fn is_empty(&self) -> bool {
        self.visits.is_empty()
    }

    /// All visits, most-recent-first — the History drawer groups these by day.
    pub(super) fn visits(&self) -> impl Iterator<Item = &VisitRecord> {
        self.visits.iter()
    }

    /// Visits whose url or title contains `query` (case-insensitive),
    /// most-recent-first — feeds the omnibox history autocomplete + drawer search.
    /// An empty query matches everything.
    pub(super) fn matching<'a>(&'a self, query: &str) -> impl Iterator<Item = &'a VisitRecord> {
        let needle = query.trim().to_ascii_lowercase();
        self.visits.iter().filter(move |v| {
            needle.is_empty()
                || v.url.to_ascii_lowercase().contains(&needle)
                || v.title.to_ascii_lowercase().contains(&needle)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consecutive_same_url_dedupes_and_backfills_title() {
        let mut h = HistoryStore::default();
        // NavState churn: commit with no title yet, then again as loading settles.
        h.record("https://example.com/a", "", 100);
        h.record("https://example.com/a", "", 150);
        // Title arrives after the commit.
        h.set_latest_title("https://example.com/a", "Example A");
        assert_eq!(h.visits().count(), 1, "same URL must not duplicate");
        let v = h.visits().next().unwrap();
        assert_eq!(v.title, "Example A");
        assert_eq!(v.first_visit_ms, 100);
        assert_eq!(v.last_visit_ms, 150);
    }

    #[test]
    fn distinct_urls_are_most_recent_first_and_revisit_promotes() {
        let mut h = HistoryStore::default();
        h.record("https://a.test/", "A", 10);
        h.record("https://b.test/", "B", 20);
        assert_eq!(h.visits().next().unwrap().url, "https://b.test/");
        // Re-visit A → it promotes to the front, keeps its original first_visit_ms.
        h.record("https://a.test/", "A", 30);
        let urls: Vec<_> = h.visits().map(|v| v.url.clone()).collect();
        assert_eq!(urls, ["https://a.test/", "https://b.test/"]);
        assert_eq!(h.visits().next().unwrap().first_visit_ms, 10);
        assert_eq!(h.visits().next().unwrap().last_visit_ms, 30);
    }

    #[test]
    fn empty_urls_are_ignored_and_clear_empties() {
        let mut h = HistoryStore::default();
        h.record("   ", "nope", 1);
        assert!(h.is_empty());
        h.record("https://x.test/", "X", 2);
        assert!(!h.is_empty());
        h.clear();
        assert!(h.is_empty());
    }

    #[test]
    fn matching_searches_url_and_title_case_insensitively() {
        let mut h = HistoryStore::default();
        h.record("https://rust-lang.org/", "The Rust Language", 1);
        h.record("https://example.com/mesh", "Mesh Docs", 2);
        let by_title: Vec<_> = h.matching("rust").map(|v| v.url.clone()).collect();
        assert_eq!(by_title, ["https://rust-lang.org/"]);
        let by_url: Vec<_> = h.matching("MESH").map(|v| v.url.clone()).collect();
        assert_eq!(by_url.len(), 1);
        assert_eq!(h.matching("").count(), 2, "empty query matches all");
    }

    #[test]
    fn cap_bounds_the_log() {
        let mut h = HistoryStore::default();
        for i in 0..(HISTORY_CAP + 25) {
            h.record(&format!("https://site.test/{i}"), "", i as u64);
        }
        assert_eq!(h.visits().count(), HISTORY_CAP);
        // The most recent survives; the oldest dropped off the back.
        assert_eq!(
            h.visits().next().unwrap().url,
            format!("https://site.test/{}", HISTORY_CAP + 24)
        );
    }
}
