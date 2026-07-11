//! Per-site data tracking for the Browser surface — how many tabs are open per
//! host, how many times a host's site-data has been cleared, and a human-readable
//! summary line. A self-contained unit (no `WebState` coupling); `use super::*`
//! only pulls in the parent's `plural`/`plural_u32` helpers + std collections.

use super::*;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SiteDataRecord {
    host: String,
    open_tabs: u32,
    last_seen_ms: u64,
    cleared_count: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SiteDataManager {
    sites: BTreeMap<String, SiteDataRecord>,
}

impl SiteDataManager {
    pub(super) fn observe_open_tabs<'a>(
        &mut self,
        hosts: impl IntoIterator<Item = &'a str>,
        now_ms: u64,
    ) {
        let mut counts = BTreeMap::<String, u32>::new();
        for host in hosts {
            let host = host.trim().to_ascii_lowercase();
            if !host.is_empty() {
                *counts.entry(host).or_insert(0) += 1;
            }
        }
        let active_hosts = counts.keys().cloned().collect::<BTreeSet<_>>();
        for (host, open_tabs) in counts {
            let record = self
                .sites
                .entry(host.clone())
                .or_insert_with(|| SiteDataRecord {
                    host,
                    ..SiteDataRecord::default()
                });
            record.open_tabs = open_tabs;
            record.last_seen_ms = now_ms;
        }
        for (host, record) in &mut self.sites {
            if !active_hosts.contains(host) {
                record.open_tabs = 0;
            }
        }
    }

    pub(super) fn mark_cleared(&mut self, host: &str, now_ms: u64) {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return;
        }
        let record = self
            .sites
            .entry(host.clone())
            .or_insert_with(|| SiteDataRecord {
                host,
                ..SiteDataRecord::default()
            });
        record.cleared_count = record.cleared_count.saturating_add(1);
        record.last_seen_ms = now_ms;
    }

    pub(super) fn summary(&self, active_host: Option<&str>) -> String {
        if self.sites.is_empty() {
            return "Site data: no visited sites tracked".to_owned();
        }
        let open_tabs = self.sites.values().map(|s| s.open_tabs).sum::<u32>();
        let cleared = active_host
            .and_then(|host| self.sites.get(host))
            .map_or(0, |s| s.cleared_count);
        match active_host {
            Some(host) => format!(
                "Site data: {} tracked site{} · {open_tabs} open tab{} · {host} cleared {cleared} time{}",
                self.sites.len(),
                plural(self.sites.len()),
                plural_u32(open_tabs),
                plural_u32(cleared),
            ),
            None => format!(
                "Site data: {} tracked site{} · {open_tabs} open tab{}",
                self.sites.len(),
                plural(self.sites.len()),
                plural_u32(open_tabs),
            ),
        }
    }
}
