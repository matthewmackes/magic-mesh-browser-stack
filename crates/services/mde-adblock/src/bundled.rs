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
//!
//! BOOKMARKS-9 (packaging): the seed bodies live as plain EasyList-format text
//! files under `seed/` and are `include_str!`'d here — so the compiled binary and
//! the loose copies the RPM ships at `/usr/share/magic-mesh/adblock/` are the SAME
//! bytes from ONE source (no drift). The packaging `assets` array ships those same
//! `seed/*.txt` files, satisfying the acceptance's "bundles the seed filter lists".

use crate::store::FilterListSource;

/// Common ad-network block + cosmetic rules (`EasyList` subset).
pub const EASYLIST_SEED: &str = include_str!("../seed/easylist-seed.txt");

/// Common tracker / analytics block rules (`EasyPrivacy` subset).
pub const EASYPRIVACY_SEED: &str = include_str!("../seed/easyprivacy-seed.txt");

/// Common resource-type + cosmetic rules (uBlock-base subset).
pub const UBLOCK_BASE_SEED: &str = include_str!("../seed/ublock-base-seed.txt");

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
