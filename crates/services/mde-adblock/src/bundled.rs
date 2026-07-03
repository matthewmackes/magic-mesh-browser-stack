//! [`bundled_sources`] — the compact bundled **seed** filter lists (BOOKMARKS-7).
//!
//! The offline fallback that ships in the RPM: a small, hand-curated subset of
//! the most common ad-network, tracker and cosmetic filters so a fresh node
//! blocks the worst offenders *before* its first upstream sync. These are **real,
//! matchable rules** (not a mock) — the same parser + matcher runs them as would
//! run the full lists. The complete `EasyList` / `EasyPrivacy` / `uBlock-base` blobs
//! (tens of thousands of rules) are fetched from the `url` and replicated over
//! Syncthing by the mackesd `adfilter` worker (the BOOKMARKS-7 wiring follow-on);
//! [`crate::Staleness`] tells the operator when a node is still on this seed.

use crate::store::FilterListSource;

/// Common ad-network block + cosmetic rules (`EasyList` subset).
pub const EASYLIST_SEED: &str = "\
! Title: MCNF EasyList seed (bundled fallback subset)
! Full list: https://easylist.to/easylist/easylist.txt
||doubleclick.net^
||ad.doubleclick.net^
||googlesyndication.com^
||pagead2.googlesyndication.com^
||adservice.google.com^
||googleadservices.com^
||2mdn.net^
||amazon-adsystem.com^
||adnxs.com^
||advertising.com^
||moatads.com^
||criteo.com^
||criteo.net^
||taboola.com^
||outbrain.com^
||adform.net^
||rubiconproject.com^
||pubmatic.com^
||openx.net^
||casalemedia.com^
||smartadserver.com^
||adroll.com^
||media.net^
/pagead/*
/adsbygoogle
##.ad-banner
##.advertisement
##.sponsored
##.sponsored-content
###ad-container
###banner-ad
";

/// Common tracker / analytics block rules (`EasyPrivacy` subset).
pub const EASYPRIVACY_SEED: &str = "\
! Title: MCNF EasyPrivacy seed (bundled fallback subset)
! Full list: https://easylist.to/easylist/easyprivacy.txt
||google-analytics.com^
||ssl.google-analytics.com^
||googletagmanager.com^
||googletagservices.com^
||stats.g.doubleclick.net^
||scorecardresearch.com^
||quantserve.com^
||quantcount.com^
||hotjar.com^
||mixpanel.com^
||segment.io^
||segment.com^
||fullstory.com^
||amplitude.com^
||bat.bing.com^
||analytics.twitter.com^
||facebook.com/tr^
||matomo.cloud^
||chartbeat.com^
||newrelic.com^
||nr-data.net^
||branch.io^
";

/// Common resource-type + cosmetic rules (uBlock-base subset).
pub const UBLOCK_BASE_SEED: &str = "\
! Title: MCNF uBlock-base seed (bundled fallback subset)
! Full list: https://ublockorigin.github.io/uAssets/filters/filters.txt
||adsafeprotected.com^
||adsrvr.org^
||serving-sys.com^
||demdex.net^
||everesttech.net^
||3lift.com^
||sharethrough.com^
||teads.tv^
||yieldmo.com^
||indexww.com^
||bidswitch.net^
##.ads
##.ad-slot
##.banner-ads
##[id^=\"google_ads_\"]
##[class^=\"adunit\"]
";

/// The three bundled seed sources, each with its upstream URL for the worker to
/// refresh from. All enabled, `updated_ms` 0 (the baseline before a sync).
#[must_use]
pub fn bundled_sources() -> Vec<FilterListSource> {
    vec![
        FilterListSource::bundled(
            "EasyList",
            Some("https://easylist.to/easylist/easylist.txt".to_string()),
            EASYLIST_SEED,
        ),
        FilterListSource::bundled(
            "EasyPrivacy",
            Some("https://easylist.to/easylist/easyprivacy.txt".to_string()),
            EASYPRIVACY_SEED,
        ),
        FilterListSource::bundled(
            "uBlock-base",
            Some("https://ublockorigin.github.io/uAssets/filters/filters.txt".to_string()),
            UBLOCK_BASE_SEED,
        ),
    ]
}
