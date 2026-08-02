//! DEVMGR-8 — the **device-control** request/result schema (design
//! `docs/design/about-device-manager.md`, locks #12/#13/#14).
//!
//! **This JSON is the §6 contract** between the desktop-side requester (the
//! About → Device-Manager surface, `mde-shell-egui`) and the mesh-side executor
//! (`mackesd`'s `device_control` worker). Neither crate may depend on the other,
//! so — exactly like [`crate::device_inventory`] — the shape + the on-disk seam
//! live here in the mesh-neutral shared crate, and both sides
//! `use mackes_mesh_types::device_control::*`.
//!
//! ## The remote-exec seam (the proven PD-11 pattern)
//!
//! A privileged device mutation runs **on the target node itself**, never pushed
//! over SSH (`AI_GOVERNANCE.md` §9 — typed verbs, the target runs locally). The
//! shell writes a typed [`DeviceControlRequest`] into the **replicated**
//! `<workgroup_root>/fleet/device-control/<target-host>/<id>.json`; Syncthing
//! replication carries it to the target; that node's `device_control` worker
//! drains its own dir, resolves the real sysfs/`ip`/`modprobe` seam, executes it,
//! audits it on the KDC hash-chain, and writes a [`DeviceControlResult`] back —
//! the identical shape as `crate`-side `lifecycle` (PD-11), reused for hardware.
//!
//! The request carries only the **typed** fields the executor needs to pick the
//! real seam ([`DeviceTarget`]) — an op KIND + the device's category / sysfs
//! path / bound driver. There is deliberately **no command string** (§9): the
//! executor maps the typed op onto a FIXED sysfs write or binary, or refuses it
//! with a typed error (never a fabricated success).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A privileged hardware-mutation op (design #12).
///
/// The Linux-real equivalents of Windows Device-Manager's Enable / Disable /
/// Update-driver / Scan verbs. Every one is destructive-enough to sit behind
/// typed-arming (#14) on the shell side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceControlOp {
    /// Bring the device back up — bind its driver, `ip link set … up`, or
    /// re-authorize a USB device, by device kind.
    Enable,
    /// Administratively down the device — unbind its driver, `ip link set …
    /// down`, or de-authorize a USB device, by device kind.
    Disable,
    /// Reload the device's kernel module (`rmmod` + `modprobe`).
    ReloadModule,
    /// Re-enumerate the device's parent bus (the sysfs `.../rescan` write).
    RescanBus,
}

impl DeviceControlOp {
    /// Every op, in Device-Manager menu order (the shell's context-menu table).
    pub const ALL: [Self; 4] = [
        Self::Enable,
        Self::Disable,
        Self::ReloadModule,
        Self::RescanBus,
    ];

    /// The stable wire token (matches the serde `kebab-case` tag) for logs/audit.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::ReloadModule => "reload-module",
            Self::RescanBus => "rescan-bus",
        }
    }

    /// The human verb the shell menu + the arming confirm render.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Enable => "Enable device",
            Self::Disable => "Disable device",
            Self::ReloadModule => "Reload driver module",
            Self::RescanBus => "Rescan bus",
        }
    }

    /// Whether this op turns the device OFF (the reach-loss warning + the destructive
    /// tint apply): only [`Disable`](Self::Disable). Enable/Reload/Rescan restore or
    /// re-enumerate, never strand a device.
    #[must_use]
    pub const fn is_disabling(self) -> bool {
        matches!(self, Self::Disable)
    }
}

/// The device a control op targets (the executor resolves the real seam from it).
///
/// The subset of the [`crate::device_inventory::DeviceRecord`] fields the executor
/// needs, carried by value so the request is self-contained (§9 — typed params,
/// never a command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTarget {
    /// The device display name (audit/notify text + the arming echo target).
    pub name: String,
    /// The owning category key (a [`crate::device_inventory::category`] constant) —
    /// disambiguates a network adapter from a plain PCI function.
    pub category: String,
    /// The sysfs path the record was read from — the executor derives the bus,
    /// the BDF, and the driver bind/unbind/rescan nodes from it. Absent for a
    /// device with no sysfs anchor (CPU/memory), which the executor refuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
    /// The bound kernel driver / module name — the reload target + the driver
    /// bind/unbind directory. Absent when nothing is bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
}

impl DeviceTarget {
    /// A minimal target carrying just a name + category (tests + a device that
    /// carries no sysfs/driver anchor — the executor then refuses the op honestly).
    #[must_use]
    pub fn new(name: impl Into<String>, category: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            category: category.into(),
            sysfs_path: None,
            driver: None,
        }
    }
}

/// A typed device-control request (§9 — typed params, never a command string).
///
/// An op KIND + the typed [`DeviceTarget`], the target host, the requesting seat,
/// and a correlation id. The executor maps the op onto a FIXED seam or refuses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceControlRequest {
    /// Correlation id (the requester matches the eventual result on it).
    pub id: String,
    /// The privileged op to run.
    pub op: DeviceControlOp,
    /// The device to act on.
    pub target: DeviceTarget,
    /// The node whose hardware this acts on (the replicated dir it lands in).
    pub target_host: String,
    /// The requesting seat/node id (the audit actor + the notify source).
    pub from: String,
}

/// The typed result the executor writes back for the requester to poll.
///
/// `ok` mirrors the `lifecycle`/`dc/*` reply convention. `detail` carries a
/// human-readable success note; `error` is the honest failure reason (an
/// inapplicable op, a refused device, or the real sysfs/`modprobe` stderr) — a
/// non-empty `error` with `ok == false` is NEVER a fabricated success (§7).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceControlResult {
    /// The request id this answers.
    pub id: String,
    /// True once the op executed for real and every step succeeded.
    pub ok: bool,
    /// Human-readable success note (`disabled i915 on 0000:02:00.0`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The honest failure reason on a degrade path (empty on success).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl DeviceControlResult {
    /// A success result carrying a human-readable note.
    #[must_use]
    pub fn ok(id: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: true,
            detail: detail.into(),
            error: String::new(),
        }
    }

    /// A typed failure result carrying the honest reason (never a fake success).
    #[must_use]
    pub fn failed(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            detail: String::new(),
            error: error.into(),
        }
    }
}

/// The per-target request directory — `<workgroup_root>/fleet/device-control/<host>/`
/// (the same replicated `fleet/…/<target>` idiom the PD-11 `lifecycle` verb uses).
#[must_use]
pub fn control_dir(workgroup_root: &Path, target_host: &str) -> PathBuf {
    workgroup_root
        .join("fleet")
        .join("device-control")
        .join(target_host)
}

/// Write a request into `target_host`'s replicated dir (atomic temp + rename, so a
/// draining executor never reads a half-written request). Returns the written path.
///
/// # Errors
/// IO / serialization failures (surfaced to the shell as an honest error toast).
pub fn write_request(
    workgroup_root: &Path,
    req: &DeviceControlRequest,
) -> std::io::Result<PathBuf> {
    let dir = control_dir(workgroup_root, &req.target_host);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", req.id));
    let tmp = dir.join(format!(".{}.json.tmp", req.id));
    let body = serde_json::to_string_pretty(req)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Keep replicated request/result records bounded before JSON materialization.
/// Device-control records are intentionally small typed envelopes, so a finite
/// cap leaves room for forward-compatible fields without allowing a peer to
/// force an unbounded allocation in either reader.
const MAX_DEVICE_CONTROL_RECORD_BYTES: usize = 64 * 1024;

/// Read one replicated device-control record through the descriptor that will
/// be consumed. The final path component is opened without following a
/// symlink, and non-blocking semantics keep a FIFO or other special file from
/// stalling the worker. Descriptor metadata admits only regular files;
/// reading one byte beyond the cap and comparing the descriptor size before
/// and after consumption rejects oversized or changing input. UTF-8 is
/// validated before either caller invokes serde.
fn read_bounded_device_control_record(path: &Path) -> Option<String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        options.custom_flags(0o400000 | 0o4000 | 0o2000000); // O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        options.custom_flags(0x100 | 0x4); // O_NOFOLLOW | O_NONBLOCK

        // Unsupported Unix targets still fail closed for a final symlink when
        // their standard library does not expose an O_NOFOLLOW value here.
        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        )))]
        if !std::fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            return None;
        }
    }

    #[cfg(not(unix))]
    if !std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        return None;
    }

    let file = options.open(path).ok()?;
    let before = file.metadata().ok()?;
    let limit = u64::try_from(MAX_DEVICE_CONTROL_RECORD_BYTES).ok()?;
    if !before.file_type().is_file() || before.len() > limit {
        return None;
    }

    let capacity = usize::try_from(before.len())
        .ok()?
        .min(MAX_DEVICE_CONTROL_RECORD_BYTES)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    (&file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    let after = file.metadata().ok()?;
    if !after.file_type().is_file()
        || after.len() != before.len()
        || bytes.len() > MAX_DEVICE_CONTROL_RECORD_BYTES
        || bytes.len() as u64 != before.len()
    {
        return None;
    }

    String::from_utf8(bytes).ok()
}

/// Consume (read + delete) every pending request addressed to `self_host`. Result
/// files (`*.result.json`) and half-written dotfiles are skipped.
#[must_use]
#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the request/result filenames are lowercase-`.json` by construction \
              (write_request/write_result) — a case-sensitive suffix match is correct"
)]
pub fn take_requests(workgroup_root: &Path, self_host: &str) -> Vec<DeviceControlRequest> {
    let dir = control_dir(workgroup_root, self_host);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.ends_with(".json") || name.ends_with(".result.json") || name.starts_with('.') {
            continue;
        }
        if let Some(raw) = read_bounded_device_control_record(&p) {
            if let Ok(req) = serde_json::from_str::<DeviceControlRequest>(&raw) {
                let _ = std::fs::remove_file(&p);
                out.push(req);
            }
        }
    }
    out
}

/// Write the result for a request back into `target_host`'s dir (atomic).
///
/// # Errors
/// IO / serialization failures.
pub fn write_result(
    workgroup_root: &Path,
    target_host: &str,
    result: &DeviceControlResult,
) -> std::io::Result<PathBuf> {
    let dir = control_dir(workgroup_root, target_host);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.result.json", result.id));
    let tmp = dir.join(format!(".{}.result.tmp", result.id));
    std::fs::write(&tmp, serde_json::to_string_pretty(result)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Read (and consume) the result for `id`, if the executor has answered yet.
#[must_use]
pub fn take_result(
    workgroup_root: &Path,
    target_host: &str,
    id: &str,
) -> Option<DeviceControlResult> {
    let path = control_dir(workgroup_root, target_host).join(format!("{id}.result.json"));
    let raw = read_bounded_device_control_record(&path)?;
    let result = serde_json::from_str(&raw).ok()?;
    let _ = std::fs::remove_file(&path);
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> DeviceControlRequest {
        DeviceControlRequest {
            id: "01HZX".into(),
            op: DeviceControlOp::Disable,
            target: DeviceTarget {
                name: "Intel I219-V".into(),
                category: crate::device_inventory::category::NETWORK_ADAPTERS.into(),
                sysfs_path: Some("/sys/bus/pci/devices/0000:00:1f.6".into()),
                driver: Some("e1000e".into()),
            },
            target_host: "edge-2".into(),
            from: "peer:laptop-mm".into(),
        }
    }

    #[test]
    fn op_wire_tokens_are_kebab_and_stable() {
        assert_eq!(
            serde_json::to_string(&DeviceControlOp::ReloadModule).unwrap(),
            "\"reload-module\""
        );
        assert_eq!(DeviceControlOp::Disable.as_str(), "disable");
        assert_eq!(DeviceControlOp::ALL.len(), 4);
        assert!(DeviceControlOp::Disable.is_disabling());
        assert!(!DeviceControlOp::Enable.is_disabling());
        assert!(!DeviceControlOp::RescanBus.is_disabling());
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = sample_request();
        let json = serde_json::to_string_pretty(&req).unwrap();
        let back: DeviceControlRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
        // A minimal target omits its absent optionals (no null noise).
        let bare = DeviceTarget::new("Core i7", crate::device_inventory::category::PROCESSORS);
        let bare_json = serde_json::to_string(&bare).unwrap();
        assert!(!bare_json.contains("null"), "{bare_json}");
        assert!(!bare_json.contains("sysfs_path"), "{bare_json}");
    }

    #[test]
    fn result_shapes_stay_honest() {
        let ok = DeviceControlResult::ok("01A", "disabled e1000e");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["detail"], "disabled e1000e");
        assert!(!v.as_object().unwrap().contains_key("error"));

        let bad = DeviceControlResult::failed("01B", "no bound driver/module to reload");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&bad).unwrap()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "no bound driver/module to reload");
        assert!(!v.as_object().unwrap().contains_key("detail"));
    }

    #[test]
    fn request_dispatch_and_result_round_trip_through_the_replicated_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let req = sample_request();

        // The requester writes into the target's replicated dir.
        let path = write_request(root, &req).unwrap();
        assert_eq!(
            path,
            control_dir(root, "edge-2").join("01HZX.json"),
            "the request lands under fleet/device-control/<target>/"
        );

        // A peer that is NOT the target drains nothing.
        assert!(take_requests(root, "other-host").is_empty());
        // Half-written dotfiles + result files are ignored on the drain.
        std::fs::write(control_dir(root, "edge-2").join(".x.json.tmp"), "{}").unwrap();

        // The target drains exactly its request (consumed — a second drain is empty).
        let got = take_requests(root, "edge-2");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], req);
        assert!(take_requests(root, "edge-2").is_empty());

        // The executor writes a result back; the requester consumes it once.
        write_result(
            root,
            "edge-2",
            &DeviceControlResult::ok("01HZX", "disabled e1000e"),
        )
        .unwrap();
        let result = take_result(root, "edge-2", "01HZX").expect("result present");
        assert!(result.ok);
        assert_eq!(result.detail, "disabled e1000e");
        assert!(
            take_result(root, "edge-2", "01HZX").is_none(),
            "consumed once"
        );
    }

    #[test]
    fn request_reader_rejects_hostile_files_without_consuming_them() {
        let tmp = tempfile::tempdir().unwrap();
        let request = serde_json::to_vec_pretty(&sample_request()).unwrap();

        let oversized_dir = control_dir(tmp.path(), "oversized");
        std::fs::create_dir_all(&oversized_dir).unwrap();
        let oversized = oversized_dir.join("oversized.json");
        std::fs::write(&oversized, vec![b' '; MAX_DEVICE_CONTROL_RECORD_BYTES + 1]).unwrap();
        assert!(take_requests(tmp.path(), "oversized").is_empty());
        assert!(oversized.exists(), "rejected requests remain pending");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::process::Command;

            let symlink_dir = control_dir(tmp.path(), "symlink");
            std::fs::create_dir_all(&symlink_dir).unwrap();
            let outside = tmp.path().join("request-outside.json");
            std::fs::write(&outside, &request).unwrap();
            let link = symlink_dir.join("planted.json");
            symlink(&outside, &link).unwrap();
            assert!(take_requests(tmp.path(), "symlink").is_empty());
            assert!(link.exists(), "rejected symlinks remain pending");

            let special_dir = control_dir(tmp.path(), "special");
            std::fs::create_dir_all(&special_dir).unwrap();
            let fifo = special_dir.join("request.fifo.json");
            if Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            {
                assert!(take_requests(tmp.path(), "special").is_empty());
                assert!(fifo.exists(), "rejected special files remain pending");
            }
        }
    }

    #[test]
    fn result_reader_rejects_hostile_files_without_consuming_them() {
        let tmp = tempfile::tempdir().unwrap();
        let result = serde_json::to_vec_pretty(&DeviceControlResult::ok("01HZX", "done")).unwrap();

        let oversized_dir = control_dir(tmp.path(), "oversized");
        std::fs::create_dir_all(&oversized_dir).unwrap();
        let oversized = oversized_dir.join("01HZX.result.json");
        std::fs::write(&oversized, vec![b' '; MAX_DEVICE_CONTROL_RECORD_BYTES + 1]).unwrap();
        assert!(take_result(tmp.path(), "oversized", "01HZX").is_none());
        assert!(oversized.exists(), "rejected results remain pending");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::process::Command;

            let symlink_dir = control_dir(tmp.path(), "symlink");
            std::fs::create_dir_all(&symlink_dir).unwrap();
            let outside = tmp.path().join("result-outside.json");
            std::fs::write(&outside, &result).unwrap();
            let link = symlink_dir.join("01HZX.result.json");
            symlink(&outside, &link).unwrap();
            assert!(take_result(tmp.path(), "symlink", "01HZX").is_none());
            assert!(link.exists(), "rejected symlinks remain pending");

            let special_dir = control_dir(tmp.path(), "special");
            std::fs::create_dir_all(&special_dir).unwrap();
            let fifo = special_dir.join("01HZX.result.json");
            if Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
            {
                assert!(take_result(tmp.path(), "special", "01HZX").is_none());
                assert!(fifo.exists(), "rejected special files remain pending");
            }
        }
    }
}
