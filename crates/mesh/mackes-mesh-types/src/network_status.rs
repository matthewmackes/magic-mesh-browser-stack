//! Typed, credential-free node-local network observations.
//!
//! This is the additive `network.interfaces[]` portion of the world-readable
//! mesh-status document.  It deliberately contains link/provider facts only:
//! NetworkManager and ModemManager must never put SSIDs, APNs, connection
//! profiles, passwords, PSKs, or modem credentials on this boundary.

use serde::{Deserialize, Serialize};

/// Maximum number of provider observations a status producer may publish.
pub const MAX_PROVIDER_LINKS: usize = 8;

/// Linux interface names are bounded by `IFNAMSIZ - 1` bytes.
pub const MAX_INTERFACE_NAME_BYTES: usize = 15;

/// The provider family that owns a link observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProvider {
    /// Wi-Fi managed by NetworkManager.
    Wifi,
    /// Wired Ethernet managed by NetworkManager.
    Ethernet,
    /// Cellular data managed by NetworkManager/ModemManager.
    Cellular,
}

/// Honest provider/link state.  Unknown and unavailable are distinct from
/// disconnected: the former means the provider did not prove a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderLinkState {
    /// The provider did not return a usable state.
    Unknown,
    /// The provider/device is not currently available.
    Unavailable,
    /// The device is present but not connected.
    Disconnected,
    /// The device is negotiating a connection.
    Connecting,
    /// The link is connected.
    Connected,
}

impl Default for ProviderLinkState {
    fn default() -> Self {
        Self::Unknown
    }
}

impl ProviderLinkState {
    /// Whether this observation proves a usable link.
    #[must_use]
    pub const fn is_up(self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// A safe, bounded provider/link observation for `network.interfaces[]`.
///
/// `interface` is a kernel link name or an empty provider-level identifier;
/// it is never a connection profile name.  `cidr` is limited to addresses
/// already observed on that link.  No provider payload is retained here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLinkObservation {
    /// Provider family (`wifi`, `ethernet`, or `cellular`).
    pub provider: NetworkProvider,
    /// Kernel interface name, when the provider supplies one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub interface: String,
    /// Current provider/link state.
    #[serde(default)]
    pub status: ProviderLinkState,
    /// Whether the provider explicitly reports a connected link.
    pub up: bool,
    /// An observed address/prefix for the link, never a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    /// Optional signal percentage supplied by the provider, 0..=100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_percent: Option<u8>,
}

impl ProviderLinkObservation {
    /// Construct a normalized observation from a provider state.
    #[must_use]
    pub fn new(
        provider: NetworkProvider,
        interface: impl Into<String>,
        status: ProviderLinkState,
    ) -> Self {
        Self {
            provider,
            interface: interface.into(),
            status,
            up: status.is_up(),
            cidr: None,
            signal_percent: None,
        }
    }

    /// Whether the identifier is safe to publish on the world-readable status
    /// boundary. Empty identifiers are allowed for provider-level observations.
    /// NetworkManager names must be kernel interface names; ModemManager may
    /// additionally report a device node directly beneath `/dev`.
    #[must_use]
    pub fn has_safe_interface_identifier(&self) -> bool {
        let identifier = self.interface.as_str();
        if identifier.is_empty() {
            return true;
        }

        let name = identifier.strip_prefix("/dev/").unwrap_or(identifier);
        !name.is_empty()
            && name.len() <= MAX_INTERFACE_NAME_BYTES
            && name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            })
            && (!identifier.starts_with('/') || identifier.starts_with("/dev/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_identifier_rejects_malformed_and_credential_shaped_values() {
        for identifier in ["wlan0", "wwan0.1", "/dev/cdc-wdm0", ""] {
            assert!(ProviderLinkObservation::new(
                NetworkProvider::Wifi,
                identifier,
                ProviderLinkState::Connected,
            )
            .has_safe_interface_identifier());
        }

        for identifier in [
            "office wifi",
            "user@example.com",
            "password=hunter2",
            "../../secret",
            "/dev/mapper/private",
            "interface-name-is-too-long",
        ] {
            assert!(
                !ProviderLinkObservation::new(
                    NetworkProvider::Wifi,
                    identifier,
                    ProviderLinkState::Connected,
                )
                .has_safe_interface_identifier(),
                "accepted {identifier:?}"
            );
        }
    }
}
