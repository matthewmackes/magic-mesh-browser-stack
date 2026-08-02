//! Shared media-source roster wire types.
//!
//! `mackesd` publishes these records on [`MEDIA_SOURCES_TOPIC`] and desktop
//! surfaces consume the same schema without depending on the daemon crate.

use serde::{Deserialize, Serialize};

/// The retained-latest media-source state topic.
pub const MEDIA_SOURCES_TOPIC: &str = "state/media/sources";

/// Placeholder `UserId` a GUI client may use when dialing a Jellyfin gateway.
///
/// The gateway proxy rewrites this value to the sealed server-side Jellyfin
/// `user_id` before forwarding. This lets clients use the normal typed
/// Jellyfin API without learning or persisting the real upstream user id.
pub const JELLYFIN_GATEWAY_USER_SENTINEL: &str = "__mde_gateway_user__";

/// The kind of media source, as the Media Workspace acceptance enumerates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A Jellyfin media server.
    Jellyfin,
    /// A DLNA/UPnP media server.
    Dlna,
    /// This-player-as-server — a peer running the mesh media server.
    MeshPlayer,
    /// A mesh file share (`/mnt/mesh-storage`) browsable for media.
    FileShare,
}

impl MediaKind {
    /// Stable wire/log tag.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Jellyfin => "jellyfin",
            Self::Dlna => "dlna",
            Self::MeshPlayer => "mesh_player",
            Self::FileShare => "file_share",
        }
    }
}

/// A protocol a media source is reached over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaProtocol {
    /// The Jellyfin REST API (HTTP).
    Jellyfin,
    /// DLNA/UPnP (SOAP over HTTP).
    Dlna,
    /// A plain HTTP media endpoint.
    Http,
    /// Browse files over the mesh sshfs mount.
    MeshFs,
}

impl MediaProtocol {
    /// Stable wire/log tag.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Jellyfin => "jellyfin",
            Self::Dlna => "dlna",
            Self::Http => "http",
            Self::MeshFs => "mesh-fs",
        }
    }

    /// The natural protocol set a source of this kind is dialed over.
    #[must_use]
    pub fn for_kind(kind: MediaKind) -> Vec<Self> {
        match kind {
            MediaKind::Jellyfin => vec![Self::Jellyfin],
            MediaKind::Dlna => vec![Self::Dlna],
            MediaKind::MeshPlayer => vec![Self::Http],
            MediaKind::FileShare => vec![Self::MeshFs],
        }
    }
}

/// Derived reachability of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    /// Roster state says the source should answer.
    Reachable,
    /// Roster state says it will not answer.
    Unreachable,
    /// Nothing derivable — honest.
    Unknown,
}

/// Which discovery lane produced a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceOrigin {
    /// Peer-advertised via the replicated peers plane.
    MeshPeer,
    /// Discovered on the local LAN via mDNS.
    Mdns,
    /// Manually registered LAN server proxied through a mesh gateway node.
    Gateway,
}

/// One merged media source — a row of the published roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSource {
    /// Stable source id.
    pub id: String,
    /// Display name for the Sources panel.
    pub name: String,
    /// The node/host the panel groups by.
    pub node: String,
    /// The kind of media source.
    pub kind: MediaKind,
    /// The address a client dials.
    pub host: String,
    /// The advertised/known port, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The dialable locator.
    pub endpoint: String,
    /// Protocols the source is reached over, deduped + sorted.
    pub protocols: Vec<MediaProtocol>,
    /// The discovery lane this source came from.
    pub origin: SourceOrigin,
    /// Derived reachability.
    pub reachability: Reachability,
    /// Human-readable reason when not reachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Gateway node when [`origin`](Self::origin) is [`SourceOrigin::Gateway`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_node: Option<String>,
    /// Canonical upstream URL used to dedupe direct/mDNS rows against a gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_key: Option<String>,
    /// Secret-store reference for sealed shared read-only credentials/tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Whether this gateway source is the mesh-wide default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_default: Option<bool>,
}

/// One discovery lane's honest status (`ok …` / `gated: …`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneStatus {
    /// Lane name (`mesh-registry` / `gateway` / `mdns`).
    pub lane: String,
    /// Status string.
    pub status: String,
}

/// The full media-source roster record published to [`MEDIA_SOURCES_TOPIC`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaSourcesState {
    /// Publishing node id.
    pub node: String,
    /// The merged, deduped source roster.
    pub sources: Vec<MediaSource>,
    /// Per-lane discovery status.
    pub lanes: Vec<LaneStatus>,
    /// Wall-clock publish time (ms since the Unix epoch).
    pub published_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_jellyfin_state_round_trips() {
        let state = MediaSourcesState {
            node: "seat-15".to_string(),
            sources: vec![MediaSource {
                id: "jellyfin-gateway:seat-15:abcd".to_string(),
                name: "Jellyfin gateway via seat-15".to_string(),
                node: "seat-15".to_string(),
                kind: MediaKind::Jellyfin,
                host: "seat-15.mesh".to_string(),
                port: Some(8097),
                endpoint: "http://seat-15.mesh:8097/mde/jellyfin/jellyfin-gateway:seat-15:abcd"
                    .to_string(),
                protocols: vec![MediaProtocol::Jellyfin],
                origin: SourceOrigin::Gateway,
                reachability: Reachability::Unreachable,
                reason: Some("gateway degraded".to_string()),
                gateway_node: Some("seat-15".to_string()),
                upstream_key: Some("http://192.168.1.60:8096".to_string()),
                credential_ref: Some("media/jellyfin/shared-readonly".to_string()),
                mesh_default: Some(true),
            }],
            lanes: vec![LaneStatus {
                lane: "gateway".to_string(),
                status: "ok".to_string(),
            }],
            published_at_ms: 42,
        };

        let json = serde_json::to_string(&state).expect("serialize");
        assert!(json.contains("\"origin\":\"gateway\""));
        assert!(json.contains("\"reachability\":\"unreachable\""));
        assert!(json.contains("\"credential_ref\":\"media/jellyfin/shared-readonly\""));
        assert!(json.contains("\"mesh_default\":true"));
        assert_eq!(JELLYFIN_GATEWAY_USER_SENTINEL, "__mde_gateway_user__");
        let decoded: MediaSourcesState = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded, state);
    }
}
