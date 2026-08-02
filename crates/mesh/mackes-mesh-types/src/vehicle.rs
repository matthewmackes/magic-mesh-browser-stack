//! Provider-neutral **vehicle-gateway** mirror + command contract — the "Rolling Node".
//!
//! A workstation-side adapter (the mackesd `vehicle` worker) SSH/HTTP-polls a mobile
//! gateway (a Sierra AirLink **MG90** / oMG today) and publishes a latest-wins
//! `state/vehicle/<node>` mirror; the shell's maps-location cockpit folds it into its
//! live models (`Mg90Status`/`CellularLink`/`LocationSample`/`VehicleTelemetry`). Config
//!
//! mutations go out as `action/vehicle/<verb>` and resolve via `reply/<ulid>` — the same
//! Bus idiom as the `cloud` mirror. This crate stays pure data + a pure NMEA parser; the
//! worker owns the SSH/HTTP transport.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Topic prefix for the per-node vehicle-gateway mirror.
pub const VEHICLE_STATE_PREFIX: &str = "state/vehicle/";

/// The schema version of the additive, identity-addressed vehicle snapshot.
pub const VEHICLE_STATE_V2_SCHEMA_VERSION: u16 = 2;

/// Maximum number of radio records accepted in a v2 snapshot.
///
/// The six native MG90 interfaces fit below this limit; the remaining slots are
/// for bounded,
/// explicitly named extension radios discovered by a typed probe.
pub const VEHICLE_STATE_V2_MAX_RADIOS: usize = 16;

/// Maximum number of approved management nodes carried by one MG90 snapshot.
///
/// This matches the bounded multi-source worker roster while keeping the wire
/// contract safe for multiple workstation managers.
pub const VEHICLE_STATE_V2_MAX_MANAGERS: usize = 8;

/// The `state/vehicle/<node>` mirror topic for a node.
#[must_use]
pub fn vehicle_state_topic(node: &str) -> String {
    format!("{VEHICLE_STATE_PREFIX}{node}")
}

/// The v2 `state/vehicle/<management-node>/<mg90-id>` mirror topic.
#[must_use]
pub fn vehicle_state_v2_topic(management_node_id: &str, mg90_id: &str) -> String {
    format!("{VEHICLE_STATE_PREFIX}{management_node_id}/{mg90_id}")
}

/// Command prefix for gateway mutations (`action/vehicle/<verb>`).
pub const VEHICLE_ACTION_PREFIX: &str = "action/vehicle/";

/// The `action/vehicle/<verb>` request topic for a verb.
#[must_use]
pub fn vehicle_action_topic(verb: &str) -> String {
    format!("{VEHICLE_ACTION_PREFIX}{verb}")
}

/// A GNSS fix parsed from the gateway's NMEA (oMG `omgtime.g.info` `$GPGGA`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GpsFix {
    /// Human fix label: `no-fix` | `gps` | `dgps`.
    pub fix_type: String,
    /// Latitude, decimal degrees (N positive).
    pub latitude: f64,
    /// Longitude, decimal degrees (E positive).
    pub longitude: f64,
    /// Altitude, meters MSL.
    pub altitude_m: f32,
    /// Horizontal dilution of precision (lower is better; 99 = no fix).
    pub hdop: f32,
    /// Satellites used in the fix.
    pub satellites: u8,
    /// Ground speed, mph (from RMC/VTG when available; 0 from GGA alone).
    pub speed_mph: f32,
    /// Heading, degrees true (0 from GGA alone).
    pub heading_deg: f32,
    /// Age of this fix, seconds.
    pub age_s: f32,
    /// Observed update rate, Hz.
    pub update_rate_hz: f32,
}

impl GpsFix {
    /// Whether the gateway currently holds a position lock.
    #[must_use]
    pub fn has_fix(&self) -> bool {
        self.fix_type != "no-fix" && self.satellites > 0
    }
}

/// A 6-axis inertial sample from the gateway's built-in IMU (oMG `$PSIWMMPU`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ImuSample {
    /// Acceleration X/Y/Z (g).
    pub accel_g: [f32; 3],
    /// Angular rate X/Y/Z (deg/s).
    pub gyro_dps: [f32; 3],
}

/// One cellular link's live status (mirrors the cockpit `CellularLink`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CellLink {
    /// SIM state (`ready` / `standby` / `absent` / …).
    pub sim_state: String,
    /// Carrier / operator name.
    pub carrier: String,
    /// Received signal strength, dBm (negative; e.g. -72).
    pub signal_dbm: i32,
    /// Radio access technology (`5G/LTE-A` / `LTE` / …).
    pub technology: String,
    /// Assigned WAN IP, or `not active`.
    pub wan_ip: String,
    /// Link health per the gateway.
    pub healthy: bool,
}

/// The gateway's multi-WAN uplink status (mirrors the cockpit `Mg90Status`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WanStatus {
    /// The currently-active WAN label (e.g. `Cellular A`).
    pub active_wan: String,
    /// Cellular modem A.
    pub cellular_a: CellLink,
    /// Cellular modem B.
    pub cellular_b: CellLink,
    /// Wi-Fi-as-WAN / AP state label.
    pub wifi_state: String,
    /// Ethernet WAN state label.
    pub ethernet_state: String,
    /// VPN state label.
    pub vpn_state: String,
    /// Failover events observed this session.
    pub failover_events: u32,
    /// Uplink latency, ms.
    pub latency_ms: u32,
    /// Uplink packet loss, percent.
    pub packet_loss_percent: f32,
    /// Overall link-quality label.
    pub link_quality: String,
}

impl WanStatus {
    /// The active cellular link, when the active WAN is cellular.
    #[must_use]
    pub fn active_cellular(&self) -> Option<&CellLink> {
        match self.active_wan.as_str() {
            "Cellular A" => Some(&self.cellular_a),
            "Cellular B" => Some(&self.cellular_b),
            _ => None,
        }
    }
}

/// The typed result of an optional vehicle-device probe.
///
/// This is deliberately separate from [`RadioPresence`]: a probe can be absent
/// without proving that the device itself is absent, and a reachable endpoint
/// can still be unsupported when its payload schema is not admitted. The
/// worker uses these states instead of turning a missing or failed probe into
/// zero-filled telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeviceProbeStatus {
    /// A typed, verified device payload is available.
    Supported,
    /// No device probe is installed or configured for this adapter.
    NotInstalled,
    /// The endpoint answered, but this adapter does not support its payload or
    /// protocol well enough to expose typed values.
    Unsupported {
        /// Safe operator-facing explanation; never raw device payload.
        reason: String,
    },
    /// The configured probe was attempted but failed before producing a typed
    /// observation.
    Failed {
        /// Safe transport/configuration failure detail.
        reason: String,
    },
    /// Legacy snapshots did not carry a probe verdict.
    #[default]
    Unknown,
}

impl DeviceProbeStatus {
    /// Whether this status is the legacy no-verdict value.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// Whether this status authorizes the OBD fields as typed observations.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Vehicle power + OBD/CAN telemetry (mirrors the cockpit `VehicleTelemetry`, plus the
/// MCU-sourced board temp). Power fields (`battery_v`/`internal_temp_c`/`ignition_on`)
/// come from the gateway MCU; the rest from OBD-II when `obd_present` and the
/// [`DeviceProbeStatus`] is [`DeviceProbeStatus::Supported`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VehicleTelem {
    /// Main/charging bus voltage, volts (MCU).
    pub battery_v: f32,
    /// Gateway internal board temperature, °C (MCU).
    pub internal_temp_c: f32,
    /// Ignition-sense line state (MCU, `IGNTHRESH`).
    pub ignition_on: bool,
    /// Motion state (from GNSS speed or IMU).
    pub moving: bool,
    /// Whether an OBD-II source is present (the fields below are meaningful).
    pub obd_present: bool,
    /// Typed verdict for the optional OBD/HDOBD/device probe. This is kept
    /// alongside the legacy boolean so old readers remain wire-compatible.
    #[serde(default, skip_serializing_if = "DeviceProbeStatus::is_unknown")]
    pub obd_probe_status: DeviceProbeStatus,
    /// Vehicle speed, mph (OBD).
    pub speed_mph: f32,
    /// Engine RPM (OBD).
    pub rpm: u32,
    /// Coolant temperature, °C (OBD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coolant_c: Option<f32>,
    /// Fuel level, percent (OBD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fuel_percent: Option<f32>,
    /// Diagnostic trouble code count (OBD).
    pub dtc_count: u32,
    /// Odometer, miles (OBD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi: Option<u32>,
    /// Engine runtime, minutes (OBD).
    pub runtime_min: u32,
}

/// The per-node `state/vehicle/<node>` mirror — one gateway's live snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VehicleState {
    /// This node's id (the mirror `host` stamp + topic namespace).
    pub host: String,
    /// Gateway model (e.g. `MG90`).
    pub model: String,
    /// Gateway electronic serial number.
    pub esn: String,
    /// Gateway firmware version (e.g. `4.3.0.1`).
    pub mgos_version: String,
    /// Whether the adapter currently reaches the gateway.
    pub online: bool,
    /// Latest GNSS fix.
    pub gps: GpsFix,
    /// Latest IMU sample, when the gateway exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imu: Option<ImuSample>,
    /// Multi-WAN uplink status.
    pub wan: WanStatus,
    /// Vehicle power + OBD telemetry.
    pub telem: VehicleTelem,
    /// What this adapter could NOT report (honest-partial note; empty when full).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
    /// Wall-clock publish time (ms since the Unix epoch).
    pub published_at_ms: i64,
}

impl VehicleState {
    /// An honest offline snapshot for a node whose gateway is unreachable.
    #[must_use]
    pub fn offline(host: &str) -> Self {
        Self {
            host: host.to_string(),
            model: String::new(),
            esn: String::new(),
            mgos_version: String::new(),
            online: false,
            gps: GpsFix::default(),
            imu: None,
            wan: WanStatus::default(),
            telem: VehicleTelem::default(),
            gaps: vec!["gateway unreachable".to_string()],
            published_at_ms: 0,
        }
    }
}

/// Stable identifiers for the six native MG90 radio/GNSS interfaces.
///
/// Extensions are serialized as bounded strings beginning with `ext-`; this
/// keeps future/vendor interfaces forward-readable without allowing an
/// unbounded inventory or silently treating an unknown value as a native radio.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RadioId {
    /// Cellular modem A.
    CellularA,
    /// Cellular modem B.
    CellularB,
    /// Wi-Fi interface A.
    WifiA,
    /// Wi-Fi interface B.
    WifiB,
    /// Bluetooth interface.
    Bluetooth,
    /// GNSS receiver.
    Gnss,
    /// A bounded, explicitly named vendor/extension interface.
    Extension(String),
}

impl RadioId {
    /// Build an extension identifier only when it is in the bounded extension
    /// namespace. Native identifiers are never accepted as extensions.
    #[must_use]
    pub fn extension(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let bytes = value.as_bytes();
        if !(5..=32).contains(&bytes.len())
            || !value.starts_with("ext-")
            || bytes[4..]
                .iter()
                .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-'))
        {
            return None;
        }
        Some(Self::Extension(value))
    }

    /// The stable wire spelling of this identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::CellularA => "cellular-a",
            Self::CellularB => "cellular-b",
            Self::WifiA => "wifi-a",
            Self::WifiB => "wifi-b",
            Self::Bluetooth => "bluetooth",
            Self::Gnss => "gnss",
            Self::Extension(value) => value,
        }
    }
}

impl Serialize for RadioId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RadioId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "cellular-a" => Ok(Self::CellularA),
            "cellular-b" => Ok(Self::CellularB),
            "wifi-a" => Ok(Self::WifiA),
            "wifi-b" => Ok(Self::WifiB),
            "bluetooth" => Ok(Self::Bluetooth),
            "gnss" => Ok(Self::Gnss),
            _ => Self::extension(value).ok_or_else(|| {
                serde::de::Error::custom("radio id is not a native id or bounded ext-* id")
            }),
        }
    }
}

/// Whether a radio is known to be fitted to the gateway. `Unknown` is
/// intentionally distinct from `NotInstalled`: absence in a v1 snapshot does
/// not prove that hardware is absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum RadioPresence {
    /// A typed probe reported the interface as fitted.
    Installed,
    /// A typed probe proved the interface is not fitted.
    NotInstalled,
    /// The source did not prove either condition.
    #[default]
    Unknown,
}

/// Operational state for one radio. `Stale` is a consumer-visible state and is
/// not used to turn a missing source into synthetic telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum RadioOperation {
    /// The interface is the selected active path.
    Active,
    /// The interface is fitted and available as a backup/standby path.
    Standby,
    /// The interface is fitted and currently searching/acquiring.
    Acquiring,
    /// The interface reported a degraded condition.
    Degraded,
    /// The interface reported a fault.
    Fault,
    /// The interface is explicitly disabled.
    Disabled,
    /// No operation state was reported.
    #[default]
    Unknown,
    /// A consumer has retained the record past its freshness budget.
    Stale,
}

/// The configured role of a radio, kept separate from signal/health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum RadioRole {
    /// Cellular or Wi-Fi wide-area uplink.
    Wan,
    /// Wi-Fi access-point service.
    AccessPoint,
    /// Wi-Fi mesh/backhaul service.
    Backhaul,
    /// Bluetooth service.
    Bluetooth,
    /// GNSS receiver.
    Gnss,
    /// The source did not report a configured role.
    #[default]
    Unknown,
}

/// Stable reason codes for an honest radio-health row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum RadioReasonCode {
    /// The receiver is installed but has no fix yet.
    NoFix,
    /// The source did not report the field.
    NotReported,
    /// The gateway disabled the interface.
    DisabledByGateway,
    /// The reported signal is outside the worker's healthy threshold.
    WeakSignal,
    /// The gateway was unreachable when the snapshot was folded.
    GatewayOffline,
    /// A typed probe proved the interface is not fitted.
    NotInstalled,
    /// No more specific reason is known.
    #[default]
    Unknown,
}

/// Typed metrics reported by a cellular interface. Optional fields mean
/// "not reported", never a zero-filled measurement.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CellularRadioMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub technology: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrp_dbm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rsrq_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sinr_db: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// The Wi-Fi role as reported by a gateway inventory probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum WifiRole {
    /// Wi-Fi is used as a WAN source.
    Wan,
    /// Wi-Fi is serving local clients.
    AccessPoint,
    /// Wi-Fi is carrying a backhaul.
    Backhaul,
}

/// Typed metrics reported by a Wi-Fi interface.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WifiRadioMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<WifiRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub associated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub band: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backhaul: Option<String>,
}

/// Typed metrics reported by a Bluetooth inventory probe.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BluetoothRadioMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub powered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discoverable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_devices: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connected_devices: Option<u32>,
}

/// Typed metrics reported by a GNSS receiver.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GnssRadioMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub satellites: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdop: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_m: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_reckoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_rate_hz: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
}

/// Per-radio metrics are tagged so consumers do not infer a cellular field for
/// Wi-Fi/Bluetooth/GNSS or vice versa.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
pub enum RadioMetrics {
    /// Cellular metrics.
    Cellular(CellularRadioMetrics),
    /// Wi-Fi metrics.
    Wifi(WifiRadioMetrics),
    /// Bluetooth metrics.
    Bluetooth(BluetoothRadioMetrics),
    /// GNSS metrics.
    Gnss(GnssRadioMetrics),
    /// No typed metrics were reported.
    #[default]
    Unknown,
}

/// One bounded, freshness-aware radio inventory row.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadioHealth {
    /// Stable native or bounded extension identifier.
    pub id: RadioId,
    /// Whether hardware presence was proven.
    pub presence: RadioPresence,
    /// Current operation, independent from signal strength.
    pub operation: RadioOperation,
    /// Why the row is not active/complete, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<RadioReasonCode>,
    /// Age of the row's source observation, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// Configured role, not inferred from active-path selection.
    pub configured_role: RadioRole,
    /// Whether this interface currently carries the selected uplink.
    pub active_path: bool,
    /// Interface-specific typed metrics.
    pub metrics: RadioMetrics,
}

/// A bounded radio inventory. Deserialization rejects over-capacity payloads
/// instead of truncating or accepting an unbounded remote list.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RadioInventory(Vec<RadioHealth>);

impl RadioInventory {
    /// Construct a bounded inventory.
    ///
    /// # Errors
    ///
    /// Returns an error when the inventory exceeds
    /// [`VEHICLE_STATE_V2_MAX_RADIOS`].
    pub fn new(entries: Vec<RadioHealth>) -> Result<Self, String> {
        if entries.len() > VEHICLE_STATE_V2_MAX_RADIOS {
            return Err(format!(
                "radio inventory has {} entries; maximum is {}",
                entries.len(),
                VEHICLE_STATE_V2_MAX_RADIOS
            ));
        }
        let mut seen = HashSet::with_capacity(entries.len());
        for entry in &entries {
            if !seen.insert(entry.id.clone()) {
                return Err(format!("radio inventory repeats id {}", entry.id.as_str()));
            }
        }
        Ok(Self(entries))
    }

    /// Borrow the inventory in wire order. Native rows remain in stable order.
    #[must_use]
    pub fn as_slice(&self) -> &[RadioHealth] {
        &self.0
    }

    /// Number of rows in the inventory.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the inventory is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Return the six native rows in their stable UI/wire positions.
    ///
    /// A missing row remains `None`: consumers must render an honest unknown
    /// or unavailable state rather than treating a sparse remote inventory as
    /// proof that hardware is absent. Extension rows are intentionally not
    /// included in this fixed native layout.
    #[must_use]
    pub fn native_slots(&self) -> [Option<&RadioHealth>; 6] {
        [
            self.by_id(&RadioId::CellularA),
            self.by_id(&RadioId::CellularB),
            self.by_id(&RadioId::WifiA),
            self.by_id(&RadioId::WifiB),
            self.by_id(&RadioId::Bluetooth),
            self.by_id(&RadioId::Gnss),
        ]
    }

    /// Find one row by its stable identity without inferring presence.
    #[must_use]
    pub fn by_id(&self, id: &RadioId) -> Option<&RadioHealth> {
        self.0.iter().find(|entry| &entry.id == id)
    }
}

impl Serialize for RadioInventory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RadioInventory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<RadioHealth>::deserialize(deserializer)?;
        Self::new(entries).map_err(serde::de::Error::custom)
    }
}

/// Snapshot source/relay provenance. A direct worker has no relay; a relay is
/// represented explicitly instead of being silently folded into source id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum SnapshotSource {
    /// Read by the management node from the attached gateway.
    DirectGateway,
    /// Relayed by another mesh node.
    MeshRelay,
    /// Source was not present in the v1 payload.
    #[default]
    Unknown,
}

/// Provenance attached to every v2 snapshot.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SnapshotProvenance {
    /// Transport/source class.
    pub source: SnapshotSource,
    /// Source node or gateway id, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Relay node, when this is not a direct snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<String>,
}

/// Approval state for management of a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum ApprovalState {
    Approved,
    Pending,
    Revoked,
    #[default]
    Unknown,
}

/// Sharing policy visible to remote renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum ShareState {
    Private,
    Shared,
    ReadOnly,
    #[default]
    Unknown,
}

/// Whether the manager list is authoritative. v1 has no manager set, so it
/// converts to `Unknown` rather than an empty-but-authoritative set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum ManagerSetState {
    Complete,
    #[default]
    Unknown,
}

/// Validation failures for an approved MG90 manager set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerSetValidationError {
    /// A manager identifier contained no non-whitespace characters.
    BlankId {
        /// Position of the blank identifier in the input list.
        index: usize,
    },
    /// The same manager identifier appeared more than once.
    DuplicateId(String),
    /// The manager set exceeded [`VEHICLE_STATE_V2_MAX_MANAGERS`].
    Capacity {
        /// Number of identifiers supplied.
        len: usize,
        /// Maximum permitted number of identifiers.
        max: usize,
    },
}

impl std::fmt::Display for ManagerSetValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlankId { index } => write!(f, "manager id at index {index} is blank"),
            Self::DuplicateId(id) => write!(f, "manager id {id:?} is duplicated"),
            Self::Capacity { len, max } => {
                write!(f, "manager set has {len} ids; maximum is {max}")
            }
        }
    }
}

/// Authorized managers of a gateway.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagerSet {
    pub state: ManagerSetState,
    pub ids: Vec<String>,
}

impl ManagerSet {
    /// Construct an authoritative, ordered set of approved manager IDs.
    ///
    /// IDs are retained exactly as supplied and in stable order. Blank IDs,
    /// duplicates, and sets larger than [`VEHICLE_STATE_V2_MAX_MANAGERS`] are
    /// rejected before they can enter the wire model.
    pub fn approved(ids: Vec<String>) -> Result<Self, ManagerSetValidationError> {
        Self::validate_ids(&ids)?;
        Ok(Self {
            state: ManagerSetState::Complete,
            ids,
        })
    }

    /// Construct an authoritative manager set; equivalent to [`Self::approved`].
    pub fn new(ids: Vec<String>) -> Result<Self, ManagerSetValidationError> {
        Self::approved(ids)
    }

    fn validate_ids(ids: &[String]) -> Result<(), ManagerSetValidationError> {
        if ids.len() > VEHICLE_STATE_V2_MAX_MANAGERS {
            return Err(ManagerSetValidationError::Capacity {
                len: ids.len(),
                max: VEHICLE_STATE_V2_MAX_MANAGERS,
            });
        }
        for (index, id) in ids.iter().enumerate() {
            if id.trim().is_empty() {
                return Err(ManagerSetValidationError::BlankId { index });
            }
            if ids[..index].iter().any(|previous| previous == id) {
                return Err(ManagerSetValidationError::DuplicateId(id.clone()));
            }
        }
        Ok(())
    }
}

impl Serialize for ManagerSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        struct WireManagerSet<'a> {
            state: &'a ManagerSetState,
            ids: &'a [String],
        }

        WireManagerSet {
            state: &self.state,
            ids: &self.ids,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ManagerSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct WireManagerSet {
            state: ManagerSetState,
            #[serde(default)]
            ids: Vec<String>,
        }

        let wire = WireManagerSet::deserialize(deserializer)?;
        Self::validate_ids(&wire.ids).map_err(serde::de::Error::custom)?;
        Ok(Self {
            state: wire.state,
            ids: wire.ids,
        })
    }
}

/// Freshness of one logical vehicle domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
#[allow(missing_docs, reason = "v2 wire contract")]
pub enum FreshnessState {
    Fresh,
    Stale,
    #[default]
    Unknown,
}

/// Freshness plus an honest reason when the source is incomplete.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DomainFreshness {
    pub state: FreshnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Stable per-domain freshness slots used by Car and Construct renderers.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VehicleDomainFreshness {
    pub identity: DomainFreshness,
    pub radios: DomainFreshness,
    pub gnss: DomainFreshness,
    pub vehicle: DomainFreshness,
    pub power: DomainFreshness,
}

/// MG90 identity carried by the v2 snapshot. The alias remains optional until
/// an approved inventory/config source reports one.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Mg90Identity {
    /// Topic identity; the worker uses a confirmed ESN here.
    pub id: String,
    pub esn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub model: String,
    pub firmware: String,
}

/// Additive, identity-addressed `VehicleState` v2 snapshot.
///
/// The existing [`VehicleState`] remains the v1 compatibility mirror for one
/// rolling upgrade release. New readers can accept [`VehicleStateEnvelope`]
/// and call [`VehicleStateV2::from_v1`] to map all v2-only fields to explicit
/// `Unknown` values instead of guessing.
#[allow(missing_docs, reason = "v2 wire contract")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VehicleStateV2 {
    pub schema_version: u16,
    pub sequence: u64,
    pub observed_at_ms: i64,
    pub published_at_ms: i64,
    pub expected_interval_ms: u64,
    pub management_node_id: String,
    pub mg90: Mg90Identity,
    pub approval: ApprovalState,
    pub sharing: ShareState,
    pub managers: ManagerSet,
    pub provenance: SnapshotProvenance,
    pub online: bool,
    pub freshness: VehicleDomainFreshness,
    pub radios: RadioInventory,
    pub gps: GpsFix,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imu: Option<ImuSample>,
    pub wan: WanStatus,
    pub telem: VehicleTelem,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
}

impl VehicleStateV2 {
    /// Convert the current v1 mirror without inventing v2-only authority.
    #[must_use]
    pub fn from_v1(
        legacy: &VehicleState,
        management_node_id: impl Into<String>,
        sequence: u64,
        expected_interval_ms: u64,
        published_at_ms: i64,
        provenance: SnapshotProvenance,
    ) -> Self {
        let management_node_id = management_node_id.into();
        let mg90_id = if legacy.esn.trim().is_empty() {
            String::new()
        } else {
            legacy.esn.clone()
        };
        let radios = radio_inventory_from_v1(legacy);
        let freshness = freshness_from_v1(legacy, published_at_ms);
        Self {
            schema_version: VEHICLE_STATE_V2_SCHEMA_VERSION,
            sequence,
            observed_at_ms: legacy.published_at_ms.max(0),
            published_at_ms,
            expected_interval_ms,
            management_node_id,
            mg90: Mg90Identity {
                id: mg90_id,
                esn: legacy.esn.clone(),
                alias: None,
                model: legacy.model.clone(),
                firmware: legacy.mgos_version.clone(),
            },
            approval: ApprovalState::Unknown,
            sharing: ShareState::Unknown,
            managers: ManagerSet::default(),
            provenance,
            online: legacy.online,
            freshness,
            radios,
            gps: legacy.gps.clone(),
            imu: legacy.imu.clone(),
            wan: legacy.wan.clone(),
            telem: legacy.telem.clone(),
            gaps: legacy.gaps.clone(),
        }
    }
}

/// One-release reader for both the old unversioned mirror and the new v2 snapshot.
///
/// Ordering matters: v2 is attempted first, then the compatible v1 shape.
/// Callers can migrate from v1 without weakening the v2 contract.
#[allow(missing_docs, reason = "v2 wire contract")]
#[allow(
    clippy::large_enum_variant,
    reason = "the one-release reader keeps both typed snapshots by value"
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VehicleStateEnvelope {
    /// Versioned v2 snapshot.
    V2(VehicleStateV2),
    /// Legacy v1 snapshot accepted during the rolling upgrade.
    V1(VehicleState),
}

impl VehicleStateEnvelope {
    /// Convert either accepted wire shape to v2, marking v1-only omissions as
    /// unknown and retaining the legacy readings verbatim.
    #[must_use]
    pub fn into_v2(
        self,
        management_node_id: impl Into<String>,
        sequence: u64,
        expected_interval_ms: u64,
        published_at_ms: i64,
    ) -> VehicleStateV2 {
        match self {
            Self::V2(snapshot) => snapshot,
            Self::V1(legacy) => VehicleStateV2::from_v1(
                &legacy,
                management_node_id,
                sequence,
                expected_interval_ms,
                published_at_ms,
                SnapshotProvenance::default(),
            ),
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the six stable native radio rows are one compatibility mapping"
)]
fn radio_inventory_from_v1(legacy: &VehicleState) -> RadioInventory {
    let active = |id: &str| legacy.online && normalize_wan_label(&legacy.wan.active_wan) == id;
    let online_or = |known: bool, operation: RadioOperation| {
        if !legacy.online {
            (
                RadioOperation::Unknown,
                Some(RadioReasonCode::GatewayOffline),
            )
        } else if known {
            (operation, None)
        } else {
            (RadioOperation::Unknown, Some(RadioReasonCode::NotReported))
        }
    };

    let cellular = |id: RadioId, link: &CellLink, is_active: bool| {
        let known = cell_link_reported(link);
        let operation = if is_active {
            if link.healthy {
                RadioOperation::Active
            } else {
                RadioOperation::Degraded
            }
        } else if link.healthy {
            RadioOperation::Standby
        } else {
            RadioOperation::Unknown
        };
        let (operation, reason_code) = online_or(known, operation);
        let reason_code = reason_code.or_else(|| {
            (!link.healthy && link.signal_dbm != 0).then_some(RadioReasonCode::WeakSignal)
        });
        RadioHealth {
            id,
            presence: if known {
                RadioPresence::Installed
            } else {
                RadioPresence::Unknown
            },
            operation,
            reason_code,
            age_ms: (legacy.online && known).then_some(0),
            configured_role: RadioRole::Wan,
            active_path: is_active,
            metrics: RadioMetrics::Cellular(cellular_metrics(link)),
        }
    };

    let wifi_known = !legacy.wan.wifi_state.trim().is_empty();
    let wifi_operation = match legacy.wan.wifi_state.to_ascii_lowercase().as_str() {
        "disabled" => RadioOperation::Disabled,
        "active" => RadioOperation::Active,
        "standby" => RadioOperation::Standby,
        _ => RadioOperation::Unknown,
    };
    let (wifi_operation, wifi_reason) = if !legacy.online {
        (
            RadioOperation::Unknown,
            Some(RadioReasonCode::GatewayOffline),
        )
    } else if wifi_known && wifi_operation == RadioOperation::Disabled {
        (wifi_operation, Some(RadioReasonCode::DisabledByGateway))
    } else if wifi_known {
        (wifi_operation, None)
    } else {
        (RadioOperation::Unknown, Some(RadioReasonCode::NotReported))
    };
    let wifi = |id: RadioId, known: bool, operation: RadioOperation, reason_code| RadioHealth {
        id,
        presence: if known {
            RadioPresence::Installed
        } else {
            RadioPresence::Unknown
        },
        operation,
        reason_code,
        age_ms: (legacy.online && known).then_some(0),
        configured_role: RadioRole::Unknown,
        active_path: active("wifi"),
        metrics: RadioMetrics::Wifi(WifiRadioMetrics::default()),
    };

    let gnss_known = !legacy.gps.fix_type.trim().is_empty();
    let (gnss_operation, gnss_reason) = if !legacy.online {
        (
            RadioOperation::Unknown,
            Some(RadioReasonCode::GatewayOffline),
        )
    } else if !gnss_known {
        (RadioOperation::Unknown, Some(RadioReasonCode::NotReported))
    } else if legacy.gps.has_fix() {
        (RadioOperation::Active, None)
    } else {
        (RadioOperation::Acquiring, Some(RadioReasonCode::NoFix))
    };

    let entries = vec![
        cellular(
            RadioId::CellularA,
            &legacy.wan.cellular_a,
            active("cellulara"),
        ),
        cellular(
            RadioId::CellularB,
            &legacy.wan.cellular_b,
            active("cellularb"),
        ),
        wifi(RadioId::WifiA, wifi_known, wifi_operation, wifi_reason),
        wifi(
            RadioId::WifiB,
            false,
            RadioOperation::Unknown,
            Some(if legacy.online {
                RadioReasonCode::NotReported
            } else {
                RadioReasonCode::GatewayOffline
            }),
        ),
        RadioHealth {
            id: RadioId::Bluetooth,
            presence: RadioPresence::Unknown,
            operation: RadioOperation::Unknown,
            reason_code: Some(if legacy.online {
                RadioReasonCode::NotReported
            } else {
                RadioReasonCode::GatewayOffline
            }),
            age_ms: None,
            configured_role: RadioRole::Bluetooth,
            active_path: false,
            metrics: RadioMetrics::Bluetooth(BluetoothRadioMetrics::default()),
        },
        RadioHealth {
            id: RadioId::Gnss,
            presence: if gnss_known {
                RadioPresence::Installed
            } else {
                RadioPresence::Unknown
            },
            operation: gnss_operation,
            reason_code: gnss_reason,
            age_ms: (legacy.online && gnss_known).then_some(0),
            configured_role: RadioRole::Gnss,
            active_path: false,
            metrics: RadioMetrics::Gnss(GnssRadioMetrics {
                fix: gnss_known.then_some(legacy.gps.has_fix()),
                satellites: gnss_known.then_some(legacy.gps.satellites),
                hdop: gnss_known.then_some(legacy.gps.hdop),
                accuracy_m: None,
                dead_reckoning: None,
                update_rate_hz: (legacy.gps.update_rate_hz > 0.0)
                    .then_some(legacy.gps.update_rate_hz),
                age_ms: age_ms_from_seconds(legacy.gps.age_s),
            }),
        },
    ];
    RadioInventory::new(entries).expect("native radio inventory is bounded")
}

fn age_ms_from_seconds(age_s: f32) -> Option<u64> {
    if age_s <= 0.0 || !age_s.is_finite() {
        return None;
    }
    std::time::Duration::try_from_secs_f32(age_s)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn cell_link_reported(link: &CellLink) -> bool {
    !link.sim_state.is_empty()
        || !link.carrier.is_empty()
        || link.signal_dbm != 0
        || !link.technology.is_empty()
        || (!link.wan_ip.is_empty() && link.wan_ip != "not active")
}

fn cellular_metrics(link: &CellLink) -> CellularRadioMetrics {
    CellularRadioMetrics {
        sim_state: (!link.sim_state.is_empty()).then(|| link.sim_state.clone()),
        registration: None,
        carrier: (!link.carrier.is_empty()).then(|| link.carrier.clone()),
        technology: (!link.technology.is_empty()).then(|| link.technology.clone()),
        rssi_dbm: (link.signal_dbm != 0).then_some(link.signal_dbm),
        rsrp_dbm: None,
        rsrq_db: None,
        sinr_db: None,
        address: (!link.wan_ip.is_empty() && link.wan_ip != "not active")
            .then(|| link.wan_ip.clone()),
    }
}

fn normalize_wan_label(label: &str) -> String {
    label
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace() && *ch != '-')
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn freshness_from_v1(legacy: &VehicleState, _published_at_ms: i64) -> VehicleDomainFreshness {
    let observation_age_ms = || {
        let observed = u64::try_from(legacy.published_at_ms).ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis();
        Some(
            now.saturating_sub(u128::from(observed))
                .min(u128::from(u64::MAX)) as u64,
        )
    };
    let unknown = |reason: &str| DomainFreshness {
        state: FreshnessState::Unknown,
        age_ms: None,
        reason: Some(reason.to_string()),
    };
    let fresh = || DomainFreshness {
        state: if legacy.online {
            FreshnessState::Fresh
        } else {
            FreshnessState::Unknown
        },
        age_ms: legacy.online.then(observation_age_ms).flatten(),
        reason: (!legacy.online).then(|| "gateway-offline".to_string()),
    };
    let has_gap = |needle: &str| legacy.gaps.iter().any(|gap| gap.contains(needle));
    VehicleDomainFreshness {
        identity: if legacy.model.is_empty() || legacy.esn.is_empty() {
            unknown("identity-not-reported")
        } else {
            fresh()
        },
        radios: if has_gap("wan status unavailable") {
            unknown("wan-not-reported")
        } else {
            fresh()
        },
        gnss: if has_gap("gps/imu unavailable") || legacy.gps.fix_type.is_empty() {
            unknown("gnss-not-reported")
        } else {
            fresh()
        },
        vehicle: if has_gap("OBD") {
            unknown("vehicle-obd-not-reported")
        } else {
            fresh()
        },
        power: if has_gap("telem.battery_v") || has_gap("telem.internal_temp_c") {
            unknown("power-not-reported")
        } else {
            fresh()
        },
    }
}

/// The typed reply published to `reply/<ulid>` for an `action/vehicle/*` verb.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VehicleReply {
    /// `true` when the verb was applied.
    pub ok: bool,
    /// The verb this reply answers.
    #[serde(default)]
    pub verb: String,
    /// An honest gate reason (nothing performed; retry later).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<String>,
    /// A rejection or backend failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// A summary of what was applied (e.g. the committed config file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<String>,
    /// Whether a destructive op (reset/reboot/failover) was performed + audited.
    #[serde(default)]
    pub audited: bool,
}

/// Parse one NMEA `$GPGGA` sentence into a [`GpsFix`] (position/altitude/sats/HDOP).
///
/// GGA carries no speed/heading (those come from RMC/VTG) — those stay `0.0`. Returns
/// `None` when the line is not a well-formed GGA sentence. Coordinates are `ddmm.mmmm`
/// / `dddmm.mmmm` with a hemisphere field, converted to signed decimal degrees.
#[must_use]
pub fn parse_gpgga(line: &str) -> Option<GpsFix> {
    let line = line.trim();
    // Accept "$GPGGA,..." / "$GNGGA,..." (strip any leading transport noise before
    // the sentence). When a checksum is present, verify it rather than folding a
    // truncated or corrupted sentence into the mirror.
    let sentence_start = line.find('$')?;
    let sentence = &line[sentence_start..];
    let start = sentence.find("GGA,")?;
    let body_with_checksum = &sentence[start + 4..];
    let body = body_with_checksum
        .split('*')
        .next()
        .unwrap_or(body_with_checksum);
    if let Some(checksum) = body_with_checksum.split_once('*').map(|(_, value)| value) {
        let checksum = checksum.trim();
        if checksum.len() != 2 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let expected = u8::from_str_radix(checksum, 16).ok()?;
        let actual = sentence.as_bytes()[1..sentence.find('*')?]
            .iter()
            .fold(0_u8, |sum, byte| sum ^ byte);
        if actual != expected {
            return None;
        }
    }
    let f: Vec<&str> = body.split(',').collect();
    // Fields after "GGA,": 0=time,1=lat,2=N/S,3=lon,4=E/W,5=quality,6=numSats,7=HDOP,8=alt
    if f.len() < 9 {
        return None;
    }
    let lat = nmea_coord(f.get(1)?, f.get(2)?);
    let lon = nmea_coord(f.get(3)?, f.get(4)?);
    let quality: u8 = f.get(5)?.trim().parse().ok()?;
    // NMEA 0183 GGA quality values are 0..=8. Keep the existing neutral mapping
    // below, but reject arbitrary values instead of silently calling them GPS.
    if quality > 8 {
        return None;
    }
    let satellites: u8 = f.get(6)?.trim().parse().ok()?;
    if satellites > 99 {
        return None;
    }
    let hdop: f32 = f.get(7)?.trim().parse().ok()?;
    if !hdop.is_finite() || !(0.0..=99.9).contains(&hdop) {
        return None;
    }
    let altitude_m: f32 = f.get(8)?.trim().parse().ok()?;
    if !altitude_m.is_finite() || !(-12_000.0..=100_000.0).contains(&altitude_m) {
        return None;
    }
    let (latitude, longitude) = match (lat, lon) {
        (Some(latitude), Some(longitude)) => (latitude, longitude),
        // A no-fix GGA may omit both coordinates. Preserve the honest no-fix sample
        // without inventing a position; a partial or fixed position is invalid.
        (None, None) if quality == 0 => (0.0, 0.0),
        _ => return None,
    };
    let fix_type = match quality {
        0 => "no-fix",
        2 => "dgps",
        _ => "gps",
    }
    .to_string();
    Some(GpsFix {
        fix_type,
        latitude,
        longitude,
        altitude_m,
        hdop,
        satellites,
        speed_mph: 0.0,
        heading_deg: 0.0,
        age_s: 0.0,
        update_rate_hz: 0.0,
    })
}

/// Convert an NMEA `ddmm.mmmm` value + hemisphere into signed decimal degrees.
#[allow(
    clippy::float_cmp,
    reason = "NMEA coordinate bounds compare the exact degree limit"
)]
fn nmea_coord(value: &str, hemi: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let hemi = hemi.trim();
    let max_degrees = match hemi {
        "N" | "S" => 90.0,
        "E" | "W" => 180.0,
        _ => return None,
    };
    let v: f64 = value.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let deg = (v / 100.0).trunc();
    let min = v - deg * 100.0;
    if min >= 60.0 || deg > max_degrees || (deg == max_degrees && min > 0.0) {
        return None;
    }
    let mut dd = deg + min / 60.0;
    if matches!(hemi, "S" | "W") {
        dd = -dd;
    }
    Some(dd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_are_namespaced() {
        assert_eq!(vehicle_state_topic("eagle"), "state/vehicle/eagle");
        assert_eq!(
            vehicle_state_v2_topic("eagle", "ND84720078011035"),
            "state/vehicle/eagle/ND84720078011035"
        );
        assert_eq!(
            vehicle_action_topic("set-failover"),
            "action/vehicle/set-failover"
        );
    }

    #[test]
    fn parse_real_gpgga_no_lock_sample() {
        // The exact sentence captured from the bench MG90's omgtime.g.info.
        let fix = parse_gpgga(
            "$GPGGA,111504.000,3210.07993,N,09550.95445,W,0,00,99.0,081.94,M,-24.2,M,,*66",
        )
        .expect("valid GGA");
        assert_eq!(fix.fix_type, "no-fix");
        assert_eq!(fix.satellites, 0);
        assert!(!fix.has_fix(), "quality 0 / 0 sats ⇒ no lock");
        assert!(
            (fix.latitude - 32.167_998).abs() < 1e-4,
            "lat {}",
            fix.latitude
        );
        assert!(
            (fix.longitude + 95.849_240).abs() < 1e-4,
            "lon {}",
            fix.longitude
        );
        assert!((fix.altitude_m - 81.94).abs() < 0.01);
        assert!((fix.hdop - 99.0).abs() < 0.01);
    }

    #[test]
    fn parse_gpgga_with_lock() {
        let fix = parse_gpgga("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47")
            .expect("valid GGA");
        assert_eq!(fix.fix_type, "gps");
        assert_eq!(fix.satellites, 8);
        assert!(fix.has_fix());
        assert!((fix.latitude - 48.117_3).abs() < 1e-3);
        assert!((fix.longitude - 11.516_6).abs() < 1e-3);
    }

    #[test]
    fn parse_rejects_non_gga() {
        assert!(parse_gpgga("$PSIWMMPU,48.850,0.26605").is_none());
        assert!(parse_gpgga("garbage").is_none());
    }

    #[test]
    fn parse_gpgga_rejects_bad_checksum_and_out_of_range_fields() {
        assert!(
            parse_gpgga("$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*00")
                .is_none()
        );
        assert!(
            parse_gpgga("$GPGGA,123519,4860.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,").is_none()
        );
        assert!(
            parse_gpgga("$GPGGA,123519,4807.038,N,01131.000,E,9,08,0.9,545.4,M,46.9,M,,").is_none()
        );
        assert!(
            parse_gpgga("$GPGGA,123519,4807.038,N,01131.000,E,1,100,0.9,545.4,M,46.9,M,,")
                .is_none()
        );
    }

    #[test]
    fn parse_gpgga_allows_coordinate_free_no_fix_but_not_partial_coordinates() {
        let no_fix = parse_gpgga("$GPGGA,123519,,,,,0,00,99.0,0.0,M,0.0,M,,*00");
        assert!(no_fix.is_none(), "bad checksum must still be rejected");

        assert!(parse_gpgga("$GPGGA,123519,,,,,0,00,99.0,0.0,M,0.0,M,,").is_some());
        assert!(parse_gpgga("$GPGGA,123519,4807.038,N,,,0,00,99.0,0.0,M,0.0,M,,").is_none());
    }

    #[test]
    fn offline_snapshot_is_honest() {
        let s = VehicleState::offline("eagle");
        assert!(!s.online);
        assert!(!s.gps.has_fix());
        assert_eq!(s.gaps, vec!["gateway unreachable".to_string()]);
    }

    #[test]
    fn mirror_round_trips_json() {
        let mut s = VehicleState::offline("rig-1");
        s.online = true;
        s.model = "MG90".to_string();
        s.wan.active_wan = "Cellular A".to_string();
        s.wan.cellular_a.signal_dbm = -72;
        let j = serde_json::to_string(&s).unwrap();
        let back: VehicleState = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
        assert_eq!(back.wan.active_cellular().map(|l| l.signal_dbm), Some(-72));
    }

    #[test]
    fn approved_manager_set_preserves_order() {
        let managers = ManagerSet::approved(vec!["manager-b".into(), "manager-a".into()])
            .expect("valid manager set");
        assert_eq!(managers.state, ManagerSetState::Complete);
        assert_eq!(managers.ids, vec!["manager-b", "manager-a"]);

        let json = serde_json::to_string(&managers).unwrap();
        assert_eq!(
            json,
            r#"{"state":"complete","ids":["manager-b","manager-a"]}"#
        );
        assert_eq!(serde_json::from_str::<ManagerSet>(&json).unwrap(), managers);
    }

    #[test]
    fn approved_manager_set_rejects_blank_duplicate_and_over_capacity_ids() {
        assert_eq!(
            ManagerSet::approved(vec!["manager-a".into(), "  ".into()]),
            Err(ManagerSetValidationError::BlankId { index: 1 })
        );
        assert_eq!(
            ManagerSet::approved(vec!["manager-a".into(), "manager-a".into()]),
            Err(ManagerSetValidationError::DuplicateId("manager-a".into()))
        );
        assert_eq!(
            ManagerSet::approved(
                (0..=VEHICLE_STATE_V2_MAX_MANAGERS)
                    .map(|index| format!("manager-{index}"))
                    .collect()
            ),
            Err(ManagerSetValidationError::Capacity {
                len: VEHICLE_STATE_V2_MAX_MANAGERS + 1,
                max: VEHICLE_STATE_V2_MAX_MANAGERS,
            })
        );
    }

    #[test]
    fn manager_set_deserialization_rejects_invalid_ids() {
        let duplicate = r#"{"state":"complete","ids":["manager-a","manager-a"]}"#;
        assert!(serde_json::from_str::<ManagerSet>(duplicate).is_err());

        let too_many = serde_json::json!({
            "state": "complete",
            "ids": (0..=VEHICLE_STATE_V2_MAX_MANAGERS)
                .map(|index| format!("manager-{index}"))
                .collect::<Vec<_>>()
        });
        assert!(serde_json::from_value::<ManagerSet>(too_many).is_err());
    }

    #[test]
    fn v2_conversion_preserves_v1_data_and_marks_omissions_unknown() {
        let mut legacy = VehicleState::offline("rig-1");
        legacy.online = true;
        legacy.published_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_millis()
            .saturating_sub(2_000) as i64;
        legacy.model = "MG90".to_string();
        legacy.esn = "ND84720078011035".to_string();
        legacy.mgos_version = "4.3.0.1".to_string();
        legacy.wan.active_wan = "CellularA".to_string();
        legacy.wan.cellular_a.sim_state = "ready".to_string();
        legacy.wan.cellular_a.signal_dbm = -72;
        legacy.wan.cellular_a.healthy = true;
        legacy.gps.fix_type = "gps".to_string();
        legacy.gps.satellites = 8;

        let snapshot = VehicleStateV2::from_v1(
            &legacy,
            "rig-1",
            9,
            5_000,
            1_700_000_000_100,
            SnapshotProvenance {
                source: SnapshotSource::DirectGateway,
                source_id: Some("rig-1".to_string()),
                relay: None,
            },
        );
        assert_eq!(snapshot.schema_version, VEHICLE_STATE_V2_SCHEMA_VERSION);
        assert_eq!(snapshot.sequence, 9);
        assert_eq!(snapshot.expected_interval_ms, 5_000);
        assert_eq!(snapshot.management_node_id, "rig-1");
        assert_eq!(snapshot.mg90.id, "ND84720078011035");
        assert_eq!(snapshot.mg90.alias, None);
        assert_eq!(snapshot.approval, ApprovalState::Unknown);
        assert_eq!(snapshot.sharing, ShareState::Unknown);
        assert_eq!(snapshot.managers.state, ManagerSetState::Unknown);
        assert_eq!(
            snapshot.radios.len(),
            6,
            "native rows have stable positions"
        );
        assert_eq!(snapshot.radios.as_slice()[0].id, RadioId::CellularA);
        assert_eq!(
            snapshot.radios.as_slice()[0].operation,
            RadioOperation::Active
        );
        assert_eq!(
            snapshot.radios.as_slice()[1].presence,
            RadioPresence::Unknown,
            "v1 absence does not prove cellular B is not installed"
        );
        assert_eq!(
            snapshot.radios.as_slice()[4].operation,
            RadioOperation::Unknown
        );
        assert_eq!(
            snapshot.radios.as_slice()[5].operation,
            RadioOperation::Active
        );
        assert_eq!(snapshot.freshness.gnss.state, FreshnessState::Fresh);
        assert!(snapshot.freshness.identity.age_ms.unwrap_or_default() >= 1_000);

        let json = serde_json::to_string(&snapshot).unwrap();
        let round_trip: VehicleStateV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot, round_trip);
    }

    #[test]
    fn envelope_accepts_v1_and_maps_it_through_the_v2_reader() {
        let legacy = VehicleState::offline("rig-1");
        let json = serde_json::to_string(&legacy).unwrap();
        let envelope: VehicleStateEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(envelope, VehicleStateEnvelope::V1(_)));
        let snapshot = envelope.into_v2("rig-1", 1, 5_000, 42);
        assert_eq!(snapshot.schema_version, VEHICLE_STATE_V2_SCHEMA_VERSION);
        assert_eq!(snapshot.approval, ApprovalState::Unknown);
        assert_eq!(snapshot.provenance.source, SnapshotSource::Unknown);
        assert_eq!(snapshot.freshness.identity.state, FreshnessState::Unknown);
    }

    #[test]
    fn radio_ids_and_inventory_are_bounded_on_the_wire() {
        assert_eq!(RadioId::CellularA.as_str(), "cellular-a");
        assert_eq!(
            serde_json::to_string(&RadioId::extension("ext-lmr").unwrap()).unwrap(),
            "\"ext-lmr\""
        );
        assert!(RadioId::extension("ext-").is_none());
        assert!(RadioId::extension("vendor-lmr").is_none());
        assert!(RadioId::extension(format!("ext-{}", "x".repeat(29))).is_none());

        let row = || RadioHealth {
            id: RadioId::extension("ext-lmr").unwrap(),
            presence: RadioPresence::Unknown,
            operation: RadioOperation::Unknown,
            reason_code: Some(RadioReasonCode::NotReported),
            age_ms: None,
            configured_role: RadioRole::Unknown,
            active_path: false,
            metrics: RadioMetrics::Unknown,
        };
        let entries = (0..=VEHICLE_STATE_V2_MAX_RADIOS).map(|_| row()).collect();
        assert!(RadioInventory::new(entries).is_err());
        let encoded = serde_json::to_string(
            &(0..=VEHICLE_STATE_V2_MAX_RADIOS)
                .map(|_| row())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(serde_json::from_str::<RadioInventory>(&encoded).is_err());
    }

    #[test]
    fn radio_inventory_rejects_duplicate_ids_and_exposes_sparse_native_slots() {
        let row = |id: RadioId| RadioHealth {
            id,
            presence: RadioPresence::Unknown,
            operation: RadioOperation::Unknown,
            reason_code: Some(RadioReasonCode::NotReported),
            age_ms: None,
            configured_role: RadioRole::Unknown,
            active_path: false,
            metrics: RadioMetrics::Unknown,
        };
        assert!(
            RadioInventory::new(vec![row(RadioId::CellularA), row(RadioId::CellularA),]).is_err()
        );

        let inventory = RadioInventory::new(vec![
            row(RadioId::Gnss),
            row(RadioId::WifiB),
            row(RadioId::extension("ext-lmr").unwrap()),
        ])
        .unwrap();
        let slots = inventory.native_slots();
        assert!(slots[0].is_none(), "missing cellular A remains unknown");
        assert!(slots[3].is_some(), "wifi B is in its stable slot");
        assert!(slots[5].is_some(), "GNSS is in its stable slot");
        assert_eq!(
            inventory
                .by_id(&RadioId::extension("ext-lmr").unwrap())
                .unwrap()
                .id
                .as_str(),
            "ext-lmr"
        );
    }
}
