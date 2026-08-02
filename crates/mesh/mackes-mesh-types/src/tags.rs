//! Portal-18.a (v6.0, R12 lock 2026-05-26) — universal tag schema +
//! per-peer storage layer.
//!
//! Tags are the platform's universal grouping primitive. A tag can
//! gather apps, files, peers, contacts, containers, workspaces, tray
//! items, and zones into a single named bucket; consumers across
//! every crate share the same [`Tag`] definition. Storage is a JSON
//! file at `<XDG_DATA_HOME>/mde/tags.json` (resolved by [`default_tags_path`]).
//! Once the Syncthing mesh-home is linked under XDG the file inherits
//! mesh replication automatically; pre-mesh-home boots fall
//! back to per-peer.
//!
//! The R12 round of locks (2026-05-26) extended the schema with four
//! workspace-policy fields up front so downstream Round 12 features
//! (Portal-42 / Portal-44 / Portal-47 / Portal-50 / Portal-54 /
//! Portal-56 / Portal-58 + BUS-5.7) just consume the schema rather
//! than extending it:
//!
//!   * `group_color` (R12-Q21) — drives focused-border tinting + the
//!     transient previous-workspace segment color.
//!   * `preferred_output` (R12-Q2) — `workspace::init` routes a tag-
//!     owned workspace to this output.
//!   * `default_layout` (R12-Q4) — splith/splitv/tabbed/stacked
//!     baseline for new windows in a tag-owned workspace.
//!   * `autostart` (R12-Q16) — `Vec<String>` `app_ids` that launch
//!     on first init of a tag-owned workspace per mded-lifetime.
//!
//! Three tag flavors are locked (R1-Q103 + R1-Q63):
//!
//!   * `Manual` (default) — explicit member-list.
//!   * `Smart { predicate }` — runtime-evaluated predicate string
//!     (Portal-18.c implements the evaluator).
//!   * `Preset { launch_bundle }` — clicking the tag card fires
//!     `swaymsg exec <cmd>` for each entry.
//!
//! Tag NAMES are the natural key; renaming requires a transactional
//! pass across surfaces (Portal-18.b modal). IDs are deliberately
//! omitted — names are short, human-readable, and stable under the
//! mesh-synced model (operators don't rename frequently).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One tag — the platform's universal grouping primitive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    /// Human-readable name + natural key. Must be unique within the
    /// tag store. Leading/trailing whitespace is trimmed on add.
    pub name: String,
    /// Flavor — Manual / Smart / Preset.
    #[serde(default = "default_flavor")]
    pub flavor: TagFlavor,
    /// Members — explicit list for Manual tags, ignored for Smart
    /// (whose predicate computes membership at query time) and
    /// Preset (whose `launch_bundle` is the click-action, not a
    /// membership set).
    #[serde(default)]
    pub members: Vec<TagMember>,
    /// R12-Q21 — focused-border tint + Portal-43 segment color.
    /// CSS hex string (e.g. `#42be65`); `None` falls back to the
    /// platform default (Material blue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_color: Option<String>,
    /// R12-Q2 — `workspace::init` routes a tag-owned workspace to
    /// this sway output name (e.g. `HDMI-A-1`). `None` lets sway
    /// pick naturally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_output: Option<String>,
    /// R12-Q4 — baseline container layout for new windows in a
    /// tag-owned workspace. One of `splith`, `splitv`, `tabbed`,
    /// `stacked`. `None` follows sway's parent default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_layout: Option<String>,
    /// R12-Q16 — `app_ids` to launch on first init of a tag-owned
    /// workspace per mded-lifetime. NOT XDG-autostart-compliant;
    /// tag-driven is the only mechanism.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub autostart: Vec<String>,
}

const fn default_flavor() -> TagFlavor {
    TagFlavor::Manual
}

/// Tag flavor — three locked variants per R1-Q103 + R1-Q63.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagFlavor {
    /// Default flavor. Member list is operator-curated.
    Manual,
    /// Smart tag — runtime-evaluated predicate (e.g.
    /// `"app:firefox or app:chromium"`). The evaluator lives in
    /// Portal-18.c; this crate only stores the source string.
    Smart {
        /// Predicate source — grammar defined by Portal-18.c.
        predicate: String,
    },
    /// Preset launch-bundle (R1-Q63). Click fires `swaymsg exec
    /// <cmd>` for each entry — does not contribute members.
    Preset {
        /// `app_ids` (or shell commands) launched on tag-card click.
        launch_bundle: Vec<String>,
    },
}

/// Members of a Manual tag. The `kind` discriminator distinguishes
/// surfaces so per-surface tag listings can filter cheaply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TagMember {
    /// Sway / desktop-entry `app_id`.
    App {
        /// e.g. `firefox`, `foot`, `helix`.
        app_id: String,
    },
    /// Mesh peer hostname.
    Peer {
        /// Peer hostname as it appears on Nebula.
        hostname: String,
    },
    /// Contact card ULID.
    Contact {
        /// Contact-card identifier (ULID per Portal-32).
        ulid: String,
    },
    /// Sway workspace number.
    Workspace {
        /// Numeric workspace ID.
        num: i32,
    },
    /// Container (podman) name.
    Container {
        /// Podman container name.
        name: String,
    },
    /// Tray (`StatusNotifierItem`) bus name.
    Tray {
        /// SNI bus name (e.g. `org.freedesktop.StatusNotifier-1234-1`).
        bus_name: String,
    },
    /// Files are tagged via xattr `user.mde.tags`, NOT in tag.json —
    /// this variant exists for cross-surface queries to identify
    /// a file-typed tag membership without xattr access (e.g.
    /// pre-cached search index).
    File {
        /// Absolute path.
        path: String,
    },
    /// Activity card identifier.
    Activity {
        /// Activity ULID per Portal-33.
        ulid: String,
    },
    /// Zone within the platform (e.g. `navigation-pinned`, `navigation-tray`).
    /// One designated tag drives the navigation pinned zone per R3-Q88.
    Zone {
        /// Zone identifier.
        name: String,
    },
}

/// Top-level tag store. Wraps `Vec<Tag>` with a stable on-disk
/// representation + atomic-write helpers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagStore {
    /// Schema version. Bump on backwards-incompatible changes; for
    /// now the field is informational (no migration framework
    /// since 1.0 ships with one shape).
    #[serde(default = "schema_version_default")]
    pub schema_version: u32,
    /// All tags in the store. Order is preserved on save.
    #[serde(default)]
    pub tags: Vec<Tag>,
}

impl Default for TagStore {
    fn default() -> Self {
        Self {
            schema_version: schema_version_default(),
            tags: Vec::new(),
        }
    }
}

const fn schema_version_default() -> u32 {
    1
}

/// Error surface for the tag store.
#[derive(Debug)]
pub enum TagStoreError {
    /// I/O failure on read/write.
    Io(io::Error),
    /// JSON parse failure.
    Parse(serde_json::Error),
    /// Tag with the given name already exists (case-sensitive
    /// exact match).
    DuplicateName(String),
    /// The tag-store path can't be resolved (`XDG_DATA_HOME`
    /// unset AND $HOME unset — vanishingly rare).
    PathResolution,
}

impl std::fmt::Display for TagStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "tag-store I/O: {e}"),
            Self::Parse(e) => write!(f, "tag-store parse: {e}"),
            Self::DuplicateName(n) => write!(f, "tag '{n}' already exists"),
            Self::PathResolution => write!(f, "could not resolve tag-store path"),
        }
    }
}

impl std::error::Error for TagStoreError {}

impl From<io::Error> for TagStoreError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for TagStoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Parse(e)
    }
}

impl TagStore {
    /// Load the tag store from `<XDG_DATA_HOME>/mde/tags.json`.
    /// Missing file returns an empty store (first-run path).
    pub fn load_default() -> Result<Self, TagStoreError> {
        let path = default_tags_path()?;
        Self::load_from(&path)
    }

    /// Load the tag store from an explicit path. Missing file is
    /// NOT an error — returns an empty store so the first-run
    /// path is the same as load-then-add.
    pub fn load_from(path: &Path) -> Result<Self, TagStoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let store: Self = serde_json::from_str(&raw)?;
        Ok(store)
    }

    /// Save the tag store to `<XDG_DATA_HOME>/mde/tags.json`.
    pub fn save_default(&self) -> Result<(), TagStoreError> {
        let path = default_tags_path()?;
        self.save_to(&path)
    }

    /// Save to an explicit path. Atomic via temp + rename so a
    /// crash mid-write leaves the existing file intact. Creates
    /// the parent directory if missing.
    pub fn save_to(&self, path: &Path) -> Result<(), TagStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let pretty = serde_json::to_string_pretty(self)?;
        let mut tmp_path = path.to_path_buf();
        tmp_path.set_extension("json.tmp");
        fs::write(&tmp_path, pretty)?;
        fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Append a new tag. Returns `DuplicateName` if a tag with that
    /// name already exists. The name is trimmed before insertion;
    /// after trimming, an empty name is rejected as
    /// `DuplicateName("")` to keep the error surface simple.
    pub fn add(&mut self, mut tag: Tag) -> Result<(), TagStoreError> {
        tag.name = tag.name.trim().to_string();
        if tag.name.is_empty() {
            return Err(TagStoreError::DuplicateName(String::new()));
        }
        if self.find_by_name(&tag.name).is_some() {
            return Err(TagStoreError::DuplicateName(tag.name));
        }
        self.tags.push(tag);
        Ok(())
    }

    /// Remove the tag with the given name. Returns `true` if a tag
    /// was removed, `false` otherwise.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.tags.len();
        self.tags.retain(|t| t.name != name);
        self.tags.len() != before
    }

    /// Find a tag by name (case-sensitive exact match).
    #[must_use]
    pub fn find_by_name(&self, name: &str) -> Option<&Tag> {
        self.tags.iter().find(|t| t.name == name)
    }

    /// Mutable `find_by_name` for in-place edits.
    pub fn find_by_name_mut(&mut self, name: &str) -> Option<&mut Tag> {
        self.tags.iter_mut().find(|t| t.name == name)
    }
}

/// Resolve `<XDG_DATA_HOME>/mde/tags.json`. Returns
/// `TagStoreError::PathResolution` only if BOTH `$XDG_DATA_HOME`
/// and `$HOME` are unset (the `dirs` crate's fallback chain).
pub fn default_tags_path() -> Result<PathBuf, TagStoreError> {
    let data_home = dirs::data_dir().ok_or(TagStoreError::PathResolution)?;
    Ok(data_home.join("mde").join("tags.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dev_tag() -> Tag {
        Tag {
            name: "Dev".to_string(),
            flavor: TagFlavor::Manual,
            members: vec![
                TagMember::App {
                    app_id: "helix".to_string(),
                },
                TagMember::App {
                    app_id: "foot".to_string(),
                },
            ],
            group_color: Some("#42be65".to_string()),
            preferred_output: Some("HDMI-A-1".to_string()),
            default_layout: Some("splith".to_string()),
            autostart: vec!["helix".to_string(), "foot".to_string()],
        }
    }

    #[test]
    fn empty_store_serde_round_trips() {
        let store = TagStore::default();
        let json = serde_json::to_string(&store).unwrap();
        let parsed: TagStore = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert!(parsed.tags.is_empty());
    }

    #[test]
    fn manual_tag_with_r12_fields_round_trips() {
        let store = TagStore {
            schema_version: 1,
            tags: vec![sample_dev_tag()],
        };
        let json = serde_json::to_string_pretty(&store).unwrap();
        let parsed: TagStore = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tags.len(), 1);
        let t = &parsed.tags[0];
        assert_eq!(t.name, "Dev");
        assert_eq!(t.flavor, TagFlavor::Manual);
        assert_eq!(t.group_color.as_deref(), Some("#42be65"));
        assert_eq!(t.preferred_output.as_deref(), Some("HDMI-A-1"));
        assert_eq!(t.default_layout.as_deref(), Some("splith"));
        assert_eq!(t.autostart, vec!["helix".to_string(), "foot".to_string()]);
        assert_eq!(t.members.len(), 2);
    }

    #[test]
    fn smart_tag_round_trips() {
        let store = TagStore {
            schema_version: 1,
            tags: vec![Tag {
                name: "Browsers".to_string(),
                flavor: TagFlavor::Smart {
                    predicate: "app:firefox or app:chromium".to_string(),
                },
                members: Vec::new(),
                group_color: None,
                preferred_output: None,
                default_layout: None,
                autostart: Vec::new(),
            }],
        };
        let json = serde_json::to_string(&store).unwrap();
        let parsed: TagStore = serde_json::from_str(&json).unwrap();
        match &parsed.tags[0].flavor {
            TagFlavor::Smart { predicate } => {
                assert_eq!(predicate, "app:firefox or app:chromium");
            }
            other => panic!("expected Smart, got {other:?}"),
        }
    }

    #[test]
    fn preset_tag_round_trips() {
        let store = TagStore {
            schema_version: 1,
            tags: vec![Tag {
                name: "Work Setup".to_string(),
                flavor: TagFlavor::Preset {
                    launch_bundle: vec![
                        "firefox".to_string(),
                        "foot".to_string(),
                        "helix".to_string(),
                    ],
                },
                members: Vec::new(),
                group_color: None,
                preferred_output: None,
                default_layout: None,
                autostart: Vec::new(),
            }],
        };
        let json = serde_json::to_string(&store).unwrap();
        let parsed: TagStore = serde_json::from_str(&json).unwrap();
        match &parsed.tags[0].flavor {
            TagFlavor::Preset { launch_bundle } => {
                assert_eq!(launch_bundle.len(), 3);
                assert_eq!(launch_bundle[0], "firefox");
            }
            other => panic!("expected Preset, got {other:?}"),
        }
    }

    #[test]
    fn add_rejects_duplicate_name() {
        let mut store = TagStore::default();
        store.add(sample_dev_tag()).unwrap();
        let dup = store.add(sample_dev_tag());
        match dup {
            Err(TagStoreError::DuplicateName(n)) => assert_eq!(n, "Dev"),
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    #[test]
    fn add_rejects_empty_name() {
        let mut store = TagStore::default();
        let mut blank = sample_dev_tag();
        blank.name = "   ".to_string();
        let err = store.add(blank);
        assert!(matches!(err, Err(TagStoreError::DuplicateName(s)) if s.is_empty()));
    }

    #[test]
    fn add_trims_whitespace_in_name() {
        let mut store = TagStore::default();
        let mut padded = sample_dev_tag();
        padded.name = "  Padded  ".to_string();
        store.add(padded).unwrap();
        assert!(store.find_by_name("Padded").is_some());
        assert!(store.find_by_name("  Padded  ").is_none());
    }

    #[test]
    fn remove_returns_true_only_when_present() {
        let mut store = TagStore::default();
        assert!(!store.remove("Nope"));
        store.add(sample_dev_tag()).unwrap();
        assert!(store.remove("Dev"));
        assert!(store.find_by_name("Dev").is_none());
        assert!(!store.remove("Dev"));
    }

    #[test]
    fn load_from_missing_file_returns_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope/tags.json");
        let store = TagStore::load_from(&path).unwrap();
        assert!(store.tags.is_empty());
        assert_eq!(store.schema_version, 1);
    }

    #[test]
    fn save_to_then_load_from_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/tags.json");
        let mut store = TagStore::default();
        store.add(sample_dev_tag()).unwrap();
        store.save_to(&path).unwrap();
        let loaded = TagStore::load_from(&path).unwrap();
        assert_eq!(loaded, store);
    }

    #[test]
    fn save_atomic_leaves_no_tmp_file_on_success() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tags.json");
        let mut store = TagStore::default();
        store.add(sample_dev_tag()).unwrap();
        store.save_to(&path).unwrap();
        let tmp_sibling = path.with_extension("json.tmp");
        assert!(
            !tmp_sibling.exists(),
            "atomic write must clean up tmp sibling"
        );
        assert!(path.exists());
    }

    #[test]
    fn find_by_name_mut_allows_in_place_edits() {
        let mut store = TagStore::default();
        store.add(sample_dev_tag()).unwrap();
        let t = store.find_by_name_mut("Dev").unwrap();
        t.group_color = Some("#33b1ff".to_string());
        assert_eq!(
            store.find_by_name("Dev").unwrap().group_color.as_deref(),
            Some("#33b1ff")
        );
    }

    /// The `kind` tag discriminator on `TagMember` keeps every
    /// surface filterable from the JSON without parsing the inner
    /// payload. Lock the contract.
    #[test]
    fn tag_member_serialization_includes_kind_discriminator() {
        let m = TagMember::App {
            app_id: "firefox".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"app\""));
        let m = TagMember::Peer {
            hostname: "fedora".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"kind\":\"peer\""));
    }

    /// Missing optional R12 fields don't break deserialization —
    /// pre-R12 tag.json files load cleanly.
    #[test]
    fn pre_r12_tag_json_loads_without_optional_fields() {
        let json = r#"{
            "schema_version": 1,
            "tags": [
                {"name": "Legacy", "members": []}
            ]
        }"#;
        let store: TagStore = serde_json::from_str(json).unwrap();
        assert_eq!(store.tags.len(), 1);
        let t = &store.tags[0];
        assert_eq!(t.name, "Legacy");
        assert_eq!(t.flavor, TagFlavor::Manual);
        assert!(t.group_color.is_none());
        assert!(t.preferred_output.is_none());
        assert!(t.default_layout.is_none());
        assert!(t.autostart.is_empty());
    }
}
