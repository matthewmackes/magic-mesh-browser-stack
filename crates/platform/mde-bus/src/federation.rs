//! WL-SEC-002 — cross-mesh federation **runtime enforcement**.
//!
//! The federation CONFIG lifecycle ([`crate::cli::federation`]: mint / accept /
//! revoke / rotate) writes grants into `<bus_root>/federation.yaml`, but until this
//! module those grants were INERT: no runtime code read `subscribe-topics` /
//! `publish-topics` / `excluded-topics`, `accept` never installed the cross-mesh
//! Nebula trust cert, and nothing checked a foreign mesh's identity at routing.
//!
//! This module is the shared enforcement bridge + the accept/revoke core the CLI
//! and the `mackesd` `federation_enforcer` worker both drive:
//!
//!  * [`FederationGrants`] — the parsed `federation.yaml` (accepted pairs + grants).
//!  * [`FederationGrants::decide`] — the **load-bearing DEFAULT-DENY** decision: a
//!    topic crosses the federation boundary for a peer mesh IFF that mesh has an
//!    ACCEPTED (present, non-revoked) pair AND the topic matches a directional grant
//!    AND is not an excluded topic. Everything else — an unaccepted mesh, a revoked
//!    mesh (revoke removes the pair), an excluded topic, an ungranted topic — is
//!    REFUSED.
//!  * [`accept_passcode`] — consume the single-use mint, write the pair, and INSTALL
//!    the cross-mesh Nebula trust cert (so accepting actually establishes trust).
//!  * [`revoke_pair`] — remove the pair and DELETE the trust cert (symmetric).
//!  * [`CrossMeshEnvelope`] / [`ingress_dir`] — the cross-mesh ingress spool the live
//!    Nebula bridge writes foreign-mesh messages into; the worker drains it through
//!    [`FederationGrants::decide`] so only granted topics from accepted meshes cross.
//!
//! Cite: docs/design/v1.0-federation-pairing.md §1–§8; WL-SEC-002.

#![forbid(unsafe_code)]

use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ── Grant store (federation.yaml) ──────────────────────────────────────────────

/// The parsed `<bus_root>/federation.yaml`: every ESTABLISHED (accepted,
/// non-revoked) federation pair with its directional topic grants. `revoke` removes
/// a pair from this list, so "is there an accepted pair for this mesh?" is exactly
/// "is it present here?".
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FederationGrants {
    /// Established pairs. Empty (or an absent file) means NO foreign mesh is
    /// accepted — the default-deny baseline.
    #[serde(default)]
    pub pairs: Vec<FederationPair>,
}

/// One accepted federation pair + its topic grants. Kebab-case on the wire (the
/// `federation.yaml` schema the CLI has always written).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FederationPair {
    /// The foreign mesh's stable id (the ULID minted when this pair was accepted).
    #[serde(rename = "peer-mesh-id")]
    pub peer_mesh_id: String,
    /// Human label for the remote mesh (shown in the shell).
    #[serde(rename = "peer-mesh-label")]
    pub peer_mesh_label: String,
    /// RFC-3339 timestamp the pair was accepted / last rotated.
    pub established: String,
    /// Topics a foreign-mesh message may cross INBOUND (ingress) into this mesh.
    #[serde(rename = "subscribe-topics", default)]
    pub subscribe_topics: Vec<String>,
    /// Topics this mesh may publish OUTBOUND (egress) into the foreign mesh.
    #[serde(rename = "publish-topics", default)]
    pub publish_topics: Vec<String>,
    /// Topics that NEVER cross in either direction, even when a grant would
    /// otherwise allow them (exclusions always win — the secret/control lanes).
    #[serde(rename = "excluded-topics", default)]
    pub excluded_topics: Vec<String>,
}

/// The default exclusion set every accepted pair starts with: the secret + control
/// lanes that must never cross a federation boundary regardless of grants.
#[must_use]
pub fn default_excluded_topics() -> Vec<String> {
    vec![
        "passcode/*".to_string(),
        "federation/*".to_string(),
        "clipboard/*".to_string(),
        "voip/presence/*".to_string(),
        "input/*".to_string(),
    ]
}

/// Path of the grant store for `bus_root`.
#[must_use]
pub fn federation_yaml_path(bus_root: &Path) -> PathBuf {
    bus_root.join("federation.yaml")
}

/// A grant store is small metadata, not a general-purpose document. Keep an
/// attacker-controlled replica from turning YAML parsing into an allocation
/// sink while leaving ample room for a useful federation roster.
const MAX_FEDERATION_GRANTS_BYTES: u64 = 256 * 1024;

/// A mint envelope contains only a ULID, a six-word mnemonic, an expiry, and a
/// flag. This generous bound is still small enough that a hostile pending-mint
/// entry cannot consume unbounded memory before JSON parsing.
const MAX_MINT_ENVELOPE_BYTES: u64 = 64 * 1024;

/// Read a UTF-8 regular file through one descriptor, without following a final
/// symlink. The descriptor metadata is checked both before and after the
/// bounded read so a file that grows or changes while being consumed is
/// rejected rather than partially materialized.
fn read_bounded_utf8_file(path: &Path, max_bytes: u64) -> io::Result<String> {
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
    if !std::fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-regular file {}", path.display()),
        ));
    }

    let file = options.open(path)?;
    let before = file.metadata()?;
    if !before.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-regular file {}", path.display()),
        ));
    }
    if before.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("file {} exceeds {max_bytes}-byte limit", path.display()),
        ));
    }

    read_bounded_utf8_descriptor(&file, &before, path, max_bytes)
}

/// Read an already-open descriptor. Keeping this separate makes the
/// before/after growth check deterministic in tests without weakening the
/// production path's descriptor-backed read.
fn read_bounded_utf8_descriptor(
    file: &std::fs::File,
    before: &std::fs::Metadata,
    path: &Path,
    max_bytes: u64,
) -> io::Result<String> {
    let max_usize = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let capacity = usize::try_from(before.len())
        .unwrap_or(max_usize)
        .min(max_usize)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;

    let after = file.metadata()?;
    if !after.file_type().is_file()
        || after.len() != before.len()
        || bytes.len() > max_usize
        || bytes.len() as u64 != before.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file {} changed while being read", path.display()),
        ));
    }

    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Read the grant store. An absent file is the honest empty (default-deny) baseline,
/// never an error.
///
/// # Errors
/// Propagates a read or YAML-parse failure.
pub fn read_grants(bus_root: &Path) -> Result<FederationGrants> {
    let path = federation_yaml_path(bus_root);
    let text = match read_bounded_utf8_file(&path, MAX_FEDERATION_GRANTS_BYTES) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(FederationGrants::default());
        }
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Atomically write the grant store (temp + rename).
///
/// # Errors
/// Propagates a serialize / write / rename failure.
pub fn write_grants(bus_root: &Path, grants: &FederationGrants) -> Result<()> {
    let path = federation_yaml_path(bus_root);
    let text = serde_yaml::to_string(grants).context("serialize federation.yaml")?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
    Ok(())
}

// ── Topic matcher (federation dialect: `#`, trailing `*`, `+`, literals) ─────────

/// `true` when the federation topic `pattern` matches the concrete `topic`.
///
/// The federation grant/exclusion dialect (`federation.yaml`) mixes MQTT `#` /
/// `+` with a glob-style trailing `*` (e.g. `passcode/*`, `portal/peer-presence/*`,
/// `#`). Semantics, pinned by the exhaustive tests below:
///
///  * `#` — matches this level and every level below (incl. none); last-segment.
///  * trailing `*` — same as `#`: matches this level and everything below.
///  * a non-terminal `*` or `+` — matches EXACTLY one level (MQTT single-level).
///  * a literal segment — must equal the topic segment.
///
/// A pattern with fewer literal segments than the topic (and no trailing `#`/`*`)
/// does NOT match — the match must consume the whole topic.
#[must_use]
pub fn topic_pattern_matches(pattern: &str, topic: &str) -> bool {
    if pattern.is_empty() || topic.is_empty() {
        return false;
    }
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = topic.split('/').collect();
    let mut pi = 0usize;
    let mut ti = 0usize;
    while pi < p.len() {
        match p[pi] {
            // multi-level tail: matches everything remaining (incl. nothing).
            "#" => return true,
            "*" if pi + 1 == p.len() => return true,
            // single-level wildcard: consume exactly one topic segment.
            "*" | "+" => {
                if ti >= t.len() {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
            literal => {
                if ti >= t.len() || t[ti] != literal {
                    return false;
                }
                pi += 1;
                ti += 1;
            }
        }
    }
    // Pattern exhausted — it matches only if the topic is fully consumed too.
    ti == t.len()
}

// ── The load-bearing decision (default DENY) ────────────────────────────────────

/// Which way a cross-mesh message flows relative to THIS mesh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// A foreign-mesh message arriving INBOUND — gated by `subscribe-topics`.
    Ingress,
    /// A local message flowing OUTBOUND to a foreign mesh — gated by `publish-topics`.
    Egress,
}

/// Why a cross-mesh message was refused (for audit + diagnostics).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    /// No accepted pair for this mesh id — unknown or revoked foreign mesh.
    NoAcceptedPair,
    /// The topic is on the pair's exclusion list (never crosses, grant or not).
    ExcludedTopic,
    /// No directional grant matches the topic.
    NotGranted,
}

impl DenyReason {
    /// Stable machine tag for audit payloads.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            DenyReason::NoAcceptedPair => "no-accepted-pair",
            DenyReason::ExcludedTopic => "excluded-topic",
            DenyReason::NotGranted => "not-granted",
        }
    }
}

/// The enforcement verdict for one cross-mesh message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The topic may cross the boundary for this (accepted) mesh.
    Allow,
    /// The topic is refused, with the reason.
    Deny(DenyReason),
}

impl Decision {
    /// `true` iff the message is allowed to cross.
    #[must_use]
    pub const fn is_allow(self) -> bool {
        matches!(self, Decision::Allow)
    }
}

impl FederationGrants {
    /// The accepted pair for `peer_mesh_id`, if any. `None` = unaccepted OR revoked
    /// (revoke removes the pair) — either way the mesh is not trusted.
    #[must_use]
    pub fn accepted_peer(&self, peer_mesh_id: &str) -> Option<&FederationPair> {
        self.pairs.iter().find(|p| p.peer_mesh_id == peer_mesh_id)
    }

    /// **The load-bearing enforcement decision.** DEFAULT DENY:
    ///
    /// 1. No accepted pair for `peer_mesh_id` ⇒ `Deny(NoAcceptedPair)` — an
    ///    unaccepted or revoked foreign mesh can neither connect nor route.
    /// 2. `topic` matches an excluded pattern ⇒ `Deny(ExcludedTopic)` — exclusions
    ///    win even over an explicit grant.
    /// 3. `topic` matches a directional grant ⇒ `Allow`.
    /// 4. Otherwise ⇒ `Deny(NotGranted)`.
    #[must_use]
    pub fn decide(&self, peer_mesh_id: &str, topic: &str, dir: Direction) -> Decision {
        let Some(pair) = self.accepted_peer(peer_mesh_id) else {
            return Decision::Deny(DenyReason::NoAcceptedPair);
        };
        if pair
            .excluded_topics
            .iter()
            .any(|pat| topic_pattern_matches(pat, topic))
        {
            return Decision::Deny(DenyReason::ExcludedTopic);
        }
        let grants = match dir {
            Direction::Ingress => &pair.subscribe_topics,
            Direction::Egress => &pair.publish_topics,
        };
        if grants.iter().any(|pat| topic_pattern_matches(pat, topic)) {
            Decision::Allow
        } else {
            Decision::Deny(DenyReason::NotGranted)
        }
    }
}

// ── Cross-mesh ingress spool (the routing/ingress seam) ─────────────────────────

/// One foreign-mesh message queued for the enforcement gate. The live Nebula
/// cross-mesh bridge drops these into [`ingress_dir`]; the `federation_enforcer`
/// worker drains each through [`FederationGrants::decide`] (Ingress) and either
/// republishes it onto the local bus (Allow) or drops + audits it (Deny). This is
/// the message-routing identity check: a message carries its ORIGIN mesh id, which
/// must match an accepted, non-revoked grant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CrossMeshEnvelope {
    /// The claimed origin mesh id — checked against the accepted grants.
    #[serde(rename = "peer-mesh-id")]
    pub peer_mesh_id: String,
    /// The bus topic the foreign mesh is trying to publish into this mesh.
    pub topic: String,
    /// The message body (opaque here; forwarded verbatim on Allow).
    #[serde(default)]
    pub body: Option<String>,
    /// Optional title (notification envelopes).
    #[serde(default)]
    pub title: Option<String>,
}

/// The cross-mesh ingress spool directory for `bus_root`.
#[must_use]
pub fn ingress_dir(bus_root: &Path) -> PathBuf {
    bus_root.join("federation-ingress")
}

// ── Mint envelopes (single-use pairing secrets) ─────────────────────────────────

/// A pending mint envelope — the plaintext single-use pairing mnemonic (mode 0600).
#[derive(Debug, Serialize, Deserialize)]
pub struct MintEnvelope {
    /// The pair id this mint establishes on acceptance.
    pub ulid: String,
    /// The 6-word pairing mnemonic (the shared secret).
    pub mnemonic: String,
    /// Expiry (unix ms) — mints are single-use AND time-boxed.
    pub expires_at_unix_ms: i64,
    /// Consumed flag — set true the moment `accept` matches this mint.
    pub used: bool,
}

/// The pending-mints directory for `bus_root`.
#[must_use]
pub fn mints_dir(bus_root: &Path) -> PathBuf {
    bus_root.join("federation-mints")
}

/// Path of a mint envelope by ULID.
#[must_use]
pub fn mint_path(bus_root: &Path, ulid: &str) -> PathBuf {
    mints_dir(bus_root).join(format!("{ulid}.json"))
}

/// Normalize a mnemonic/passcode for comparison: trim, lowercase, single-space.
#[must_use]
pub fn normalize_passcode(s: &str) -> String {
    s.split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Find the pending mint whose mnemonic matches `passcode`, verify it is unexpired
/// and unconsumed, mark it `used` on disk (consume), and return it. **Fails closed:**
/// a passcode with no matching live envelope is rejected — the documented single-use
/// contract (§8), enforced in code, not prose.
///
/// # Errors
/// A passcode with no matching, unused, unexpired mint.
pub fn consume_matching_mint(bus_root: &Path, passcode: &str) -> Result<MintEnvelope> {
    let want = normalize_passcode(passcode);
    let dir = mints_dir(bus_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        bail!("no pending mints — mint a passcode on the peer mesh first");
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue; // skip *.json.tmp and anything else
        }
        let Ok(text) = read_bounded_utf8_file(&path, MAX_MINT_ENVELOPE_BYTES) else {
            continue;
        };
        let Ok(mut env) = serde_json::from_str::<MintEnvelope>(&text) else {
            continue;
        };
        if normalize_passcode(&env.mnemonic) != want {
            continue;
        }
        if env.used {
            bail!("that passcode has already been consumed (single-use)");
        }
        if env.expires_at_unix_ms <= now_unix_ms() {
            bail!("that passcode has expired (mints are valid for 24 h)");
        }
        env.used = true;
        let json = serde_json::to_string_pretty(&env).context("serialize consumed mint")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        return Ok(env);
    }
    bail!("no pending mint matches that passcode — check the words (they may be mistyped, expired, or already consumed)")
}

/// Write a fresh mint envelope (mode 0600) and return it. The mnemonic is generated
/// by the caller (the CLI owns the wordlist).
///
/// # Errors
/// mkdir / write failure.
pub fn write_mint_envelope(bus_root: &Path, env: &MintEnvelope) -> Result<()> {
    let dir = mints_dir(bus_root);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let path = mint_path(bus_root, &env.ulid);
    let json = serde_json::to_string_pretty(env).context("serialize mint envelope")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(json.as_bytes())
        })
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

// ── Cross-mesh Nebula trust cert (install on accept / remove on revoke) ──────────

/// The default trust-anchor directory: peer-mesh CA certs Nebula's CA bundle folds
/// so a foreign mesh's host certs verify. `revoke` deletes an entry here; `accept`
/// (via [`install_trust_cert`]) installs one — that is what makes an accepted
/// federation actually establish trust.
pub const DEFAULT_TRUST_DIR: &str = "/etc/nebula/federation-trusts";

/// The active trust-anchor dir: `MDE_FEDERATION_TRUST_DIR` when set (tests + the
/// two-identity harness point it at a tempdir), else [`DEFAULT_TRUST_DIR`].
#[must_use]
pub fn trust_dir() -> PathBuf {
    match std::env::var_os("MDE_FEDERATION_TRUST_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => PathBuf::from(DEFAULT_TRUST_DIR),
    }
}

/// The trust-anchor path for a peer mesh under `trust_dir`.
#[must_use]
pub fn trust_cert_path(trust_dir: &Path, peer_mesh_id: &str) -> PathBuf {
    trust_dir.join(format!("{peer_mesh_id}.crt"))
}

/// Defence-in-depth: a peer-mesh id used in a filesystem path must be a bare
/// `[A-Za-z0-9_-]` token (the ULID shape), never a path-traversal string.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// A PEM certificate body (Nebula or X.509)? Used to decide whether supplied
/// `peer_ca_pem` is installed verbatim vs. a self-describing trust-anchor record.
fn is_pem_cert(s: &str) -> bool {
    s.contains("-----BEGIN") && s.contains("CERTIFICATE-----")
}

/// The trust-anchor record written when no peer CA PEM is supplied — so the
/// accept↔revoke lifecycle (install-on-accept / remove-on-revoke) still holds and is
/// testable. A live cross-mesh Nebula trust supplies the real peer CA via
/// `mde-bus federation accept --peer-ca <ca.crt>`.
fn trust_anchor_record(peer_mesh_id: &str, established: &str, label: &str) -> String {
    serde_json::json!({
        "kind": "mde-federation-trust-anchor",
        "peer-mesh-id": peer_mesh_id,
        "peer-mesh-label": label,
        "established": established,
        "note": "trust-anchor placeholder — supply the peer mesh CA via \
                 `mde-bus federation accept --peer-ca <ca.crt>` for a live cross-mesh Nebula trust",
    })
    .to_string()
}

/// Install the cross-mesh Nebula trust anchor for `peer_mesh_id` under `trust_dir`
/// (the mirror of [`remove_trust_cert`], which `revoke` calls). When `peer_ca_pem`
/// is a PEM cert it is written verbatim; otherwise a self-describing trust-anchor
/// record is written.
///
/// **Best-effort by parent existence:** if `trust_dir`'s parent does not exist (a
/// build/test box with no `/etc/nebula`), this is a clean no-op returning `Ok(None)`
/// — it never fabricates a `/etc/nebula` tree just to drop a file. A real deployed
/// node (where `nebula_supervisor` has written `/etc/nebula/ca.crt`) installs.
///
/// # Errors
/// An unsafe `peer_mesh_id`, or a mkdir / write / rename failure.
pub fn install_trust_cert(
    trust_dir: &Path,
    peer_mesh_id: &str,
    peer_ca_pem: Option<&str>,
    established: &str,
    label: &str,
) -> io::Result<Option<PathBuf>> {
    if !is_safe_id(peer_mesh_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe peer-mesh-id for a trust-cert path",
        ));
    }
    if let Some(parent) = trust_dir.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Ok(None);
        }
    }
    std::fs::create_dir_all(trust_dir)?;
    let path = trust_cert_path(trust_dir, peer_mesh_id);
    let body = match peer_ca_pem {
        Some(pem) if is_pem_cert(pem) => pem.to_string(),
        _ => trust_anchor_record(peer_mesh_id, established, label),
    };
    let tmp = path.with_extension("crt.tmp");
    std::fs::write(&tmp, body.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(Some(path))
}

/// Remove the cross-mesh trust anchor for `peer_mesh_id` (called by `revoke`).
/// Returns `Ok(true)` when a file was removed, `Ok(false)` when there was none.
///
/// # Errors
/// An unsafe `peer_mesh_id`, or a non-`NotFound` removal failure.
pub fn remove_trust_cert(trust_dir: &Path, peer_mesh_id: &str) -> io::Result<bool> {
    if !is_safe_id(peer_mesh_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe peer-mesh-id for a trust-cert path",
        ));
    }
    let path = trust_cert_path(trust_dir, peer_mesh_id);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// ── Accept / revoke core (the privileged pairing acts) ──────────────────────────

/// What [`accept_passcode`] established — returned so the CLI can print/audit and
/// the worker can publish the updated state mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptOutcome {
    /// The new pair's id.
    pub peer_mesh_id: String,
    /// The human label recorded.
    pub label: String,
    /// The RFC-3339 establish timestamp.
    pub established: String,
    /// The default exclusions applied.
    pub excluded_topics: Vec<String>,
    /// Where the trust cert landed (`None` if the trust dir's parent was absent).
    pub cert_path: Option<PathBuf>,
}

/// **The single accept core** the CLI `accept` and the GUI-driven worker both call.
///
/// Consumes the single-use mint matching `passcode`, writes the established pair to
/// `federation.yaml` (default: subscribe `#`, no publish, the standard exclusions),
/// and — AFTER the pair is durably written (fail-closed: no trust anchor without an
/// established pair) — INSTALLS the cross-mesh Nebula trust cert. Cert install is
/// best-effort (like revoke's delete): a failure warns but does not undo the pair.
///
/// # Errors
/// A non-6-word passcode, no matching/unexpired/unused mint, or a duplicate pair.
pub fn accept_passcode(
    bus_root: &Path,
    trust_dir: &Path,
    passcode: &str,
    label: &str,
    peer_ca_pem: Option<&str>,
) -> Result<AcceptOutcome> {
    let word_count = passcode.split_whitespace().count();
    if word_count != 6 {
        bail!("passcode must be exactly 6 words ({word_count} provided)");
    }
    // §8 — the mnemonic is a single-use secret; it MUST match a pending, unexpired,
    // unconsumed mint. Consuming fails closed, so an arbitrary 6-word string can
    // never establish a pair (the old auth-bypass this replaced).
    let env = consume_matching_mint(bus_root, passcode)?;
    let peer_mesh_id = env.ulid;
    let established = now_rfc3339();

    let mut fed = read_grants(bus_root)?;
    if fed.pairs.iter().any(|p| p.peer_mesh_id == peer_mesh_id) {
        bail!("pair with id {peer_mesh_id} already exists");
    }
    let excluded = default_excluded_topics();
    fed.pairs.push(FederationPair {
        peer_mesh_id: peer_mesh_id.clone(),
        peer_mesh_label: label.to_string(),
        established: established.clone(),
        subscribe_topics: vec!["#".to_string()],
        publish_topics: vec![],
        excluded_topics: excluded.clone(),
    });
    write_grants(bus_root, &fed)?;

    // Trust cert install is the act that makes an accepted federation actually
    // establish trust. Best-effort — a cert-dir write failure (e.g. non-root, or no
    // /etc/nebula on a fresh box) must not roll back an established pair.
    let cert_path =
        match install_trust_cert(trust_dir, &peer_mesh_id, peer_ca_pem, &established, label) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    peer_mesh_id = %peer_mesh_id,
                    error = %e,
                    "federation trust-cert install failed (non-fatal); pair is still established"
                );
                None
            }
        };

    Ok(AcceptOutcome {
        peer_mesh_id,
        label: label.to_string(),
        established,
        excluded_topics: excluded,
        cert_path,
    })
}

/// **The single revoke core** — remove the pair from `federation.yaml` and DELETE
/// the trust cert (symmetric with [`accept_passcode`]). After this the mesh has no
/// accepted pair, so [`FederationGrants::decide`] refuses it again (default-deny).
///
/// # Errors
/// No pair for `peer_mesh_id`, or a grant-store write failure. Cert deletion is
/// best-effort (a failure warns, non-fatal).
pub fn revoke_pair(bus_root: &Path, trust_dir: &Path, peer_mesh_id: &str) -> Result<()> {
    let mut fed = read_grants(bus_root)?;
    let before = fed.pairs.len();
    fed.pairs.retain(|p| p.peer_mesh_id != peer_mesh_id);
    if fed.pairs.len() == before {
        bail!("no pair found for peer-mesh-id {peer_mesh_id}");
    }
    write_grants(bus_root, &fed)?;

    if let Err(e) = remove_trust_cert(trust_dir, peer_mesh_id) {
        tracing::warn!(
            peer_mesh_id = %peer_mesh_id,
            error = %e,
            "federation trust-cert removal failed (non-fatal)"
        );
    }
    Ok(())
}

// ── time helpers ────────────────────────────────────────────────────────────────

/// Wall-clock unix milliseconds (saturating, never panicking).
#[must_use]
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis() as i64
}

/// RFC-3339 wall-clock timestamp for the `established` field.
#[must_use]
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A trust dir whose PARENT exists (so `install_trust_cert` actually writes),
    /// rooted in the tempdir — never `/etc`.
    fn trusts(dir: &Path) -> PathBuf {
        let parent = dir.join("nebula");
        std::fs::create_dir_all(&parent).unwrap();
        parent.join("federation-trusts")
    }

    /// Mint a single-use passcode into `bus_root` and return its mnemonic.
    fn mint(bus_root: &Path) -> String {
        let ulid = ulid::Ulid::new().to_string();
        let env = MintEnvelope {
            ulid: ulid.clone(),
            mnemonic: format!(
                "mesh node link mint mode {}",
                &ulid[..4].to_ascii_lowercase()
            ),
            expires_at_unix_ms: now_unix_ms() + 86_400_000,
            used: false,
        };
        write_mint_envelope(bus_root, &env).unwrap();
        env.mnemonic
    }

    // ── topic matcher ──────────────────────────────────────────────────────────

    #[test]
    fn matcher_hash_matches_everything() {
        assert!(topic_pattern_matches("#", "fleet/sec"));
        assert!(topic_pattern_matches("#", "a/b/c/d"));
        assert!(topic_pattern_matches("#", "single"));
    }

    #[test]
    fn matcher_trailing_star_is_a_prefix_glob() {
        assert!(topic_pattern_matches("passcode/*", "passcode/secret"));
        assert!(topic_pattern_matches(
            "federation/*",
            "federation/pair-established/x"
        ));
        assert!(topic_pattern_matches(
            "portal/peer-presence/*",
            "portal/peer-presence/eagle"
        ));
        // trailing star matches the bare prefix too (parent level).
        assert!(topic_pattern_matches(
            "portal/peer-presence/*",
            "portal/peer-presence"
        ));
    }

    #[test]
    fn matcher_literals_and_plus() {
        assert!(topic_pattern_matches("fleet/sec", "fleet/sec"));
        assert!(!topic_pattern_matches("fleet/sec", "fleet/info"));
        assert!(!topic_pattern_matches("portal/*", "fleet/sec"));
        // `+` single level.
        assert!(topic_pattern_matches("peer/+/alerts", "peer/eagle/alerts"));
        assert!(!topic_pattern_matches(
            "peer/+/alerts",
            "peer/eagle/deep/alerts"
        ));
        // shorter pattern must not partial-match a longer topic.
        assert!(!topic_pattern_matches("fleet", "fleet/sec"));
    }

    // ── the three security decisions ───────────────────────────────────────────

    fn accepted_grants() -> FederationGrants {
        FederationGrants {
            pairs: vec![FederationPair {
                peer_mesh_id: "MESH-A".to_string(),
                peer_mesh_label: "Mesh A".to_string(),
                established: "now".to_string(),
                subscribe_topics: vec!["portal/peer-presence/*".to_string()],
                publish_topics: vec!["chat/mesh/*".to_string()],
                excluded_topics: default_excluded_topics(),
            }],
        }
    }

    #[test]
    fn security_unaccepted_mesh_is_denied() {
        let grants = accepted_grants();
        // A DIFFERENT mesh id — never accepted.
        assert_eq!(
            grants.decide(
                "MESH-UNKNOWN",
                "portal/peer-presence/eagle",
                Direction::Ingress
            ),
            Decision::Deny(DenyReason::NoAcceptedPair)
        );
        // Even a granted-shaped topic from an empty (nothing accepted) store denies.
        let empty = FederationGrants::default();
        assert_eq!(
            empty.decide("MESH-A", "portal/peer-presence/eagle", Direction::Ingress),
            Decision::Deny(DenyReason::NoAcceptedPair)
        );
    }

    #[test]
    fn security_accepted_mesh_allows_only_granted_topics() {
        let grants = accepted_grants();
        // Granted ingress topic → Allow.
        assert!(grants
            .decide("MESH-A", "portal/peer-presence/eagle", Direction::Ingress)
            .is_allow());
        // Granted egress topic → Allow.
        assert!(grants
            .decide("MESH-A", "chat/mesh/room1", Direction::Egress)
            .is_allow());
        // An UNGRANTED topic (accepted mesh, but no matching grant) → Deny.
        assert_eq!(
            grants.decide("MESH-A", "fleet/sec", Direction::Ingress),
            Decision::Deny(DenyReason::NotGranted)
        );
        // An EXCLUDED topic never crosses even though subscribe would otherwise be
        // broad — exclusions win.
        assert_eq!(
            grants.decide("MESH-A", "passcode/anything", Direction::Ingress),
            Decision::Deny(DenyReason::ExcludedTopic)
        );
        assert_eq!(
            grants.decide(
                "MESH-A",
                "federation/pair-established/x",
                Direction::Ingress
            ),
            Decision::Deny(DenyReason::ExcludedTopic)
        );
    }

    #[test]
    fn security_subscribe_all_still_excludes_secret_lanes() {
        // The accept default is subscribe `#` — broad — but exclusions must STILL
        // block the secret/control lanes.
        let grants = FederationGrants {
            pairs: vec![FederationPair {
                peer_mesh_id: "MESH-A".into(),
                peer_mesh_label: "A".into(),
                established: "now".into(),
                subscribe_topics: vec!["#".into()],
                publish_topics: vec![],
                excluded_topics: default_excluded_topics(),
            }],
        };
        assert!(grants
            .decide("MESH-A", "portal/anything/here", Direction::Ingress)
            .is_allow());
        assert_eq!(
            grants.decide("MESH-A", "clipboard/copy", Direction::Ingress),
            Decision::Deny(DenyReason::ExcludedTopic)
        );
        assert_eq!(
            grants.decide("MESH-A", "input/keys", Direction::Ingress),
            Decision::Deny(DenyReason::ExcludedTopic)
        );
    }

    #[test]
    fn security_revoked_mesh_is_denied_again() {
        // Accept then revoke ⇒ the pair is gone ⇒ decide denies (NoAcceptedPair).
        let dir = tmp();
        let bus = dir.path();
        let trust = trusts(bus);
        let mnemonic = mint(bus);
        let outcome = accept_passcode(bus, &trust, &mnemonic, "Peer", None).unwrap();
        let peer = outcome.peer_mesh_id.clone();

        // While accepted, subscribe `#` allows a granted (non-excluded) topic.
        let grants = read_grants(bus).unwrap();
        assert!(grants
            .decide(&peer, "portal/hi", Direction::Ingress)
            .is_allow());

        // Revoke → default-deny returns.
        revoke_pair(bus, &trust, &peer).unwrap();
        let grants = read_grants(bus).unwrap();
        assert_eq!(
            grants.decide(&peer, "portal/hi", Direction::Ingress),
            Decision::Deny(DenyReason::NoAcceptedPair)
        );
    }

    // ── cert install on accept / remove on revoke ──────────────────────────────

    #[test]
    fn accept_installs_trust_cert_and_revoke_removes_it() {
        let dir = tmp();
        let bus = dir.path();
        let trust = trusts(bus);
        let mnemonic = mint(bus);
        let outcome = accept_passcode(bus, &trust, &mnemonic, "Peer", None).unwrap();
        let cert = outcome
            .cert_path
            .expect("cert installed under an existing parent");
        assert!(cert.exists(), "accept installs the trust cert");
        assert_eq!(cert, trust_cert_path(&trust, &outcome.peer_mesh_id));

        revoke_pair(bus, &trust, &outcome.peer_mesh_id).unwrap();
        assert!(!cert.exists(), "revoke removes the trust cert");
    }

    #[test]
    fn accept_installs_supplied_peer_ca_verbatim() {
        let dir = tmp();
        let bus = dir.path();
        let trust = trusts(bus);
        let mnemonic = mint(bus);
        let pem = "-----BEGIN NEBULA CERTIFICATE-----\nAAAA\n-----END NEBULA CERTIFICATE-----\n";
        let outcome = accept_passcode(bus, &trust, &mnemonic, "Peer", Some(pem)).unwrap();
        let cert = outcome.cert_path.unwrap();
        assert_eq!(std::fs::read_to_string(cert).unwrap(), pem);
    }

    #[test]
    fn install_is_a_noop_without_an_existing_parent() {
        // A trust dir whose parent is absent (a box with no /etc/nebula) → clean
        // no-op, never fabricating the tree.
        let dir = tmp();
        let phantom = dir.path().join("no-such-nebula").join("federation-trusts");
        let r = install_trust_cert(&phantom, "MESH-A", None, "now", "A").unwrap();
        assert!(r.is_none());
        assert!(!phantom.exists());
    }

    #[test]
    fn trust_id_rejects_path_traversal() {
        let dir = tmp();
        let trust = trusts(dir.path());
        assert!(install_trust_cert(&trust, "../escape", None, "now", "x").is_err());
        assert!(remove_trust_cert(&trust, "a/b").is_err());
    }

    // ── accept core fails closed ───────────────────────────────────────────────

    #[test]
    fn accept_rejects_wrong_word_count() {
        let dir = tmp();
        let trust = trusts(dir.path());
        assert!(
            accept_passcode(dir.path(), &trust, "only five words here now", "X", None).is_err()
        );
    }

    #[test]
    fn accept_rejects_unmatched_passcode_and_writes_no_pair() {
        let dir = tmp();
        let bus = dir.path();
        let trust = trusts(bus);
        let _ = mint(bus); // a real mint exists, but the passcode below is not it.
        assert!(accept_passcode(
            bus,
            &trust,
            "mesh node link mint mode zzzz",
            "Imposter",
            None
        )
        .is_err());
        assert!(read_grants(bus).unwrap().pairs.is_empty());
    }

    #[test]
    fn accept_is_single_use() {
        let dir = tmp();
        let bus = dir.path();
        let trust = trusts(bus);
        let mnemonic = mint(bus);
        accept_passcode(bus, &trust, &mnemonic, "First", None).unwrap();
        // Replay must fail (single-use) and add no second pair.
        assert!(accept_passcode(bus, &trust, &mnemonic, "Replay", None).is_err());
        assert_eq!(read_grants(bus).unwrap().pairs.len(), 1);
    }

    #[test]
    fn grants_roundtrip_yaml() {
        let dir = tmp();
        let bus = dir.path();
        let g = accepted_grants();
        write_grants(bus, &g).unwrap();
        let raw = std::fs::read_to_string(federation_yaml_path(bus)).unwrap();
        assert!(raw.contains("peer-mesh-id:"));
        assert!(raw.contains("subscribe-topics:"));
        assert_eq!(read_grants(bus).unwrap(), g);
    }

    #[test]
    fn absent_grant_store_remains_default_deny() {
        let dir = tmp();
        assert_eq!(
            read_grants(dir.path()).unwrap(),
            FederationGrants::default()
        );
    }

    #[test]
    fn grant_store_rejects_final_symlink() {
        let dir = tmp();
        let target = dir.path().join("real-federation.yaml");
        std::fs::write(&target, serde_yaml::to_string(&accepted_grants()).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, federation_yaml_path(dir.path())).unwrap();

        assert!(read_grants(dir.path()).is_err());
    }

    #[test]
    fn grant_store_rejects_non_regular_oversized_and_invalid_utf8_files() {
        let dir = tmp();
        let path = federation_yaml_path(dir.path());
        std::fs::create_dir(&path).unwrap();
        assert!(read_grants(dir.path()).is_err());
        std::fs::remove_dir(&path).unwrap();

        std::fs::write(
            &path,
            vec![b' '; usize::try_from(MAX_FEDERATION_GRANTS_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert!(read_grants(dir.path()).is_err());

        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        assert!(read_grants(dir.path()).is_err());
    }

    #[test]
    fn bounded_file_reader_rejects_growth_after_descriptor_snapshot() {
        use std::io::Write as _;

        let dir = tmp();
        let path = dir.path().join("growing.json");
        std::fs::write(&path, b"{}\n").unwrap();
        let file = std::fs::OpenOptions::new().read(true).open(&path).unwrap();
        let before = file.metadata().unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"growth")
            .unwrap();

        assert!(
            read_bounded_utf8_descriptor(&file, &before, &path, MAX_MINT_ENVELOPE_BYTES).is_err()
        );
    }

    #[test]
    fn pending_mint_reads_skip_final_symlink() {
        let dir = tmp();
        let bus = dir.path();
        let env = MintEnvelope {
            ulid: "symlinked".to_string(),
            mnemonic: "mesh node link mint mode symlink".to_string(),
            expires_at_unix_ms: now_unix_ms() + 86_400_000,
            used: false,
        };
        let target = bus.join("outside-mint.json");
        std::fs::write(&target, serde_json::to_vec(&env).unwrap()).unwrap();
        std::fs::create_dir_all(mints_dir(bus)).unwrap();
        std::os::unix::fs::symlink(&target, mint_path(bus, &env.ulid)).unwrap();

        assert!(consume_matching_mint(bus, &env.mnemonic).is_err());
    }

    #[test]
    fn pending_mint_reads_skip_non_regular_oversized_and_invalid_utf8_files() {
        let dir = tmp();
        let bus = dir.path();
        let mint_dir = mints_dir(bus);
        std::fs::create_dir_all(&mint_dir).unwrap();

        std::fs::create_dir(mint_dir.join("directory.json")).unwrap();
        assert!(consume_matching_mint(bus, "directory entry").is_err());
        std::fs::remove_dir(mint_dir.join("directory.json")).unwrap();

        std::fs::write(
            mint_dir.join("oversized.json"),
            vec![b' '; usize::try_from(MAX_MINT_ENVELOPE_BYTES + 1).unwrap()],
        )
        .unwrap();
        assert!(consume_matching_mint(bus, "oversized entry").is_err());

        std::fs::write(mint_dir.join("invalid.json"), [0xff, 0xfe, 0xfd]).unwrap();
        assert!(consume_matching_mint(bus, "invalid entry").is_err());
    }
}
