//! BUS-1.6 — first-run seed of the 12 curated default topics.
//!
//! Round 3 of the 104-Q poll: every fresh mde-bus install ships with
//! a documented set of 12 topics that cover the existing MON +
//! GF-17 + FDO use cases, so operators do not stare at an empty
//! Workbench Bus subpage on day one. The set is locked in
//! `docs/design/v6.x-mackes-bus.md` § 3.1.
//!
//! The seed is **idempotent**. Calling [`seed_defaults`] twice on
//! the same registry leaves it unchanged after the first call, which
//! makes the worklist BUS-1.6 acceptance criterion ("second launch is
//! a no-op") trivially testable.

use crate::topic::{Priority, Registry, Topic, TopicError};

/// Per-peer hostname placeholder used when a curated default refers
/// to `peer/$hostname/*`. The seed function substitutes the current
/// hostname (or this fallback) when generating those topics.
const HOSTNAME_FALLBACK: &str = "self";

/// Curated default topics shipped on first run. Adding a row here
/// adds a topic to the bus.
///
/// `name_template` may contain the literal string `$hostname`, which
/// the seed function replaces with the current peer's hostname so
/// each peer ends up with a `peer/<its-hostname>/...` lane.
struct CuratedTopic {
    name_template: &'static str,
    description: &'static str,
    priority_default: Priority,
    retention_s: Option<u64>,
}

/// The 12 curated defaults. Source of truth — `docs/design/v6.x-mackes-bus.md`
/// § 3.1. Any change here also updates the design doc.
const CURATED: &[CuratedTopic] = &[
    CuratedTopic {
        name_template: "fleet/announce",
        description: "Mesh-wide operator announcements.",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "fleet/sec",
        description: "Security events — passcode rotation, enrolment, signed-CSR transitions.",
        priority_default: Priority::High,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "peer/$hostname/alerts",
        description: "Per-peer alert lane (local mackesd publishes here).",
        priority_default: Priority::High,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "peer/$hostname/system",
        description: "Per-peer system events from mded (boot, shutdown, reload).",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mon/cpu",
        description: "Netdata aggregator — CPU threshold breaches (BUS-4.3 dual-write).",
        priority_default: Priority::High,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mon/memory",
        description: "Netdata aggregator — RAM threshold breaches.",
        priority_default: Priority::High,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mon/disk",
        description: "Netdata aggregator — disk + mesh-storage quota breaches.",
        priority_default: Priority::High,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mon/network",
        description: "Netdata aggregator — bandwidth + Nebula link state.",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mesh/peers",
        description: "Peer up/down events from mackesd::workers::health_reconciler.",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "mesh/leader",
        description: "QNM-Shared lockfile transitions — leader-election changes.",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "fdo/system",
        description: "FDO desktop notifications from mded — system tray, daemon health.",
        priority_default: Priority::Default,
        retention_s: None,
    },
    CuratedTopic {
        name_template: "clipboard/sync",
        description: "Global clipboard payloads from the clipboard_sync worker + KDC phone clip bridge (BUS-5).",
        priority_default: Priority::Min,
        retention_s: None,
    },
];

/// Resolve the current hostname for `peer/$hostname/*` topic seeding.
/// Falls back to `self` when [`hostname::get`] is unavailable, which
/// matches `crates/mde-portal/src/app.rs:Portal-6`'s pre-mesh-home
/// fallback behavior so the seeded topic names match what other
/// surfaces display.
fn current_hostname() -> String {
    // /proc/sys/kernel/hostname is the kernel's source of truth on
    // Linux and avoids pulling a `hostname` crate dep for this single
    // call. Matches the read pattern in
    // `crates/mde-portal/src/app.rs`.
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| HOSTNAME_FALLBACK.to_string())
}

/// Seed the 12 curated default topics into the given registry.
/// Returns the number of topics **newly created** by this call
/// (existing topics are left untouched, so a second call returns 0).
pub fn seed_defaults(reg: &mut Registry) -> Result<usize, TopicError> {
    seed_defaults_with_hostname(reg, &current_hostname())
}

/// Same as [`seed_defaults`] but takes an explicit hostname so unit
/// tests can pin the substitution.
pub fn seed_defaults_with_hostname(
    reg: &mut Registry,
    hostname: &str,
) -> Result<usize, TopicError> {
    let mut created = 0usize;
    for c in CURATED {
        let name = c.name_template.replace("$hostname", hostname);
        let topic = Topic {
            name,
            description: c.description.to_string(),
            priority_default: c.priority_default,
            retention_s: c.retention_s,
        };
        if reg.create(topic)? {
            created += 1;
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seed_creates_twelve_topics() {
        let mut r = Registry::new();
        let created = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        assert_eq!(created, 12, "expected 12 curated topics on first run");
        assert_eq!(r.len(), 12);
    }

    #[test]
    fn second_seed_is_a_noop() {
        let mut r = Registry::new();
        let _ = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        let created = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        assert_eq!(created, 0, "expected the second seed call to be a no-op");
        assert_eq!(r.len(), 12);
    }

    #[test]
    fn hostname_substitution_runs() {
        let mut r = Registry::new();
        let _ = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        assert!(
            r.get("peer/alpha/alerts").is_some(),
            "peer/$hostname/alerts should expand to peer/alpha/alerts"
        );
        assert!(
            r.get("peer/alpha/system").is_some(),
            "peer/$hostname/system should expand to peer/alpha/system"
        );
        assert!(
            r.get("peer/$hostname/alerts").is_none(),
            "literal $hostname must not survive substitution"
        );
    }

    #[test]
    fn dotted_unenrolled_hostname_substitution_runs() {
        let mut r = Registry::new();
        let created = seed_defaults_with_hostname(&mut r, "localhost.localdomain").unwrap();
        assert_eq!(created, 12);
        assert!(
            r.get("peer/localhost.localdomain/alerts").is_some(),
            "fresh Fedora's default dotted hostname must not break topic seeding"
        );
        assert!(
            r.get("peer/localhost.localdomain/system").is_some(),
            "fresh Fedora's default dotted hostname must not break topic seeding"
        );
    }

    #[test]
    fn seeded_topics_cover_every_class() {
        let mut r = Registry::new();
        let _ = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        // Each top-level class is represented at least once — guards
        // against accidental deletion from CURATED.
        for class in ["fleet/", "peer/", "mon/", "mesh/", "fdo/", "clipboard/"] {
            assert!(
                r.iter().any(|t| t.name.starts_with(class)),
                "expected at least one curated topic under `{class}`"
            );
        }
    }

    #[test]
    fn clipboard_sync_is_min_priority() {
        let mut r = Registry::new();
        let _ = seed_defaults_with_hostname(&mut r, "alpha").unwrap();
        // Round 11 lock — clipboard adds land as neutral-grey
        // Breadcrumb segments, never as audible high/urgent banners.
        // Encoded by seeding the topic with Min priority so any
        // unspecified publish stays silent.
        let t = r
            .get("clipboard/sync")
            .expect("clipboard/sync must be seeded");
        assert_eq!(t.priority_default, Priority::Min);
    }
}
