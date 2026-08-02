//! VDI clipboard capability status shared by desktop backends and broker records.
//!
//! WL-FUNC-016 accepts RDP/SPICE clipboard work only when the backend either
//! drives the protocol's real clipboard channel or reports an explicit unsupported
//! state. This type is that shared status surface: it is serializable for retained
//! Bus records and also cheap for the RDP/SPICE session crates to expose directly.

use serde::{Deserialize, Serialize};

/// Maximum encoded UTF-8 size of one VDI text clipboard value.
///
/// The limit is measured in bytes, not Unicode scalar values, because VDI
/// transport caps and serialized payload sizes are byte-based. Values are
/// accepted only when the complete UTF-8 encoding fits; they are never split
/// or silently truncated by this type.
pub const MAX_VDI_CLIPBOARD_TEXT_BYTES: usize = 1024 * 1024;

/// Node-local, latest-wins handoff from an authorized daemon action to the
/// direct DRM seat. This is deliberately separate from the replicated
/// `event/clipboard/clip` history lane: a guest copy addressed to one seat must
/// not be replayed into every VDI session on that node.
pub const CLIPBOARD_MATERIALIZATION_TOPIC: &str = "state/clipboard/materialize";

/// Maximum time a seat may defer consuming a daemon materialization. A stale
/// handoff is ignored rather than pasted after a shell restart.
pub const CLIPBOARD_MATERIALIZATION_MAX_AGE_SECS: i64 = 60;

/// A validated, bounded UTF-8 text value for the VDI clipboard lane.
///
/// Construct this value at a protocol or platform boundary so every consumer
/// can rely on the same byte bound. Empty text is valid and represents a
/// clipboard clear; callers that require a non-empty clip should apply that
/// policy separately.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VdiClipboardText(String);

impl VdiClipboardText {
    /// Validate and wrap an owned UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns [`VdiClipboardTextValidationError::TooLarge`] when the encoded
    /// value exceeds [`MAX_VDI_CLIPBOARD_TEXT_BYTES`].
    pub fn new(text: impl Into<String>) -> Result<Self, VdiClipboardTextValidationError> {
        let text = text.into();
        if text.len() > MAX_VDI_CLIPBOARD_TEXT_BYTES {
            return Err(VdiClipboardTextValidationError::TooLarge {
                bytes: text.len(),
                max_bytes: MAX_VDI_CLIPBOARD_TEXT_BYTES,
            });
        }
        Ok(Self(text))
    }

    /// Validate and decode raw clipboard bytes as UTF-8.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Result<Self, VdiClipboardTextValidationError> {
        let bytes = bytes.into();
        if bytes.len() > MAX_VDI_CLIPBOARD_TEXT_BYTES {
            return Err(VdiClipboardTextValidationError::TooLarge {
                bytes: bytes.len(),
                max_bytes: MAX_VDI_CLIPBOARD_TEXT_BYTES,
            });
        }
        String::from_utf8(bytes)
            .map(Self)
            .map_err(|_| VdiClipboardTextValidationError::InvalidUtf8)
    }

    /// Borrow the validated text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return the deterministic encoded UTF-8 size in bytes.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.0.len()
    }

    /// Whether this value represents a clipboard clear.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for VdiClipboardText {
    type Error = VdiClipboardTextValidationError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<VdiClipboardText> for String {
    fn from(text: VdiClipboardText) -> Self {
        text.0
    }
}

/// A bounded, target-seat clipboard handoff produced only after the daemon has
/// verified the signed VDI action. It is a transient local delivery record, not
/// a second clipboard history or authorization envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardMaterialization {
    /// The exact enrolled seat/hostname that may consume this handoff.
    pub target_seat: String,
    /// The bounded UTF-8 text; empty text is an explicit clear.
    pub text: VdiClipboardText,
    /// The already-authorized producer/session attribution.
    pub source: String,
    /// RFC3339 issuance time used for stale-handoff rejection.
    pub time: String,
}

impl ClipboardMaterialization {
    /// Build a node-local target-seat handoff from validated text.
    #[must_use]
    pub fn new(
        target_seat: impl Into<String>,
        text: VdiClipboardText,
        source: impl Into<String>,
        time: impl Into<String>,
    ) -> Self {
        Self {
            target_seat: target_seat.into(),
            text,
            source: source.into(),
            time: time.into(),
        }
    }

    /// Validate routing and attribution fields at the local handoff boundary.
    pub fn validate(&self) -> Result<(), String> {
        let target = self.target_seat.trim();
        if target.is_empty()
            || target.len() > 128
            || target.bytes().any(|byte| {
                !byte.is_ascii_alphanumeric()
                    && !matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
            })
        {
            return Err("clipboard materialization target_seat is unsafe or empty".to_owned());
        }
        if self.source.trim().is_empty() {
            return Err("clipboard materialization source is empty".to_owned());
        }
        if self.time.trim().is_empty() {
            return Err("clipboard materialization time is empty".to_owned());
        }
        Ok(())
    }
}

/// Why a VDI clipboard text value was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VdiClipboardTextValidationError {
    /// The encoded value exceeds the canonical byte ceiling.
    TooLarge {
        /// The rejected encoded byte length.
        bytes: usize,
        /// The canonical maximum encoded byte length.
        max_bytes: usize,
    },
    /// Raw clipboard bytes were not valid UTF-8.
    InvalidUtf8,
}

impl std::fmt::Display for VdiClipboardTextValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { bytes, max_bytes } => {
                write!(
                    formatter,
                    "clipboard text is {bytes} bytes; maximum is {max_bytes}"
                )
            }
            Self::InvalidUtf8 => formatter.write_str("clipboard text is not valid UTF-8"),
        }
    }
}

impl std::error::Error for VdiClipboardTextValidationError {}

/// RDP's real text clipboard channel is CLIPRDR. The current backend has not wired
/// that virtual channel, so both directions must report unsupported explicitly.
pub const RDP_CLIPBOARD_UNSUPPORTED_REASON: &str =
    "RDP CLIPRDR clipboard channel is not implemented in mde-vdi-rdp";

/// SPICE text clipboard rides the vdagent/main-channel clipboard messages. The
/// current backend has not wired that path, so both directions must report
/// unsupported explicitly.
pub const SPICE_CLIPBOARD_UNSUPPORTED_REASON: &str =
    "SPICE vdagent clipboard channel is not implemented in mde-vdi-spice";

/// The protocol-native channel backing a supported VDI clipboard lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VdiClipboardChannel {
    /// RDP CLIPRDR virtual channel.
    RdpCliprdr,
    /// SPICE vdagent clipboard messages.
    SpiceVdagent,
}

/// One directional clipboard lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VdiClipboardLaneStatus {
    /// The lane is backed by a real protocol clipboard channel.
    Supported {
        /// The protocol channel used for this direction.
        channel: VdiClipboardChannel,
    },
    /// The lane is not available and the reason is operator-visible.
    Unsupported {
        /// Human-readable reason. This must name the missing protocol path.
        reason: String,
    },
}

impl VdiClipboardLaneStatus {
    /// A directional unsupported status.
    #[must_use]
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported {
            reason: reason.into(),
        }
    }

    /// Whether this lane has a real protocol channel behind it.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }
}

/// Bidirectional text clipboard capability for a VDI endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VdiClipboardStatus {
    /// Host/mesh clipboard materialization into the guest.
    pub host_to_guest: VdiClipboardLaneStatus,
    /// Guest clipboard publication back to the host/mesh lane.
    pub guest_to_host: VdiClipboardLaneStatus,
}

impl VdiClipboardStatus {
    /// A bidirectional unsupported report using the same explicit reason for both
    /// lanes.
    #[must_use]
    pub fn unsupported(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            host_to_guest: VdiClipboardLaneStatus::unsupported(reason.clone()),
            guest_to_host: VdiClipboardLaneStatus::unsupported(reason),
        }
    }

    /// Current RDP status: display/input are live, but CLIPRDR clipboard is absent.
    #[must_use]
    pub fn rdp_unsupported() -> Self {
        Self::unsupported(RDP_CLIPBOARD_UNSUPPORTED_REASON)
    }

    /// Current SPICE status: display/input are live, but vdagent clipboard is absent.
    #[must_use]
    pub fn spice_unsupported() -> Self {
        Self::unsupported(SPICE_CLIPBOARD_UNSUPPORTED_REASON)
    }

    /// Whether both directions are backed by real protocol clipboard channels.
    #[must_use]
    pub fn is_bidirectional(&self) -> bool {
        self.host_to_guest.is_supported() && self.guest_to_host.is_supported()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_value_uses_encoded_bytes_for_the_limit() {
        let prefix = "a".repeat(MAX_VDI_CLIPBOARD_TEXT_BYTES - 2);
        let text = VdiClipboardText::new(format!("{prefix}é")).expect("UTF-8 boundary is safe");

        assert_eq!(text.as_str(), format!("{prefix}é"));
        assert_eq!(text.len_bytes(), MAX_VDI_CLIPBOARD_TEXT_BYTES);
        assert!(text.as_str().is_char_boundary(text.len_bytes()));

        let exact = VdiClipboardText::new("é".repeat(MAX_VDI_CLIPBOARD_TEXT_BYTES / 2))
            .expect("exact encoded byte limit");
        assert_eq!(exact.len_bytes(), MAX_VDI_CLIPBOARD_TEXT_BYTES);
    }

    #[test]
    fn text_value_rejects_oversized_strings_without_truncation() {
        let value = "x".repeat(MAX_VDI_CLIPBOARD_TEXT_BYTES + 1);
        assert_eq!(
            VdiClipboardText::new(value),
            Err(VdiClipboardTextValidationError::TooLarge {
                bytes: MAX_VDI_CLIPBOARD_TEXT_BYTES + 1,
                max_bytes: MAX_VDI_CLIPBOARD_TEXT_BYTES,
            })
        );
    }

    #[test]
    fn raw_bytes_require_valid_utf8_before_materialization() {
        assert_eq!(
            VdiClipboardText::from_bytes(vec![b'v', 0xff]),
            Err(VdiClipboardTextValidationError::InvalidUtf8)
        );
    }

    #[test]
    fn serde_round_trip_preserves_empty_and_rejects_oversized_values() {
        let empty = VdiClipboardText::new("").expect("empty clears are valid");
        let body = serde_json::to_string(&empty).expect("serialize text");
        assert_eq!(body, "\"\"");
        assert_eq!(
            serde_json::from_str::<VdiClipboardText>(&body).expect("deserialize text"),
            empty
        );

        let oversized = format!("\"{}\"", "x".repeat(MAX_VDI_CLIPBOARD_TEXT_BYTES + 1));
        assert!(serde_json::from_str::<VdiClipboardText>(&oversized).is_err());
    }

    #[test]
    fn rdp_unsupported_names_cliprdr_in_both_directions() {
        let status = VdiClipboardStatus::rdp_unsupported();
        assert!(!status.is_bidirectional());
        for lane in [&status.host_to_guest, &status.guest_to_host] {
            match lane {
                VdiClipboardLaneStatus::Unsupported { reason } => {
                    assert!(reason.contains("CLIPRDR"));
                    assert!(reason.contains("mde-vdi-rdp"));
                }
                other => panic!("expected unsupported RDP lane, got {other:?}"),
            }
        }
    }

    #[test]
    fn spice_unsupported_names_vdagent_in_both_directions() {
        let status = VdiClipboardStatus::spice_unsupported();
        assert!(!status.is_bidirectional());
        for lane in [&status.host_to_guest, &status.guest_to_host] {
            match lane {
                VdiClipboardLaneStatus::Unsupported { reason } => {
                    assert!(reason.contains("vdagent"));
                    assert!(reason.contains("mde-vdi-spice"));
                }
                other => panic!("expected unsupported SPICE lane, got {other:?}"),
            }
        }
    }

    #[test]
    fn wire_shape_is_stable_and_explicit() {
        let body = serde_json::to_string(&VdiClipboardStatus::rdp_unsupported())
            .expect("serialize status");
        assert!(body.contains(r#""host_to_guest":{"state":"unsupported""#));
        assert!(body.contains(r#""guest_to_host":{"state":"unsupported""#));
        assert!(body.contains("CLIPRDR"));

        let back: VdiClipboardStatus = serde_json::from_str(&body).expect("round-trip");
        assert_eq!(back, VdiClipboardStatus::rdp_unsupported());
    }
}
