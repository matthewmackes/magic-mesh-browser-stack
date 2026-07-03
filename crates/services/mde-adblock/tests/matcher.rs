//! BOOKMARKS-7 — request **matcher** vectors (the definition-of-done fold tests).
//!
//! Builds an [`Engine`] from inline filter lists and asserts the block/allow
//! decision for concrete requests: an ad/tracker URL blocks, a benign
//! first-party asset passes, domain-anchored + `$option` rules match precisely,
//! `@@` exceptions override blocks, cosmetic-hide selectors are extracted per
//! host, and mesh/overlay + allowlisted sites are exempt.

use mde_adblock::{Decision, Engine, FilterList, FilterListStore, ResourceType};

fn engine(list: &str) -> Engine {
    let mut e = Engine::new();
    e.add_list(&FilterList::parse(list));
    e
}

const RT: ResourceType = ResourceType::Document;

#[test]
fn tracker_url_blocks_benign_passes() {
    let e = engine("||doubleclick.net^\n||google-analytics.com^\n");

    // An ad/tracker request is blocked.
    assert!(e
        .match_request("https://doubleclick.net/ad?x=1", RT, "news.example")
        .is_block());
    // A subdomain of the anchored domain is blocked.
    assert!(e
        .match_request(
            "https://stats.google-analytics.com/g/collect",
            RT,
            "news.example"
        )
        .is_block());
    // A benign first-party asset passes.
    assert!(!e
        .match_request("https://news.example/logo.png", RT, "news.example")
        .is_block());
}

#[test]
fn domain_anchor_respects_label_boundary() {
    let e = engine("||doubleclick.net^\n");
    // Exact + subdomain match.
    assert!(e
        .match_request("https://doubleclick.net/x", RT, "a.example")
        .is_block());
    assert!(e
        .match_request("https://ad.doubleclick.net/x", RT, "a.example")
        .is_block());
    // A look-alike domain must NOT match (the `^` separator + label boundary).
    assert!(!e
        .match_request("https://notdoubleclick.net/x", RT, "a.example")
        .is_block());
    assert!(!e
        .match_request("https://doubleclick.network/x", RT, "a.example")
        .is_block());
}

#[test]
fn resource_type_option_scopes_the_rule() {
    let e = engine("||cdn.example/widget$script\n");
    // Same URL, different resource type → only the script is blocked.
    assert!(e
        .match_request(
            "https://cdn.example/widget.js",
            ResourceType::Script,
            "site.test"
        )
        .is_block());
    assert!(!e
        .match_request(
            "https://cdn.example/widget.png",
            ResourceType::Image,
            "site.test"
        )
        .is_block());
}

#[test]
fn third_party_option_scopes_the_rule() {
    let e = engine("||analytics.example^$third-party\n");
    // Third-party context blocks.
    assert!(e
        .match_request("https://analytics.example/collect", RT, "publisher.test")
        .is_block());
    // First-party context (same registrable domain) passes.
    assert!(!e
        .match_request("https://analytics.example/collect", RT, "analytics.example")
        .is_block());
}

#[test]
fn domain_option_scopes_the_rule() {
    let e = engine("/track.js$domain=publisher.test\n");
    // On the named first-party page → blocked.
    assert!(e
        .match_request("https://cdn.other/track.js", RT, "publisher.test")
        .is_block());
    // On a different page → not blocked.
    assert!(!e
        .match_request("https://cdn.other/track.js", RT, "elsewhere.test")
        .is_block());
}

#[test]
fn exception_rule_overrides_block() {
    let e = engine("||track.net^\n@@||track.net/allowed.js\n");
    // The allowlisted asset is un-blocked via the @@ exception.
    let decision = e.match_request("https://track.net/allowed.js", RT, "site.test");
    assert!(
        matches!(
            decision,
            Decision::Allow(mde_adblock::AllowReason::Exception(_))
        ),
        "an @@ rule should win and record an Exception reason, got {decision:?}"
    );
    // A different asset on the same host is still blocked.
    assert!(e
        .match_request("https://track.net/ads.js", RT, "site.test")
        .is_block());
}

#[test]
fn cosmetic_selectors_extracted_per_host() {
    let e = engine("##.ad-banner\nnews.example##.promo\nnews.example#@#.ad-banner\n");

    // On news.example: the specific `.promo` hides; the generic `.ad-banner` is
    // un-hidden by the site's #@# exception.
    let news = e.cosmetic_selectors("news.example");
    assert_eq!(
        news,
        vec![".promo".to_string()],
        "specific hide, generic un-hidden"
    );

    // On another site: only the generic `.ad-banner` applies.
    let other = e.cosmetic_selectors("other.example");
    assert_eq!(other, vec![".ad-banner".to_string()]);
}

#[test]
fn mesh_and_overlay_hosts_are_exempt() {
    let e = engine("||track.net^\n||collect.mesh^\n");
    // A `.mesh` service host is never blocked, even with a matching rule.
    assert!(matches!(
        e.match_request("https://collect.mesh/api", RT, "shell.mesh"),
        Decision::Allow(mde_adblock::AllowReason::Exempt)
    ));
    // A Nebula overlay IP (10.42.0.0/16) is exempt.
    assert!(matches!(
        e.match_request("http://10.42.0.9/metrics", RT, "shell.mesh"),
        Decision::Allow(mde_adblock::AllowReason::Exempt)
    ));
    // A normal tracker is still blocked.
    assert!(e
        .match_request("https://track.net/x", RT, "site.test")
        .is_block());
}

#[test]
fn allowlisted_site_disables_blocking() {
    let mut store = FilterListStore::new();
    store.add_source(mde_adblock::FilterListSource::custom(
        "test",
        None,
        "||doubleclick.net^",
        1,
    ));
    store.allow_site("news.example", "host-a", 10);
    let e = Engine::from_store(&store);

    // On the allowlisted site, even a tracker request is allowed.
    assert!(matches!(
        e.match_request("https://doubleclick.net/ad", RT, "news.example"),
        Decision::Allow(mde_adblock::AllowReason::Allowlisted)
    ));
    // On a non-allowlisted site, it is blocked.
    assert!(e
        .match_request("https://doubleclick.net/ad", RT, "other.example")
        .is_block());
    // No cosmetic hiding on an allowlisted site.
    assert!(e.cosmetic_selectors("news.example").is_empty());
}

#[test]
fn hostless_urls_are_allowed() {
    let e = engine("||doubleclick.net^\n");
    // data:/about: URLs carry no host to anchor a rule to.
    assert!(!e
        .match_request("data:text/html,<b>hi</b>", RT, "site.test")
        .is_block());
    assert!(!e.match_request("about:blank", RT, "site.test").is_block());
}

#[test]
fn bundled_seed_blocks_common_trackers() {
    let e = Engine::from_store(&FilterListStore::with_bundled());
    assert!(e.network_rule_count() > 20, "seed carries real rules");
    for url in [
        "https://www.google-analytics.com/collect",
        "https://securepubads.g.doubleclick.net/x",
        "https://static.criteo.net/js/ld.js",
        "https://cdn.taboola.com/libtrc/x.js",
    ] {
        assert!(
            e.match_request(url, ResourceType::Script, "news.example")
                .is_block(),
            "seed should block {url}"
        );
    }
    // A first-party asset is untouched by the seed.
    assert!(!e
        .match_request("https://news.example/article.html", RT, "news.example")
        .is_block());
}
