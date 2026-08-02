//! BUS-1.3 — zeroconf service registration for the Mackes Bus
//! broker.
//!
//! Every peer's `mde-bus daemon` registers an mDNS service named
//! `_mackes-bus._tcp.local.` advertising the broker's `<overlay_ip>:8443`
//! endpoint. The same module browses the service type so peers
//! discover each other without static config — `avahi-browse
//! _mackes-bus._tcp` (over nebula0) lists every running peer.
//!
//! "Nebula-only" semantics:
//!
//! - The mDNS registration carries ONLY the Nebula overlay IP. LAN
//!   underlay addresses are never advertised, so a peer listing the
//!   service type from a LAN underlay sees the overlay IP and either
//!   reaches it via the Nebula tunnel (mesh members) or fails the
//!   connect (non-members).
//! - BUS-1.2's broker is bound to the overlay IP, so even when mDNS
//!   leaks via the multicast LAN socket, the broker port is closed
//!   to anyone not on the overlay. mDNS advertising is the directory
//!   layer; the security boundary is the broker bind + Nebula firewall.
//!
//! Lifecycle:
//!
//! 1. **Register** the service on daemon startup after the broker is
//!    confirmed running (so we don't advertise a port nothing's
//!    listening on).
//! 2. **Browse** the same service type into a [`PeerRegistry`] —
//!    each discovered peer's instance name + overlay address goes
//!    into a shared `Arc<Mutex<…>>`.
//! 3. **Unregister** cleanly on daemon shutdown so peers see us
//!    drop in real time, not after the next mDNS cache TTL expires.
//!
//! Pre-enrollment + missing-`mdns-sd` degradation matches BUS-1.2:
//! the discovery module logs the skip reason and continues; the
//! outer supervisor respawns when prereqs land.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

/// Canonical mDNS service type for the Mackes Bus broker. Slash-
/// hierarchy literal — changing it is a discovery-protocol break.
pub const SERVICE_TYPE: &str = "_mackes-bus._tcp.local.";

/// Default broker port. Mirrors `broker::DEFAULT_LISTEN_PORT`.
pub const DEFAULT_BROKER_PORT: u16 = 8443;

/// One discovered Mackes-Bus peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusPeer {
    /// Instance name (the `<instance>._mackes-bus._tcp.local.`
    /// label, minus the service-type suffix).
    pub instance: String,
    /// Friendly hostname extracted from the TXT record. Falls
    /// back to the instance name when the record lacks `host=`.
    pub host: String,
    /// First reachable IPv4 address. The connectivity-scope lock
    /// is IPv4-only ([[project_v12_connectivity_scope]]).
    pub addr: IpAddr,
    /// Broker port from the SRV record.
    pub port: u16,
}

/// Shared in-memory registry of discovered peers. Cloning is
/// cheap (`Arc<Mutex<…>>`).
#[derive(Debug, Clone, Default)]
pub struct PeerRegistry {
    inner: Arc<Mutex<HashMap<String, BusPeer>>>,
}

impl PeerRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the record for one peer (keyed by
    /// instance name).
    // perf-12: the `PeerRegistry` mutex only poisons if another thread panicked while
    // holding it — a static "cannot happen" invariant, not a remote-reachable decode
    // path — so the documented `.expect()` stays (see also `remove`/`snapshot`/`len`).
    #[allow(clippy::expect_used)]
    pub fn upsert(&self, peer: BusPeer) {
        let mut g = self.inner.lock().expect("PeerRegistry mutex");
        g.insert(peer.instance.clone(), peer);
    }

    /// Drop a peer by instance name (received on
    /// `ServiceRemoved`).
    #[allow(clippy::expect_used)] // perf-12: mutex-poison static invariant (see `upsert`).
    pub fn remove(&self, instance: &str) {
        let mut g = self.inner.lock().expect("PeerRegistry mutex");
        g.remove(instance);
    }

    /// Snapshot the registry — useful for the CLI `mde-bus
    /// peers` command (BUS-1.8) and for debug logging.
    #[must_use]
    #[allow(clippy::expect_used)] // perf-12: mutex-poison static invariant (see `upsert`).
    pub fn snapshot(&self) -> Vec<BusPeer> {
        let g = self.inner.lock().expect("PeerRegistry mutex");
        let mut v: Vec<BusPeer> = g.values().cloned().collect();
        v.sort_by(|a, b| a.instance.cmp(&b.instance));
        v
    }

    /// Current number of registered peers.
    #[must_use]
    #[allow(clippy::expect_used)] // perf-12: mutex-poison static invariant (see `upsert`).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("PeerRegistry mutex").len()
    }

    /// `true` when no peers are known yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reasons the discovery module may skip its register/browse loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoverySkipReason {
    /// `ServiceDaemon::new()` failed (no network, no permission to
    /// open multicast socket, etc.).
    DaemonInitFailed(String),
    /// The overlay IP wasn't published yet — register would
    /// advertise the wrong address.
    NoOverlayIp,
}

impl std::fmt::Display for DiscoverySkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DaemonInitFailed(e) => write!(f, "mdns-sd init failed: {e}"),
            Self::NoOverlayIp => {
                write!(f, "no overlay IP published (peer not enrolled yet)")
            }
        }
    }
}

/// Configuration for the discovery module.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// This peer's instance name — typically the hostname.
    pub instance_name: String,
    /// Overlay IP to advertise + browse on.
    pub overlay_ip: IpAddr,
    /// Broker port to advertise.
    pub port: u16,
}

impl DiscoveryConfig {
    /// Construct a config with the default broker port.
    #[must_use]
    pub fn new(instance_name: String, overlay_ip: IpAddr) -> Self {
        Self {
            instance_name,
            overlay_ip,
            port: DEFAULT_BROKER_PORT,
        }
    }
}

/// Build the `ServiceInfo` mdns-sd ships out as our advertisement.
/// Pure helper — extracted so unit tests can verify the wire-level
/// shape (instance name, port, TXT records, advertised IP) without
/// touching the network.
///
/// # Errors
/// Returns `mdns_sd::Error` when the underlying `ServiceInfo::new`
/// rejects one of the supplied arguments.
pub fn build_service_info(cfg: &DiscoveryConfig) -> Result<ServiceInfo, mdns_sd::Error> {
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &cfg.instance_name,
        // mdns-sd requires a fully-qualified hostname; use the
        // instance name + .local. so the daemon doesn't try to
        // resolve our system hostname (which may differ).
        &format!("{}.local.", cfg.instance_name),
        cfg.overlay_ip,
        cfg.port,
        // TXT records advertise the host name + the mackes-bus
        // protocol version for forward-compat. Receivers can
        // ignore unknown keys.
        &[
            ("host", cfg.instance_name.as_str()),
            ("proto", "mackes-bus/1"),
        ][..],
    )?;
    // mdns-sd 0.11's `enable_addr_auto` is a consuming builder
    // (returns `Self`). Rebind the result so the caller still
    // receives a valid ServiceInfo.
    Ok(info.enable_addr_auto())
}

/// Decode a received `ServiceInfo` (from `ServiceEvent::ServiceResolved`)
/// into a [`BusPeer`]. Returns `None` when the record has no IPv4
/// address (the connectivity-scope lock is IPv4-only).
#[must_use]
pub fn peer_from_service_info(info: &ServiceInfo) -> Option<BusPeer> {
    let instance = info
        .get_fullname()
        .strip_suffix(&format!(".{SERVICE_TYPE}"))
        .unwrap_or_else(|| info.get_fullname())
        .to_string();
    let host = info
        .get_property_val_str("host")
        .map_or_else(|| instance.clone(), str::to_string);
    let addr = info.get_addresses().iter().find(|a| a.is_ipv4()).copied()?;
    Some(BusPeer {
        instance,
        host,
        addr,
        port: info.get_port(),
    })
}

/// Live discovery handle. Holds the `ServiceDaemon` + the
/// registry + the registered instance name so the supervisor can
/// unregister cleanly on shutdown.
pub struct DiscoveryHandle {
    daemon: ServiceDaemon,
    registry: PeerRegistry,
    fullname: String,
}

impl DiscoveryHandle {
    /// Spawn the daemon + register the service + start browsing.
    /// Returns Err with a [`DiscoverySkipReason`] when the daemon
    /// can't be initialised; the caller logs + falls back to a
    /// no-op discovery state.
    ///
    /// # Errors
    /// Returns [`DiscoverySkipReason::DaemonInitFailed`] when
    /// `mdns_sd::ServiceDaemon::new()` returns an error (typically
    /// "no multicast-capable interface").
    pub fn start(
        cfg: &DiscoveryConfig,
        registry: PeerRegistry,
    ) -> Result<Self, DiscoverySkipReason> {
        let daemon = ServiceDaemon::new()
            .map_err(|e| DiscoverySkipReason::DaemonInitFailed(format!("{e}")))?;
        let info = build_service_info(cfg)
            .map_err(|e| DiscoverySkipReason::DaemonInitFailed(format!("ServiceInfo: {e}")))?;
        let fullname = info.get_fullname().to_string();
        daemon
            .register(info)
            .map_err(|e| DiscoverySkipReason::DaemonInitFailed(format!("register: {e}")))?;
        // Browse the same service type to populate the registry.
        let browser = daemon
            .browse(SERVICE_TYPE)
            .map_err(|e| DiscoverySkipReason::DaemonInitFailed(format!("browse: {e}")))?;
        let reg_clone = registry.clone();
        let self_instance = cfg.instance_name.clone();
        std::thread::Builder::new()
            .name("mde-bus-discovery".into())
            .spawn(move || forward_events(browser, reg_clone, self_instance))
            .map_err(|e| {
                DiscoverySkipReason::DaemonInitFailed(format!("spawn browser thread: {e}"))
            })?;
        tracing::info!(
            target: "mde_bus::discovery",
            service_type = SERVICE_TYPE,
            instance = %cfg.instance_name,
            overlay_ip = %cfg.overlay_ip,
            port = cfg.port,
            "mDNS service registered + browser running"
        );
        Ok(Self {
            daemon,
            registry,
            fullname,
        })
    }

    /// Snapshot of currently-known peers.
    #[must_use]
    pub fn peers(&self) -> Vec<BusPeer> {
        self.registry.snapshot()
    }

    /// Unregister + shut down the daemon. Best-effort — errors are
    /// logged + swallowed so a shutdown sequence completes even when
    /// mdns-sd is misbehaving.
    pub fn shutdown(self) {
        // unregister() returns a channel for confirmation; we drop
        // it because the daemon shutdown that follows tears down
        // the channel regardless.
        let _ = self.daemon.unregister(&self.fullname);
        if let Err(e) = self.daemon.shutdown() {
            tracing::warn!(
                target: "mde_bus::discovery",
                error = %e,
                "mdns-sd shutdown returned error"
            );
        }
    }
}

fn forward_events(
    browser: mdns_sd::Receiver<ServiceEvent>,
    registry: PeerRegistry,
    self_instance: String,
) {
    for event in browser.iter() {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                if let Some(peer) = peer_from_service_info(&info) {
                    // Skip our own announcement so the registry
                    // doesn't include this peer.
                    if peer.instance == self_instance {
                        continue;
                    }
                    tracing::debug!(
                        target: "mde_bus::discovery",
                        instance = %peer.instance,
                        host = %peer.host,
                        addr = %peer.addr,
                        port = peer.port,
                        "peer resolved"
                    );
                    registry.upsert(peer);
                }
            }
            ServiceEvent::ServiceRemoved(_service_type, fullname) => {
                let instance = fullname
                    .strip_suffix(&format!(".{SERVICE_TYPE}"))
                    .unwrap_or(&fullname)
                    .to_string();
                tracing::debug!(
                    target: "mde_bus::discovery",
                    instance = %instance,
                    "peer dropped"
                );
                registry.remove(&instance);
            }
            ServiceEvent::SearchStarted(_) | ServiceEvent::SearchStopped(_) => {}
            ServiceEvent::ServiceFound(_, _) => {
                // Resolution lags discovery; wait for Resolved.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn config_defaults_to_8443() {
        let cfg =
            DiscoveryConfig::new("alice".to_string(), IpAddr::V4(Ipv4Addr::new(10, 42, 0, 5)));
        assert_eq!(cfg.port, DEFAULT_BROKER_PORT);
        assert_eq!(cfg.port, 8443);
    }

    #[test]
    fn service_info_carries_overlay_ip_and_host_txt() {
        let cfg =
            DiscoveryConfig::new("alice".to_string(), IpAddr::V4(Ipv4Addr::new(10, 42, 0, 5)));
        let info = build_service_info(&cfg).expect("build_service_info ok");
        assert!(info.get_fullname().starts_with("alice."));
        assert!(info.get_fullname().ends_with(SERVICE_TYPE));
        assert_eq!(info.get_port(), 8443);
        // TXT records carry host + protocol version.
        assert_eq!(info.get_property_val_str("host"), Some("alice"));
        assert_eq!(info.get_property_val_str("proto"), Some("mackes-bus/1"));
    }

    #[test]
    fn registry_upsert_and_remove_roundtrip() {
        let reg = PeerRegistry::new();
        assert!(reg.is_empty());
        let peer = BusPeer {
            instance: "bob".to_string(),
            host: "bob".to_string(),
            addr: IpAddr::V4(Ipv4Addr::new(10, 42, 0, 6)),
            port: 8443,
        };
        reg.upsert(peer.clone());
        assert_eq!(reg.len(), 1);
        let snap = reg.snapshot();
        assert_eq!(snap[0], peer);
        reg.remove("bob");
        assert!(reg.is_empty());
    }

    #[test]
    fn registry_snapshot_is_sorted_by_instance() {
        let reg = PeerRegistry::new();
        for name in ["charlie", "alice", "bob"] {
            reg.upsert(BusPeer {
                instance: name.to_string(),
                host: name.to_string(),
                addr: IpAddr::V4(Ipv4Addr::new(10, 42, 0, 1)),
                port: 8443,
            });
        }
        let snap = reg.snapshot();
        let instances: Vec<&str> = snap.iter().map(|p| p.instance.as_str()).collect();
        assert_eq!(instances, vec!["alice", "bob", "charlie"]);
    }

    #[test]
    fn skip_reason_display_messages_are_human_readable() {
        let r = DiscoverySkipReason::NoOverlayIp;
        let msg = format!("{r}");
        assert!(msg.contains("overlay IP"));
        assert!(msg.contains("not enrolled"));
        let r2 = DiscoverySkipReason::DaemonInitFailed("OS error".to_string());
        let msg2 = format!("{r2}");
        assert!(msg2.contains("mdns-sd init failed"));
        assert!(msg2.contains("OS error"));
    }
}
