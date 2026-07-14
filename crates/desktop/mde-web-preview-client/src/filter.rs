//! [`RequestFilter`] — the shell-side ad-filter decision layer wired into the
//! browser seam (BOOKMARKS-7).
//!
//! The sandboxed `mde-web-preview` helper issues every network request; before it
//! fetches a subresource it asks the shell over the session socket
//! ([`crate::wire::EventMsg::ResourceRequest`]). The shell holds the compiled
//! [`mde_adblock::Engine`] and answers with a
//! [`crate::wire::ControlMsg::ResourceVerdict`]: on a [`Decision::Block`] the
//! helper drops the request **before** the network, and the shell bumps a per-page
//! blocked counter the Browser surface renders. Once per page the shell also pushes
//! the element-hide [`Engine::cosmetic_selectors`] as a JS-off-safe user-stylesheet
//! ([`crate::wire::ControlMsg::CosmeticFilters`]) to hide leftover ad frames.
//!
//! Mesh/overlay hosts (`*.mesh`, `localhost`, the Nebula `10.42.0.0/16` range) are
//! never filtered — the engine already returns an exempt allow for them, so this
//! layer just honors its [`Decision`]. Secure pages also reject public plain-HTTP
//! subresources before fetch (`mixed-content:http`), preserving those mesh/overlay
//! exemptions and leaving top-level document navigations to the shell's navigation
//! policy.
//!
//! The engine is injected (the shell compiles it from the mackesd `adfilter`
//! worker's replicated `state/adfilter` blob); the default filter blocks nothing,
//! so a session with no filter behaves exactly as before this unit.

use std::collections::BTreeSet;

use mde_adblock::{
    host_of, AllowReason, BlockTally, Decision, Engine, FilterListStore, ResourceType,
};

/// A mesh-synced safe-browsing host blocklist.
///
/// Hosts match exactly or by subdomain suffix, so listing `malware.test` blocks
/// both `https://malware.test/` and `https://cdn.malware.test/`. Mesh/overlay
/// exemptions still win in [`RequestFilter::decide`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SafeBrowsingBlocklist {
    hosts: BTreeSet<String>,
}

impl SafeBrowsingBlocklist {
    /// Build an empty blocklist.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            hosts: BTreeSet::new(),
        }
    }

    /// Build a blocklist from mesh-hosted host entries.
    #[must_use]
    pub fn from_hosts(hosts: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut list = Self::default();
        for host in hosts {
            list.insert(host.as_ref());
        }
        list
    }

    fn insert(&mut self, host: &str) {
        let host = host
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if !host.is_empty() {
            self.hosts.insert(host);
        }
    }

    /// Does this URL or host match the unsafe-host set?
    #[must_use]
    pub fn matches(&self, url_or_host: &str) -> Option<&str> {
        let host = host_of(url_or_host)
            .unwrap_or_else(|| url_or_host.trim().to_ascii_lowercase())
            .trim_end_matches('.')
            .to_owned();
        self.hosts.iter().find_map(|blocked| {
            (host == *blocked || host.ends_with(&format!(".{blocked}"))).then_some(blocked.as_str())
        })
    }
}

/// Operator-managed URL blocking policy.
///
/// Rules are either host suffixes (`example.com`, `*.example.com`, or
/// `host:example.com`) or URL prefixes (`https://example.com/private/` or
/// `url:https://example.com/private/`). Unlike the ad-filter and safe-browsing
/// list, this is an enterprise/admin policy surface, so it is enforced before
/// mesh/overlay exemptions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ManagedUrlPolicy {
    rules: BTreeSet<ManagedUrlRule>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ManagedUrlRule {
    Host(String),
    UrlPrefix(String),
}

impl ManagedUrlPolicy {
    /// Build an empty managed policy.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            rules: BTreeSet::new(),
        }
    }

    /// Build a managed policy from operator-provided rule lines.
    #[must_use]
    pub fn from_rules(rules: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut policy = Self::default();
        for rule in rules {
            policy.insert(rule.as_ref());
        }
        policy
    }

    fn insert(&mut self, rule: &str) {
        let rule = rule.trim();
        if rule.is_empty() {
            return;
        }
        let lower = rule.to_ascii_lowercase();
        if let Some(host) = lower.strip_prefix("host:") {
            self.insert_host(host);
        } else if let Some(prefix) = lower.strip_prefix("url:") {
            self.insert_url_prefix(prefix);
        } else if lower.contains("://") {
            self.insert_url_prefix(&lower);
        } else {
            self.insert_host(&lower);
        }
    }

    fn insert_host(&mut self, host: &str) {
        let host = host
            .trim()
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_owned();
        if !host.is_empty() {
            self.rules.insert(ManagedUrlRule::Host(host));
        }
    }

    fn insert_url_prefix(&mut self, prefix: &str) {
        let prefix = normalized_policy_url(prefix);
        if !prefix.is_empty() {
            self.rules.insert(ManagedUrlRule::UrlPrefix(prefix));
        }
    }

    /// Number of active managed-policy rules.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no managed-policy rules are active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Does this URL match a managed block rule?
    #[must_use]
    pub fn matches(&self, url: &str) -> Option<String> {
        let url = url.trim();
        let normalized_url = normalized_policy_url(url);
        let host = host_of(&normalized_url)
            .unwrap_or_else(|| normalized_url.clone())
            .trim_end_matches('.')
            .to_owned();
        self.rules.iter().find_map(|rule| match rule {
            ManagedUrlRule::Host(blocked) => (host == *blocked
                || host.ends_with(&format!(".{blocked}")))
            .then(|| format!("host:{blocked}")),
            ManagedUrlRule::UrlPrefix(prefix) => {
                managed_url_prefix_matches(&normalized_url, prefix).then(|| format!("url:{prefix}"))
            }
        })
    }
}

fn normalized_policy_url(url: &str) -> String {
    let lowered = url.trim().to_ascii_lowercase();
    normalize_default_http_port(&lowered).unwrap_or(lowered)
}

fn normalize_default_http_port(lowered_url: &str) -> Option<String> {
    let (scheme, rest) = lowered_url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority_end = rest
        .find(|ch| matches!(ch, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(authority_end);
    let authority = normalize_policy_authority(scheme, authority)?;
    Some(format!("{scheme}://{authority}{suffix}"))
}

fn normalize_policy_authority(scheme: &str, authority: &str) -> Option<String> {
    if authority.is_empty() {
        return None;
    }
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if authority.is_empty() {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after_host) = rest.split_once(']')?;
        let port = after_host.strip_prefix(':');
        return match port {
            Some(port) if is_default_http_port(scheme, port) => Some(format!("[{host}]")),
            Some(port) if !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()) => {
                Some(format!("[{host}]:{port}"))
            }
            Some(_) => None,
            None if after_host.is_empty() => Some(format!("[{host}]")),
            None => None,
        };
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| {
            if port.chars().all(|ch| ch.is_ascii_digit()) {
                (host, Some(port))
            } else {
                (authority, None)
            }
        });
    if host.is_empty() {
        return None;
    }
    match port {
        Some(port) if is_default_http_port(scheme, port) => Some(host.to_owned()),
        Some(port) => Some(format!("{host}:{port}")),
        None => Some(host.to_owned()),
    }
}

fn is_default_http_port(scheme: &str, port: &str) -> bool {
    matches!((scheme, port), ("http", "80") | ("https", "443"))
}

fn managed_url_prefix_matches(url: &str, prefix: &str) -> bool {
    if !url.starts_with(prefix) {
        return false;
    }
    if !url_prefix_ends_at_authority(prefix) {
        return true;
    }
    url.as_bytes()
        .get(prefix.len())
        .is_none_or(|next| matches!(next, b'/' | b'?' | b'#'))
}

fn url_prefix_ends_at_authority(prefix: &str) -> bool {
    let Some((_, rest)) = prefix.split_once("://") else {
        return false;
    };
    !rest.contains(['/', '?', '#'])
}

/// Map a compact wire discriminant back to a [`ResourceType`].
///
/// The discriminant is `ResourceType as u8` (the same value
/// [`ResourceType`]'s option mask is built from). An unknown byte maps to
/// [`ResourceType::Other`] so a future helper adding a resource class the shell
/// doesn't know still gets a conservative, matchable classification.
#[must_use]
pub const fn resource_from_wire(v: u8) -> ResourceType {
    match v {
        0 => ResourceType::Document,
        1 => ResourceType::Subdocument,
        2 => ResourceType::Stylesheet,
        3 => ResourceType::Script,
        4 => ResourceType::Image,
        5 => ResourceType::Font,
        6 => ResourceType::Media,
        7 => ResourceType::Object,
        8 => ResourceType::XmlHttpRequest,
        9 => ResourceType::Ping,
        10 => ResourceType::WebSocket,
        _ => ResourceType::Other,
    }
}

/// The compact wire discriminant for a [`ResourceType`] (the inverse of
/// [`resource_from_wire`]).
#[must_use]
pub const fn resource_to_wire(ty: ResourceType) -> u8 {
    ty as u8
}

/// The shell-side ad-filter layer for one browser session: the compiled engine,
/// the current page's first-party origin context, and the per-page
/// blocked-request count.
pub struct RequestFilter {
    /// The compiled matcher (empty = blocks nothing; the default).
    engine: Engine,
    /// Operator-managed URL policy, enforced before any network request.
    managed_policy: ManagedUrlPolicy,
    /// Mesh-hosted unsafe host blocklist.
    safe_browsing: SafeBrowsingBlocklist,
    /// The host of the top-level page every subresource is judged against.
    first_party: String,
    /// The scheme of the top-level page, used for HTTPS mixed-content enforcement.
    first_party_scheme: String,
    /// Requests blocked on the current page (reset when the page host changes).
    blocked: u32,
    /// Per-page breakdown of what was blocked (by domain / by filter), for the
    /// in-chrome "N blocked" shield's detail hover. Reset with [`Self::blocked`].
    tally: BlockTally,
}

impl Default for RequestFilter {
    fn default() -> Self {
        Self::empty()
    }
}

impl RequestFilter {
    /// A filter that blocks nothing — the default until the shell injects a
    /// compiled engine. (The engine still exempts mesh/overlay hosts.)
    #[must_use]
    pub fn empty() -> Self {
        Self {
            engine: Engine::new(),
            managed_policy: ManagedUrlPolicy::empty(),
            safe_browsing: SafeBrowsingBlocklist::empty(),
            first_party: String::new(),
            first_party_scheme: String::new(),
            blocked: 0,
            tally: BlockTally::new(),
        }
    }

    /// Wrap an already-compiled [`Engine`].
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            managed_policy: ManagedUrlPolicy::empty(),
            safe_browsing: SafeBrowsingBlocklist::empty(),
            first_party: String::new(),
            first_party_scheme: String::new(),
            blocked: 0,
            tally: BlockTally::new(),
        }
    }

    /// Attach a mesh safe-browsing blocklist to this filter.
    #[must_use]
    pub fn with_safe_browsing(mut self, safe_browsing: SafeBrowsingBlocklist) -> Self {
        self.safe_browsing = safe_browsing;
        self
    }

    /// Attach an operator-managed URL policy to this filter.
    #[must_use]
    pub fn with_managed_policy(mut self, managed_policy: ManagedUrlPolicy) -> Self {
        self.managed_policy = managed_policy;
        self
    }

    /// Compile a filter from a [`FilterListStore`] (the primary glue point — the
    /// mackesd `adfilter` worker publishes the store, the shell compiles it here).
    #[must_use]
    pub fn from_store(store: &FilterListStore) -> Self {
        Self::new(Engine::from_store(store))
    }

    /// Compile a filter from the serialized store blob the `adfilter` worker
    /// replicates over Syncthing (`state/adfilter` / the compiled engine blob).
    ///
    /// # Errors
    /// Returns a human-readable message when `json` is not a valid serialized
    /// [`FilterListStore`].
    pub fn from_store_json(json: &str) -> Result<Self, String> {
        let store = FilterListStore::from_json(json)
            .map_err(|e| format!("adfilter blob is not a valid filter store: {e}"))?;
        Ok(Self::from_store(&store))
    }

    /// Set the current page's first-party host from its URL (or a bare host),
    /// resetting the per-page blocked counter **only** when the host actually
    /// changed. Returns whether the host changed (the caller re-pushes the
    /// cosmetic stylesheet on a change).
    pub fn set_page(&mut self, page_url: &str) -> bool {
        let scheme = scheme_of(page_url).unwrap_or_default();
        let host = host_of(page_url).unwrap_or_else(|| page_url.trim().to_ascii_lowercase());
        if host == self.first_party && scheme == self.first_party_scheme {
            return false;
        }
        self.first_party = host;
        self.first_party_scheme = scheme;
        self.blocked = 0;
        self.tally = BlockTally::new();
        true
    }

    /// The current first-party page host.
    #[must_use]
    pub fn first_party(&self) -> &str {
        &self.first_party
    }

    /// The current first-party page scheme (`https`, `http`, or empty for hostless pages).
    #[must_use]
    pub fn first_party_scheme(&self) -> &str {
        &self.first_party_scheme
    }

    /// Judge one outgoing subresource request against the engine. On a
    /// [`Decision::Block`] the per-page blocked counter is incremented; the caller
    /// drops the request. Mesh/overlay + allowlisted hosts are allowed by the
    /// engine (honored, not re-derived here).
    pub fn decide(&mut self, url: &str, resource_type: ResourceType) -> Decision {
        if let Some(rule) = self.managed_policy.matches(url) {
            self.blocked = self.blocked.saturating_add(1);
            let decision = Decision::Block {
                filter: format!("managed-policy:{rule}"),
            };
            self.tally.record(&decision, url);
            return decision;
        }
        if let Some(host) = host_of(url) {
            if matches!(
                self.engine
                    .match_request(url, resource_type, &self.first_party),
                Decision::Allow(AllowReason::Exempt)
            ) {
                return Decision::Allow(AllowReason::Exempt);
            }
            if let Some(blocked) = self.safe_browsing.matches(&host) {
                self.blocked = self.blocked.saturating_add(1);
                let decision = Decision::Block {
                    filter: format!("safe-browsing:{blocked}"),
                };
                self.tally.record(&decision, url);
                return decision;
            }
            if self.blocks_mixed_content(url, resource_type) {
                self.blocked = self.blocked.saturating_add(1);
                let decision = Decision::Block {
                    filter: "mixed-content:http".to_owned(),
                };
                self.tally.record(&decision, url);
                return decision;
            }
        }
        let decision = self
            .engine
            .match_request(url, resource_type, &self.first_party);
        if decision.is_block() {
            self.blocked = self.blocked.saturating_add(1);
            self.tally.record(&decision, url);
        }
        decision
    }

    fn blocks_mixed_content(&self, url: &str, resource_type: ResourceType) -> bool {
        self.first_party_scheme == "https"
            && resource_type != ResourceType::Document
            && url_scheme_is(url, "http")
    }

    /// The per-page block breakdown (by domain / by filter) — powers the "N blocked"
    /// shield's detail hover. Reset each time the page host changes.
    #[must_use]
    pub const fn tally(&self) -> &BlockTally {
        &self.tally
    }

    /// The number of requests blocked on the current page — the Browser surface's
    /// "N blocked" indicator.
    #[must_use]
    pub const fn blocked_count(&self) -> u32 {
        self.blocked
    }

    /// The JS-off-safe cosmetic user-stylesheet for the current page: every
    /// element-hide selector collapsed into one `display:none !important` rule.
    /// Empty when the host has no cosmetic rules (or is exempt/allowlisted — the
    /// engine returns no selectors there).
    #[must_use]
    pub fn cosmetic_stylesheet(&self) -> String {
        let selectors = self.engine.cosmetic_selectors(&self.first_party);
        if selectors.is_empty() {
            return String::new();
        }
        format!("{} {{ display: none !important; }}", selectors.join(", "))
    }
}

fn scheme_of(url: &str) -> Option<String> {
    let (scheme, _) = url.trim_start().split_once("://")?;
    (!scheme.is_empty()
        && scheme
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')))
    .then(|| scheme.to_ascii_lowercase())
}

fn url_scheme_is(url: &str, expected: &str) -> bool {
    url.trim_start()
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled_filter(page: &str) -> RequestFilter {
        let mut f = RequestFilter::from_store(&FilterListStore::with_bundled());
        f.set_page(page);
        f
    }

    #[test]
    fn resource_discriminant_round_trips_every_variant() {
        for ty in [
            ResourceType::Document,
            ResourceType::Subdocument,
            ResourceType::Stylesheet,
            ResourceType::Script,
            ResourceType::Image,
            ResourceType::Font,
            ResourceType::Media,
            ResourceType::Object,
            ResourceType::XmlHttpRequest,
            ResourceType::Ping,
            ResourceType::WebSocket,
            ResourceType::Other,
        ] {
            assert_eq!(resource_from_wire(resource_to_wire(ty)), ty);
        }
        // An unknown byte is the conservative `Other`.
        assert_eq!(resource_from_wire(200), ResourceType::Other);
    }

    #[test]
    fn a_bundled_tracker_request_is_blocked_and_counted() {
        let mut f = bundled_filter("https://news.example.com/");
        let d = f.decide(
            "https://www.google-analytics.com/collect",
            ResourceType::Script,
        );
        assert!(d.is_block(), "a bundled EasyPrivacy rule must block GA");
        assert_eq!(f.blocked_count(), 1);
        // A second tracker on the same page keeps counting.
        let d2 = f.decide("https://doubleclick.net/ad", ResourceType::Image);
        assert!(d2.is_block());
        assert_eq!(f.blocked_count(), 2);
    }

    #[test]
    fn the_block_tally_breaks_down_blocks_by_domain_and_resets_per_page() {
        let mut f = bundled_filter("https://news.example.com/");
        assert!(f
            .decide(
                "https://www.google-analytics.com/collect",
                ResourceType::Script
            )
            .is_block());
        assert!(f
            .decide("https://doubleclick.net/ad", ResourceType::Image)
            .is_block());

        // The shield's detail hover reads this: each blocked domain, counted.
        let by_domain = f.tally().by_domain();
        assert!(
            by_domain
                .iter()
                .any(|(d, _)| d.contains("google-analytics")),
            "GA appears in the by-domain breakdown"
        );
        assert!(
            by_domain.iter().any(|(d, _)| d.contains("doubleclick")),
            "doubleclick appears in the breakdown"
        );
        assert_eq!(
            by_domain.iter().map(|(_, n)| *n).sum::<u64>(),
            2,
            "two blocks recorded in total"
        );

        // Navigating to a new page host resets the per-page breakdown.
        f.set_page("https://other.example.com/");
        assert!(f.tally().is_empty(), "the tally resets with the page");
    }

    #[test]
    fn a_benign_first_party_request_passes_uncounted() {
        let mut f = bundled_filter("https://news.example.com/");
        let d = f.decide("https://news.example.com/app.js", ResourceType::Script);
        assert!(!d.is_block(), "the page's own script must pass");
        assert_eq!(f.blocked_count(), 0);
    }

    #[test]
    fn a_mesh_overlay_request_is_exempt() {
        let mut f = bundled_filter("https://news.example.com/");
        // Even a URL that would otherwise look ad-ish is exempt on the mesh TLD.
        let d = f.decide("https://media.mesh/pagead/x", ResourceType::XmlHttpRequest);
        assert!(!d.is_block(), "*.mesh is never filtered");
        assert!(matches!(
            d,
            Decision::Allow(mde_adblock::AllowReason::Exempt)
        ));
        // The Nebula overlay range is exempt too.
        assert!(!f
            .decide("https://10.42.0.9/pagead/x", ResourceType::Script)
            .is_block());
        assert_eq!(f.blocked_count(), 0);
    }

    #[test]
    fn cosmetic_stylesheet_carries_bundled_selectors() {
        let f = bundled_filter("https://news.example.com/");
        let css = f.cosmetic_stylesheet();
        assert!(css.contains("display: none !important"));
        // A generic bundled element-hide selector reaches the stylesheet.
        assert!(css.contains(".advertisement"), "css = {css}");
    }

    #[test]
    fn set_page_resets_the_counter_only_on_a_host_change() {
        let mut f = bundled_filter("https://a.example.com/");
        assert!(f
            .decide("https://doubleclick.net/", ResourceType::Image)
            .is_block());
        assert_eq!(f.blocked_count(), 1);
        // Same host (a different path) does NOT reset.
        assert!(!f.set_page("https://a.example.com/other"));
        assert_eq!(f.blocked_count(), 1);
        // A new host resets the per-page count.
        assert!(f.set_page("https://b.example.com/"));
        assert_eq!(f.blocked_count(), 0);
    }

    #[test]
    fn an_empty_filter_blocks_nothing_but_still_exempts_mesh() {
        let mut f = RequestFilter::empty();
        f.set_page("https://news.example.com/");
        assert!(!f
            .decide("https://doubleclick.net/ad", ResourceType::Script)
            .is_block());
        assert_eq!(f.blocked_count(), 0);
        assert!(f.cosmetic_stylesheet().is_empty());
    }

    #[test]
    fn from_store_json_round_trips_the_blob() {
        let json = FilterListStore::with_bundled()
            .to_json()
            .expect("serialize");
        let mut f = RequestFilter::from_store_json(&json).expect("parse blob");
        f.set_page("https://news.example.com/");
        assert!(f
            .decide("https://scorecardresearch.com/beacon", ResourceType::Ping)
            .is_block());
        // A malformed blob is a typed error, never a panic.
        assert!(RequestFilter::from_store_json("{not json").is_err());
    }

    #[test]
    fn safe_browsing_blocks_exact_and_subdomain_hosts_before_network() {
        let mut f = RequestFilter::empty()
            .with_safe_browsing(SafeBrowsingBlocklist::from_hosts(["malware.test"]));
        f.set_page("https://news.example.com/");

        let exact = f.decide("https://malware.test/payload", ResourceType::Document);
        assert_eq!(exact.blocked_by(), Some("safe-browsing:malware.test"));
        let subdomain = f.decide("https://cdn.malware.test/pixel", ResourceType::Image);
        assert_eq!(subdomain.blocked_by(), Some("safe-browsing:malware.test"));
        assert_eq!(f.blocked_count(), 2);
    }

    #[test]
    fn managed_policy_blocks_host_suffix_and_url_prefix_before_network() {
        let policy = ManagedUrlPolicy::from_rules([
            "blocked.example",
            "*.wild.example",
            "url:https://docs.example/private/",
        ]);
        assert_eq!(policy.len(), 3);
        assert_eq!(
            policy
                .matches("https://sub.blocked.example/path")
                .as_deref(),
            Some("host:blocked.example")
        );
        assert_eq!(
            policy.matches("https://news.wild.example/").as_deref(),
            Some("host:wild.example")
        );
        assert_eq!(
            policy
                .matches("https://docs.example/private/roadmap")
                .as_deref(),
            Some("url:https://docs.example/private/")
        );
        assert!(policy.matches("https://docs.example/public/").is_none());

        let mut f = RequestFilter::empty().with_managed_policy(policy);
        f.set_page("https://ok.example/");
        let decision = f.decide("https://blocked.example/", ResourceType::Document);
        assert_eq!(
            decision.blocked_by(),
            Some("managed-policy:host:blocked.example")
        );
        let decision = f.decide("https://docs.example/private/audit", ResourceType::Script);
        assert_eq!(
            decision.blocked_by(),
            Some("managed-policy:url:https://docs.example/private/")
        );
        assert_eq!(f.blocked_count(), 2);
    }

    #[test]
    fn managed_policy_url_prefixes_canonicalize_default_ports() {
        let policy = ManagedUrlPolicy::from_rules([
            "url:https://portal.example:443/admin/",
            "url:http://intranet.example:80/reports/",
        ]);

        assert_eq!(
            policy
                .matches("https://portal.example/admin/users")
                .as_deref(),
            Some("url:https://portal.example/admin/")
        );
        assert_eq!(
            policy
                .matches("https://portal.example:443/admin/users")
                .as_deref(),
            Some("url:https://portal.example/admin/")
        );
        assert_eq!(
            policy
                .matches("http://intranet.example:80/reports/q1")
                .as_deref(),
            Some("url:http://intranet.example/reports/")
        );
        assert!(
            policy
                .matches("https://portal.example:444/admin/users")
                .is_none(),
            "non-default ports remain distinct policy targets"
        );
    }

    #[test]
    fn managed_policy_authority_only_url_prefixes_keep_host_boundaries() {
        let policy = ManagedUrlPolicy::from_rules(["url:https://docs.example"]);

        assert_eq!(
            policy.matches("https://docs.example/private").as_deref(),
            Some("url:https://docs.example")
        );
        assert_eq!(
            policy
                .matches("https://docs.example:443/private")
                .as_deref(),
            Some("url:https://docs.example")
        );
        assert!(
            policy
                .matches("https://docs.example.evil/private")
                .is_none(),
            "an authority-only URL prefix must not raw-prefix match a different host"
        );
    }

    #[test]
    fn managed_policy_url_prefixes_ignore_authority_userinfo() {
        let policy = ManagedUrlPolicy::from_rules(["url:https://portal.example/admin/"]);

        assert_eq!(
            policy
                .matches("https://alice@portal.example/admin/users")
                .as_deref(),
            Some("url:https://portal.example/admin/")
        );
        assert_eq!(
            policy
                .matches("https://alice:secret@portal.example:443/admin/users")
                .as_deref(),
            Some("url:https://portal.example/admin/")
        );
        assert!(
            policy
                .matches("https://alice@portal.example.evil/admin/users")
                .is_none(),
            "userinfo stripping must not collapse a different host into the protected prefix"
        );
    }

    #[test]
    fn managed_policy_can_block_mesh_because_it_is_admin_policy() {
        let mut f = RequestFilter::empty()
            .with_managed_policy(ManagedUrlPolicy::from_rules(["search.mesh"]));
        f.set_page("https://ok.example/");

        let decision = f.decide("https://search.mesh/", ResourceType::Document);

        assert_eq!(
            decision.blocked_by(),
            Some("managed-policy:host:search.mesh")
        );
        assert_eq!(f.blocked_count(), 1);
    }

    #[test]
    fn safe_browsing_keeps_mesh_overlay_exempt() {
        let mut f = RequestFilter::empty().with_safe_browsing(SafeBrowsingBlocklist::from_hosts([
            "media.mesh",
            "10.42.0.9",
        ]));
        f.set_page("https://news.example.com/");

        assert!(matches!(
            f.decide("https://media.mesh/malware", ResourceType::Script),
            Decision::Allow(mde_adblock::AllowReason::Exempt)
        ));
        assert!(matches!(
            f.decide("https://10.42.0.9/malware", ResourceType::Script),
            Decision::Allow(mde_adblock::AllowReason::Exempt)
        ));
        assert_eq!(f.blocked_count(), 0);
    }

    #[test]
    fn https_pages_block_public_plain_http_subresources_before_network() {
        let mut f = RequestFilter::empty();
        assert!(f.set_page("https://app.example/"));
        assert_eq!(f.first_party_scheme(), "https");

        let mixed = f.decide("HTTP://cdn.example.test/app.js", ResourceType::Script);
        assert_eq!(mixed.blocked_by(), Some("mixed-content:http"));
        assert_eq!(f.blocked_count(), 1);

        let top_level_http = f.decide("http://docs.example.test/", ResourceType::Document);
        assert!(
            !top_level_http.is_block(),
            "top-level HTTP navigations stay with the shell navigation prompt"
        );
        let secure_subresource =
            f.decide("https://cdn.example.test/app.css", ResourceType::Stylesheet);
        assert!(!secure_subresource.is_block());
        assert_eq!(f.blocked_count(), 1);
    }

    #[test]
    fn mixed_content_keeps_mesh_overlay_exempt_and_http_pages_unaffected() {
        let mut f = RequestFilter::empty();
        f.set_page("https://portal.example/");

        assert!(matches!(
            f.decide("http://media.mesh/widget.js", ResourceType::Script),
            Decision::Allow(mde_adblock::AllowReason::Exempt)
        ));
        assert!(matches!(
            f.decide("http://10.42.0.9/status.json", ResourceType::XmlHttpRequest),
            Decision::Allow(mde_adblock::AllowReason::Exempt)
        ));
        assert_eq!(f.blocked_count(), 0);

        assert!(f.set_page("http://portal.example/"));
        assert_eq!(f.first_party_scheme(), "http");
        assert!(
            !f.decide("http://cdn.example.test/app.js", ResourceType::Script)
                .is_block(),
            "plain-HTTP pages are already downgraded, so this is not mixed content"
        );
    }
}
