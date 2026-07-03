//! [`FilterListStore`] — the service half of the ad-blocker (BOOKMARKS-7).
//!
//! The serde-persisted, **mesh-distributable** state the mackesd `adfilter`
//! worker owns: the set of enabled filter sources (the bundled seed + operator
//! custom lists), the per-site allowlist (block-on-by-default; the operator opts
//! a site out), the operator-added exempt suffixes, and the [`Staleness`]
//! metadata for the honest "lists are N days old" indicator.
//!
//! This crate keeps the store **headless** — it is the typed model + the
//! last-writer-wins [`FilterListStore::merge`] that makes two nodes' stores
//! converge. The live plumbing — a leader compiling the store and replicating
//! the JSON blob over Syncthing, fetching upstream list updates — is the mackesd
//! `adfilter` worker (the BOOKMARKS-7 wiring follow-on). Timestamps are passed
//! in (`now_ms`), never read from a clock here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::bundled::bundled_sources;

/// Where a filter list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ListKind {
    /// Ships in the RPM as the offline fallback seed.
    Bundled,
    /// Added by the operator (a custom list or a mirror of an upstream list).
    Custom,
}

/// One filter list in the store: its text plus the metadata to update + merge it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterListSource {
    /// A stable name, unique within the store (the merge key) — e.g. `EasyList`.
    pub name: String,
    /// Bundled seed vs operator custom.
    pub kind: ListKind,
    /// The upstream fetch URL the worker refreshes from, if any.
    pub url: Option<String>,
    /// The raw EasyList-format text (the replicated blob body).
    pub raw: String,
    /// Whether the engine compiles this source.
    pub enabled: bool,
    /// When `raw` was last updated (unix ms) — the LWW key + staleness input.
    pub updated_ms: u64,
}

impl FilterListSource {
    /// A bundled seed source (enabled, `updated_ms` 0 = the baseline).
    #[must_use]
    pub fn bundled(name: impl Into<String>, url: Option<String>, raw: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: ListKind::Bundled,
            url,
            raw: raw.into(),
            enabled: true,
            updated_ms: 0,
        }
    }

    /// An operator custom source.
    #[must_use]
    pub fn custom(
        name: impl Into<String>,
        url: Option<String>,
        raw: impl Into<String>,
        now_ms: u64,
    ) -> Self {
        Self {
            name: name.into(),
            kind: ListKind::Custom,
            url,
            raw: raw.into(),
            enabled: true,
            updated_ms: now_ms,
        }
    }
}

/// One allowlist decision for a site, carried with attribution + a timestamp so
/// the mesh converges on the newest write (LWW).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowlistEntry {
    /// The first-party domain the decision is about.
    pub domain: String,
    /// `true` = allowlisted (blocking off for this site); `false` = a later
    /// re-enable of blocking (kept as an LWW tombstone, not a hard delete).
    pub allowed: bool,
    /// The host that made the decision (attribution).
    pub added_by: String,
    /// When the decision was made (unix ms) — the LWW key.
    pub updated_ms: u64,
}

/// The per-site allowlist: a domain → [`AllowlistEntry`] map with LWW merge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Allowlist {
    entries: BTreeMap<String, AllowlistEntry>,
}

impl Allowlist {
    /// Allowlist `domain` (turn blocking off for it), attributed to `by`.
    pub fn allow(&mut self, domain: &str, by: &str, now_ms: u64) {
        self.set(domain, true, by, now_ms);
    }

    /// Re-enable blocking on `domain` (remove it from the allowlist).
    pub fn block(&mut self, domain: &str, by: &str, now_ms: u64) {
        self.set(domain, false, by, now_ms);
    }

    fn set(&mut self, domain: &str, allowed: bool, by: &str, now_ms: u64) {
        let key = domain.to_ascii_lowercase();
        let replace = self
            .entries
            .get(&key)
            .is_none_or(|e| now_ms >= e.updated_ms);
        if replace {
            self.entries.insert(
                key.clone(),
                AllowlistEntry {
                    domain: key,
                    allowed,
                    added_by: by.to_string(),
                    updated_ms: now_ms,
                },
            );
        }
    }

    /// Is `domain` currently allowlisted?
    #[must_use]
    pub fn is_allowed(&self, domain: &str) -> bool {
        self.entries
            .get(&domain.to_ascii_lowercase())
            .is_some_and(|e| e.allowed)
    }

    /// The domains currently allowlisted (the un-tombstoned entries).
    #[must_use = "the returned iterator is lazy and does nothing unless consumed"]
    pub fn domains(&self) -> impl Iterator<Item = &String> {
        self.entries
            .values()
            .filter(|e| e.allowed)
            .map(|e| &e.domain)
    }

    /// Every entry (incl. re-enable tombstones), for inspection / the UI.
    #[must_use = "the returned iterator is lazy and does nothing unless consumed"]
    pub fn entries(&self) -> impl Iterator<Item = &AllowlistEntry> {
        self.entries.values()
    }

    /// LWW-merge `other` into `self`: per domain, the newest `updated_ms` wins.
    pub fn merge(&mut self, other: &Self) {
        for (key, entry) in &other.entries {
            let replace = self
                .entries
                .get(key)
                .is_none_or(|mine| entry.updated_ms > mine.updated_ms);
            if replace {
                self.entries.insert(key.clone(), entry.clone());
            }
        }
    }
}

/// How fresh the filter lists are — the honest indicator the operator sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Staleness {
    /// Synced from upstream within the freshness window.
    Fresh,
    /// Last upstream sync is older than the window — running on last-synced lists.
    Stale {
        /// How long since the last successful sync (ms).
        age_ms: u64,
    },
    /// Never synced from upstream — running on the bundled seed only.
    NeverSynced,
}

/// The mesh-distributable ad-blocker state (serde-persisted at `state/adfilter/`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterListStore {
    /// The filter sources, unique by [`FilterListSource::name`].
    sources: Vec<FilterListSource>,
    /// The per-site allowlist.
    allowlist: Allowlist,
    /// Operator-added exempt domain suffixes (beyond the engine defaults).
    exempt_suffixes: BTreeSet<String>,
    /// When any source was last successfully synced from upstream (unix ms).
    synced_ms: Option<u64>,
}

impl FilterListStore {
    /// An empty store (no sources).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store seeded with the bundled EasyList/EasyPrivacy/uBlock-base sources —
    /// the state a fresh node starts from before its first upstream sync.
    #[must_use]
    pub fn with_bundled() -> Self {
        Self {
            sources: bundled_sources(),
            ..Self::default()
        }
    }

    /// The enabled sources, for the engine to compile.
    #[must_use = "the returned iterator is lazy and does nothing unless consumed"]
    pub fn enabled_sources(&self) -> impl Iterator<Item = &FilterListSource> {
        self.sources.iter().filter(|s| s.enabled)
    }

    /// All sources (enabled or not), for the UI.
    #[must_use]
    pub fn sources(&self) -> &[FilterListSource] {
        &self.sources
    }

    /// A source by name.
    #[must_use]
    pub fn source(&self, name: &str) -> Option<&FilterListSource> {
        self.sources.iter().find(|s| s.name == name)
    }

    /// Add or replace a source (matched by name).
    pub fn add_source(&mut self, source: FilterListSource) {
        if let Some(existing) = self.sources.iter_mut().find(|s| s.name == source.name) {
            *existing = source;
        } else {
            self.sources.push(source);
        }
    }

    /// Replace a source's text (an upstream refresh), stamping `updated_ms` and
    /// recording the sync. Returns `false` if there is no such source.
    pub fn update_source(&mut self, name: &str, raw: impl Into<String>, now_ms: u64) -> bool {
        let Some(source) = self.sources.iter_mut().find(|s| s.name == name) else {
            return false;
        };
        source.raw = raw.into();
        source.updated_ms = now_ms;
        self.synced_ms = Some(now_ms);
        true
    }

    /// Enable or disable a source. Returns `false` if there is no such source.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        let Some(source) = self.sources.iter_mut().find(|s| s.name == name) else {
            return false;
        };
        source.enabled = enabled;
        true
    }

    /// The per-site allowlist (read).
    #[must_use]
    pub const fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    /// The per-site allowlist (mutate).
    pub const fn allowlist_mut(&mut self) -> &mut Allowlist {
        &mut self.allowlist
    }

    /// Allowlist a first-party site (block-on-by-default opt-out).
    pub fn allow_site(&mut self, domain: &str, by: &str, now_ms: u64) {
        self.allowlist.allow(domain, by, now_ms);
    }

    /// Re-enable blocking on a first-party site.
    pub fn block_site(&mut self, domain: &str, by: &str, now_ms: u64) {
        self.allowlist.block(domain, by, now_ms);
    }

    /// Add an operator exempt domain suffix (never filtered).
    pub fn add_exempt_suffix(&mut self, suffix: &str) {
        self.exempt_suffixes.insert(suffix.to_ascii_lowercase());
    }

    /// The operator-added exempt suffixes (the engine also applies its defaults).
    #[must_use = "the returned iterator is lazy and does nothing unless consumed"]
    pub fn exempt_suffixes(&self) -> impl Iterator<Item = String> + '_ {
        self.exempt_suffixes.iter().cloned()
    }

    /// When any source was last synced from upstream (unix ms), if ever.
    #[must_use]
    pub const fn synced_ms(&self) -> Option<u64> {
        self.synced_ms
    }

    /// Classify freshness given the current time and a freshness window.
    #[must_use]
    pub fn staleness(&self, now_ms: u64, ttl_ms: u64) -> Staleness {
        self.synced_ms.map_or(Staleness::NeverSynced, |synced| {
            let age = now_ms.saturating_sub(synced);
            if age <= ttl_ms {
                Staleness::Fresh
            } else {
                Staleness::Stale { age_ms: age }
            }
        })
    }

    /// LWW-merge `other` into `self` so two nodes' stores converge:
    ///   * per source (by name), the newest `updated_ms` wins (a fresher list, or
    ///     an enable/disable, replaces the older copy);
    ///   * the allowlist merges per domain (newest write wins);
    ///   * exempt suffixes union;
    ///   * `synced_ms` takes the more recent of the two.
    pub fn merge(&mut self, other: &Self) {
        for source in &other.sources {
            match self.sources.iter_mut().find(|s| s.name == source.name) {
                Some(mine) if source.updated_ms > mine.updated_ms => *mine = source.clone(),
                Some(_) => {}
                None => self.sources.push(source.clone()),
            }
        }
        self.allowlist.merge(&other.allowlist);
        self.exempt_suffixes
            .extend(other.exempt_suffixes.iter().cloned());
        self.synced_ms = match (self.synced_ms, other.synced_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    /// Serialize the store to the JSON blob the worker replicates over Syncthing.
    ///
    /// # Errors
    /// Propagates any [`serde_json`] serialization error (unreachable for this
    /// plain-data model, but returned rather than panicking).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse a store from the replicated JSON blob.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] if `json` is not a valid serialized store.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}
