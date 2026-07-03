//! BOOKMARKS-7 — filter-list **store / service** tests.
//!
//! The mesh-distributable half: the bundled seed loads + compiles, two nodes'
//! stores converge under the last-writer-wins [`FilterListStore::merge`] (both
//! for filter sources and the per-site allowlist), the [`Staleness`] indicator
//! classifies freshness honestly, and the store round-trips through the JSON
//! blob the worker replicates over Syncthing.

use mde_adblock::{Engine, FilterListSource, FilterListStore, ListKind, ResourceType, Staleness};

const RT: ResourceType = ResourceType::Document;

#[test]
fn bundled_store_loads_and_compiles() {
    let store = FilterListStore::with_bundled();
    assert_eq!(store.enabled_sources().count(), 3, "3 seed lists");
    assert!(store.sources().iter().all(|s| s.kind == ListKind::Bundled));

    let e = Engine::from_store(&store);
    assert!(e
        .match_request("https://doubleclick.net/ad", RT, "news.example")
        .is_block());
}

#[test]
fn disabled_source_is_not_compiled() {
    let mut store = FilterListStore::new();
    store.add_source(FilterListSource::custom("t", None, "||doubleclick.net^", 1));
    assert!(store.set_enabled("t", false));
    let e = Engine::from_store(&store);
    assert!(!e
        .match_request("https://doubleclick.net/ad", RT, "news.example")
        .is_block());
}

#[test]
fn merge_sources_is_last_writer_wins() {
    let mut a = FilterListStore::new();
    a.add_source(FilterListSource::custom(
        "EasyList",
        None,
        "||old.example^",
        100,
    ));

    let mut b = FilterListStore::new();
    // Same-name source, newer timestamp + different text → should win.
    b.add_source(FilterListSource::custom(
        "EasyList",
        None,
        "||new.example^",
        200,
    ));
    // A source only b has → should be added.
    b.add_source(FilterListSource::custom(
        "Custom",
        None,
        "||extra.example^",
        50,
    ));

    a.merge(&b);
    assert_eq!(a.sources().len(), 2);
    let easylist = a.source("EasyList").expect("EasyList present");
    assert_eq!(easylist.raw, "||new.example^", "newer write wins");
    assert!(a.source("Custom").is_some(), "b-only source added");

    // Merging the OLDER store back into b must not regress b.
    let mut b2 = b.clone();
    b2.merge(&a);
    assert_eq!(
        b2.source("EasyList").expect("present").raw,
        "||new.example^",
        "older write does not clobber newer"
    );
}

#[test]
fn merge_allowlist_is_last_writer_wins() {
    let mut a = FilterListStore::new();
    a.allow_site("news.example", "host-a", 100);

    let mut b = FilterListStore::new();
    // A later re-enable of blocking on the same site.
    b.block_site("news.example", "host-b", 200);

    a.merge(&b);
    assert!(
        !a.allowlist().is_allowed("news.example"),
        "the later block wins over the earlier allow"
    );

    // The reverse order converges to the same result.
    let mut c = FilterListStore::new();
    c.block_site("news.example", "host-b", 200);
    c.merge(&{
        let mut x = FilterListStore::new();
        x.allow_site("news.example", "host-a", 100);
        x
    });
    assert!(
        !c.allowlist().is_allowed("news.example"),
        "merge converges regardless of order"
    );
}

#[test]
fn staleness_indicator_is_honest() {
    let day_ms: u64 = 24 * 60 * 60 * 1000;

    // A fresh store has never synced upstream → running on the bundled seed.
    let mut store = FilterListStore::with_bundled();
    assert_eq!(store.staleness(day_ms, 7 * day_ms), Staleness::NeverSynced);

    // After an upstream refresh, it is fresh within the window …
    assert!(store.update_source("EasyList", "||new.example^", 10 * day_ms));
    assert_eq!(
        store.staleness(11 * day_ms, 7 * day_ms),
        Staleness::Fresh,
        "1 day old, 7-day window"
    );

    // … and stale beyond it, reporting its age.
    let stale = store.staleness(20 * day_ms, 7 * day_ms);
    assert!(
        matches!(stale, Staleness::Stale { age_ms } if age_ms == 10 * day_ms),
        "expected Stale(10d), got {stale:?}"
    );
}

#[test]
fn store_round_trips_through_json() {
    let mut store = FilterListStore::with_bundled();
    store.allow_site("news.example", "host-a", 42);
    store.add_exempt_suffix("corp.internal");
    assert!(store.update_source("EasyPrivacy", "||tracker.example^", 99));

    let json = store.to_json().expect("serialize");
    let back = FilterListStore::from_json(&json).expect("deserialize");
    assert_eq!(store, back, "serde round-trip is lossless");

    // The exempt suffix survives into the compiled engine.
    let e = Engine::from_store(&back);
    assert!(e.is_exempt("host.corp.internal"));
}
