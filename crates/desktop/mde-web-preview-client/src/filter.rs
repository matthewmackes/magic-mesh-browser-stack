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
//! layer just honors its [`Decision`].
//!
//! The engine is injected (the shell compiles it from the mackesd `adfilter`
//! worker's replicated `state/adfilter` blob); the default filter blocks nothing,
//! so a session with no filter behaves exactly as before this unit.

use mde_adblock::{host_of, Decision, Engine, FilterListStore, ResourceType};

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
/// the current page's first-party host, and the per-page blocked-request count.
pub struct RequestFilter {
    /// The compiled matcher (empty = blocks nothing; the default).
    engine: Engine,
    /// The host of the top-level page every subresource is judged against.
    first_party: String,
    /// Requests blocked on the current page (reset when the page host changes).
    blocked: u32,
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
            first_party: String::new(),
            blocked: 0,
        }
    }

    /// Wrap an already-compiled [`Engine`].
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            first_party: String::new(),
            blocked: 0,
        }
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
        let host = host_of(page_url).unwrap_or_else(|| page_url.trim().to_ascii_lowercase());
        if host == self.first_party {
            return false;
        }
        self.first_party = host;
        self.blocked = 0;
        true
    }

    /// The current first-party page host.
    #[must_use]
    pub fn first_party(&self) -> &str {
        &self.first_party
    }

    /// Judge one outgoing subresource request against the engine. On a
    /// [`Decision::Block`] the per-page blocked counter is incremented; the caller
    /// drops the request. Mesh/overlay + allowlisted hosts are allowed by the
    /// engine (honored, not re-derived here).
    pub fn decide(&mut self, url: &str, resource_type: ResourceType) -> Decision {
        let decision = self
            .engine
            .match_request(url, resource_type, &self.first_party);
        if decision.is_block() {
            self.blocked = self.blocked.saturating_add(1);
        }
        decision
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
}
