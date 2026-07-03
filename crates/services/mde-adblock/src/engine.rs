//! [`Engine`] — the compiled request matcher (BOOKMARKS-7).
//!
//! The glue point the `mde-web-preview` Servo browser calls: build an [`Engine`]
//! from a [`crate::FilterListStore`] (or straight from [`crate::FilterList`]s),
//! then for every outgoing request call [`Engine::match_request`] to get a
//! [`Decision`] (block before fetch, or allow), and once per page call
//! [`Engine::cosmetic_selectors`] to get the element-hide selectors for the
//! injected user-stylesheet.
//!
//! Matching precedence follows Adblock Plus: an **exempt** mesh/overlay host is
//! always allowed; an **allowlisted** first-party site is always allowed
//! (block-on-by-default, the operator's per-site opt-out); otherwise a network
//! **block** rule blocks the request unless an `@@` **exception** rule overrides
//! it.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::parser::FilterList;
use crate::resource::ResourceType;
use crate::rule::{CosmeticRule, NetworkRule};
use crate::store::FilterListStore;
use crate::url::{host_of, is_subdomain_of, is_third_party};

/// Why a request was allowed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AllowReason {
    /// No rule matched (the default — most requests).
    Default,
    /// An `@@` exception rule overrode a block; carries the exception's raw text.
    Exception(String),
    /// The first-party site is on the operator's allowlist.
    Allowlisted,
    /// The request targets an exempt mesh/overlay host.
    Exempt,
}

/// The verdict for one request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Block the request; carries the filter that matched (for the per-page
    /// blocked-count UI / logs).
    Block {
        /// The raw filter line that matched.
        filter: String,
    },
    /// Allow the request; carries why.
    Allow(AllowReason),
}

impl Decision {
    /// Was the request blocked?
    #[must_use]
    pub const fn is_block(&self) -> bool {
        matches!(self, Self::Block { .. })
    }

    /// The raw filter that caused a block, if any.
    #[must_use]
    pub fn blocked_by(&self) -> Option<&str> {
        match self {
            Self::Block { filter } => Some(filter),
            Self::Allow(_) => None,
        }
    }
}

/// A request to test, for the struct-style [`Engine::check`] entry point.
#[derive(Clone, Copy, Debug)]
pub struct RequestContext<'a> {
    /// The full request URL.
    pub url: &'a str,
    /// The request's resource type.
    pub resource_type: ResourceType,
    /// The host of the top-level page the request originates from.
    pub first_party: &'a str,
}

/// The compiled ad-blocking engine: indexed rules + the allowlist + exempt hosts.
#[derive(Clone, Debug, Default)]
pub struct Engine {
    block: Vec<NetworkRule>,
    allow: Vec<NetworkRule>,
    cosmetic_hide: Vec<CosmeticRule>,
    cosmetic_unhide: Vec<CosmeticRule>,
    /// First-party sites the operator has allowlisted (block-on-by-default opt-out).
    allowlist: BTreeSet<String>,
    /// Domain suffixes that are never blocked (mesh/overlay); see [`Self::is_exempt`].
    exempt_suffixes: Vec<String>,
}

impl Engine {
    /// An empty engine that blocks nothing (the default exempt suffixes still
    /// apply). Add lists with [`Self::add_list`] or start from
    /// [`Self::from_store`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            exempt_suffixes: default_exempt_suffixes(),
            ..Self::default()
        }
    }

    /// Compile an engine from a [`FilterListStore`]: parse every **enabled**
    /// source, fold in the per-site allowlist, and apply the store's exempt
    /// suffixes. This is the primary glue point — the mackesd `adfilter` worker
    /// hands the browser a store, the browser compiles it here.
    #[must_use]
    pub fn from_store(store: &FilterListStore) -> Self {
        let mut engine = Self::new();
        for source in store.enabled_sources() {
            engine.add_list(&FilterList::parse(&source.raw));
        }
        engine.allowlist = store.allowlist().domains().cloned().collect();
        engine.exempt_suffixes.extend(store.exempt_suffixes());
        engine
    }

    /// Fold one parsed [`FilterList`] into the engine's indexes.
    pub fn add_list(&mut self, list: &FilterList) {
        for rule in &list.network {
            if rule.is_exception {
                self.allow.push(rule.clone());
            } else {
                self.block.push(rule.clone());
            }
        }
        for rule in &list.cosmetic {
            if rule.is_exception {
                self.cosmetic_unhide.push(rule.clone());
            } else {
                self.cosmetic_hide.push(rule.clone());
            }
        }
    }

    /// Allowlist a first-party site (block-on-by-default opt-out).
    pub fn allowlist_site(&mut self, domain: &str) {
        self.allowlist.insert(domain.to_ascii_lowercase());
    }

    /// Test a request. `url` is the full request URL, `resource_type` its class,
    /// and `first_party` the host of the page it originates from.
    ///
    /// This is the signature the browser wires to its network layer.
    #[must_use]
    pub fn match_request(
        &self,
        url: &str,
        resource_type: ResourceType,
        first_party: &str,
    ) -> Decision {
        // No authority host (data:/about:/blob:) — nothing to match against.
        let Some(host) = host_of(url) else {
            return Decision::Allow(AllowReason::Default);
        };
        if self.is_exempt(&host) {
            return Decision::Allow(AllowReason::Exempt);
        }
        if self.is_allowlisted(first_party) {
            return Decision::Allow(AllowReason::Allowlisted);
        }

        let range = host_range(url);
        let third_party = is_third_party(&host, first_party);
        // Lowercase the request URL once and reuse the bytes across every rule.
        let lower = url.to_ascii_lowercase();
        let bytes = lower.as_bytes();

        // Find the first blocking rule that matches.
        let blocked = self.block.iter().find(|r| {
            r.options.applies(resource_type, first_party, third_party)
                && r.pattern.matches_lower(bytes, range)
        });
        let Some(block_rule) = blocked else {
            return Decision::Allow(AllowReason::Default);
        };

        // A block hit: an `@@` exception overrides it.
        if let Some(exc) = self.allow.iter().find(|r| {
            r.options.applies(resource_type, first_party, third_party)
                && r.pattern.matches_lower(bytes, range)
        }) {
            return Decision::Allow(AllowReason::Exception(exc.raw.clone()));
        }
        Decision::Block {
            filter: block_rule.raw.clone(),
        }
    }

    /// The struct-style entry point, equivalent to [`Self::match_request`].
    #[must_use]
    pub fn check(&self, ctx: &RequestContext) -> Decision {
        self.match_request(ctx.url, ctx.resource_type, ctx.first_party)
    }

    /// The element-hide CSS selectors that apply on `host`, for the browser's
    /// injected user-stylesheet. Generic (`##…`) plus domain-scoped (`d.com##…`)
    /// hide selectors, minus any `#@#` un-hide exceptions for the host. Returns
    /// an empty set for an exempt or allowlisted host (no cosmetic hiding there).
    /// The result is sorted for deterministic output.
    #[must_use]
    pub fn cosmetic_selectors(&self, host: &str) -> Vec<String> {
        if self.is_exempt(host) || self.is_allowlisted(host) {
            return Vec::new();
        }
        let unhidden: BTreeSet<&str> = self
            .cosmetic_unhide
            .iter()
            .filter(|r| r.applies_to(host))
            .map(|r| r.selector.as_str())
            .collect();
        self.cosmetic_hide
            .iter()
            .filter(|r| r.applies_to(host))
            .map(|r| r.selector.clone())
            .filter(|s| !unhidden.contains(s.as_str()))
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect()
    }

    /// Is `first_party` on the allowlist (equal to, or a subdomain of, an entry)?
    #[must_use]
    pub fn is_allowlisted(&self, first_party: &str) -> bool {
        let host = first_party.to_ascii_lowercase();
        self.allowlist.iter().any(|d| is_subdomain_of(&host, d))
    }

    /// Is `host` an exempt mesh/overlay host — a `*.mesh`/`localhost` name, an
    /// operator-added exempt suffix, or a Nebula overlay IP (`10.42.0.0/16`)?
    #[must_use]
    pub fn is_exempt(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self
            .exempt_suffixes
            .iter()
            .any(|s| is_subdomain_of(&host, s))
        {
            return true;
        }
        // The Nebula overlay range is never filtered (mesh services are reached
        // over it): 10.42.0.0/16.
        host.parse::<Ipv4Addr>()
            .is_ok_and(|ip| ip.octets()[0] == 10 && ip.octets()[1] == 42)
    }

    /// The number of network rules the engine holds (block + allow) — for the
    /// operator's "N rules active" indicator.
    #[must_use]
    pub fn network_rule_count(&self) -> usize {
        self.block.len() + self.allow.len()
    }

    /// The number of cosmetic rules the engine holds (hide + un-hide).
    #[must_use]
    pub fn cosmetic_rule_count(&self) -> usize {
        self.cosmetic_hide.len() + self.cosmetic_unhide.len()
    }
}

/// The default exempt domain suffixes: the mesh TLD + loopback.
fn default_exempt_suffixes() -> Vec<String> {
    vec!["mesh".to_string(), "localhost".to_string()]
}

/// The byte range of the host within `url`, for the `||` domain anchor. `None`
/// when the URL has no `://` authority.
fn host_range(url: &str) -> Option<(usize, usize)> {
    let scheme_end = url.find("://")? + 3;
    let after = &url[scheme_end..];
    let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..auth_end];
    let host_start_rel = authority.rfind('@').map_or(0, |i| i + 1);
    let host_part = &authority[host_start_rel..];
    // IPv6 literal: include up to and including the closing bracket; otherwise
    // trim the port.
    let host_end_rel = host_part.strip_prefix('[').map_or_else(
        || host_part.find(':').unwrap_or(host_part.len()),
        |rest| rest.find(']').map_or(host_part.len(), |i| i + 2),
    );
    let start = scheme_end + host_start_rel;
    Some((start, start + host_end_rel))
}
