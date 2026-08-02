//! DEVMGR — the device-inventory schema (`docs/design/about-device-manager.md`,
//! locked 2026-07-04).
//!
//! **This JSON is the §6 contract** between the mesh-side
//! producer (`mackesd`'s `hardware_probe` worker, DEVMGR-1) and the desktop-side
//! consumer (the About → Device-Manager surface, DEVMGR-2..6). Neither crate may
//! depend on the other, so the shape lives here in the mesh-neutral shared crate
//! (alongside [`crate::peer_probe`], the pre-existing hardware schema) — both
//! sides `use mackes_mesh_types::device_inventory::*`.
//!
//! Each node self-enumerates its full Linux hardware taxonomy (#4) into a
//! [`DeviceInventory`] tree and publishes it to
//! `<workgroup_root>/device-inventory/<hostname>.json` (the SEC-5 own-row idiom
//! the `node_grade` worker uses), so every peer reads every host's hardware. The
//! shell maps [`DeviceStatus`] to Windows-style MDM problem codes (#11) — the
//! producer exposes the **honest normalized status + the real Linux reason**
//! ([`DeviceRecord::problem`]), never a fabricated Windows code.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Stable category keys (the `key` field of each [`DeviceCategory`]).
///
/// The shell groups + orders on these, so they are part of the wire contract.
/// The human [`category_label`] is derived from the key for a fresh producer, but
/// the shell reads whatever label the producer wrote.
pub mod category {
    /// Processors (CPU packages / logical cores).
    pub const PROCESSORS: &str = "processors";
    /// System memory (RAM total + DIMMs when DMI is readable).
    pub const MEMORY: &str = "memory";
    /// Physical disk drives (block devices).
    pub const DISK_DRIVES: &str = "disk-drives";
    /// Storage controllers (SATA/NVMe/RAID host controllers).
    pub const STORAGE_CONTROLLERS: &str = "storage-controllers";
    /// Network adapters (wired + wireless).
    pub const NETWORK_ADAPTERS: &str = "network-adapters";
    /// Display / GPU adapters.
    pub const DISPLAY: &str = "display";
    /// USB controllers + hubs.
    pub const USB_CONTROLLERS: &str = "usb-controllers";
    /// PCI / system devices that don't fall under a richer category.
    pub const PCI_DEVICES: &str = "pci-devices";
    /// Audio devices.
    pub const AUDIO: &str = "audio";
    /// Human-interface / input devices.
    pub const INPUT: &str = "input";
    /// Sensors + thermal zones.
    pub const SENSORS: &str = "sensors";
    /// Bluetooth radios.
    pub const BLUETOOTH: &str = "bluetooth";
    /// Battery / power supplies.
    pub const POWER: &str = "power";

    /// The full locked taxonomy (#4) in render order.
    ///
    /// A producer emits the categories it actually found (non-PC hosts skip empty
    /// ones, #22); this is the canonical order + membership the shell can lay a
    /// rail out against.
    pub const ORDER: &[&str] = &[
        PROCESSORS,
        MEMORY,
        DISK_DRIVES,
        STORAGE_CONTROLLERS,
        NETWORK_ADAPTERS,
        DISPLAY,
        USB_CONTROLLERS,
        PCI_DEVICES,
        AUDIO,
        INPUT,
        SENSORS,
        BLUETOOTH,
        POWER,
    ];
}

/// The human label for a stable category [`key`](category), or the key itself
/// (title-cased words) when it is an unknown future key — an honest fallback that
/// never panics on an unrecognized producer.
#[must_use]
pub fn category_label(key: &str) -> String {
    match key {
        category::PROCESSORS => "Processors".to_string(),
        category::MEMORY => "Memory".to_string(),
        category::DISK_DRIVES => "Disk drives".to_string(),
        category::STORAGE_CONTROLLERS => "Storage controllers".to_string(),
        category::NETWORK_ADAPTERS => "Network adapters".to_string(),
        category::DISPLAY => "Display adapters".to_string(),
        category::USB_CONTROLLERS => "USB controllers".to_string(),
        category::PCI_DEVICES => "System devices".to_string(),
        category::AUDIO => "Audio".to_string(),
        category::INPUT => "Input devices".to_string(),
        category::SENSORS => "Sensors".to_string(),
        category::BLUETOOTH => "Bluetooth".to_string(),
        category::POWER => "Batteries".to_string(),
        other => other.replace(['-', '_'], " "),
    }
}

/// The normalized device state, derived from real Linux facts.
///
/// Read from driver binding, `enable`, `operstate`, and dmesg. The shell maps
/// each variant to a Windows MDM
/// problem code (#11) — e.g. [`NoDriver`](DeviceStatus::NoDriver) → *Code 28*,
/// [`Disabled`](DeviceStatus::Disabled) → *Code 22*, [`Degraded`](DeviceStatus::Degraded)
/// → *Code 10* — but the mapping is the consumer's; here the state stays honest
/// and the real reason rides [`DeviceRecord::problem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceStatus {
    /// Working normally (a driver is bound, or the category never binds one —
    /// CPU/memory/thermal).
    #[default]
    Ok,
    /// A bus device with no kernel driver bound (→ MDM Code 28).
    NoDriver,
    /// Administratively disabled — PCI `enable == 0`, an interface set `down`,
    /// a de-authorized USB device (→ MDM Code 22).
    Disabled,
    /// Bound + present but reporting errors (a dmesg error line, an I/O fault)
    /// (→ MDM Code 10).
    Degraded,
    /// State could not be determined (an honest `unknown`, never a fake `ok`).
    Unknown,
}

impl DeviceStatus {
    /// Whether this state is a problem the shell should badge (anything but
    /// [`Ok`](DeviceStatus::Ok)).
    #[must_use]
    pub const fn is_problem(self) -> bool {
        !matches!(self, Self::Ok)
    }

    /// A short stable label (the wire token) for logs + tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NoDriver => "no-driver",
            Self::Disabled => "disabled",
            Self::Degraded => "degraded",
            Self::Unknown => "unknown",
        }
    }
}

/// The IRQ / I/O-port / memory-window resources a device holds (MDM's Resources
/// tab, #10). Every field is best-effort — an absent read is an empty list / a
/// `None`, never a fabricated range.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceResources {
    /// Assigned IRQ line, when the device exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irq: Option<u32>,
    /// I/O-port windows (`0x1000-0x107f`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub io_ports: Vec<String>,
    /// Memory-mapped windows (`0xf0000000-0xf00fffff`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory: Vec<String>,
    /// DMA channels, when known.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dma: Vec<String>,
}

impl DeviceResources {
    /// Whether nothing at all was resolved (used to hide an empty Resources tab).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.irq.is_none()
            && self.io_ports.is_empty()
            && self.memory.is_empty()
            && self.dma.is_empty()
    }
}

/// One device in the tree — the record every property tab (#10) renders from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// The display name (`vendor model`, an id-db lookup, or the sysfs node).
    pub name: String,
    /// Vendor string (id-db resolved), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Model / product string, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `vendor:product` hex ids (`8086:5916`), when the bus exposes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ids: Option<String>,
    /// The sysfs path this record was read from (the Details tab, #10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
    /// The bound kernel driver / module, when one is bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The bound module's version (sysfs `driver/module/version`), when exported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    /// The normalized state (the shell maps to an MDM problem code).
    pub status: DeviceStatus,
    /// The honest Linux reason behind a non-[`Ok`](DeviceStatus::Ok) status
    /// (`no kernel driver bound`, `link down`, a dmesg error) — kept beside the
    /// synthetic problem code so the emulation stays honest (design "Risks").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// IRQ / I/O / memory resources (the Resources tab, #10).
    #[serde(default, skip_serializing_if = "DeviceResources::is_empty")]
    pub resources: DeviceResources,
    /// Recent dmesg / udev lines mentioning this device (the Events tab, #10).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

impl DeviceRecord {
    /// A minimal record carrying just a name + a status (the honest floor a
    /// category populates and then enriches).
    #[must_use]
    pub fn new(name: impl Into<String>, status: DeviceStatus) -> Self {
        Self {
            name: name.into(),
            vendor: None,
            model: None,
            ids: None,
            sysfs_path: None,
            driver: None,
            driver_version: None,
            status,
            problem: None,
            resources: DeviceResources::default(),
            events: Vec::new(),
        }
    }
}

/// A category node (Processors, Network adapters, …) grouping its devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCategory {
    /// The stable key (a [`category`] constant).
    pub key: String,
    /// The human label the tree renders.
    pub label: String,
    /// The devices in this category (may be empty on a partial host, #22 —
    /// producers should simply not emit an empty category).
    pub devices: Vec<DeviceRecord>,
}

impl DeviceCategory {
    /// A category with its canonical label derived from the key.
    #[must_use]
    pub fn new(key: &str, devices: Vec<DeviceRecord>) -> Self {
        Self {
            key: key.to_string(),
            label: category_label(key),
            devices,
        }
    }

    /// Devices in this category that carry a problem status (badge count).
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.status.is_problem())
            .count()
    }
}

/// The per-host summary the rich header card renders (#20). Every field is
/// optional so a shallow / non-PC host (#22) carries an honest partial summary
/// rather than fabricated totals.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostSummary {
    /// `PRETTY_NAME` from `/etc/os-release`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Kernel release (`uname -r`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// Uptime in seconds (`/proc/uptime`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// CPU model name (`/proc/cpuinfo`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Logical CPU count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    /// Total RAM in kB (`/proc/meminfo` `MemTotal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_kb: Option<u64>,
}

/// Which optional deep-detail tools were available at enumeration time (#15).
///
/// The tree is built sysfs-first; these flags let the shell surface an honest
/// "install lshw for deep DMI details" hint rather than the tree looking broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
// Four presence flags is exactly the point of this record — not a state-machine
// smell the `struct_excessive_bools` lint is meant to catch.
#[allow(clippy::struct_excessive_bools)]
pub struct ToolAvailability {
    /// `lshw` was present (deep DMI/firmware JSON).
    pub lshw: bool,
    /// `dmidecode` was present (SMBIOS/DMI).
    pub dmidecode: bool,
    /// A `pci.ids` database was found (PCI vendor/device names).
    pub pci_ids: bool,
    /// A `usb.ids` database was found (USB vendor/product names).
    pub usb_ids: bool,
}

/// The complete tree published for one host — the top-level document at
/// `<workgroup_root>/device-inventory/<hostname>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInventory {
    /// The publishing node's short hostname (the file stem + rail key).
    pub host: String,
    /// Wall-clock ms when this snapshot was published (freshness, #8).
    pub published_at_ms: u64,
    /// The header-card summary (#20).
    pub summary: HostSummary,
    /// Which deep-detail tools were available (#15).
    pub tools: ToolAvailability,
    /// The categorized device tree (#4), in [`category::ORDER`].
    pub categories: Vec<DeviceCategory>,
}

impl DeviceInventory {
    /// Total device count across every category (the header badge, #20).
    #[must_use]
    pub fn device_count(&self) -> usize {
        self.categories.iter().map(|c| c.devices.len()).sum()
    }

    /// Total problem-status device count across every category (#20 badge).
    #[must_use]
    pub fn problem_count(&self) -> usize {
        self.categories
            .iter()
            .map(DeviceCategory::problem_count)
            .sum()
    }

    /// A deterministic fixture (tests + the shell's `--dry-run` preview): a
    /// laptop-shaped host with one healthy GPU + one driverless PCI device.
    #[must_use]
    pub fn fixture() -> Self {
        let gpu = DeviceRecord {
            name: "Intel UHD Graphics 620".into(),
            vendor: Some("Intel Corporation".into()),
            model: Some("UHD Graphics 620".into()),
            ids: Some("8086:5917".into()),
            sysfs_path: Some("/sys/bus/pci/devices/0000:00:02.0".into()),
            driver: Some("i915".into()),
            driver_version: None,
            status: DeviceStatus::Ok,
            problem: None,
            resources: DeviceResources {
                irq: Some(131),
                io_ports: vec![],
                memory: vec!["0xdd000000-0xddffffff".into()],
                dma: vec![],
            },
            events: vec![],
        };
        let orphan = DeviceRecord {
            name: "SD Host Controller".into(),
            vendor: Some("Realtek Semiconductor Co., Ltd.".into()),
            model: None,
            ids: Some("10ec:5227".into()),
            sysfs_path: Some("/sys/bus/pci/devices/0000:02:00.0".into()),
            driver: None,
            driver_version: None,
            status: DeviceStatus::NoDriver,
            problem: Some("no kernel driver bound".into()),
            resources: DeviceResources::default(),
            events: vec![],
        };
        Self {
            host: "laptop-mm".into(),
            published_at_ms: 1_720_000_000_000,
            summary: HostSummary {
                os: Some("Fedora Linux 44 (Workstation Edition)".into()),
                kernel: Some("7.0.8-200.fc44.x86_64".into()),
                uptime_secs: Some(48_120),
                cpu_model: Some("Intel(R) Core(TM) i7-8650U CPU @ 1.90GHz".into()),
                cpu_count: Some(8),
                mem_total_kb: Some(16_072_192),
            },
            tools: ToolAvailability {
                lshw: true,
                dmidecode: true,
                pci_ids: true,
                usb_ids: true,
            },
            categories: vec![
                DeviceCategory::new(category::DISPLAY, vec![gpu]),
                DeviceCategory::new(category::PCI_DEVICES, vec![orphan]),
            ],
        }
    }
}

/// The replicated directory holding every node's published inventory —
/// `<workgroup_root>/device-inventory/`.
#[must_use]
pub fn inventory_dir(workgroup_root: &Path) -> PathBuf {
    workgroup_root.join("device-inventory")
}

/// The published-file path for one host — `<dir>/<hostname>.json`.
#[must_use]
pub fn inventory_path(workgroup_root: &Path, hostname: &str) -> PathBuf {
    inventory_dir(workgroup_root).join(format!("{hostname}.json"))
}

/// Keep replicated inventory input bounded before `serde_json` materializes it
/// into an owned document.
const MAX_INVENTORY_BYTES: usize = 256 * 1024;

/// Read one replicated inventory row through the descriptor that will be
/// consumed. Reject final symlinks, blocking special files, oversized input,
/// growth while reading, and invalid UTF-8 before the JSON parser sees it.
fn read_bounded_inventory_json(path: &Path) -> Option<String> {
    use std::io::Read as _;

    #[cfg(unix)]
    let file: std::fs::File = {
        use std::os::unix::fs::OpenOptionsExt;

        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0o400000 | 0o4000 | 0o2000000); // O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        options.custom_flags(0x100 | 0x4); // O_NOFOLLOW | O_NONBLOCK

        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        if !std::fs::symlink_metadata(path).ok()?.file_type().is_file() {
            return None;
        }

        options.open(path).ok()?
    };
    #[cfg(not(unix))]
    let file = {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.file_type().is_file() {
            return None;
        }
        std::fs::File::open(path).ok()?
    };

    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_INVENTORY_BYTES as u64 {
        return None;
    }
    let initial_len = metadata.len();

    let capacity = usize::try_from(initial_len)
        .unwrap_or(MAX_INVENTORY_BYTES)
        .min(MAX_INVENTORY_BYTES)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    (&file)
        .take((MAX_INVENTORY_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_INVENTORY_BYTES || bytes.len() as u64 != initial_len {
        return None;
    }

    // A regular file can be extended after the first metadata snapshot. Keep
    // the row fail-soft if the descriptor observes any size change while it is
    // being consumed.
    if file.metadata().ok()?.len() != initial_len {
        return None;
    }

    String::from_utf8(bytes).ok()
}

/// Read one host's published inventory, or `None` when it is absent / unreadable
/// / half-replicated (an honest miss, never a panic).
#[must_use]
pub fn read_inventory(workgroup_root: &Path, hostname: &str) -> Option<DeviceInventory> {
    let data = read_bounded_inventory_json(&inventory_path(workgroup_root, hostname))?;
    serde_json::from_str(&data).ok()
}

/// Read every node's published inventory (own file included).
///
/// Junk / half-replicated files are skipped, and the result is sorted by hostname
/// for a stable render order (the By-node view + host rail, #3/#5). Mirrors
/// `node_grade::read_grades`.
#[must_use]
pub fn read_all(workgroup_root: &Path) -> Vec<DeviceInventory> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(inventory_dir(workgroup_root)) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        if let Some(data) = read_bounded_inventory_json(&path) {
            if let Ok(inv) = serde_json::from_str::<DeviceInventory>(&data) {
                out.push(inv);
            }
        }
    }
    out.sort_by(|a, b| a.host.cmp(&b.host));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&DeviceStatus::NoDriver).unwrap(),
            "\"no-driver\""
        );
        assert_eq!(serde_json::to_string(&DeviceStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(DeviceStatus::default(), DeviceStatus::Ok);
        assert!(DeviceStatus::NoDriver.is_problem());
        assert!(!DeviceStatus::Ok.is_problem());
    }

    #[test]
    fn category_label_falls_back_for_unknown_keys() {
        assert_eq!(
            category_label(category::NETWORK_ADAPTERS),
            "Network adapters"
        );
        // An unknown future key degrades to a readable label, never a panic.
        assert_eq!(category_label("future-thing"), "future thing");
        // The order list carries the full locked taxonomy.
        assert_eq!(category::ORDER.len(), 13);
    }

    #[test]
    fn fixture_round_trips_and_counts() {
        let inv = DeviceInventory::fixture();
        assert_eq!(inv.device_count(), 2);
        assert_eq!(inv.problem_count(), 1, "the driverless PCI device");
        let json = serde_json::to_string_pretty(&inv).unwrap();
        let back: DeviceInventory = serde_json::from_str(&json).unwrap();
        assert_eq!(inv, back);
        // No foo/bar/test placeholders in human fields (the "calm enterprise" tone).
        assert!(!inv.host.contains("test"));
        assert!(inv.summary.os.as_deref().unwrap().contains("Fedora"));
    }

    #[test]
    fn absent_optionals_are_omitted_not_null() {
        // A bare record serializes tightly — no `"vendor":null` noise, and an
        // Ok device carries no phantom `problem`.
        let rec = DeviceRecord::new("A thermal zone", DeviceStatus::Ok);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("null"), "no null fields: {json}");
        assert!(!json.contains("problem"), "no phantom problem: {json}");
        assert!(json.contains("\"status\":\"ok\""));
        // But it still round-trips (serde defaults fill the omitted fields).
        let back: DeviceRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(rec, back);
    }

    #[test]
    fn publish_path_and_read_round_trip_through_the_substrate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let inv = DeviceInventory::fixture();
        std::fs::create_dir_all(inventory_dir(root)).unwrap();
        let path = inventory_path(root, &inv.host);
        assert_eq!(path, inventory_dir(root).join("laptop-mm.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&inv).unwrap()).unwrap();
        // A single-host read + the fleet read both find it.
        assert_eq!(read_inventory(root, "laptop-mm").unwrap(), inv);
        let all = read_all(root);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], inv);
        // A half-replicated dotfile + a non-json file are ignored.
        std::fs::write(inventory_dir(root).join(".other.json.tmp"), "{}").unwrap();
        std::fs::write(inventory_dir(root).join("README.txt"), "x").unwrap();
        assert_eq!(read_all(root).len(), 1);
        // A missing host reads as an honest None.
        assert!(read_inventory(root, "ghost").is_none());
    }

    #[test]
    fn valid_rows_are_read_and_sorted_by_hostname() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(inventory_dir(root)).unwrap();

        let mut zulu = DeviceInventory::fixture();
        zulu.host = "zulu".to_string();
        let mut alpha = DeviceInventory::fixture();
        alpha.host = "alpha".to_string();
        std::fs::write(
            inventory_path(root, &zulu.host),
            serde_json::to_vec(&zulu).unwrap(),
        )
        .unwrap();
        std::fs::write(
            inventory_path(root, &alpha.host),
            serde_json::to_vec(&alpha).unwrap(),
        )
        .unwrap();

        assert_eq!(read_inventory(root, "alpha"), Some(alpha));
        let all = read_all(root);
        assert_eq!(
            all.iter()
                .map(|inventory| inventory.host.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zulu"]
        );
    }

    #[test]
    fn hostile_rows_are_fail_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(inventory_dir(root)).unwrap();

        std::fs::write(inventory_path(root, "invalid-utf8"), [0xff, 0xfe]).unwrap();
        std::fs::write(
            inventory_path(root, "oversized"),
            vec![b' '; MAX_INVENTORY_BYTES + 1],
        )
        .unwrap();
        std::fs::create_dir(inventory_path(root, "directory")).unwrap();

        assert!(read_inventory(root, "invalid-utf8").is_none());
        assert!(read_inventory(root, "oversized").is_none());
        assert!(read_inventory(root, "directory").is_none());
        assert!(read_all(root).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn final_symlinks_and_unix_sockets_are_ignored() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(inventory_dir(root)).unwrap();

        let inventory = DeviceInventory::fixture();
        let target = inventory_path(root, &inventory.host);
        std::fs::write(&target, serde_json::to_vec(&inventory).unwrap()).unwrap();
        symlink(&target, inventory_path(root, "linked")).unwrap();
        let _socket = UnixListener::bind(inventory_path(root, "socket")).unwrap();

        assert!(read_inventory(root, "linked").is_none());
        assert!(read_inventory(root, "socket").is_none());
        assert_eq!(read_all(root), vec![inventory]);
    }
}
