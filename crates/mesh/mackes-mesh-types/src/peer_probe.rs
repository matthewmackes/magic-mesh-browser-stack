//! Peer-probe schema — what `mded`'s peer-join worker (PC-3)
//! writes to `~/.cache/mde/peers/<peer-id>/probe.json` for the
//! Peer Connection Card to consume.
//!
//! Lives here (production home) since PC-2 (2026-05-21). The
//! placeholder previously shipped in `mde_peer_card::probe::PeerProbe`
//! now re-exports from this module so cross-crate consumers
//! (mded, the GUI surfaces, future tooling) share one definition.

use serde::{Deserialize, Serialize};

/// NAT class observed during ICE negotiation. Affects the
/// "connectivity" line in the Bus & topology section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NatClass {
    /// Full-cone NAT — bi-directional any-source.
    OpenInternet,
    /// Address-restricted cone.
    AddressRestricted,
    /// Port-restricted cone.
    PortRestricted,
    /// Symmetric NAT — hardest to traverse.
    Symmetric,
    /// Behind a port-randomizing carrier-grade NAT.
    CarrierGrade,
    /// Direct LAN, no NAT traversal needed.
    Lan,
}

/// Bus / topology section data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusTopology {
    /// Mesh routing path from local node to peer (named hops).
    pub mesh_path: Vec<String>,
    /// Round-trip latency in milliseconds.
    pub rtt_ms: u32,
    /// NAT classification observed at probe time.
    pub nat_class: NatClass,
    /// ICE candidate used for the connection (e.g.
    /// `udp,host,192.168.1.5:51820` or `udp,relay,derp.mde.io`).
    pub ice_candidate: String,
    /// PCI tree summary as seen on the peer (`lspci -tv` flat-
    /// rendered).
    pub pci_tree: Vec<String>,
    /// USB tree summary (`lsusb -t` flat-rendered).
    pub usb_tree: Vec<String>,
}

/// Kernel + driver section data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelDriver {
    /// `uname -a` output.
    pub uname: String,
    /// Bound transport kernel module (e.g. `wireguard`).
    pub transport_module: String,
    /// `mded` build version on the peer.
    pub mded_version: String,
    /// Tail of dmesg lines relating to the new link (max 6).
    pub dmesg_tail: Vec<String>,
}

/// Power + thermal section data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PowerThermal {
    /// Battery percentage 0..=100, or `None` if no battery
    /// (desktop / server peer).
    pub battery_pct: Option<u8>,
    /// AC adapter connected.
    pub on_ac: bool,
    /// CPU package temperature in degrees Celsius (from
    /// `lm_sensors`).
    pub cpu_pkg_c: Option<f32>,
    /// Fan RPM (from `lm_sensors`).
    pub fan_rpm: Option<u32>,
}

/// Descriptors + capabilities section data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptors {
    /// Advertised mesh services (e.g. `["ssh", "playbook-runner"]`).
    pub mesh_services: Vec<String>,
    /// `/sys/class/*` device classes present on the peer.
    pub sysfs_classes: Vec<String>,
    /// USB descriptor tree of attached named peripherals.
    pub usb_descriptors: Vec<String>,
}

/// Complete probe written by `mded`'s peer-join worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerProbe {
    /// Stable peer identifier (the mesh node UUID, not the
    /// connection-attempt ID).
    pub peer_id: String,
    /// Display hostname (`hostname -s` on the peer).
    pub hostname: String,
    /// Vendor ID (e.g. PCI `8086`). Cache-key contributor for
    /// the enrichment layer.
    pub vendor_id: String,
    /// Product ID (e.g. PCI `5916`). Cache-key contributor.
    pub product_id: String,
    /// Distribution short-name (e.g. `Fedora 44`).
    pub distro: String,
    /// Bus / topology section.
    pub bus: BusTopology,
    /// Kernel + driver section.
    pub kernel: KernelDriver,
    /// Power + thermal section.
    pub power: PowerThermal,
    /// Descriptors + capabilities section.
    pub descriptors: Descriptors,
}

impl PeerProbe {
    /// Deterministic fixture used in tests + the `--dry-run`
    /// preview mode of the binary.
    #[must_use]
    pub fn fixture() -> Self {
        Self {
            peer_id: "fixture-peer-1".into(),
            hostname: "laptop-mm".into(),
            vendor_id: "8086".into(),
            product_id: "5916".into(),
            distro: "Fedora 44".into(),
            bus: BusTopology {
                mesh_path: vec!["edge-1".into(), "laptop-mm".into()],
                rtt_ms: 14,
                nat_class: NatClass::Lan,
                ice_candidate: "udp,host,192.168.1.42:51820".into(),
                pci_tree: vec![
                    "00:02.0 VGA: Intel UHD 620".into(),
                    "00:14.0 USB controller".into(),
                ],
                usb_tree: vec!["Bus 001.Port 1: Logitech MX Master 3S".into()],
            },
            kernel: KernelDriver {
                uname: "Linux laptop-mm 7.0.8-200.fc44.x86_64".into(),
                transport_module: "wireguard".into(),
                mded_version: "2.0.1".into(),
                dmesg_tail: vec!["wireguard: peer fixture-peer-1 added".into()],
            },
            power: PowerThermal {
                battery_pct: Some(82),
                on_ac: true,
                cpu_pkg_c: Some(46.0),
                fan_rpm: Some(2400),
            },
            descriptors: Descriptors {
                mesh_services: vec!["ssh".into(), "playbook-runner".into()],
                sysfs_classes: vec!["net".into(), "bluetooth".into()],
                usb_descriptors: vec!["Logitech MX Master 3S [046d:b034]".into()],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_class_serializes_kebab_case() {
        let s = serde_json::to_string(&NatClass::PortRestricted).unwrap();
        assert_eq!(s, "\"port-restricted\"");
        let s = serde_json::to_string(&NatClass::CarrierGrade).unwrap();
        assert_eq!(s, "\"carrier-grade\"");
    }

    #[test]
    fn fixture_round_trips() {
        let p = PeerProbe::fixture();
        let s = serde_json::to_string(&p).unwrap();
        let back: PeerProbe = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn fixture_has_realistic_defaults() {
        let p = PeerProbe::fixture();
        // Sanity checks against the "calm enterprise" tone of
        // the visual identity — no foo/bar/test strings in
        // human-visible fields.
        assert!(!p.hostname.contains("foo"));
        assert!(!p.hostname.contains("test"));
        assert!(!p.distro.is_empty());
        assert!(p.bus.rtt_ms < 1000);
    }
}
