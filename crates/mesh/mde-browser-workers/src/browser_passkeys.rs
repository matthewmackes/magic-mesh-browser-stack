//! BROWSER-DD-6 — Browser passkey/WebAuthn ceremony owner.
//!
//! The Browser shell/engine path does not mint credentials itself. It publishes
//! WebAuthn ceremony metadata to `action/browser/passkey`; this worker validates
//! the request shape, persists the pending challenge, and owns the software
//! platform-authenticator key store and completion artifacts. It also owns the
//! hardware-key readiness probe plus CTAP HID packet framing needed for a live
//! hardware exchange. CTAP2 credential commands, phone-as-authenticator, and live
//! relying-party E2E proof remain separate owners.
//!
//! ## User-presence posture (security-2)
//!
//! The authenticator-data User Present (`UP`) bit is set **only** when the
//! ceremony carried a real presence signal (`PasskeyRequest::user_present`) —
//! never hardcoded. In the production Browser path, `mde-shell-egui` holds each
//! page-origin ceremony behind a shell-rendered Approve/Deny prompt and stamps
//! `user_present=true` only after approval; a denied ceremony never reaches this
//! worker. A ceremony with no verified presence honestly signs `UP=0`, which a
//! relying party rejects, rather than fabricating "a human was here". User
//! Verified (`UV`) remains unset; this is consent/presence, not PIN/biometric
//! verification.

// arch-7: unconditionally compiled — `mde-browser-workers` IS the async worker
// code; `mackesd` pulls it in only under its own `async-services` feature.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use mde_bus::hooks::config::Priority;
use mde_bus::persist::Persist;
use p256::ecdsa::{
    signature::{Signer, Verifier},
    Signature, SigningKey, VerifyingKey,
};
use p256::elliptic_curve::Generate as _;
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

use mde_worker_core::{ShutdownToken, Worker};

use crate::RetainedStatusPublisher;

/// Browser-owned WebAuthn/passkey ceremony handoff topic.
pub const ACTION_TOPIC: &str = "action/browser/passkey";

/// Retained-latest status topic prefix for this node.
pub const STATE_PREFIX: &str = "state/browser-passkeys/";

/// Pending ceremony event topic prefix for this node.
pub const EVENT_PREFIX: &str = "event/browser-passkeys/";

/// Local/share subdirectory for Browser passkey daemon state.
pub const PASSKEY_SUBDIR: &str = "browser-passkeys";

/// Subdirectory holding pending, not yet completed, WebAuthn ceremonies.
pub const PENDING_SUBDIR: &str = "pending";

/// Subdirectory holding platform-authenticator credential metadata + sealed keys.
pub const CREDENTIALS_SUBDIR: &str = "credentials";

/// Public credential records live here and are safe to mirror.
pub const PUBLIC_CREDENTIALS_SUBDIR: &str = "public";

/// Encrypted private-key envelopes live here.
pub const SEALED_CREDENTIALS_SUBDIR: &str = "sealed";

/// Default poll cadence. WebAuthn ceremonies are explicit user actions.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

/// Explicit opt-in for the live CTAPHID_INIT hidraw diagnostic. The default
/// status path only frames the request, because reading hidraw nodes during a
/// periodic status poll can otherwise block or perturb an authenticator.
pub const CTAPHID_LIVE_PROBE_ENV: &str = "MDE_BROWSER_PASSKEY_CTAPHID_LIVE_PROBE";

const MAX_HOST_CHARS: usize = 128;
const MAX_ORIGIN_CHARS: usize = 2048;
const MAX_RP_ID_CHARS: usize = 253;
const MAX_USER_NAME_CHARS: usize = 256;
const MAX_CHALLENGE_CHARS: usize = 2048;
const MAX_CREDENTIAL_ID_CHARS: usize = 2048;
const MAX_CREDENTIAL_IDS: usize = 64;
const CTAPHID_REPORT_SIZE: usize = 64;
const CTAPHID_INIT_HEADER_SIZE: usize = 7;
const CTAPHID_CONT_HEADER_SIZE: usize = 5;
const CTAPHID_INIT_PAYLOAD_MAX: usize = CTAPHID_REPORT_SIZE - CTAPHID_INIT_HEADER_SIZE;
const CTAPHID_CONT_PAYLOAD_MAX: usize = CTAPHID_REPORT_SIZE - CTAPHID_CONT_HEADER_SIZE;
const CTAPHID_MAX_CONTINUATIONS: usize = 128;
const CTAPHID_MAX_PAYLOAD: usize =
    CTAPHID_INIT_PAYLOAD_MAX + (CTAPHID_CONT_PAYLOAD_MAX * CTAPHID_MAX_CONTINUATIONS);
const CTAPHID_COMMAND_BIT: u8 = 0x80;
const CTAPHID_CMD_INIT: u8 = 0x06;
const CTAPHID_BROADCAST_CID: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
const CTAPHID_INIT_NONCE_LEN: usize = 8;
const CTAPHID_INIT_RESPONSE_LEN: usize = 17;

type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Browser-origin WebAuthn ceremony request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PasskeyRequest {
    /// Request id from the Bus ULID.
    pub id: String,
    /// Helper/page request id used to resolve the pending page promise.
    pub client_request_id: Option<String>,
    /// Browser host that published the request.
    pub host: String,
    /// Browser engine (`servo` or `cef`).
    pub engine: String,
    /// WebAuthn ceremony: `create` or `get`.
    pub ceremony: String,
    /// Origin URL for the focused page.
    pub origin: String,
    /// Origin host extracted from [`Self::origin`].
    pub origin_host: String,
    /// Requested WebAuthn RP id.
    pub rp_id: String,
    /// Browser-provided challenge, base64url without padding.
    pub challenge_b64url: String,
    /// Optional user handle for registration ceremonies.
    pub user_handle_b64url: Option<String>,
    /// Optional user display/name metadata for registration ceremonies.
    pub user_name: Option<String>,
    /// Optional allow-list credential ids for assertion ceremonies.
    pub allow_credentials: Vec<String>,
    /// Optional browser-suggested timeout.
    pub timeout_ms: Option<u64>,
    /// Whether a user-presence step accompanied this ceremony. In the normal
    /// Browser path this is stamped by the trusted shell after its Approve/Deny
    /// prompt; older helper/direct-Bus paths may omit it.
    ///
    /// security-2: the authenticator-data User Present (`UP`) bit is set **only**
    /// when this is true, rather than hardcoded. Defaults to `false` when the
    /// producer omits the signal, so an absent/forged-empty request yields an
    /// honest UP=0 (which a relying party rejects) instead of a fabricated
    /// "human was here". This is consent/presence, not user verification.
    pub user_present: bool,
}

/// Durable pending ceremony record. This intentionally contains no private key
/// material, signatures, client-data JSON, or authenticator data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingPasskeyCeremony {
    /// Pending request metadata.
    pub request: PasskeyRequest,
    /// Node that accepted the request.
    pub node: String,
    /// Pending state marker.
    pub state: String,
    /// Local persist timestamp.
    pub pending_ms: u64,
    /// Shared-root mirror timestamp, when mirrored.
    pub mirrored_ms: Option<u64>,
}

/// Public platform-authenticator credential metadata. Private key bytes are
/// stored only in a sealed sibling file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlatformCredentialRecord {
    /// Browser-generated local credential id, base64url without padding.
    pub credential_id_b64url: String,
    /// WebAuthn RP id this credential is scoped to.
    pub rp_id: String,
    /// Registration user handle.
    pub user_handle_b64url: String,
    /// Registration user display/name metadata.
    pub user_name: String,
    /// SEC1 uncompressed P-256 public key, base64url without padding.
    pub public_key_sec1_b64url: String,
    /// COSE algorithm id. `-7` is ES256.
    pub cose_alg: i64,
    /// Monotonic authenticator signature counter.
    pub sign_count: u32,
    /// Registration timestamp.
    pub created_ms: u64,
    /// Last update/sign timestamp.
    pub updated_ms: u64,
    /// Shared-root mirror timestamp, when mirrored.
    pub mirrored_ms: Option<u64>,
}

/// Retained status for this node's Browser passkey owner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PasskeyStatus {
    /// Node identifier that owns this status record.
    pub node: String,
    /// Most recent accepted request id.
    pub last_request_id: Option<String>,
    /// Browser host from the most recent accepted request.
    pub last_host: Option<String>,
    /// Ceremony type from the most recent accepted request.
    pub last_ceremony: Option<String>,
    /// RP id from the most recent accepted request.
    pub last_rp_id: Option<String>,
    /// Outcome state: `idle`, `pending`, or `error`.
    pub state: String,
    /// True when the newest pending ceremony was mirrored to the shared root.
    pub mirrored: bool,
    /// Last validation or persistence error.
    pub last_error: Option<String>,
    /// Accepted requests since worker start.
    pub accepted: u64,
    /// Requests rejected as malformed.
    pub rejected: u64,
    /// Timestamp of the most recent accepted ceremony.
    pub last_pending_ms: Option<u64>,
    /// CTAP/FIDO HID readiness: `unknown`, `unavailable`, `present_permission_denied`, or `ready`.
    pub hardware_state: String,
    /// Likely FIDO/CTAP HID devices detected from sysfs.
    pub hardware_key_count: u64,
    /// Detected FIDO/CTAP HID devices the daemon can open through `/dev/hidraw*`.
    pub hardware_readable_count: u64,
    /// CTAP HID diagnostic state: `unknown`, `unavailable`, or `init_request_ready`.
    pub hardware_ctaphid_state: String,
    /// CTAP HID frames needed for the daemon's broadcast INIT diagnostic request.
    pub hardware_ctaphid_init_frame_count: u64,
    /// Live CTAPHID_INIT diagnostic state: `unknown`, `disabled`, `unavailable`,
    /// `init_exchange_ready`, or `error`.
    pub hardware_ctaphid_live_state: String,
    /// Allocated CTAP HID channel id from the live INIT exchange, hex encoded.
    pub hardware_ctaphid_live_channel_id: Option<String>,
    /// CTAP HID protocol version reported by a live INIT exchange.
    pub hardware_ctaphid_live_protocol_version: Option<u64>,
    /// Authenticator version tuple reported by a live INIT exchange.
    pub hardware_ctaphid_live_device_version: Option<String>,
    /// CTAP HID capabilities byte reported by a live INIT exchange.
    pub hardware_ctaphid_live_capabilities: Option<u64>,
    /// Last live CTAPHID_INIT diagnostic error, when the opt-in probe failed.
    pub hardware_ctaphid_live_error: Option<String>,
    /// Timestamp of the latest hardware-key readiness probe.
    pub hardware_probe_ms: u64,
    /// Timestamp of the most recent status publication.
    pub updated_ms: u64,
}

/// Daemon worker for Browser WebAuthn/passkey handoffs.
pub struct BrowserPasskeysWorker {
    node: String,
    local_root: PathBuf,
    share_root: PathBuf,
    key_path: PathBuf,
    cursor: Option<String>,
    tick: Duration,
    now_fn: NowFn,
    share_gate: Option<Arc<AtomicBool>>,
    bus_root_override: Option<PathBuf>,
    status: PasskeyStatus,
    status_publisher: RetainedStatusPublisher,
}

impl BrowserPasskeysWorker {
    /// Create a Browser passkey worker for one node and workgroup share.
    #[must_use]
    pub fn new(node: String, local_root: PathBuf, share_root: PathBuf) -> Self {
        let now_fn: NowFn = Arc::new(default_now);
        let updated_ms = now_fn();
        Self {
            node: node.clone(),
            local_root,
            share_root,
            key_path: mde_seal::age_key_path(),
            cursor: None,
            tick: DEFAULT_TICK,
            now_fn,
            share_gate: None,
            bus_root_override: None,
            status: PasskeyStatus {
                node,
                last_request_id: None,
                last_host: None,
                last_ceremony: None,
                last_rp_id: None,
                state: "idle".to_owned(),
                mirrored: false,
                last_error: None,
                accepted: 0,
                rejected: 0,
                last_pending_ms: None,
                hardware_state: "unknown".to_owned(),
                hardware_key_count: 0,
                hardware_readable_count: 0,
                hardware_ctaphid_state: "unknown".to_owned(),
                hardware_ctaphid_init_frame_count: 0,
                hardware_ctaphid_live_state: "unknown".to_owned(),
                hardware_ctaphid_live_channel_id: None,
                hardware_ctaphid_live_protocol_version: None,
                hardware_ctaphid_live_device_version: None,
                hardware_ctaphid_live_capabilities: None,
                hardware_ctaphid_live_error: None,
                hardware_probe_ms: updated_ms,
                updated_ms,
            },
            status_publisher: RetainedStatusPublisher::new(),
        }
    }

    /// Override the worker polling interval.
    #[must_use]
    pub const fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    /// Override the clock used for deterministic tests.
    #[must_use]
    pub fn with_now_fn(mut self, now: NowFn) -> Self {
        self.now_fn = now;
        self
    }

    /// Override shared-root availability with a test-controlled gate.
    #[must_use]
    pub fn with_share_gate(mut self, gate: Arc<AtomicBool>) -> Self {
        self.share_gate = Some(gate);
        self
    }

    /// Override the Bus root used by `Persist`.
    #[must_use]
    pub fn with_bus_root(mut self, root: PathBuf) -> Self {
        self.bus_root_override = Some(root);
        self
    }

    /// Override the mesh age identity path used to seal platform passkeys.
    #[must_use]
    pub fn with_key_path(mut self, path: PathBuf) -> Self {
        self.key_path = path;
        self
    }

    fn now_ms(&self) -> u64 {
        (self.now_fn)()
    }

    fn share_writable(&self) -> bool {
        self.share_gate.as_ref().map_or_else(
            || mackes_mesh_types::mesh_storage::shared_root_writable(&self.share_root),
            |g| g.load(Ordering::SeqCst),
        )
    }

    fn drain_requests(&mut self, persist: &Persist) {
        let msgs = match persist.list_since(ACTION_TOPIC, self.cursor.as_deref()) {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_passkeys", error = %e, "list_since failed");
                return;
            }
        };
        for msg in msgs {
            self.cursor = Some(msg.ulid.clone());
            let body = msg.body.unwrap_or_default();
            match parse_request(&body, &msg.ulid) {
                Ok(request) => self.apply_request(persist, request),
                Err(e) => {
                    self.status.rejected = self.status.rejected.saturating_add(1);
                    self.status.state = "error".to_owned();
                    self.status.last_error = Some(e);
                    self.status.updated_ms = self.now_ms();
                    self.publish_status(persist);
                }
            }
        }
    }

    fn apply_request(&mut self, persist: &Persist, request: PasskeyRequest) {
        let pending_ms = self.now_ms();
        let mut record = PendingPasskeyCeremony {
            request,
            node: self.node.clone(),
            state: "pending_platform_authenticator".to_owned(),
            pending_ms,
            mirrored_ms: None,
        };

        let local_path = pending_path(&self.local_root, &record.request.host, &record.request.id);
        let Ok(initial_body) = serde_json::to_string_pretty(&record) else {
            self.record_error(
                persist,
                "could not encode pending passkey ceremony".to_owned(),
            );
            return;
        };
        if let Err(e) = write_atomic(&local_path, &initial_body) {
            self.record_error(
                persist,
                format!("could not persist local pending ceremony: {e}"),
            );
            return;
        }

        let mut mirrored = false;
        if self.share_writable() {
            record.mirrored_ms = Some(self.now_ms());
            if let Ok(mirror_body) = serde_json::to_string_pretty(&record) {
                let share_path =
                    pending_path(&self.share_root, &record.request.host, &record.request.id);
                mirrored = write_atomic(&share_path, &mirror_body).is_ok();
                if mirrored {
                    let _ = write_atomic(&local_path, &mirror_body);
                }
            }
            if !mirrored {
                record.mirrored_ms = None;
            }
        }

        self.status.accepted = self.status.accepted.saturating_add(1);
        self.status.last_request_id = Some(record.request.id.clone());
        self.status.last_host = Some(record.request.host.clone());
        self.status.last_ceremony = Some(record.request.ceremony.clone());
        self.status.last_rp_id = Some(record.request.rp_id.clone());
        self.status.state = "pending".to_owned();
        self.status.mirrored = mirrored;
        self.status.last_error = None;
        self.status.last_pending_ms = Some(pending_ms);
        self.status.updated_ms = self.now_ms();
        self.publish_event(persist, &record, mirrored);
        match record.request.ceremony.as_str() {
            "create" => {
                if let Err(e) = self.complete_create(persist, &record) {
                    self.record_error(persist, e);
                    return;
                }
            }
            "get" => {
                if let Err(e) = self.complete_get(persist, &record) {
                    self.record_error(persist, e);
                    return;
                }
            }
            _ => {}
        }
        self.publish_status(persist);
    }

    fn complete_create(
        &mut self,
        persist: &Persist,
        pending: &PendingPasskeyCeremony,
    ) -> Result<(), String> {
        let request = &pending.request;
        let user_handle = request
            .user_handle_b64url
            .clone()
            .ok_or_else(|| "create request missing user handle".to_owned())?;
        let user_name = request
            .user_name
            .clone()
            .ok_or_else(|| "create request missing user name".to_owned())?;
        let credential_id = new_credential_id();
        let signing_key = SigningKey::generate();
        let public_key_sec1_b64url =
            b64url(signing_key.verifying_key().to_sec1_point(false).as_bytes());
        let now = self.now_ms();
        let mut record = PlatformCredentialRecord {
            credential_id_b64url: credential_id.clone(),
            rp_id: request.rp_id.clone(),
            user_handle_b64url: user_handle,
            user_name,
            public_key_sec1_b64url,
            cose_alg: -7,
            sign_count: 0,
            created_ms: now,
            updated_ms: now,
            mirrored_ms: None,
        };
        let private_seed_b64url = b64url(signing_key.to_bytes().as_slice());
        seal_private_key(
            &self.local_root,
            &self.key_path,
            &credential_id,
            &private_seed_b64url,
        )?;
        write_credential_record(&self.local_root, &record)?;
        let mut mirrored = false;
        if self.share_writable() {
            record.mirrored_ms = Some(self.now_ms());
            if seal_private_key(
                &self.share_root,
                &self.key_path,
                &credential_id,
                &private_seed_b64url,
            )
            .and_then(|()| write_credential_record(&self.share_root, &record))
            .is_ok()
            {
                mirrored = true;
                write_credential_record(&self.local_root, &record)?;
            } else {
                record.mirrored_ms = None;
            }
        }
        self.status.state = "created".to_owned();
        self.status.mirrored = mirrored;
        self.status.last_error = None;
        self.status.updated_ms = self.now_ms();
        self.publish_platform_event(persist, "browser_passkey_created", pending, &record, None)?;
        Ok(())
    }

    fn complete_get(
        &mut self,
        persist: &Persist,
        pending: &PendingPasskeyCeremony,
    ) -> Result<(), String> {
        let request = &pending.request;
        let mut record = find_credential(&self.local_root, request)?;
        let seed = unseal_private_key(
            &self.local_root,
            &self.key_path,
            &record.credential_id_b64url,
        )
        .or_else(|_| {
            unseal_private_key(
                &self.share_root,
                &self.key_path,
                &record.credential_id_b64url,
            )
        })?;
        let signing_key = SigningKey::from_slice(&b64url_decode(&seed)?)
            .map_err(|e| format!("platform passkey private key decode: {e}"))?;
        let payload = assertion_payload(request, record.sign_count.saturating_add(1));
        let signature: Signature = signing_key.sign(&payload.signing_bytes);
        let verifying_key =
            VerifyingKey::from_sec1_bytes(&b64url_decode(&record.public_key_sec1_b64url)?)
                .map_err(|e| format!("platform passkey public key decode: {e}"))?;
        verifying_key
            .verify(&payload.signing_bytes, &signature)
            .map_err(|e| format!("platform passkey signature self-check failed: {e}"))?;

        record.sign_count = record.sign_count.saturating_add(1);
        record.updated_ms = self.now_ms();
        record.mirrored_ms = None;
        write_credential_record(&self.local_root, &record)?;
        if self.share_writable() {
            let mut mirrored_record = record.clone();
            mirrored_record.mirrored_ms = Some(self.now_ms());
            if write_credential_record(&self.share_root, &mirrored_record).is_ok() {
                record = mirrored_record;
                write_credential_record(&self.local_root, &record)?;
            }
        }
        self.status.state = "asserted".to_owned();
        self.status.mirrored = record.mirrored_ms.is_some();
        self.status.last_error = None;
        self.status.updated_ms = self.now_ms();
        self.publish_platform_event(
            persist,
            "browser_passkey_assertion",
            pending,
            &record,
            Some(AssertionEvent {
                authenticator_data_b64url: b64url(&payload.authenticator_data),
                client_data_json_b64url: b64url(payload.client_data_json.as_bytes()),
                client_data_hash_b64url: b64url(&payload.client_data_hash),
                signature_b64url: b64url(signature.to_bytes().as_slice()),
                sign_count: record.sign_count,
            }),
        )?;
        Ok(())
    }

    fn record_error(&mut self, persist: &Persist, error: String) {
        self.status.rejected = self.status.rejected.saturating_add(1);
        self.status.state = "error".to_owned();
        self.status.last_error = Some(error);
        self.status.updated_ms = self.now_ms();
        self.publish_status(persist);
    }

    fn refresh_hardware_status(&mut self) {
        let hardware = probe_hardware_key_status(Path::new("/sys/class/hidraw"), Path::new("/dev"));
        let changed = self.status.hardware_state != hardware.state
            || self.status.hardware_key_count != hardware.key_count
            || self.status.hardware_readable_count != hardware.readable_count
            || self.status.hardware_ctaphid_state != hardware.ctaphid_state
            || self.status.hardware_ctaphid_init_frame_count != hardware.ctaphid_init_frame_count
            || self.status.hardware_ctaphid_live_state != hardware.ctaphid_live_state
            || self.status.hardware_ctaphid_live_channel_id != hardware.ctaphid_live_channel_id
            || self.status.hardware_ctaphid_live_protocol_version
                != hardware.ctaphid_live_protocol_version
            || self.status.hardware_ctaphid_live_device_version
                != hardware.ctaphid_live_device_version
            || self.status.hardware_ctaphid_live_capabilities != hardware.ctaphid_live_capabilities
            || self.status.hardware_ctaphid_live_error != hardware.ctaphid_live_error;
        if !changed {
            return;
        }
        self.status.hardware_state = hardware.state;
        self.status.hardware_key_count = hardware.key_count;
        self.status.hardware_readable_count = hardware.readable_count;
        self.status.hardware_ctaphid_state = hardware.ctaphid_state;
        self.status.hardware_ctaphid_init_frame_count = hardware.ctaphid_init_frame_count;
        self.status.hardware_ctaphid_live_state = hardware.ctaphid_live_state;
        self.status.hardware_ctaphid_live_channel_id = hardware.ctaphid_live_channel_id;
        self.status.hardware_ctaphid_live_protocol_version = hardware.ctaphid_live_protocol_version;
        self.status.hardware_ctaphid_live_device_version = hardware.ctaphid_live_device_version;
        self.status.hardware_ctaphid_live_capabilities = hardware.ctaphid_live_capabilities;
        self.status.hardware_ctaphid_live_error = hardware.ctaphid_live_error;
        let now = self.now_ms();
        self.status.hardware_probe_ms = now;
        self.status.updated_ms = now;
    }

    fn publish_status(&mut self, persist: &Persist) {
        self.refresh_hardware_status();
        let topic = format!("{STATE_PREFIX}{}", self.node);
        if let Ok(body) = serde_json::to_string(&self.status) {
            self.status_publisher
                .publish(persist, &topic, Priority::Min, body);
        }
    }

    fn publish_event(&self, persist: &Persist, record: &PendingPasskeyCeremony, mirrored: bool) {
        let topic = format!("{EVENT_PREFIX}{}", self.node);
        let body = serde_json::json!({
            "op": "browser_passkey_pending",
            "source": "browser_passkeys",
            "node": self.node,
            "request_id": &record.request.id,
            "client_request_id": &record.request.client_request_id,
            "host": &record.request.host,
            "engine": &record.request.engine,
            "ceremony": &record.request.ceremony,
            "origin": &record.request.origin,
            "rp_id": &record.request.rp_id,
            "state": &record.state,
            "mirrored": mirrored,
            "pending_ms": record.pending_ms,
            "updated_ms": self.now_ms(),
        })
        .to_string();
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
    }

    fn publish_platform_event(
        &self,
        persist: &Persist,
        op: &str,
        pending: &PendingPasskeyCeremony,
        credential: &PlatformCredentialRecord,
        assertion: Option<AssertionEvent>,
    ) -> Result<(), String> {
        let topic = format!("{EVENT_PREFIX}{}", self.node);
        let mut body = serde_json::json!({
            "op": op,
            "source": "browser_passkeys",
            "node": self.node,
            "request_id": &pending.request.id,
            "client_request_id": &pending.request.client_request_id,
            "host": &pending.request.host,
            "engine": &pending.request.engine,
            "ceremony": &pending.request.ceremony,
            "origin": &pending.request.origin,
            "rp_id": &pending.request.rp_id,
            "credential_id_b64url": &credential.credential_id_b64url,
            "user_handle_b64url": &credential.user_handle_b64url,
            "user_name": &credential.user_name,
            "public_key_sec1_b64url": &credential.public_key_sec1_b64url,
            "public_key_spki_der_b64url": b64url(&spki_der_from_sec1(&b64url_decode(&credential.public_key_sec1_b64url)?)?),
            "cose_alg": credential.cose_alg,
            "sign_count": credential.sign_count,
            "mirrored": credential.mirrored_ms.is_some(),
            "updated_ms": self.now_ms(),
        });
        if let Some(assertion) = assertion {
            body["authenticator_data_b64url"] =
                serde_json::Value::String(assertion.authenticator_data_b64url);
            body["client_data_json_b64url"] =
                serde_json::Value::String(assertion.client_data_json_b64url);
            body["client_data_hash_b64url"] =
                serde_json::Value::String(assertion.client_data_hash_b64url);
            body["signature_b64url"] = serde_json::Value::String(assertion.signature_b64url);
            body["sign_count"] = serde_json::Value::from(assertion.sign_count);
        } else if op == "browser_passkey_created" {
            let payload = assertion_payload(&pending.request, credential.sign_count);
            let auth_data = registration_authenticator_data(
                &pending.request.rp_id,
                credential,
                pending.request.user_present,
            )?;
            let attestation_object = none_attestation_object(&auth_data);
            body["client_data_json_b64url"] =
                serde_json::Value::String(b64url(payload.client_data_json.as_bytes()));
            body["client_data_hash_b64url"] =
                serde_json::Value::String(b64url(&payload.client_data_hash));
            body["authenticator_data_b64url"] = serde_json::Value::String(b64url(&auth_data));
            body["attestation_object_b64url"] =
                serde_json::Value::String(b64url(&attestation_object));
        }
        let body = serde_json::to_string(&body)
            .map_err(|e| format!("could not encode platform passkey event: {e}"))?;
        let _ = persist.write(&topic, Priority::Default, None, Some(&body));
        Ok(())
    }
}

#[async_trait::async_trait]
impl Worker for BrowserPasskeysWorker {
    fn name(&self) -> &'static str {
        "browser_passkeys"
    }

    async fn run(&mut self, mut shutdown: ShutdownToken) -> anyhow::Result<()> {
        let Some(bus_root) = self
            .bus_root_override
            .clone()
            .or_else(mde_bus::default_data_dir)
        else {
            tracing::debug!(target: "mackesd::browser_passkeys", "no bus root; worker idle");
            shutdown.wait().await;
            return Ok(());
        };
        let persist = match Persist::open(bus_root) {
            Ok(persist) => persist,
            Err(e) => {
                tracing::debug!(target: "mackesd::browser_passkeys", error = %e, "persist open failed; worker idle");
                shutdown.wait().await;
                return Ok(());
            }
        };
        self.publish_status(&persist);
        let mut tick = tokio::time::interval(self.tick);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.drain_requests(&persist);
                    self.publish_status(&persist);
                }
                () = shutdown.wait() => break,
            }
        }
        self.publish_status(&persist);
        Ok(())
    }
}

fn parse_request(body: &str, id: &str) -> Result<PasskeyRequest, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("browser-passkey JSON: {e}"))?;
    if v.get("op").and_then(serde_json::Value::as_str) != Some("browser_passkey") {
        return Err("wrong op".to_owned());
    }
    if v.get("source").and_then(serde_json::Value::as_str) != Some("browser") {
        return Err("wrong source".to_owned());
    }
    let id = safe_component(id);
    if id.is_empty() {
        return Err("empty request id".to_owned());
    }
    let host = safe_component(&required_string(&v, "host", MAX_HOST_CHARS)?);
    if host.is_empty() {
        return Err("invalid host".to_owned());
    }
    let engine = required_string(&v, "engine", 16)?.to_ascii_lowercase();
    if !matches!(engine.as_str(), "servo" | "cef") {
        return Err("unsupported engine".to_owned());
    }
    let ceremony = required_string(&v, "ceremony", 16)?.to_ascii_lowercase();
    if !matches!(ceremony.as_str(), "create" | "get") {
        return Err("unsupported ceremony".to_owned());
    }
    let origin = required_string(&v, "origin", MAX_ORIGIN_CHARS)?;
    let origin_host =
        origin_host(&origin).ok_or_else(|| "origin must be https or localhost http".to_owned())?;
    let rp_id = required_string(&v, "rp_id", MAX_RP_ID_CHARS)?.to_ascii_lowercase();
    if !valid_rp_id(&rp_id) || !rp_matches_origin(&rp_id, &origin_host) {
        return Err("rp_id does not match origin".to_owned());
    }
    // browser-6: reject an rp_id that is itself a public suffix (e.g. a page at
    // `attacker.github.io` requesting `rp_id = "github.io"`, which would match
    // every `*.github.io` tenant). Combined with the label-boundary check above,
    // requiring a non-public-suffix rp_id also guarantees the rp_id covers at
    // least the origin's registrable domain (eTLD+1).
    if is_public_suffix(&rp_id) {
        return Err("rp_id is a public suffix".to_owned());
    }
    let challenge_b64url = required_string(&v, "challenge_b64url", MAX_CHALLENGE_CHARS)?;
    if !valid_b64url_token(&challenge_b64url, 22, MAX_CHALLENGE_CHARS) {
        return Err("invalid challenge_b64url".to_owned());
    }

    let user_handle_b64url = optional_string(&v, "user_handle_b64url", MAX_CREDENTIAL_ID_CHARS)?;
    if let Some(handle) = &user_handle_b64url {
        if !valid_b64url_token(handle, 8, MAX_CREDENTIAL_ID_CHARS) {
            return Err("invalid user_handle_b64url".to_owned());
        }
    }
    let user_name = optional_string(&v, "user_name", MAX_USER_NAME_CHARS)?;
    if ceremony == "create" && (user_handle_b64url.is_none() || user_name.is_none()) {
        return Err("create ceremonies require user_handle_b64url and user_name".to_owned());
    }
    let allow_credentials = string_array(&v, "allow_credentials", MAX_CREDENTIAL_IDS)?;
    for credential in &allow_credentials {
        if !valid_b64url_token(credential, 8, MAX_CREDENTIAL_ID_CHARS) {
            return Err("invalid allow_credentials entry".to_owned());
        }
    }
    let timeout_ms = optional_u64(&v, "timeout_ms")?;
    if timeout_ms.is_some_and(|ms| !(1_000..=600_000).contains(&ms)) {
        return Err("timeout_ms out of range".to_owned());
    }
    // security-2: presence signal from the Browser shell. Absent => not present.
    let user_present = v
        .get("user_present")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    Ok(PasskeyRequest {
        id,
        client_request_id: optional_string(&v, "client_request_id", MAX_HOST_CHARS)?
            .map(|id| safe_component(&id))
            .filter(|id| !id.is_empty()),
        host,
        engine,
        ceremony,
        origin,
        origin_host,
        rp_id,
        challenge_b64url,
        user_handle_b64url,
        user_name,
        allow_credentials,
        timeout_ms,
        user_present,
    })
}

fn required_string(v: &serde_json::Value, key: &str, max_chars: usize) -> Result<String, String> {
    let value = v
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("missing {key}"))?
        .trim();
    if value.is_empty() {
        return Err(format!("empty {key}"));
    }
    if value.chars().count() > max_chars {
        return Err(format!("{key} is too long"));
    }
    Ok(value.to_owned())
}

fn optional_string(
    v: &serde_json::Value,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, String> {
    let Some(value) = v.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let Some(text) = value.as_str() else {
        return Err(format!("{key} is not a string"));
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.chars().count() > max_chars {
        return Err(format!("{key} is too long"));
    }
    Ok(Some(text.to_owned()))
}

fn optional_u64(v: &serde_json::Value, key: &str) -> Result<Option<u64>, String> {
    let Some(value) = v.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .map(Some)
        .ok_or_else(|| format!("{key} is not an unsigned integer"))
}

fn string_array(v: &serde_json::Value, key: &str, max_len: usize) -> Result<Vec<String>, String> {
    let Some(value) = v.get(key) else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let array = value
        .as_array()
        .ok_or_else(|| format!("{key} is not an array"))?;
    if array.len() > max_len {
        return Err(format!("{key} has too many entries"));
    }
    array
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("{key} contains a non-string entry"))
        })
        .collect()
}

fn origin_host(origin: &str) -> Option<String> {
    let origin = origin.trim();
    let (secure, rest) = if let Some(rest) = origin.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (false, rest)
    } else {
        return None;
    };
    let host_port = rest.split(['/', '?', '#']).next()?.trim();
    if host_port.is_empty() || host_port.contains('@') {
        return None;
    }
    let host = if host_port.starts_with('[') {
        return None;
    } else {
        host_port.split(':').next()?
    }
    .trim_end_matches('.')
    .to_ascii_lowercase();
    if !secure && !matches!(host.as_str(), "localhost" | "127.0.0.1") {
        return None;
    }
    valid_rp_id(&host).then_some(host)
}

fn valid_rp_id(rp_id: &str) -> bool {
    let rp_id = rp_id.trim_end_matches('.');
    !rp_id.is_empty()
        && rp_id.len() <= MAX_RP_ID_CHARS
        && !rp_id.starts_with('.')
        && !rp_id.ends_with('.')
        && rp_id.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn rp_matches_origin(rp_id: &str, origin_host: &str) -> bool {
    origin_host == rp_id
        || origin_host
            .strip_suffix(rp_id)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Curated Public Suffix List snapshot (interim; see the note below).
///
/// browser-6: `rp_matches_origin`'s label-boundary suffix check alone let a page
/// at `attacker.github.io` claim `rp_id = "github.io"` and thereby match every
/// `*.github.io` tenant — a cross-tenant credential-phishing hole. The real fix
/// is a public-suffix check: an `rp_id` that is itself a public suffix must be
/// rejected. Bundling the full Mozilla PSL (or pulling a `publicsuffix`/`psl`
/// crate) is not viable on the airgapped daemon build today, so this is a
/// curated snapshot of the highest-value suffixes — common multi-tenant hosting
/// domains (the direct attack surface) plus common country-code second-level
/// registries. Every single-label TLD (`com`, `io`, `test`, …) is already a
/// public suffix via the implicit default `*` rule, so only multi-label rules
/// are listed. Entries may use a leading `*` label (wildcard) or a `!` prefix
/// (exception), matching PSL semantics. The list is deliberately conservative:
/// a missing entry fails *safe* (merely less restrictive on an exotic suffix),
/// never by blocking a legitimate registrable domain. Refresh it from
/// <https://publicsuffix.org/list/public_suffix_list.dat> when the daemon can
/// vendor the full list.
const PUBLIC_SUFFIX_RULES: &[&str] = &[
    // Multi-tenant hosting / PaaS suffixes (the browser-6 attack surface).
    "github.io",
    "gitlab.io",
    "bitbucket.io",
    "pages.dev",
    "workers.dev",
    "r2.dev",
    "netlify.app",
    "vercel.app",
    "web.app",
    "firebaseapp.com",
    "appspot.com",
    "cloudfunctions.net",
    "herokuapp.com",
    "herokussl.com",
    "now.sh",
    "surge.sh",
    "glitch.me",
    "repl.co",
    "readthedocs.io",
    "blogspot.com",
    "azurewebsites.net",
    "azurestaticapps.net",
    "cloudapp.net",
    "cloudfront.net",
    "s3.amazonaws.com",
    "elasticbeanstalk.com",
    "translate.goog",
    "myshopify.com",
    // Common country-code second-level registries.
    "co.uk",
    "org.uk",
    "gov.uk",
    "ac.uk",
    "me.uk",
    "net.uk",
    "ltd.uk",
    "plc.uk",
    "sch.uk",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "gov.au",
    "id.au",
    "co.jp",
    "or.jp",
    "ne.jp",
    "ac.jp",
    "go.jp",
    "co.kr",
    "or.kr",
    "com.cn",
    "net.cn",
    "org.cn",
    "gov.cn",
    "edu.cn",
    "com.br",
    "net.br",
    "org.br",
    "gov.br",
    "com.mx",
    "com.ar",
    "com.co",
    "co.in",
    "net.in",
    "org.in",
    "co.za",
    "org.za",
    "co.nz",
    "net.nz",
    "org.nz",
    "govt.nz",
    "ac.nz",
    "com.sg",
    "com.hk",
    "com.tw",
    "com.tr",
    "co.il",
    "com.ua",
    "com.pl",
    "co.id",
    "co.th",
    "com.my",
    "com.ph",
    "com.vn",
    // Canonical PSL wildcard + exception example (exercises both rule kinds).
    "*.ck",
    "!www.ck",
];

/// Whether `domain` is itself a public suffix — an eTLD under which anyone may
/// register — per the curated snapshot plus the implicit default `*` rule that
/// makes every single-label domain a public suffix.
///
/// browser-6: a WebAuthn `rp_id` that is a public suffix must be rejected, so a
/// page cannot scope a credential to a shared multi-tenant suffix.
fn is_public_suffix(domain: &str) -> bool {
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    public_suffix_label_count(&labels) == labels.len()
}

/// Number of right-hand labels of `labels` that form its public suffix, applying
/// PSL rule precedence (exception rules win; otherwise the longest matching
/// rule; otherwise the default `*` = one label).
fn public_suffix_label_count(labels: &[&str]) -> usize {
    let mut best_normal = 0usize;
    let mut best_exception = 0usize;
    for rule in PUBLIC_SUFFIX_RULES {
        let (is_exception, body) = match rule.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, *rule),
        };
        let rule_labels: Vec<&str> = body.split('.').collect();
        if rule_labels.is_empty() || rule_labels.len() > labels.len() {
            continue;
        }
        let offset = labels.len() - rule_labels.len();
        let matches = rule_labels
            .iter()
            .enumerate()
            .all(|(i, rl)| *rl == "*" || rl.eq_ignore_ascii_case(labels[offset + i]));
        if !matches {
            continue;
        }
        if is_exception {
            best_exception = best_exception.max(rule_labels.len());
        } else {
            best_normal = best_normal.max(rule_labels.len());
        }
    }
    if best_exception > 0 {
        // Exception rule: the public suffix is the rule minus its leftmost label.
        best_exception - 1
    } else if best_normal > 0 {
        best_normal
    } else {
        // Default `*` rule: the rightmost label.
        1
    }
}

fn valid_b64url_token(value: &str, min_chars: usize, max_chars: usize) -> bool {
    let len = value.len();
    (min_chars..=max_chars).contains(&len)
        && !value.contains('=')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn pending_path(root: &Path, host: &str, request_id: &str) -> PathBuf {
    root.join(PASSKEY_SUBDIR)
        .join(PENDING_SUBDIR)
        .join(safe_component(host))
        .join(format!("{}.json", safe_component(request_id)))
}

fn credential_public_dir(root: &Path) -> PathBuf {
    root.join(PASSKEY_SUBDIR)
        .join(CREDENTIALS_SUBDIR)
        .join(PUBLIC_CREDENTIALS_SUBDIR)
}

fn credential_public_path(root: &Path, credential_id: &str) -> PathBuf {
    credential_public_dir(root).join(format!("{}.json", safe_component(credential_id)))
}

fn credential_sealed_path(root: &Path, credential_id: &str) -> PathBuf {
    root.join(PASSKEY_SUBDIR)
        .join(CREDENTIALS_SUBDIR)
        .join(SEALED_CREDENTIALS_SUBDIR)
        .join(format!("{}.age", safe_component(credential_id)))
}

fn write_credential_record(root: &Path, record: &PlatformCredentialRecord) -> Result<(), String> {
    let body = serde_json::to_string_pretty(record)
        .map_err(|e| format!("could not encode platform passkey credential: {e}"))?;
    write_atomic(
        &credential_public_path(root, &record.credential_id_b64url),
        &body,
    )
    .map_err(|e| format!("could not persist platform passkey credential: {e}"))
}

fn read_credential_record(path: &Path) -> Result<PlatformCredentialRecord, String> {
    let body = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "could not read platform passkey credential {}: {e}",
            path.display()
        )
    })?;
    serde_json::from_str(&body).map_err(|e| {
        format!(
            "could not parse platform passkey credential {}: {e}",
            path.display()
        )
    })
}

fn find_credential(
    root: &Path,
    request: &PasskeyRequest,
) -> Result<PlatformCredentialRecord, String> {
    let dir = credential_public_dir(root);
    let entries = std::fs::read_dir(&dir).map_err(|e| {
        format!(
            "platform passkey store unavailable at {}: {e}",
            dir.display()
        )
    })?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let record = read_credential_record(&path)?;
        if record.rp_id != request.rp_id {
            continue;
        }
        if request.allow_credentials.is_empty()
            || request
                .allow_credentials
                .iter()
                .any(|id| id == &record.credential_id_b64url)
        {
            candidates.push(record);
        }
    }
    candidates.sort_by(|a, b| a.credential_id_b64url.cmp(&b.credential_id_b64url));
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no platform passkey credential matches request".to_owned())
}

fn seal_private_key(
    root: &Path,
    key_path: &Path,
    credential_id: &str,
    private_seed_b64url: &str,
) -> Result<(), String> {
    let passphrase = local_passphrase(key_path)?;
    let sealed = mde_seal::seal_bytes(&passphrase, private_seed_b64url.as_bytes())
        .map_err(|e| format!("platform passkey private key seal: {e}"))?;
    let path = credential_sealed_path(root, credential_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "platform passkey private key mkdir {}: {e}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&path, sealed)
        .map_err(|e| format!("platform passkey private key write {}: {e}", path.display()))?;
    set_owner_only(&path);
    Ok(())
}

fn unseal_private_key(root: &Path, key_path: &Path, credential_id: &str) -> Result<String, String> {
    let path = credential_sealed_path(root, credential_id);
    let sealed = std::fs::read(&path)
        .map_err(|e| format!("platform passkey private key read {}: {e}", path.display()))?;
    let passphrase = local_passphrase(key_path)?;
    let plain = mde_seal::unseal_bytes(&passphrase, &sealed).map_err(|e| {
        format!(
            "platform passkey private key unseal {}: {e}",
            path.display()
        )
    })?;
    String::from_utf8(plain).map_err(|e| format!("platform passkey private key utf8: {e}"))
}

fn local_passphrase(key_path: &Path) -> Result<String, String> {
    use std::fmt::Write as _;
    let bytes = std::fs::read(key_path).map_err(|e| {
        format!(
            "platform passkey seal key {} unreadable: {e}",
            key_path.display()
        )
    })?;
    if bytes.is_empty() {
        return Err(format!(
            "platform passkey seal key {} is empty",
            key_path.display()
        ));
    }
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in &bytes {
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

fn set_owner_only(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn new_credential_id() -> String {
    let mut bytes = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    b64url(&bytes)
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64url_decode(value: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|e| format!("base64url decode failed: {e}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HardwareKeyProbe {
    state: String,
    key_count: u64,
    readable_count: u64,
    ctaphid_state: String,
    ctaphid_init_frame_count: u64,
    ctaphid_live_state: String,
    ctaphid_live_channel_id: Option<String>,
    ctaphid_live_protocol_version: Option<u64>,
    ctaphid_live_device_version: Option<String>,
    ctaphid_live_capabilities: Option<u64>,
    ctaphid_live_error: Option<String>,
}

fn probe_hardware_key_status(hidraw_sys_dir: &Path, dev_dir: &Path) -> HardwareKeyProbe {
    probe_hardware_key_status_with_live_probe(hidraw_sys_dir, dev_dir, ctaphid_live_probe_enabled())
}

fn probe_hardware_key_status_with_live_probe(
    hidraw_sys_dir: &Path,
    dev_dir: &Path,
    live_probe: bool,
) -> HardwareKeyProbe {
    let Ok(entries) = std::fs::read_dir(hidraw_sys_dir) else {
        return HardwareKeyProbe {
            state: "unknown".to_owned(),
            key_count: 0,
            readable_count: 0,
            ctaphid_state: "unknown".to_owned(),
            ctaphid_init_frame_count: 0,
            ctaphid_live_state: "unknown".to_owned(),
            ctaphid_live_channel_id: None,
            ctaphid_live_protocol_version: None,
            ctaphid_live_device_version: None,
            ctaphid_live_capabilities: None,
            ctaphid_live_error: None,
        };
    };
    let mut key_count = 0_u64;
    let mut readable_count = 0_u64;
    let mut first_readable_path = None;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("hidraw") {
            continue;
        }
        let descriptor = std::fs::read_to_string(entry.path().join("device").join("uevent"))
            .or_else(|_| std::fs::read_to_string(entry.path().join("device").join("name")))
            .unwrap_or_default();
        if !looks_like_fido_hid(&descriptor) {
            continue;
        }
        key_count = key_count.saturating_add(1);
        let dev_path = dev_dir.join(&name);
        if std::fs::File::open(&dev_path).is_ok() {
            readable_count = readable_count.saturating_add(1);
            if first_readable_path.is_none() {
                first_readable_path = Some(dev_path);
            }
        }
    }
    let state = if key_count == 0 {
        "unavailable"
    } else if readable_count == 0 {
        "present_permission_denied"
    } else {
        "ready"
    };
    let init_frames = if state == "ready" {
        ctaphid_init_request([0_u8; CTAPHID_INIT_NONCE_LEN]).len() as u64
    } else {
        0
    };
    let ctaphid_state = if state == "unknown" {
        "unknown"
    } else if init_frames > 0 {
        "init_request_ready"
    } else {
        "unavailable"
    };
    let mut ctaphid_live_state = if state == "unknown" {
        "unknown".to_owned()
    } else if state == "ready" {
        if live_probe {
            "unavailable".to_owned()
        } else {
            "disabled".to_owned()
        }
    } else {
        "unavailable".to_owned()
    };
    let mut ctaphid_live_channel_id = None;
    let mut ctaphid_live_protocol_version = None;
    let mut ctaphid_live_device_version = None;
    let mut ctaphid_live_capabilities = None;
    let mut ctaphid_live_error = None;
    if live_probe && state == "ready" {
        match first_readable_path
            .as_deref()
            .ok_or_else(|| "no readable FIDO/CTAP hidraw device path found".to_owned())
            .and_then(ctaphid_init_exchange_path)
        {
            Ok(response) => {
                ctaphid_live_state = "init_exchange_ready".to_owned();
                let _nonce = response.nonce;
                ctaphid_live_channel_id = Some(hex_bytes(&response.channel_id));
                ctaphid_live_protocol_version = Some(u64::from(response.protocol_version));
                ctaphid_live_device_version = Some(format!(
                    "{}.{}.{}",
                    response.major, response.minor, response.build
                ));
                ctaphid_live_capabilities = Some(u64::from(response.capabilities));
            }
            Err(error) => {
                ctaphid_live_state = "error".to_owned();
                ctaphid_live_error = Some(error);
            }
        }
    }
    HardwareKeyProbe {
        state: state.to_owned(),
        key_count,
        readable_count,
        ctaphid_state: ctaphid_state.to_owned(),
        ctaphid_init_frame_count: init_frames,
        ctaphid_live_state,
        ctaphid_live_channel_id,
        ctaphid_live_protocol_version,
        ctaphid_live_device_version,
        ctaphid_live_capabilities,
        ctaphid_live_error,
    }
}

fn ctaphid_live_probe_enabled() -> bool {
    std::env::var(CTAPHID_LIVE_PROBE_ENV)
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn looks_like_fido_hid(descriptor: &str) -> bool {
    let upper = descriptor.to_ascii_uppercase();
    [
        "FIDO", "CTAP", "U2F", "YUBICO", "YUBIKEY", "NITROKEY", "SOLOKEY", "ONLYKEY", "FEITIAN",
        "THETIS",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CtapHidInitResponse {
    nonce: [u8; CTAPHID_INIT_NONCE_LEN],
    channel_id: [u8; 4],
    protocol_version: u8,
    major: u8,
    minor: u8,
    build: u8,
    capabilities: u8,
}

fn ctaphid_init_exchange_path(path: &Path) -> Result<CtapHidInitResponse, String> {
    let mut nonce = [0_u8; CTAPHID_INIT_NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(0o4000);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("CTAPHID_INIT open {} failed: {e}", path.display()))?;
    ctaphid_init_exchange(&mut file, nonce)
}

fn ctaphid_init_exchange<T: Read + Write>(
    transport: &mut T,
    nonce: [u8; CTAPHID_INIT_NONCE_LEN],
) -> Result<CtapHidInitResponse, String> {
    for frame in ctaphid_init_request(nonce) {
        transport
            .write_all(&frame)
            .map_err(|e| format!("CTAPHID_INIT write failed: {e}"))?;
    }
    let mut response = [0_u8; CTAPHID_REPORT_SIZE];
    transport
        .read_exact(&mut response)
        .map_err(|e| format!("CTAPHID_INIT read failed: {e}"))?;
    parse_ctaphid_init_response(&response, nonce)
}

fn ctaphid_init_request(nonce: [u8; CTAPHID_INIT_NONCE_LEN]) -> Vec<[u8; CTAPHID_REPORT_SIZE]> {
    encode_ctaphid_request(CTAPHID_BROADCAST_CID, CTAPHID_CMD_INIT, &nonce)
        .expect("CTAPHID_INIT request is fixed-size")
}

fn encode_ctaphid_request(
    channel_id: [u8; 4],
    command: u8,
    payload: &[u8],
) -> Result<Vec<[u8; CTAPHID_REPORT_SIZE]>, String> {
    if command & CTAPHID_COMMAND_BIT != 0 {
        return Err("CTAP HID command must not include the initialization bit".to_owned());
    }
    if payload.len() > CTAPHID_MAX_PAYLOAD {
        return Err(format!(
            "CTAP HID payload too large: {} > {}",
            payload.len(),
            CTAPHID_MAX_PAYLOAD
        ));
    }
    let byte_count =
        u16::try_from(payload.len()).map_err(|_| "CTAP HID payload length overflow".to_owned())?;
    let mut frames =
        Vec::with_capacity(1 + payload.len().saturating_sub(1) / CTAPHID_CONT_PAYLOAD_MAX);
    let mut init = [0_u8; CTAPHID_REPORT_SIZE];
    init[..4].copy_from_slice(&channel_id);
    init[4] = CTAPHID_COMMAND_BIT | command;
    init[5..7].copy_from_slice(&byte_count.to_be_bytes());
    let init_len = payload.len().min(CTAPHID_INIT_PAYLOAD_MAX);
    init[CTAPHID_INIT_HEADER_SIZE..CTAPHID_INIT_HEADER_SIZE + init_len]
        .copy_from_slice(&payload[..init_len]);
    frames.push(init);

    let mut offset = init_len;
    let mut sequence = 0_u8;
    while offset < payload.len() {
        let mut continuation = [0_u8; CTAPHID_REPORT_SIZE];
        continuation[..4].copy_from_slice(&channel_id);
        continuation[4] = sequence;
        let chunk_len = (payload.len() - offset).min(CTAPHID_CONT_PAYLOAD_MAX);
        continuation[CTAPHID_CONT_HEADER_SIZE..CTAPHID_CONT_HEADER_SIZE + chunk_len]
            .copy_from_slice(&payload[offset..offset + chunk_len]);
        frames.push(continuation);
        offset += chunk_len;
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| "CTAP HID continuation sequence overflow".to_owned())?;
    }
    Ok(frames)
}

fn parse_ctaphid_init_response(
    frame: &[u8; CTAPHID_REPORT_SIZE],
    expected_nonce: [u8; CTAPHID_INIT_NONCE_LEN],
) -> Result<CtapHidInitResponse, String> {
    if frame[..4] != CTAPHID_BROADCAST_CID {
        return Err("CTAPHID_INIT response used an unexpected channel".to_owned());
    }
    if frame[4] != (CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT) {
        return Err("CTAPHID_INIT response used an unexpected command".to_owned());
    }
    let byte_count = u16::from_be_bytes([frame[5], frame[6]]) as usize;
    if byte_count != CTAPHID_INIT_RESPONSE_LEN {
        return Err(format!(
            "CTAPHID_INIT response length was {byte_count}, expected {CTAPHID_INIT_RESPONSE_LEN}"
        ));
    }
    let payload = &frame[CTAPHID_INIT_HEADER_SIZE..CTAPHID_INIT_HEADER_SIZE + byte_count];
    let mut nonce = [0_u8; CTAPHID_INIT_NONCE_LEN];
    nonce.copy_from_slice(&payload[..CTAPHID_INIT_NONCE_LEN]);
    if nonce != expected_nonce {
        return Err("CTAPHID_INIT response nonce did not match request".to_owned());
    }
    let mut channel_id = [0_u8; 4];
    channel_id.copy_from_slice(&payload[8..12]);
    if channel_id == [0, 0, 0, 0] || channel_id == CTAPHID_BROADCAST_CID {
        return Err("CTAPHID_INIT response returned an invalid allocated channel".to_owned());
    }
    Ok(CtapHidInitResponse {
        nonce,
        channel_id,
        protocol_version: payload[12],
        major: payload[13],
        minor: payload[14],
        build: payload[15],
        capabilities: payload[16],
    })
}

struct AssertionPayload {
    authenticator_data: Vec<u8>,
    client_data_json: String,
    client_data_hash: [u8; 32],
    signing_bytes: Vec<u8>,
}

struct AssertionEvent {
    authenticator_data_b64url: String,
    client_data_json_b64url: String,
    client_data_hash_b64url: String,
    signature_b64url: String,
    sign_count: u32,
}

/// Attested-credential-data (`AT`) flag bit, set on registration authenticator
/// data because the attested credential *is* appended.
const AUTH_FLAG_AT: u8 = 0x40;

/// Build the WebAuthn authenticator-data flags byte from what actually happened.
///
/// security-2: `UP` (bit 0, user present) and `UV` (bit 2, user verified) are set
/// only when a real presence / verification step occurred — never hardcoded — so
/// the signed assertion does not attest a human interaction that never took
/// place. Callers OR in [`AUTH_FLAG_AT`] for registration.
const fn authenticator_flags(user_present: bool, user_verified: bool) -> u8 {
    let mut flags = 0u8;
    if user_present {
        flags |= 0x01;
    }
    if user_verified {
        flags |= 0x04;
    }
    flags
}

fn assertion_payload(request: &PasskeyRequest, sign_count: u32) -> AssertionPayload {
    let ceremony_type = if request.ceremony == "create" {
        "webauthn.create"
    } else {
        "webauthn.get"
    };
    let client_data_json = serde_json::json!({
        "type": ceremony_type,
        "challenge": request.challenge_b64url,
        "origin": request.origin,
        "crossOrigin": false,
    })
    .to_string();
    let client_data_hash = Sha256::digest(client_data_json.as_bytes());
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&client_data_hash);
    let mut authenticator_data = Vec::with_capacity(37);
    authenticator_data.extend_from_slice(&Sha256::digest(request.rp_id.as_bytes()));
    // Flags byte. security-2: the User Present (`UP`, bit 0) bit is set only when
    // a presence step actually accompanied the ceremony (`request.user_present`),
    // not hardcoded — a ceremony with no verified presence honestly signs UP=0
    // (which a relying party rejects) rather than forging "a human was here".
    // The User Verified (`UV`, bit 2) bit stays 0: no per-ceremony verification
    // (PIN/biometric) exists in this pipeline (THREAT_MODEL.md §7.4.1).
    authenticator_data.push(authenticator_flags(request.user_present, false));
    authenticator_data.extend_from_slice(&sign_count.to_be_bytes());
    let mut signing_bytes = authenticator_data.clone();
    signing_bytes.extend_from_slice(&hash);
    AssertionPayload {
        authenticator_data,
        client_data_json,
        client_data_hash: hash,
        signing_bytes,
    }
}

fn registration_authenticator_data(
    rp_id: &str,
    credential: &PlatformCredentialRecord,
    user_present: bool,
) -> Result<Vec<u8>, String> {
    let credential_id = b64url_decode(&credential.credential_id_b64url)?;
    let public_key_sec1 = b64url_decode(&credential.public_key_sec1_b64url)?;
    if credential_id.len() > u16::MAX as usize {
        return Err("platform passkey credential id too long for authenticator data".to_owned());
    }
    let cose_key = cose_es256_public_key(&public_key_sec1)?;
    let mut out = Vec::with_capacity(32 + 1 + 4 + 16 + 2 + credential_id.len() + cose_key.len());
    out.extend_from_slice(&Sha256::digest(rp_id.as_bytes()));
    // Flags byte: AT (attested credential data is included) is always set on
    // registration; UP is set only when a real presence step occurred, no UV —
    // see the matching comment in `assertion_payload` (security-2).
    out.push(authenticator_flags(user_present, false) | AUTH_FLAG_AT);
    out.extend_from_slice(&credential.sign_count.to_be_bytes());
    out.extend_from_slice(&[0_u8; 16]);
    out.extend_from_slice(&(credential_id.len() as u16).to_be_bytes());
    out.extend_from_slice(&credential_id);
    out.extend_from_slice(&cose_key);
    Ok(out)
}

fn none_attestation_object(auth_data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    cbor_map_len(&mut out, 3);
    cbor_text(&mut out, "fmt");
    cbor_text(&mut out, "none");
    cbor_text(&mut out, "attStmt");
    cbor_map_len(&mut out, 0);
    cbor_text(&mut out, "authData");
    cbor_bytes(&mut out, auth_data);
    out
}

fn cose_es256_public_key(sec1: &[u8]) -> Result<Vec<u8>, String> {
    if sec1.len() != 65 || sec1.first() != Some(&0x04) {
        return Err("platform passkey public key must be uncompressed P-256 SEC1".to_owned());
    }
    let mut out = Vec::new();
    cbor_map_len(&mut out, 5);
    cbor_i64(&mut out, 1);
    cbor_i64(&mut out, 2);
    cbor_i64(&mut out, 3);
    cbor_i64(&mut out, -7);
    cbor_i64(&mut out, -1);
    cbor_i64(&mut out, 1);
    cbor_i64(&mut out, -2);
    cbor_bytes(&mut out, &sec1[1..33]);
    cbor_i64(&mut out, -3);
    cbor_bytes(&mut out, &sec1[33..65]);
    Ok(out)
}

fn spki_der_from_sec1(sec1: &[u8]) -> Result<Vec<u8>, String> {
    if sec1.len() != 65 || sec1.first() != Some(&0x04) {
        return Err("platform passkey public key must be uncompressed P-256 SEC1".to_owned());
    }
    let mut out = Vec::with_capacity(26 + sec1.len());
    out.extend_from_slice(&[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ]);
    out.extend_from_slice(sec1);
    Ok(out)
}

fn cbor_map_len(out: &mut Vec<u8>, len: u64) {
    cbor_major(out, 5, len);
}

fn cbor_text(out: &mut Vec<u8>, value: &str) {
    cbor_major(out, 3, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn cbor_bytes(out: &mut Vec<u8>, value: &[u8]) {
    cbor_major(out, 2, value.len() as u64);
    out.extend_from_slice(value);
}

fn cbor_i64(out: &mut Vec<u8>, value: i64) {
    if value >= 0 {
        cbor_major(out, 0, value as u64);
    } else {
        cbor_major(out, 1, (-1 - value) as u64);
    }
}

fn cbor_major(out: &mut Vec<u8>, major: u8, value: u64) {
    let tag = major << 5;
    match value {
        0..=23 => out.push(tag | value as u8),
        24..=0xff => out.extend_from_slice(&[tag | 24, value as u8]),
        0x100..=0xffff => {
            out.push(tag | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(tag | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(tag | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, path)
}

/// Resolve the local durable passkey ceremony root for this host.
#[must_use]
pub fn resolve_local_root() -> PathBuf {
    dirs::data_dir().map_or_else(
        || PathBuf::from("/var/lib/mde/browser-passkeys"),
        |d| d.join("mde").join("browser-passkeys"),
    )
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect::<String>()
}

fn default_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge() -> &'static str {
        "abcdefghijklmnopqrstuvwxyzABCD"
    }

    fn user_handle() -> &'static str {
        "userHandle_123456"
    }

    fn create_body() -> String {
        serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "work-station/1",
            "engine": "cef",
            "ceremony": "create",
            "origin": "https://login.example.test/account",
            "rp_id": "example.test",
            "challenge_b64url": challenge(),
            "client_request_id": "mde-pk-worker-1",
            "user_handle_b64url": user_handle(),
            "user_name": "alice@example.test",
            "timeout_ms": 60_000,
            "user_present": true
        })
        .to_string()
    }

    fn get_body() -> String {
        serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "servo",
            "ceremony": "get",
            "origin": "https://login.example.test/",
            "rp_id": "login.example.test",
            "challenge_b64url": challenge(),
            "allow_credentials": ["credential_id_123456"],
            "user_present": true
        })
        .to_string()
    }

    fn key_path(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("age.key");
        std::fs::write(&path, b"test mesh age identity").expect("write key");
        path
    }

    fn write_hidraw(sys: &Path, dev: &Path, name: &str, descriptor: &str, readable: bool) {
        let device = sys.join(name).join("device");
        std::fs::create_dir_all(&device).expect("hidraw device dir");
        std::fs::write(device.join("uevent"), descriptor).expect("hidraw uevent");
        if readable {
            std::fs::write(dev.join(name), b"").expect("hidraw dev node");
        }
    }

    #[test]
    fn hardware_key_probe_reports_ready_permission_denied_unavailable_and_unknown() {
        let sys = tempfile::tempdir().unwrap();
        let dev = tempfile::tempdir().unwrap();

        assert_eq!(
            probe_hardware_key_status(&sys.path().join("missing"), dev.path()).state,
            "unknown"
        );
        assert_eq!(
            probe_hardware_key_status(sys.path(), dev.path()),
            HardwareKeyProbe {
                state: "unavailable".to_owned(),
                key_count: 0,
                readable_count: 0,
                ctaphid_state: "unavailable".to_owned(),
                ctaphid_init_frame_count: 0,
                ctaphid_live_state: "unavailable".to_owned(),
                ctaphid_live_channel_id: None,
                ctaphid_live_protocol_version: None,
                ctaphid_live_device_version: None,
                ctaphid_live_capabilities: None,
                ctaphid_live_error: None,
            }
        );

        write_hidraw(
            sys.path(),
            dev.path(),
            "hidraw0",
            "HID_NAME=Yubico YubiKey OTP+FIDO+CCID\n",
            false,
        );
        assert_eq!(
            probe_hardware_key_status(sys.path(), dev.path()),
            HardwareKeyProbe {
                state: "present_permission_denied".to_owned(),
                key_count: 1,
                readable_count: 0,
                ctaphid_state: "unavailable".to_owned(),
                ctaphid_init_frame_count: 0,
                ctaphid_live_state: "unavailable".to_owned(),
                ctaphid_live_channel_id: None,
                ctaphid_live_protocol_version: None,
                ctaphid_live_device_version: None,
                ctaphid_live_capabilities: None,
                ctaphid_live_error: None,
            }
        );

        write_hidraw(
            sys.path(),
            dev.path(),
            "hidraw1",
            "HID_NAME=SoloKeys Solo 2 CTAP authenticator\n",
            true,
        );
        write_hidraw(
            sys.path(),
            dev.path(),
            "hidraw2",
            "HID_NAME=Generic Keyboard\n",
            true,
        );
        assert_eq!(
            probe_hardware_key_status(sys.path(), dev.path()),
            HardwareKeyProbe {
                state: "ready".to_owned(),
                key_count: 2,
                readable_count: 1,
                ctaphid_state: "init_request_ready".to_owned(),
                ctaphid_init_frame_count: 1,
                ctaphid_live_state: "disabled".to_owned(),
                ctaphid_live_channel_id: None,
                ctaphid_live_protocol_version: None,
                ctaphid_live_device_version: None,
                ctaphid_live_capabilities: None,
                ctaphid_live_error: None,
            }
        );
        let live_probe = probe_hardware_key_status_with_live_probe(sys.path(), dev.path(), true);
        assert_eq!(live_probe.state, "ready");
        assert_eq!(live_probe.ctaphid_state, "init_request_ready");
        assert_eq!(live_probe.ctaphid_live_state, "error");
        assert!(live_probe.ctaphid_live_error.is_some());
    }

    struct ScriptedCtapHid {
        writes: Vec<[u8; CTAPHID_REPORT_SIZE]>,
        response: std::io::Cursor<Vec<u8>>,
    }

    impl ScriptedCtapHid {
        fn new(response: [u8; CTAPHID_REPORT_SIZE]) -> Self {
            Self {
                writes: Vec::new(),
                response: std::io::Cursor::new(response.to_vec()),
            }
        }
    }

    impl std::io::Write for ScriptedCtapHid {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut frame = [0_u8; CTAPHID_REPORT_SIZE];
            let len = buf.len().min(CTAPHID_REPORT_SIZE);
            frame[..len].copy_from_slice(&buf[..len]);
            self.writes.push(frame);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Read for ScriptedCtapHid {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.response.read(buf)
        }
    }

    #[test]
    fn ctaphid_init_request_uses_broadcast_channel_and_nonce() {
        let nonce = *b"12345678";
        let frames = ctaphid_init_request(nonce);

        assert_eq!(frames.len(), 1);
        let frame = frames[0];
        assert_eq!(&frame[..4], &CTAPHID_BROADCAST_CID);
        assert_eq!(frame[4], CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT);
        assert_eq!(
            u16::from_be_bytes([frame[5], frame[6]]) as usize,
            CTAPHID_INIT_NONCE_LEN
        );
        assert_eq!(
            &frame[CTAPHID_INIT_HEADER_SIZE..CTAPHID_INIT_HEADER_SIZE + 8],
            b"12345678"
        );
        assert!(frame[CTAPHID_INIT_HEADER_SIZE + 8..]
            .iter()
            .all(|b| *b == 0));
    }

    #[test]
    fn ctaphid_request_frames_split_payloads_and_reject_invalid_shapes() {
        let channel_id = [0x01, 0x02, 0x03, 0x04];
        let payload = (0_u8..60).collect::<Vec<_>>();
        let frames =
            encode_ctaphid_request(channel_id, CTAPHID_CMD_INIT, &payload).expect("split payload");

        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..4], &channel_id);
        assert_eq!(frames[0][4], CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT);
        assert_eq!(u16::from_be_bytes([frames[0][5], frames[0][6]]), 60);
        assert_eq!(
            &frames[0][CTAPHID_INIT_HEADER_SIZE..CTAPHID_REPORT_SIZE],
            &payload[..CTAPHID_INIT_PAYLOAD_MAX]
        );
        assert_eq!(&frames[1][..4], &channel_id);
        assert_eq!(frames[1][4], 0);
        assert_eq!(
            &frames[1][CTAPHID_CONT_HEADER_SIZE..CTAPHID_CONT_HEADER_SIZE + 3],
            &payload[CTAPHID_INIT_PAYLOAD_MAX..]
        );
        assert!(frames[1][CTAPHID_CONT_HEADER_SIZE + 3..]
            .iter()
            .all(|b| *b == 0));

        let oversized = vec![0_u8; CTAPHID_MAX_PAYLOAD + 1];
        assert!(encode_ctaphid_request(channel_id, CTAPHID_CMD_INIT, &oversized).is_err());
        assert!(
            encode_ctaphid_request(channel_id, CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT, &[])
                .is_err()
        );
    }

    #[test]
    fn ctaphid_init_response_parser_validates_nonce_command_length_and_channel() {
        let nonce = *b"abcdefgh";
        let allocated = [0x10, 0x20, 0x30, 0x40];
        let mut frame = [0_u8; CTAPHID_REPORT_SIZE];
        frame[..4].copy_from_slice(&CTAPHID_BROADCAST_CID);
        frame[4] = CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT;
        frame[5..7].copy_from_slice(&(CTAPHID_INIT_RESPONSE_LEN as u16).to_be_bytes());
        frame[7..15].copy_from_slice(&nonce);
        frame[15..19].copy_from_slice(&allocated);
        frame[19] = 2;
        frame[20] = 1;
        frame[21] = 3;
        frame[22] = 5;
        frame[23] = 0x05;

        let parsed = parse_ctaphid_init_response(&frame, nonce).expect("valid init response");
        assert_eq!(parsed.nonce, nonce);
        assert_eq!(parsed.channel_id, allocated);
        assert_eq!(parsed.protocol_version, 2);
        assert_eq!(parsed.major, 1);
        assert_eq!(parsed.minor, 3);
        assert_eq!(parsed.build, 5);
        assert_eq!(parsed.capabilities, 0x05);

        let mut wrong_nonce = frame;
        wrong_nonce[7] ^= 0xff;
        assert!(parse_ctaphid_init_response(&wrong_nonce, nonce).is_err());
        let mut wrong_command = frame;
        wrong_command[4] = CTAPHID_COMMAND_BIT | 0x01;
        assert!(parse_ctaphid_init_response(&wrong_command, nonce).is_err());
        let mut wrong_length = frame;
        wrong_length[6] = 1;
        assert!(parse_ctaphid_init_response(&wrong_length, nonce).is_err());
        let mut invalid_channel = frame;
        invalid_channel[15..19].copy_from_slice(&CTAPHID_BROADCAST_CID);
        assert!(parse_ctaphid_init_response(&invalid_channel, nonce).is_err());
    }

    #[test]
    fn ctaphid_init_exchange_writes_request_and_parses_response() {
        let nonce = *b"liveinit";
        let allocated = [0x01, 0x23, 0x45, 0x67];
        let mut response = [0_u8; CTAPHID_REPORT_SIZE];
        response[..4].copy_from_slice(&CTAPHID_BROADCAST_CID);
        response[4] = CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT;
        response[5..7].copy_from_slice(&(CTAPHID_INIT_RESPONSE_LEN as u16).to_be_bytes());
        response[7..15].copy_from_slice(&nonce);
        response[15..19].copy_from_slice(&allocated);
        response[19] = 2;
        response[20] = 4;
        response[21] = 1;
        response[22] = 9;
        response[23] = 0x05;
        let mut transport = ScriptedCtapHid::new(response);

        let parsed = ctaphid_init_exchange(&mut transport, nonce).expect("init exchange");

        assert_eq!(transport.writes.len(), 1);
        assert_eq!(&transport.writes[0][..4], &CTAPHID_BROADCAST_CID);
        assert_eq!(
            transport.writes[0][4],
            CTAPHID_COMMAND_BIT | CTAPHID_CMD_INIT
        );
        assert_eq!(
            &transport.writes[0][CTAPHID_INIT_HEADER_SIZE..CTAPHID_INIT_HEADER_SIZE + 8],
            b"liveinit"
        );
        assert_eq!(parsed.channel_id, allocated);
        assert_eq!(parsed.protocol_version, 2);
        assert_eq!(parsed.major, 4);
        assert_eq!(parsed.minor, 1);
        assert_eq!(parsed.build, 9);
        assert_eq!(parsed.capabilities, 0x05);
    }

    fn event_with_op(persist: &Persist, op: &str) -> serde_json::Value {
        persist
            .list_since("event/browser-passkeys/node-a", None)
            .expect("list events")
            .into_iter()
            .filter_map(|msg| msg.body)
            .filter_map(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
            .find(|event| event["op"] == op)
            .unwrap_or_else(|| panic!("missing {op} event"))
    }

    fn decoded_client_data(
        event: &serde_json::Value,
        expected_type: &str,
        expected_origin: &str,
    ) -> serde_json::Value {
        let body = b64url_decode(event["client_data_json_b64url"].as_str().unwrap())
            .expect("client data JSON");
        let v: serde_json::Value = serde_json::from_slice(&body).expect("client data");
        assert_eq!(v["type"], expected_type);
        assert_eq!(v["challenge"], challenge());
        assert_eq!(v["origin"], expected_origin);
        assert_eq!(v["crossOrigin"], false);
        v
    }

    fn cbor_len(input: &[u8], pos: &mut usize, major: u8) -> u64 {
        let byte = *input.get(*pos).expect("cbor byte");
        *pos += 1;
        assert_eq!(byte >> 5, major, "unexpected cbor major type");
        let add = byte & 0x1f;
        match add {
            0..=23 => u64::from(add),
            24 => {
                let v = *input.get(*pos).expect("cbor u8");
                *pos += 1;
                u64::from(v)
            }
            25 => {
                let bytes: [u8; 2] = input[*pos..*pos + 2].try_into().expect("cbor u16");
                *pos += 2;
                u64::from(u16::from_be_bytes(bytes))
            }
            26 => {
                let bytes: [u8; 4] = input[*pos..*pos + 4].try_into().expect("cbor u32");
                *pos += 4;
                u64::from(u32::from_be_bytes(bytes))
            }
            27 => {
                let bytes: [u8; 8] = input[*pos..*pos + 8].try_into().expect("cbor u64");
                *pos += 8;
                u64::from_be_bytes(bytes)
            }
            _ => panic!("unsupported cbor additional info"),
        }
    }

    fn cbor_text_value(input: &[u8], pos: &mut usize) -> String {
        let len = cbor_len(input, pos, 3) as usize;
        let bytes = input[*pos..*pos + len].to_vec();
        *pos += len;
        String::from_utf8(bytes).expect("cbor text")
    }

    fn cbor_bytes_value(input: &[u8], pos: &mut usize) -> Vec<u8> {
        let len = cbor_len(input, pos, 2) as usize;
        let bytes = input[*pos..*pos + len].to_vec();
        *pos += len;
        bytes
    }

    fn cbor_int_value(input: &[u8], pos: &mut usize) -> i64 {
        let byte = *input.get(*pos).expect("cbor int");
        match byte >> 5 {
            0 => cbor_len(input, pos, 0) as i64,
            1 => -1 - cbor_len(input, pos, 1) as i64,
            _ => panic!("unexpected cbor integer major type"),
        }
    }

    fn auth_data_from_none_attestation(attestation: &[u8]) -> Vec<u8> {
        let mut pos = 0;
        let len = cbor_len(attestation, &mut pos, 5);
        let mut auth_data = None;
        for _ in 0..len {
            let key = cbor_text_value(attestation, &mut pos);
            match key.as_str() {
                "fmt" => assert_eq!(cbor_text_value(attestation, &mut pos), "none"),
                "attStmt" => assert_eq!(cbor_len(attestation, &mut pos, 5), 0),
                "authData" => auth_data = Some(cbor_bytes_value(attestation, &mut pos)),
                _ => panic!("unexpected attestation key {key}"),
            }
        }
        assert_eq!(pos, attestation.len());
        auth_data.expect("authData")
    }

    fn sec1_public_key_from_registration_auth_data(auth_data: &[u8]) -> Vec<u8> {
        assert!(auth_data.len() > 55);
        let credential_id_len = u16::from_be_bytes([auth_data[53], auth_data[54]]) as usize;
        let mut pos = 55 + credential_id_len;
        let len = cbor_len(auth_data, &mut pos, 5);
        let mut kty = None;
        let mut alg = None;
        let mut crv = None;
        let mut x = None;
        let mut y = None;
        for _ in 0..len {
            let key = cbor_int_value(auth_data, &mut pos);
            match key {
                1 => kty = Some(cbor_int_value(auth_data, &mut pos)),
                3 => alg = Some(cbor_int_value(auth_data, &mut pos)),
                -1 => crv = Some(cbor_int_value(auth_data, &mut pos)),
                -2 => x = Some(cbor_bytes_value(auth_data, &mut pos)),
                -3 => y = Some(cbor_bytes_value(auth_data, &mut pos)),
                _ => panic!("unexpected COSE key {key}"),
            }
        }
        assert_eq!(kty, Some(2));
        assert_eq!(alg, Some(-7));
        assert_eq!(crv, Some(1));
        let x = x.expect("x coordinate");
        let y = y.expect("y coordinate");
        assert_eq!(x.len(), 32);
        assert_eq!(y.len(), 32);
        let mut sec1 = Vec::with_capacity(65);
        sec1.push(0x04);
        sec1.extend_from_slice(&x);
        sec1.extend_from_slice(&y);
        sec1
    }

    #[test]
    fn parse_request_accepts_create_and_get_ceremonies() {
        let create = parse_request(&create_body(), "01REQ").expect("create request");
        assert_eq!(create.id, "01REQ");
        assert_eq!(create.host, "work-station1");
        assert_eq!(create.engine, "cef");
        assert_eq!(create.ceremony, "create");
        assert_eq!(create.client_request_id.as_deref(), Some("mde-pk-worker-1"));
        assert_eq!(create.origin_host, "login.example.test");
        assert_eq!(create.rp_id, "example.test");
        assert_eq!(create.user_name.as_deref(), Some("alice@example.test"));

        let get = parse_request(&get_body(), "02REQ").expect("get request");
        assert_eq!(get.engine, "servo");
        assert_eq!(get.ceremony, "get");
        assert_eq!(get.rp_id, "login.example.test");
        assert_eq!(get.allow_credentials, vec!["credential_id_123456"]);
    }

    #[test]
    fn parse_request_rejects_unsafe_or_mismatched_webauthn_shapes() {
        assert!(parse_request(r#"{"op":"browser_passkey","source":"cloud"}"#, "x").is_err());
        assert!(
            parse_request(
                &create_body().replace("https://login.example.test/account", "http://evil.test/"),
                "x"
            )
            .is_err(),
            "http origins are only accepted for localhost diagnostics"
        );
        let bad_rp =
            create_body().replace(r#""rp_id":"example.test""#, r#""rp_id":"different.test""#);
        assert!(
            parse_request(&bad_rp, "x").is_err(),
            "rp_id must match the origin host or a parent domain"
        );
        assert!(
            parse_request(&create_body().replace(challenge(), "not+base64"), "x").is_err(),
            "challenge must be base64url-shaped"
        );
        assert!(
            parse_request(
                &serde_json::json!({
                    "op": "browser_passkey",
                    "source": "browser",
                    "host": "node-a",
                    "engine": "cef",
                    "ceremony": "create",
                    "origin": "https://example.test/",
                    "rp_id": "example.test",
                    "challenge_b64url": challenge()
                })
                .to_string(),
                "x",
            )
            .is_err(),
            "registration requires user metadata"
        );
    }

    #[test]
    fn apply_request_persists_pending_local_mirror_and_events_without_private_keys() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        let gate = Arc::new(AtomicBool::new(true));
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(gate)
        .with_now_fn(Arc::new(|| 42));
        let request = parse_request(&create_body(), "01REQ").expect("request");

        worker.apply_request(&persist, request);

        let local_body =
            std::fs::read_to_string(pending_path(local.path(), "work-station1", "01REQ"))
                .expect("local pending");
        let share_body =
            std::fs::read_to_string(pending_path(share.path(), "work-station1", "01REQ"))
                .expect("shared pending");
        assert_eq!(local_body, share_body);
        assert!(!local_body.contains("private_key"));
        assert!(!local_body.contains("signature"));
        let record: PendingPasskeyCeremony =
            serde_json::from_str(&local_body).expect("pending JSON");
        assert_eq!(record.state, "pending_platform_authenticator");
        assert_eq!(record.pending_ms, 42);
        assert_eq!(record.mirrored_ms, Some(42));

        let status_body = persist
            .list_since("state/browser-passkeys/node-a", None)
            .expect("list status")
            .pop()
            .unwrap()
            .body
            .unwrap();
        let status: PasskeyStatus = serde_json::from_str(&status_body).unwrap();
        assert_eq!(status.state, "created");
        assert!(status.mirrored);
        assert_eq!(status.accepted, 1);
        assert_eq!(status.last_request_id.as_deref(), Some("01REQ"));
        let credentials = std::fs::read_dir(credential_public_dir(local.path()))
            .expect("credential dir")
            .collect::<Result<Vec<_>, _>>()
            .expect("credential entries");
        assert_eq!(credentials.len(), 1);
        let credential: PlatformCredentialRecord =
            read_credential_record(&credentials[0].path()).expect("credential record");
        assert_eq!(credential.rp_id, "example.test");
        assert_eq!(credential.cose_alg, -7);
        assert_eq!(credential.sign_count, 0);
        assert!(credential.mirrored_ms.is_some());
        let sealed = std::fs::read(credential_sealed_path(
            local.path(),
            &credential.credential_id_b64url,
        ))
        .expect("sealed private key");
        assert!(!sealed.is_empty());
        assert!(
            !String::from_utf8_lossy(&sealed).contains(&credential.public_key_sec1_b64url),
            "sealed private key file is not plaintext public/private JSON"
        );

        let event_body = persist
            .list_since("event/browser-passkeys/node-a", None)
            .expect("list event")
            .pop()
            .unwrap()
            .body
            .unwrap();
        let event: serde_json::Value = serde_json::from_str(&event_body).unwrap();
        assert_eq!(event["op"], "browser_passkey_created");
        assert_eq!(event["source"], "browser_passkeys");
        assert_eq!(event["client_request_id"], "mde-pk-worker-1");
        assert_eq!(event["ceremony"], "create");
        assert_eq!(event["rp_id"], "example.test");
        assert_eq!(
            event["credential_id_b64url"],
            credential.credential_id_b64url
        );
        assert_eq!(
            event["public_key_sec1_b64url"],
            credential.public_key_sec1_b64url
        );
        assert!(
            event["public_key_spki_der_b64url"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "registration event carries SPKI public key bytes for getPublicKey"
        );
        assert!(
            event["client_data_json_b64url"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "registration event carries clientDataJSON for page completion"
        );
        let auth_data =
            b64url_decode(event["authenticator_data_b64url"].as_str().unwrap()).unwrap();
        assert_eq!(
            &auth_data[..32],
            Sha256::digest("example.test".as_bytes()).as_slice()
        );
        // UP+AT, no UV (no per-ceremony verification exists).
        assert_eq!(auth_data[32], 0x41);
        let credential_id_len = u16::from_be_bytes([auth_data[53], auth_data[54]]) as usize;
        let event_credential_id =
            b64url_decode(event["credential_id_b64url"].as_str().unwrap()).expect("credential id");
        assert_eq!(credential_id_len, event_credential_id.len());
        assert_eq!(&auth_data[55..55 + credential_id_len], event_credential_id);
        assert!(
            auth_data[55 + credential_id_len..].starts_with(&[0xa5, 0x01, 0x02, 0x03, 0x26]),
            "registration authData appends an ES256 COSE_Key"
        );
        let attestation =
            b64url_decode(event["attestation_object_b64url"].as_str().unwrap()).unwrap();
        assert!(attestation.starts_with(&[0xa3]));
        assert!(attestation.windows(4).any(|w| w == b"none"));
        assert!(attestation.windows(8).any(|w| w == b"authData"));
    }

    #[test]
    fn apply_request_keeps_local_pending_when_share_is_down() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        let gate = Arc::new(AtomicBool::new(false));
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(gate);
        let request = parse_request(&create_body(), "02REQ").expect("request");

        worker.apply_request(&persist, request);

        assert!(pending_path(local.path(), "work-station1", "02REQ").is_file());
        assert!(!pending_path(share.path(), "work-station1", "02REQ").exists());
        assert!(!worker.status.mirrored);
        assert_eq!(worker.status.state, "created");
    }

    #[test]
    fn get_request_signs_with_stored_platform_credential_and_increments_counter() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(Arc::new(AtomicBool::new(true)));
        let create = parse_request(&create_body(), "01CREATE").expect("create");
        worker.apply_request(&persist, create);
        let credential = std::fs::read_dir(credential_public_dir(local.path()))
            .expect("credential dir")
            .filter_map(Result::ok)
            .map(|entry| read_credential_record(&entry.path()).expect("credential"))
            .next()
            .expect("created credential");
        let get_body = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "servo",
            "ceremony": "get",
            "origin": "https://login.example.test/",
            "rp_id": "example.test",
            "challenge_b64url": challenge(),
            "allow_credentials": [credential.credential_id_b64url],
            "user_present": true,
        })
        .to_string();
        let get = parse_request(&get_body, "02GET").expect("get");

        worker.apply_request(&persist, get);

        assert_eq!(worker.status.state, "asserted");
        let updated = read_credential_record(&credential_public_path(
            local.path(),
            &credential.credential_id_b64url,
        ))
        .expect("updated credential");
        assert_eq!(updated.sign_count, 1);
        let events = persist
            .list_since("event/browser-passkeys/node-a", None)
            .expect("list events");
        let assertion_body = events
            .iter()
            .filter_map(|msg| msg.body.as_deref())
            .find(|body| body.contains("browser_passkey_assertion"))
            .expect("assertion event");
        let event: serde_json::Value = serde_json::from_str(assertion_body).unwrap();
        assert_eq!(event["op"], "browser_passkey_assertion");
        assert_eq!(event["credential_id_b64url"], updated.credential_id_b64url);
        assert_eq!(event["sign_count"], 1);
        let auth_data =
            b64url_decode(event["authenticator_data_b64url"].as_str().unwrap()).expect("auth data");
        let client_hash =
            b64url_decode(event["client_data_hash_b64url"].as_str().unwrap()).expect("client hash");
        let signature = Signature::from_slice(
            &b64url_decode(event["signature_b64url"].as_str().unwrap()).expect("signature"),
        )
        .expect("signature");
        let mut signed = auth_data;
        signed.extend_from_slice(&client_hash);
        let public_key =
            VerifyingKey::from_sec1_bytes(&b64url_decode(&updated.public_key_sec1_b64url).unwrap())
                .expect("public key");
        public_key
            .verify(&signed, &signature)
            .expect("signature verifies");
    }

    #[test]
    fn registration_and_assertion_outputs_verify_like_a_relying_party() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(Arc::new(AtomicBool::new(true)));

        let create = parse_request(&create_body(), "01CREATE").expect("create");
        worker.apply_request(&persist, create);
        let created = event_with_op(&persist, "browser_passkey_created");
        decoded_client_data(
            &created,
            "webauthn.create",
            "https://login.example.test/account",
        );
        let attestation =
            b64url_decode(created["attestation_object_b64url"].as_str().unwrap()).unwrap();
        let auth_data = auth_data_from_none_attestation(&attestation);
        assert_eq!(
            auth_data,
            b64url_decode(created["authenticator_data_b64url"].as_str().unwrap()).unwrap()
        );
        assert_eq!(
            &auth_data[..32],
            Sha256::digest("example.test".as_bytes()).as_slice()
        );
        // UP+AT, no UV (no per-ceremony verification exists).
        assert_eq!(auth_data[32], 0x41);
        assert_eq!(u32::from_be_bytes(auth_data[33..37].try_into().unwrap()), 0);
        let credential_id_len = u16::from_be_bytes([auth_data[53], auth_data[54]]) as usize;
        let credential_id =
            b64url_decode(created["credential_id_b64url"].as_str().unwrap()).unwrap();
        assert_eq!(&auth_data[55..55 + credential_id_len], credential_id);
        let rp_public_key = sec1_public_key_from_registration_auth_data(&auth_data);
        assert_eq!(
            rp_public_key,
            b64url_decode(created["public_key_sec1_b64url"].as_str().unwrap()).unwrap()
        );

        let get_body = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "servo",
            "ceremony": "get",
            "origin": "https://login.example.test/account",
            "rp_id": "example.test",
            "challenge_b64url": challenge(),
            "allow_credentials": [created["credential_id_b64url"].as_str().unwrap()],
            "client_request_id": "mde-pk-worker-2",
            "user_present": true,
        })
        .to_string();
        let get = parse_request(&get_body, "02GET").expect("get");
        worker.apply_request(&persist, get);

        let asserted = event_with_op(&persist, "browser_passkey_assertion");
        decoded_client_data(
            &asserted,
            "webauthn.get",
            "https://login.example.test/account",
        );
        assert_eq!(
            asserted["credential_id_b64url"],
            created["credential_id_b64url"]
        );
        let assertion_auth_data =
            b64url_decode(asserted["authenticator_data_b64url"].as_str().unwrap()).unwrap();
        assert_eq!(
            &assertion_auth_data[..32],
            Sha256::digest("example.test".as_bytes()).as_slice()
        );
        // UP only, no UV (no per-ceremony verification exists).
        assert_eq!(assertion_auth_data[32], 0x01);
        assert_eq!(
            u32::from_be_bytes(assertion_auth_data[33..37].try_into().unwrap()),
            1
        );
        let client_hash =
            b64url_decode(asserted["client_data_hash_b64url"].as_str().unwrap()).unwrap();
        assert_eq!(
            Sha256::digest(
                &b64url_decode(asserted["client_data_json_b64url"].as_str().unwrap()).unwrap()
            )
            .as_slice(),
            &client_hash
        );
        let mut signed = assertion_auth_data;
        signed.extend_from_slice(&client_hash);
        let signature = Signature::from_slice(
            &b64url_decode(asserted["signature_b64url"].as_str().unwrap()).unwrap(),
        )
        .expect("assertion signature");
        VerifyingKey::from_sec1_bytes(&rp_public_key)
            .expect("RP public key")
            .verify(&signed, &signature)
            .expect("RP verifies assertion signature");
    }

    #[test]
    fn drain_requests_tracks_rejections_and_does_not_replay() {
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        persist
            .write(
                ACTION_TOPIC,
                Priority::Default,
                None,
                Some(r#"{"op":"wrong"}"#),
            )
            .expect("bad write");
        persist
            .write(ACTION_TOPIC, Priority::Default, None, Some(&create_body()))
            .expect("good write");
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(Arc::new(AtomicBool::new(true)));

        worker.drain_requests(&persist);
        assert_eq!(worker.status.rejected, 1);
        assert_eq!(worker.status.accepted, 1);
        worker.drain_requests(&persist);
        assert_eq!(worker.status.rejected, 1);
        assert_eq!(worker.status.accepted, 1);
    }

    #[test]
    fn authenticator_flags_reflect_only_what_actually_happened() {
        // security-2: the flags byte is derived, never hardcoded.
        assert_eq!(authenticator_flags(false, false), 0x00);
        assert_eq!(authenticator_flags(true, false), 0x01);
        assert_eq!(authenticator_flags(false, true), 0x04);
        assert_eq!(authenticator_flags(true, true), 0x05);
        // Registration always adds AT (attested credential data present).
        assert_eq!(authenticator_flags(true, false) | AUTH_FLAG_AT, 0x41);
        assert_eq!(authenticator_flags(false, false) | AUTH_FLAG_AT, 0x40);
    }

    #[test]
    fn absent_user_presence_signs_an_honest_up_zero_flag() {
        // security-2: with no presence signal the UP bit is 0 (honest — a relying
        // party rejects it), not a fabricated 1. A present signal sets UP=1.
        let no_presence = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://login.example.test/",
            "rp_id": "example.test",
            "challenge_b64url": challenge(),
            "allow_credentials": ["credential_id_123456"],
        })
        .to_string();
        let request = parse_request(&no_presence, "01NOUP").expect("request");
        assert!(!request.user_present);
        let payload = assertion_payload(&request, 1);
        assert_eq!(payload.authenticator_data[32], 0x00);

        let present = parse_request(&get_body(), "02UP").expect("request");
        assert!(present.user_present);
        assert_eq!(assertion_payload(&present, 1).authenticator_data[32], 0x01);
    }

    #[test]
    fn registration_without_presence_marks_attested_data_but_not_user_present() {
        // security-2: end-to-end — a create ceremony with no presence signal
        // yields registration authData flags = AT only (0x40), never UP.
        let local = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let bus = tempfile::tempdir().unwrap();
        let persist = Persist::open(bus.path().to_path_buf()).unwrap();
        let key_path = key_path(&local);
        let mut worker = BrowserPasskeysWorker::new(
            "node-a".to_owned(),
            local.path().to_path_buf(),
            share.path().to_path_buf(),
        )
        .with_key_path(key_path)
        .with_share_gate(Arc::new(AtomicBool::new(true)));
        let create_body = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "cef",
            "ceremony": "create",
            "origin": "https://login.example.test/account",
            "rp_id": "example.test",
            "challenge_b64url": challenge(),
            "user_handle_b64url": user_handle(),
            "user_name": "alice@example.test",
        })
        .to_string();
        let create = parse_request(&create_body, "01NOUP").expect("create");
        assert!(!create.user_present);
        worker.apply_request(&persist, create);
        let created = event_with_op(&persist, "browser_passkey_created");
        let auth_data =
            b64url_decode(created["authenticator_data_b64url"].as_str().unwrap()).unwrap();
        assert_eq!(auth_data[32], 0x40);
    }

    #[test]
    fn public_suffix_detection_matches_psl_semantics() {
        // Single-label TLDs and listed multi-tenant suffixes are public suffixes.
        assert!(is_public_suffix("com"));
        assert!(is_public_suffix("io"));
        assert!(is_public_suffix("github.io"));
        assert!(is_public_suffix("co.uk"));
        assert!(is_public_suffix("ck"));
        // Registrable domains (eTLD+1 and below) are not.
        assert!(!is_public_suffix("example.com"));
        assert!(!is_public_suffix("attacker.github.io"));
        assert!(!is_public_suffix("example.co.uk"));
        assert!(!is_public_suffix("login.example.test"));
        // Wildcard + exception canonical example.
        assert!(is_public_suffix("foo.ck"));
        assert!(!is_public_suffix("www.ck"));
    }

    #[test]
    fn rp_id_that_is_a_public_suffix_is_rejected() {
        // browser-6: attacker.github.io must not scope a credential to the shared
        // github.io suffix (which would match every *.github.io tenant).
        let attack = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://attacker.github.io/",
            "rp_id": "github.io",
            "challenge_b64url": challenge(),
            "user_present": true,
        })
        .to_string();
        assert!(parse_request(&attack, "01PSL").is_err());

        // A normal registrable domain still works from its subdomain origin.
        let ok = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://foo.example.com/",
            "rp_id": "example.com",
            "challenge_b64url": challenge(),
            "user_present": true,
        })
        .to_string();
        assert_eq!(
            parse_request(&ok, "02PSL")
                .expect("registrable rp_id")
                .rp_id,
            "example.com"
        );

        // A github.io tenant can still use its own full subdomain as the rp_id.
        let tenant = serde_json::json!({
            "op": "browser_passkey",
            "source": "browser",
            "host": "node-a",
            "engine": "cef",
            "ceremony": "get",
            "origin": "https://attacker.github.io/",
            "rp_id": "attacker.github.io",
            "challenge_b64url": challenge(),
            "user_present": true,
        })
        .to_string();
        assert!(parse_request(&tenant, "03PSL").is_ok());
    }
}
