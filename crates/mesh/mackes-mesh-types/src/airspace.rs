//! Typed wire contract for the MG90 airspace survey mirror.
//!
//! The desktop Airspace surface is deliberately live-only. This module keeps
//! the daemon boundary equally honest: a node with no proven survey source
//! publishes AirspaceAvailability::NoSource, a failed probe publishes
//! AirspaceAvailability::Offline, and only a typed survey supplied by an
//! injected probe can contribute contacts. This wire contract remains
//! transport-neutral; the production worker is responsible for proving any MG90
//! command or endpoint before injecting a typed survey.

use serde::{Deserialize, Serialize};

/// Topic prefix for the per-node airspace mirror.
pub const AIRSPACE_STATE_PREFIX: &str = "state/airspace/";

/// Maximum number of raw contacts accepted from one survey.
pub const MAX_SURVEY_CONTACTS: usize = 1_024;
/// Maximum number of validated contacts retained in one published snapshot.
pub const MAX_RETAINED_CONTACTS: usize = 256;
/// Maximum number of diagnostic gaps retained in one snapshot.
pub const MAX_GAPS: usize = 32;
/// Maximum UTF-8 bytes retained for a user- or device-supplied string.
pub const MAX_STRING_BYTES: usize = 128;
/// Maximum encoded JSON size of a published snapshot.
pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024;

/// The latest-wins mirror topic for one mesh node.
#[must_use]
pub fn airspace_state_topic(node: &str) -> String {
    format!("{AIRSPACE_STATE_PREFIX}{node}")
}

/// Availability of the underlying scanner source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AirspaceAvailability {
    /// No MG90 survey implementation is configured or proven on this node.
    NoSource,
    /// A configured source was attempted but could not be read.
    Offline,
    /// A survey was read successfully; its contact list may still be empty.
    Ready,
}

/// Compatibility name for consumers that refer to the field as a source
/// status rather than availability.
pub type AirspaceSourceStatus = AirspaceAvailability;

/// Scanner technology that produced a contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AirspaceContactKind {
    /// 802.11 access point or station observation.
    Wifi,
    /// Cellular/base-station observation.
    Cell,
    /// Bluetooth or Bluetooth Low Energy observation.
    Bluetooth,
}

/// One normalized contact from a single MG90 survey result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AirspaceContact {
    /// Stable source identifier (BSSID, cell id, or Bluetooth address).
    pub id: String,
    /// Scanner technology that produced the contact.
    pub kind: AirspaceContactKind,
    /// SSID, carrier, or advertised device name when supplied.
    #[serde(default)]
    pub name: String,
    /// Received signal strength in dBm.
    pub signal_dbm: i32,
    /// Bearing clockwise from the vehicle heading, in degrees.
    pub bearing_deg: f32,
    /// Channel or band number when the source supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    /// Wi-Fi security label when supplied; absent for other technologies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<String>,
    /// Whether the source marked this contact as security-notable.
    #[serde(default)]
    pub notable: bool,
    /// Whether the source matched an operator watchlist.
    #[serde(default)]
    pub watchlist: bool,
    /// Whether the source identified this as the operator's own mesh gear.
    #[serde(default)]
    pub own: bool,
}

impl AirspaceContact {
    /// Validate and bound source-controlled fields before they enter a mirror.
    ///
    /// Invalid numeric values are rejected rather than clamped into a plausible
    /// looking contact. Text is control-character filtered and UTF-8 bounded.
    pub fn bounded(self) -> Result<Self, &'static str> {
        let id = bounded_text(&self.id);
        if id.is_empty() {
            return Err("contact identifier is empty");
        }
        if !(-150..=0).contains(&self.signal_dbm) {
            return Err("contact signal is outside -150..0 dBm");
        }
        if !self.bearing_deg.is_finite() || !(0.0..=360.0).contains(&self.bearing_deg) {
            return Err("contact bearing is not finite or is outside 0..360 degrees");
        }
        let encryption = self
            .encryption
            .as_deref()
            .map(bounded_text)
            .filter(|value| !value.is_empty());
        Ok(Self {
            id,
            kind: self.kind,
            name: bounded_text(&self.name),
            signal_dbm: self.signal_dbm,
            bearing_deg: self.bearing_deg,
            channel: self.channel,
            encryption,
            notable: self.notable,
            watchlist: self.watchlist,
            own: self.own,
        })
    }
}

/// Typed result returned by an injected MG90 survey probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AirspaceSurvey {
    /// Source observation time, when the scanner supplies one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at_ms: Option<i64>,
    /// Raw contacts. The daemon applies MAX_SURVEY_CONTACTS and validation.
    #[serde(default)]
    pub contacts: Vec<AirspaceContact>,
    /// Honest source/parser notes supplied with the survey.
    #[serde(default)]
    pub gaps: Vec<String>,
}

/// Latest-wins state/airspace/<node> snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AirspaceSnapshot {
    /// Adapter node that owns this mirror.
    pub host: String,
    /// Local publication time, Unix milliseconds.
    pub published_at_ms: i64,
    /// Source observation time, when supplied by the scanner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanned_at_ms: Option<i64>,
    /// Honest state of the source plane.
    pub availability: AirspaceAvailability,
    /// Validated contacts from the most recent complete survey.
    #[serde(default)]
    pub contacts: Vec<AirspaceContact>,
    /// Number of source contacts omitted by validation or retention bounds.
    #[serde(default)]
    pub omitted_contacts: u32,
    /// Honest source, parser, or bound notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
}

impl AirspaceSnapshot {
    /// Construct the explicit no-source state. It contains no contacts and no
    /// synthetic scan timestamp.
    #[must_use]
    pub fn no_source(host: &str, published_at_ms: i64) -> Self {
        Self {
            host: bounded_text(host),
            published_at_ms,
            scanned_at_ms: None,
            availability: AirspaceAvailability::NoSource,
            contacts: Vec::new(),
            omitted_contacts: 0,
            gaps: vec!["MG90 airspace survey source is not proven or configured".to_string()],
        }
    }

    /// Construct an explicit offline state after a source read failed.
    #[must_use]
    pub fn offline(host: &str, published_at_ms: i64, reason: impl AsRef<str>) -> Self {
        Self {
            host: bounded_text(host),
            published_at_ms,
            scanned_at_ms: None,
            availability: AirspaceAvailability::Offline,
            contacts: Vec::new(),
            omitted_contacts: 0,
            gaps: vec![bounded_text(reason.as_ref())],
        }
    }

    /// Normalize a successful typed survey into a bounded ready snapshot.
    ///
    /// Contacts that fail validation are omitted and counted; they are never
    /// replaced by generated or guessed records.
    #[must_use]
    pub fn from_survey(host: &str, published_at_ms: i64, survey: AirspaceSurvey) -> Self {
        let mut gaps = bounded_gaps(survey.gaps);
        let mut omitted = 0_u32;
        let source_count = survey.contacts.len();
        if source_count > MAX_SURVEY_CONTACTS {
            omitted = omitted.saturating_add(
                u32::try_from(source_count - MAX_SURVEY_CONTACTS).unwrap_or(u32::MAX),
            );
            push_gap(
                &mut gaps,
                format!("survey contact count capped at {MAX_SURVEY_CONTACTS}; excess omitted"),
            );
        }

        let mut contacts = Vec::with_capacity(MAX_RETAINED_CONTACTS.min(source_count));
        for contact in survey.contacts.into_iter().take(MAX_SURVEY_CONTACTS) {
            let contact = match contact.bounded() {
                Ok(contact) => contact,
                Err(reason) => {
                    omitted = omitted.saturating_add(1);
                    push_gap(&mut gaps, format!("contact omitted: {reason}"));
                    continue;
                }
            };
            if contacts.len() == MAX_RETAINED_CONTACTS {
                omitted = omitted.saturating_add(1);
                continue;
            }
            contacts.push(contact);
        }
        if omitted > 0 && !gaps.iter().any(|gap| gap.contains("retention")) {
            push_gap(
                &mut gaps,
                format!("contact retention capped at {MAX_RETAINED_CONTACTS}"),
            );
        }

        Self {
            host: bounded_text(host),
            published_at_ms,
            scanned_at_ms: survey.scanned_at_ms,
            availability: AirspaceAvailability::Ready,
            contacts,
            omitted_contacts: omitted,
            gaps,
        }
    }

    /// Return the wire body's UTF-8 length, or an encoding error.
    pub fn encoded_len(&self) -> Result<usize, serde_json::Error> {
        serde_json::to_vec(self).map(|body| body.len())
    }
}

fn bounded_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        let next_len = output.len() + character.len_utf8();
        if next_len > MAX_STRING_BYTES {
            break;
        }
        output.push(character);
    }
    output.trim().to_string()
}

fn bounded_gaps(gaps: Vec<String>) -> Vec<String> {
    gaps.into_iter()
        .take(MAX_GAPS)
        .map(|gap| bounded_text(&gap))
        .filter(|gap| !gap.is_empty())
        .collect()
}

fn push_gap(gaps: &mut Vec<String>, gap: String) {
    if gaps.len() < MAX_GAPS {
        let gap = bounded_text(&gap);
        if !gap.is_empty() {
            gaps.push(gap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(id: &str) -> AirspaceContact {
        AirspaceContact {
            id: id.to_string(),
            kind: AirspaceContactKind::Wifi,
            name: "test-net".to_string(),
            signal_dbm: -55,
            bearing_deg: 42.0,
            channel: Some(36),
            encryption: Some("WPA3".to_string()),
            notable: false,
            watchlist: false,
            own: false,
        }
    }

    #[test]
    fn topic_is_the_node_scoped_latest_wins_path() {
        assert_eq!(airspace_state_topic("rig-1"), "state/airspace/rig-1");
    }

    #[test]
    fn survey_is_bounded_without_fabricating_contacts() {
        let mut survey = AirspaceSurvey {
            scanned_at_ms: Some(42),
            contacts: (0..(MAX_SURVEY_CONTACTS + 3))
                .map(|index| contact(&format!("wifi-{index}")))
                .collect(),
            gaps: Vec::new(),
        };
        survey.contacts.push(AirspaceContact {
            id: String::new(),
            ..contact("invalid")
        });
        let snapshot = AirspaceSnapshot::from_survey("rig-1", 43, survey);
        assert_eq!(snapshot.availability, AirspaceAvailability::Ready);
        assert_eq!(snapshot.contacts.len(), MAX_RETAINED_CONTACTS);
        assert!(snapshot.omitted_contacts > 0);
        assert!(snapshot
            .contacts
            .iter()
            .all(|contact| !contact.id.is_empty()));
        assert!(snapshot.encoded_len().expect("encode") <= MAX_SNAPSHOT_BYTES);
    }

    #[test]
    fn snapshots_round_trip_and_no_source_has_no_contacts() {
        let snapshot = AirspaceSnapshot::from_survey(
            "rig-1",
            43,
            AirspaceSurvey {
                scanned_at_ms: Some(42),
                contacts: vec![contact("aa:bb:cc")],
                gaps: vec!["captured fixture".to_string()],
            },
        );
        let body = serde_json::to_string(&snapshot).expect("serialize");
        let decoded: AirspaceSnapshot = serde_json::from_str(&body).expect("deserialize");
        assert_eq!(decoded, snapshot);

        let no_source = AirspaceSnapshot::no_source("rig-1", 44);
        assert_eq!(no_source.availability, AirspaceAvailability::NoSource);
        assert!(no_source.contacts.is_empty());
        assert!(no_source.scanned_at_ms.is_none());
    }

    #[test]
    fn invalid_numeric_contacts_are_rejected_not_clamped() {
        let mut bad = contact("bad");
        bad.signal_dbm = 1;
        assert_eq!(
            bad.clone().bounded(),
            Err("contact signal is outside -150..0 dBm")
        );
        bad.signal_dbm = -40;
        bad.bearing_deg = f32::NAN;
        assert_eq!(
            bad.bounded(),
            Err("contact bearing is not finite or is outside 0..360 degrees")
        );
    }
}
