//! PLANES-3 (W82–W85) — capability tags.
//!
//! Orthogonal to the §5 role (Lighthouse ⊂ Server ⊂ Workstation):
//! a node carries zero or more **gating** capability tags that any
//! enrolled operator surface may set (W83) and that decide what duty
//! the node accepts (W84 — tags GATE, they don't merely prefer):
//!
//! - `hop`        — eligible to load relay / subnet-advertise / exit
//!   config (the Network plane).
//! - `execution`  — eligible to accept job bundles beyond
//!   self-targeted ones (the Controller plane).
//! - `headless`   — GUI app-surface units are disabled here; the
//!   agent runs fully.
//! - `hypervisor` — an XCP-ng dom0 joined as a static-Nebula member
//!   that advertises compute capacity (DATACENTER-17). Orthogonal to
//!   the §5 role: a `hypervisor` pins the Server tier (`PeerRole` flattens
//!   to Host/Peer, so it is surfaced as a capability tag, not a 4th cert
//!   role) and the `xcp_host` worker self-gates on the dom0 marker.
//!
//! Stored per-target on the replicated volume at
//! `<root>/node-tags/<hostname>.json` (any node writes any target's
//! file; the target reads its own to gate). v1 vocabulary is exactly
//! these four (builder/mirror deferred — W82).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The v1 capability-tag vocabulary (W82).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityTag {
    /// Node may load relay / subnet-advertise / exit config (Network plane).
    Hop,
    /// Node accepts job bundles beyond self-targeted ones (Controller plane).
    Execution,
    /// GUI app-surface units are disabled; agent runs fully headless.
    Headless,
    /// XCP-ng dom0 joined as a static-Nebula member advertising compute
    /// capacity (DATACENTER-17). Pins the Server tier; the `xcp_host`
    /// worker self-gates on the dom0 marker.
    Hypervisor,
    /// Retired media marker retained solely for decoding historical tag files;
    /// it is not part of [`CapabilityTag::ALL`] and cannot be parsed by current
    /// surfaces.
    Media,
}

impl CapabilityTag {
    /// Every v1 capability tag, in wire order — the single source of truth
    /// any surface should iterate (the fleet `tags --json` census, profile
    /// validation, the Node-roles editor) instead of hand-maintaining a
    /// parallel list that silently drops a tag (DATACENTER-17 added a 4th).
    pub const ALL: [Self; 4] = [Self::Hop, Self::Execution, Self::Headless, Self::Hypervisor];

    /// Stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hop => "hop",
            Self::Execution => "execution",
            Self::Headless => "headless",
            Self::Hypervisor => "hypervisor",
            Self::Media => "media",
        }
    }

    /// Parse a tag token; `None` for anything outside the v1 set
    /// (an unknown tag is refused, not silently accepted — W82).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hop" => Some(Self::Hop),
            "execution" => Some(Self::Execution),
            "headless" => Some(Self::Headless),
            "hypervisor" => Some(Self::Hypervisor),
            _ => None,
        }
    }
}

/// The directory holding every node's tag file.
#[must_use]
pub fn tags_dir(workgroup_root: &Path) -> PathBuf {
    workgroup_root.join("node-tags")
}

/// A node's tag set (the JSON file shape).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeTags {
    /// The active capability tags for this node.
    #[serde(default)]
    pub tags: BTreeSet<CapabilityTag>,
}

impl NodeTags {
    /// Does this set carry `tag`? The gate query (W84).
    #[must_use]
    pub fn has(&self, tag: CapabilityTag) -> bool {
        self.tags.contains(&tag)
    }
}

/// Read a node's tags (empty when unset / unparseable — an absent
/// file means "no extra capabilities", never a panic).
#[must_use]
pub fn read_tags(workgroup_root: &Path, hostname: &str) -> NodeTags {
    let path = tags_dir(workgroup_root).join(format!("{hostname}.json"));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Write a node's tags (atomic temp + rename). Any enrolled surface
/// may call this for any target (W83 — mesh access is the
/// authorization; the caller audit-logs).
///
/// # Errors
/// IO / serialization failures.
pub fn write_tags(workgroup_root: &Path, hostname: &str, tags: &NodeTags) -> io::Result<PathBuf> {
    if tags.tags.contains(&CapabilityTag::Media) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "media capability is retired; lighthouse nodes are thin",
        ));
    }
    let dir = tags_dir(workgroup_root);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{hostname}.json"));
    let tmp = dir.join(format!(".{hostname}.json.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(tags)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_round_trips_and_refuses_unknowns() {
        for t in [
            CapabilityTag::Hop,
            CapabilityTag::Execution,
            CapabilityTag::Headless,
            CapabilityTag::Hypervisor,
        ] {
            assert_eq!(CapabilityTag::parse(t.as_str()), Some(t));
        }
        assert_eq!(
            CapabilityTag::parse("hypervisor"),
            Some(CapabilityTag::Hypervisor),
            "DATACENTER-17 — XCP-ng dom0 is a first-class v1 tag"
        );
        assert_eq!(CapabilityTag::parse("builder"), None, "deferred — not v1");
        assert_eq!(CapabilityTag::parse(""), None);
    }

    #[test]
    fn all_is_the_complete_parseable_vocabulary() {
        // ALL is the single source of truth: every entry round-trips, and
        // there are no parseable tokens outside it (a 5th variant added
        // without extending ALL trips this — the drift this guard prevents).
        for t in CapabilityTag::ALL {
            assert_eq!(CapabilityTag::parse(t.as_str()), Some(t));
        }
        assert!(CapabilityTag::ALL.contains(&CapabilityTag::Hypervisor));
    }

    #[test]
    fn tags_round_trip_and_gate_query() {
        let tmp = tempfile::tempdir().unwrap();
        let mut set = NodeTags::default();
        set.tags.insert(CapabilityTag::Execution);
        set.tags.insert(CapabilityTag::Hop);
        write_tags(tmp.path(), "oak", &set).unwrap();
        let back = read_tags(tmp.path(), "oak");
        assert!(back.has(CapabilityTag::Execution));
        assert!(back.has(CapabilityTag::Hop));
        assert!(!back.has(CapabilityTag::Headless));
    }

    #[test]
    fn an_untagged_node_has_no_capabilities() {
        let tmp = tempfile::tempdir().unwrap();
        let t = read_tags(tmp.path(), "ghost");
        assert!(!t.has(CapabilityTag::Execution));
        assert!(t.tags.is_empty());
    }
}
