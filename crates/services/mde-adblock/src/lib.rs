//! mde-adblock — the pure matching-engine + filter-list service model for the
//! mesh-wide **ad-blocker** (BOOKMARKS-7; see `docs/WORKLIST.md`).
//!
//! An EasyList/EasyPrivacy/uBlock-style filter engine that every enrolled node
//! shares: a leader compiles the filter lists into a serialized store, the store
//! replicates over Syncthing, and each node's browser matches every network
//! request against it — ad/tracker requests are blocked before fetch and
//! cosmetic filters hide leftover ad frames. This crate is the **headless
//! model** both the mackesd `adfilter` worker (the Syncthing replication +
//! leader compile) and the `mde-web-preview` Servo browser (the in-page network
//! + cosmetic blocking) import — no Servo, no Syncthing, no Bus, no I/O.
//!
//! The pieces:
//!
//!   * [`ResourceType`] — the request classes a rule can scope to (`script`,
//!     `image`, `stylesheet`, `xmlhttprequest`, `subdocument`, …), matching the
//!     ABP `$type` option names (resource.rs).
//!   * [`Pattern`] — a compiled filter **match pattern**: the ABP anchors
//!     (`||` domain-anchor, `|` start/end), the `^` separator placeholder and
//!     the `*` wildcard, matched case-insensitively against a URL (pattern.rs).
//!   * [`NetworkRule`] / [`CosmeticRule`] / [`parse_line`] — one filter line
//!     parsed into a typed rule: a network **block** or `@@` **allow/exception**
//!     rule with its `$options` (resource-type include/exclude, `third-party`,
//!     `domain=`), or a `##` element-hide / `#@#` un-hide **cosmetic** rule
//!     (rule.rs).
//!   * [`FilterList`] — a whole EasyList-format list parsed into its network +
//!     cosmetic rules, with parse stats (supported / comment / unsupported)
//!     (parser.rs).
//!   * [`Engine`] / [`Decision`] / [`RequestContext`] — the compiled matcher:
//!     [`Engine::match_request`] returns [`Decision::Block`] (a network rule
//!     matched, carrying the filter) or [`Decision::Allow`] (no rule, an `@@`
//!     exception, an allowlisted first-party, or an exempt mesh/overlay domain),
//!     and [`Engine::cosmetic_selectors`] yields the element-hide selectors for
//!     a host's injected user-stylesheet (engine.rs).
//!   * [`FilterListStore`] / [`FilterListSource`] / [`Allowlist`] — the
//!     **service half**: the serde-persisted, mesh-distributable set of enabled
//!     filter sources (bundled seed + operator custom), the per-site allowlist
//!     (block-on-by-default; the operator opts a site out), and the
//!     [`Staleness`] indicator + a last-writer-wins [`FilterListStore::merge`]
//!     so two nodes' stores converge (store.rs).
//!   * [`bundled_sources`] — the compact **bundled seed** of common ad/tracker
//!     filters that ships in the RPM as the offline fallback when a fresh sync
//!     of the full upstream lists is unavailable (bundled.rs).
//!   * [`BlockTally`] — a per-session, in-memory tally the browser chrome feeds
//!     after each [`Engine::check`] to surface a "N trackers blocked"
//!     breakdown by domain and by matched filter; no persistence, no Bus
//!     (tally.rs).
//!
//! **Zero I/O**: no Servo, no Syncthing, no Bus, no wall clock, no network — the
//! live filter-list replication is the mackesd `adfilter` worker (BOOKMARKS-7
//! wiring) and the in-browser request/cosmetic blocking is `mde-web-preview`'s.
//! Timestamps are passed in (`now_ms`), never read. Services tier: no
//! desktop-shell dep (the layered-tiers gate).

#![forbid(unsafe_code)]

mod bundled;
mod engine;
mod parser;
mod pattern;
mod resource;
mod rule;
mod store;
mod tally;
mod url;

pub use bundled::{bundled_sources, EASYLIST_SEED, EASYPRIVACY_SEED, UBLOCK_BASE_SEED};
pub use engine::{AllowReason, Decision, Engine, RequestContext};
pub use parser::{FilterList, ParseStats};
pub use pattern::{is_separator, Pattern};
pub use resource::ResourceType;
pub use rule::{parse_line, CosmeticRule, NetworkRule, ParsedLine, RuleOptions};
pub use store::{
    Allowlist, AllowlistEntry, FilterListSource, FilterListStore, ListKind, Staleness,
};
pub use tally::BlockTally;
pub use url::{host_of, is_third_party, registrable_domain};
