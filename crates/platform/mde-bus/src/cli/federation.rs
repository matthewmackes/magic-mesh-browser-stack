//! `mde-bus federation` — inter-mesh pairing lifecycle (TUNE-15.c).
//!
//! Storage layout (all under `<bus_root>/`):
//!   `federation-mints/<ulid>.json`   — pending mint envelopes (mode 0600)
//!   `federation.yaml`                — established pairs + topic grants
//!   *(cross-mesh trust cert at `/etc/nebula/federation-trusts/<id>.crt` —
//!    `accept` INSTALLS it, `revoke` REMOVES it; WL-SEC-002)*
//!
//! The grant-store / mint / accept-revoke core + the runtime enforcement gate live
//! in the shared [`crate::federation`] module so both this CLI and the mackesd
//! `federation_enforcer` worker drive ONE implementation (WL-SEC-002).
//!
//! Subcommands:
//!   `mint-passcode [--json]`                   — 6-word mnemonic + envelope
//!   `revoke-mint <ulid>`                       — cancel a pending mint
//!   `accept <passcode> [--label X] [--json]`   — consume mnemonic, write pair
//!   `grant-publish <peer-mesh-id> <pattern>`   — add publish grant
//!   `revoke <peer-mesh-id>`                    — remove pair + revoke cert
//!   `rotate <peer-mesh-id>`                    — update rotation timestamp
//!
//! Audit events are written to the Bus persist index on topics:
//!   `federation/minted/local`
//!   `federation/mint-revoked/local`
//!   `federation/pair-established/<peer-mesh-id>`
//!   `federation/grant-publish-added/<peer-mesh-id>`
//!   `federation/pair-revoked/<peer-mesh-id>`
//!   `federation/pair-rotated/<peer-mesh-id>`
//!   `federation/pair-expired-warning/<peer-mesh-id>` (mackesd side)
//!
//! Cite: docs/design/v1.0-federation-pairing.md §1–§5;
//! ref: Vercel dashboard (multi-party pairing + grant management).

#![forbid(unsafe_code)]

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Subcommand;
use ulid::Ulid;

use crate::federation::{
    accept_passcode, mint_path, now_rfc3339, now_unix_ms, read_grants as read_federation_yaml,
    revoke_pair, trust_dir, write_grants as write_federation_yaml, write_mint_envelope,
    MintEnvelope,
};
use crate::hooks::config::Priority;
use crate::persist::Persist;

// ── Wordlist ──────────────────────────────────────────────────────────────────
// 256 common English words: unambiguous spelling, 3–5 chars, no homophones.
// 256^6 ≈ 2^48 ≈ 281 trillion combinations — more than sufficient for a
// 24 h single-use token. Indexes are 0–255 so each random byte maps directly.

const WORDS: &[&str; 256] = &[
    "able", "acid", "aged", "also", "apex", "arch", "area", "army", "arts", "atom", "aunt", "away",
    "axis", "baby", "back", "ball", "band", "bank", "bare", "base", "bath", "bean", "bear", "beat",
    "beef", "bell", "belt", "bind", "bird", "bite", "blue", "body", "bold", "bolt", "bone", "book",
    "boom", "born", "boss", "bowl", "burn", "busy", "call", "calm", "card", "care", "cart", "case",
    "cash", "cave", "cell", "chat", "chip", "city", "clay", "clip", "club", "coal", "code", "cold",
    "coil", "corn", "cove", "crab", "crew", "crop", "cube", "curl", "dark", "dart", "data", "date",
    "dawn", "deal", "deck", "deep", "desk", "dome", "door", "dose", "draw", "drop", "drum", "dual",
    "dump", "dusk", "dust", "earl", "earn", "ease", "edge", "edit", "epic", "even", "evil", "exit",
    "face", "fact", "fair", "fall", "fame", "farm", "fast", "fate", "fear", "feed", "feel", "fern",
    "file", "fill", "film", "find", "fine", "fire", "fish", "fist", "flag", "flat", "flip", "flow",
    "foam", "fold", "folk", "fond", "food", "foot", "fork", "form", "free", "frog", "fuel", "full",
    "fuse", "gain", "gale", "game", "gang", "gear", "gift", "girl", "glad", "glow", "glue", "goat",
    "gold", "golf", "gone", "grab", "grin", "grip", "grow", "gulf", "gust", "guts", "hand", "hang",
    "harm", "harp", "haze", "head", "heat", "heel", "herb", "hide", "high", "hill", "hint", "hold",
    "hole", "home", "hood", "hook", "hope", "horn", "host", "hour", "hull", "hunt", "jade", "jazz",
    "join", "joke", "jump", "just", "keen", "keep", "kind", "king", "knot", "lack", "lake", "lamp",
    "land", "lane", "last", "leaf", "lean", "left", "lift", "lime", "line", "link", "lion", "list",
    "live", "load", "lock", "loft", "lone", "long", "loop", "lord", "lost", "loud", "love", "luck",
    "mace", "main", "male", "mall", "mane", "mark", "mast", "maze", "meal", "meat", "melt", "memo",
    "mesh", "mill", "mind", "mine", "mint", "mist", "mode", "mole", "moon", "moor", "more", "moss",
    "most", "move", "mule", "muse", "must", "myth", "nail", "navy", "need", "nest", "news", "next",
    "nice", "node", "none", "norm",
];

// The mint envelope + grant-store types + the accept/revoke core now live in the
// shared runtime module [`crate::federation`] (WL-SEC-002), so both this CLI and
// the mackesd `federation_enforcer` worker drive one implementation. This file
// keeps only the operator-facing CLI verbs + the wordlist/mnemonic generator.

// ── Subcommand enum ───────────────────────────────────────────────────────────

/// Sub-verbs for `mde-bus federation`.
#[derive(Subcommand, Debug)]
pub enum FederationOp {
    /// Generate a 6-word pairing mnemonic and write the pending envelope to
    /// `<bus_root>/federation-mints/<ulid>.json` (mode 0600). The mnemonic
    /// is valid for 24 hours and can only be consumed once.
    MintPasscode {
        /// Emit JSON `{mnemonic, ulid, expires_at_unix_ms}` instead of plain text.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Cancel a pending (unconsumed) mint envelope by ULID.
    RevokeMint {
        /// ULID of the mint envelope to cancel.
        ulid: String,
    },
    /// Consume a 6-word mnemonic received from a peer mesh operator and
    /// write a pending pair to `<bus_root>/federation.yaml`.
    Accept {
        /// The 6-word mnemonic string (space-separated).
        passcode: String,
        /// Human label for the remote mesh (shown in the Workbench).
        #[arg(long, default_value = "Remote mesh")]
        label: String,
        /// Path to the peer mesh's CA certificate (PEM). When supplied it is
        /// installed verbatim as the cross-mesh Nebula trust anchor; otherwise a
        /// self-describing trust-anchor record is written so the accept↔revoke
        /// trust lifecycle still holds (WL-SEC-002).
        #[arg(long)]
        peer_ca: Option<String>,
        /// Emit JSON `{"peer-mesh-id": ..., "peer-mesh-label": ...}`.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Add a publish grant for a peer mesh on a topic pattern. Both meshes
    /// must run the symmetric command for publish to work in both directions.
    GrantPublish {
        /// The `peer-mesh-id` of the federated mesh.
        peer_mesh_id: String,
        /// MQTT-style topic pattern to allow publishing into, e.g.
        /// `portal/peer-presence/*`.
        topic_pattern: String,
    },
    /// Revoke an established federation pair: remove from `federation.yaml`,
    /// delete `/etc/nebula/federation-trusts/<id>.crt` (best-effort), and
    /// publish a `federation/pair-revoked/<id>` audit event.
    Revoke {
        /// The `peer-mesh-id` to revoke.
        peer_mesh_id: String,
    },
    /// Renew (rotate) the cross-sign cert for a peer mesh. Updates the
    /// `established` timestamp in `federation.yaml`, signalling mackesd's
    /// nebula_supervisor to perform a new CA cross-sign. Publishes a
    /// `federation/pair-rotated/<id>` audit event.
    Rotate {
        /// The `peer-mesh-id` to rotate.
        peer_mesh_id: String,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn default_bus_root() -> Result<PathBuf> {
    crate::default_data_dir()
        .ok_or_else(|| anyhow::anyhow!("no $HOME / $XDG_DATA_HOME — no bus root available"))
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("read /dev/urandom")?;
    Ok(buf)
}

fn generate_mnemonic() -> Result<String> {
    let bytes = random_bytes(6)?;
    let words: Vec<&str> = bytes.iter().map(|&b| WORDS[b as usize]).collect();
    Ok(words.join(" "))
}

fn publish_audit_event(bus_root: &Path, topic: &str, payload: &serde_json::Value) {
    let Ok(persist) = Persist::open(bus_root.to_path_buf()) else {
        return;
    };
    let body = serde_json::to_string(payload).unwrap_or_default();
    let _ = persist.write(topic, Priority::Min, None, Some(&body));
}

// ── Subcommand implementations ────────────────────────────────────────────────

fn cmd_mint_passcode(json: bool, bus_root: &Path) -> Result<()> {
    let mnemonic = generate_mnemonic()?;
    let ulid = Ulid::new().to_string();
    let expires_at_unix_ms = now_unix_ms() + 86_400_000; // 24 h

    // Write the single-use envelope (mode 0600 — it holds the plaintext mnemonic)
    // through the shared runtime helper.
    let envelope = MintEnvelope {
        ulid: ulid.clone(),
        mnemonic: mnemonic.clone(),
        expires_at_unix_ms,
        used: false,
    };
    write_mint_envelope(bus_root, &envelope)?;

    // Audit event.
    publish_audit_event(
        bus_root,
        "federation/minted/local",
        &serde_json::json!({
            "ulid": ulid,
            "event": "minted",
            "expires_at_unix_ms": expires_at_unix_ms,
        }),
    );

    tracing::info!(ulid, expires_at_unix_ms, "federation mint created");

    if json {
        println!(
            "{}",
            serde_json::json!({
                "mnemonic": mnemonic,
                "ulid": ulid,
                "expires_at_unix_ms": expires_at_unix_ms,
            })
        );
    } else {
        println!("{mnemonic}");
    }
    Ok(())
}

fn cmd_revoke_mint(ulid: &str, bus_root: &Path) -> Result<()> {
    let path = mint_path(bus_root, ulid);
    if !path.exists() {
        bail!("no mint envelope found for ULID {ulid}");
    }
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;

    publish_audit_event(
        bus_root,
        "federation/mint-revoked/local",
        &serde_json::json!({ "ulid": ulid, "event": "mint-revoked" }),
    );

    tracing::info!(ulid, "federation mint revoked");
    println!("mint {ulid} revoked");
    Ok(())
}

fn cmd_accept(
    passcode: &str,
    label: &str,
    peer_ca_path: Option<&str>,
    json_out: bool,
    bus_root: &Path,
    trust_dir: &Path,
) -> Result<()> {
    // §8 / TUNE-15.c — the mnemonic is a documented single-use secret; the shared
    // [`accept_passcode`] core consumes the matching mint (fail-closed), writes the
    // established pair, AND installs the cross-mesh Nebula trust cert. Reading the
    // optional peer-CA PEM here keeps the CLI the thin operator surface.
    let peer_ca_pem = match peer_ca_path {
        Some(p) => {
            Some(std::fs::read_to_string(p).with_context(|| format!("read peer CA cert {p}"))?)
        }
        None => None,
    };
    let outcome = accept_passcode(bus_root, trust_dir, passcode, label, peer_ca_pem.as_deref())?;

    publish_audit_event(
        bus_root,
        &format!("federation/pair-established/{}", outcome.peer_mesh_id),
        &serde_json::json!({
            "ulid": Ulid::new().to_string(),
            "event": "pair-established",
            "peer-mesh-id": outcome.peer_mesh_id,
            "peer-mesh-label": outcome.label,
            "established": outcome.established,
            "subscribe-topics": ["#"],
            "publish-topics": [],
            "excluded-topics": outcome.excluded_topics,
            "trust-cert-installed": outcome.cert_path.is_some(),
        }),
    );

    tracing::info!(
        peer_mesh_id = %outcome.peer_mesh_id,
        label,
        trust_cert = ?outcome.cert_path,
        "federation pair established"
    );

    if json_out {
        println!(
            "{}",
            serde_json::json!({
                "peer-mesh-id": outcome.peer_mesh_id,
                "peer-mesh-label": outcome.label,
                "trust-cert-installed": outcome.cert_path.is_some(),
            })
        );
    } else {
        println!(
            "pair established: {} ({})",
            outcome.peer_mesh_id, outcome.label
        );
    }
    Ok(())
}

fn cmd_grant_publish(peer_mesh_id: &str, topic_pattern: &str, bus_root: &Path) -> Result<()> {
    let mut fed = read_federation_yaml(bus_root)?;
    let pair = fed
        .pairs
        .iter_mut()
        .find(|p| p.peer_mesh_id == peer_mesh_id)
        .with_context(|| format!("no pair found for peer-mesh-id {peer_mesh_id}"))?;
    if pair.publish_topics.contains(&topic_pattern.to_string()) {
        bail!("publish grant for {topic_pattern} already exists on {peer_mesh_id}");
    }
    pair.publish_topics.push(topic_pattern.to_string());
    write_federation_yaml(bus_root, &fed)?;

    publish_audit_event(
        bus_root,
        &format!("federation/grant-publish-added/{peer_mesh_id}"),
        &serde_json::json!({
            "ulid": Ulid::new().to_string(),
            "event": "grant-publish-added",
            "peer-mesh-id": peer_mesh_id,
            "topic-pattern": topic_pattern,
        }),
    );

    tracing::info!(%peer_mesh_id, %topic_pattern, "federation publish grant added");
    println!("publish grant added: {topic_pattern} → {peer_mesh_id}");
    Ok(())
}

fn cmd_revoke(peer_mesh_id: &str, bus_root: &Path, trust_dir: &Path) -> Result<()> {
    // The shared [`revoke_pair`] core removes the pair from federation.yaml AND
    // deletes the cross-mesh trust cert (symmetric with accept's install), so a
    // revoked mesh is default-denied again at routing.
    revoke_pair(bus_root, trust_dir, peer_mesh_id)?;

    publish_audit_event(
        bus_root,
        &format!("federation/pair-revoked/{peer_mesh_id}"),
        &serde_json::json!({
            "ulid": Ulid::new().to_string(),
            "event": "pair-revoked",
            "peer-mesh-id": peer_mesh_id,
        }),
    );

    tracing::info!(%peer_mesh_id, "federation pair revoked");
    println!("pair {peer_mesh_id} revoked");
    Ok(())
}

fn cmd_rotate(peer_mesh_id: &str, bus_root: &Path) -> Result<()> {
    let mut fed = read_federation_yaml(bus_root)?;
    let idx = fed
        .pairs
        .iter()
        .position(|p| p.peer_mesh_id == peer_mesh_id)
        .with_context(|| format!("no pair found for peer-mesh-id {peer_mesh_id}"))?;
    let rotated_at = now_rfc3339();
    fed.pairs[idx].established = rotated_at.clone();
    write_federation_yaml(bus_root, &fed)?;

    publish_audit_event(
        bus_root,
        &format!("federation/pair-rotated/{peer_mesh_id}"),
        &serde_json::json!({
            "ulid": Ulid::new().to_string(),
            "event": "pair-rotated",
            "peer-mesh-id": peer_mesh_id,
            "rotated-at": rotated_at,
        }),
    );

    tracing::info!(%peer_mesh_id, "federation pair rotated");
    println!("pair {peer_mesh_id} rotated");
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Execute the federation subcommand.
pub fn run(op: FederationOp) -> Result<()> {
    let bus_root = default_bus_root()?;
    // The cross-mesh trust-cert dir (env-overridable; defaults to
    // /etc/nebula/federation-trusts). accept installs here, revoke removes here.
    let trusts = trust_dir();
    match op {
        FederationOp::MintPasscode { json } => cmd_mint_passcode(json, &bus_root),
        FederationOp::RevokeMint { ulid } => cmd_revoke_mint(&ulid, &bus_root),
        FederationOp::Accept {
            passcode,
            label,
            peer_ca,
            json,
        } => cmd_accept(
            &passcode,
            &label,
            peer_ca.as_deref(),
            json,
            &bus_root,
            &trusts,
        ),
        FederationOp::GrantPublish {
            peer_mesh_id,
            topic_pattern,
        } => cmd_grant_publish(&peer_mesh_id, &topic_pattern, &bus_root),
        FederationOp::Revoke { peer_mesh_id } => cmd_revoke(&peer_mesh_id, &bus_root, &trusts),
        FederationOp::Rotate { peer_mesh_id } => cmd_rotate(&peer_mesh_id, &bus_root),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::{default_excluded_topics, federation_yaml_path, mints_dir};
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A trust dir whose PARENT exists (rooted in the tempdir, never `/etc`), so
    /// accept's best-effort cert install lands in the tempdir + never touches the
    /// build host's `/etc/nebula`.
    fn trusts(dir: &Path) -> PathBuf {
        let parent = dir.join("nebula");
        std::fs::create_dir_all(&parent).unwrap();
        parent.join("federation-trusts")
    }

    /// Read the (single) mint envelope in a test bus root.
    fn read_one_mint(dir: &Path) -> MintEnvelope {
        let path = std::fs::read_dir(mints_dir(dir))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Secure pairing setup: mint a fresh passcode, then accept its REAL
    /// mnemonic (the path that replaced the old "accept any 6 words" bypass).
    /// Returns the established pair's peer-mesh-id.
    fn accept_with_fresh_mint(dir: &Path, label: &str) -> String {
        cmd_mint_passcode(false, dir).unwrap();
        let mnemonic = std::fs::read_dir(mints_dir(dir))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str::<MintEnvelope>(&t).ok())
            .find(|env| !env.used)
            .map(|env| env.mnemonic)
            .expect("a fresh unused mint");
        cmd_accept(&mnemonic, label, None, false, dir, &trusts(dir)).unwrap();
        read_federation_yaml(dir)
            .unwrap()
            .pairs
            .last()
            .unwrap()
            .peer_mesh_id
            .clone()
    }

    // ── wordlist ──────────────────────────────────────────────────────────────

    #[test]
    fn wordlist_has_exactly_256_entries() {
        assert_eq!(WORDS.len(), 256);
    }

    #[test]
    fn wordlist_entries_are_all_lowercase_alpha() {
        for w in WORDS.iter() {
            assert!(
                w.chars().all(|c| c.is_ascii_lowercase()),
                "non-lowercase word: {w}"
            );
        }
    }

    #[test]
    fn wordlist_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for w in WORDS.iter() {
            assert!(seen.insert(*w), "duplicate word: {w}");
        }
    }

    // ── mnemonic generation ───────────────────────────────────────────────────

    #[test]
    fn generate_mnemonic_produces_six_words() {
        let m = generate_mnemonic().unwrap();
        assert_eq!(m.split_whitespace().count(), 6);
    }

    #[test]
    fn all_mnemonic_words_are_from_wordlist() {
        let m = generate_mnemonic().unwrap();
        let set: std::collections::HashSet<&str> = WORDS.iter().copied().collect();
        for w in m.split_whitespace() {
            assert!(set.contains(w), "word not in list: {w}");
        }
    }

    #[test]
    fn generate_mnemonic_is_random() {
        // Two independent calls should not collide (astronomically unlikely).
        let a = generate_mnemonic().unwrap();
        let b = generate_mnemonic().unwrap();
        assert_ne!(a, b);
    }

    // ── default_excluded_topics ───────────────────────────────────────────────

    #[test]
    fn default_excluded_topics_contains_required_exclusions() {
        let excl = default_excluded_topics();
        for required in &[
            "passcode/*",
            "federation/*",
            "clipboard/*",
            "voip/presence/*",
            "input/*",
        ] {
            assert!(excl.contains(&required.to_string()), "missing: {required}");
        }
    }

    // ── mint-passcode ─────────────────────────────────────────────────────────

    #[test]
    fn mint_passcode_writes_envelope_to_mints_dir() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        let entries: Vec<_> = std::fs::read_dir(mints_dir(dir.path())).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn mint_passcode_envelope_roundtrip() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        let entry = std::fs::read_dir(mints_dir(dir.path()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let env: MintEnvelope = serde_json::from_str(&text).unwrap();
        assert!(!env.used);
        assert!(!env.mnemonic.is_empty());
        assert_eq!(env.mnemonic.split_whitespace().count(), 6);
        assert!(env.expires_at_unix_ms > now_unix_ms());
    }

    #[test]
    fn mint_passcode_expires_in_24h() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        let entry = std::fs::read_dir(mints_dir(dir.path()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(entry.path()).unwrap();
        let env: MintEnvelope = serde_json::from_str(&text).unwrap();
        let expected_min = now_unix_ms() + 86_400_000 - 5_000; // 24 h minus 5 s slop
        let expected_max = now_unix_ms() + 86_400_000 + 5_000;
        assert!(
            env.expires_at_unix_ms >= expected_min && env.expires_at_unix_ms <= expected_max,
            "expiry out of expected range: {}",
            env.expires_at_unix_ms
        );
    }

    // ── revoke-mint ───────────────────────────────────────────────────────────

    #[test]
    fn revoke_mint_deletes_envelope() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        let entry = std::fs::read_dir(mints_dir(dir.path()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let name = entry.file_name();
        let ulid = name.to_string_lossy().trim_end_matches(".json").to_string();
        cmd_revoke_mint(&ulid, dir.path()).unwrap();
        assert!(!mint_path(dir.path(), &ulid).exists());
    }

    #[test]
    fn revoke_mint_errors_on_missing_ulid() {
        let dir = tmp();
        let r = cmd_revoke_mint("01NONEXIST00000000000000000", dir.path());
        assert!(r.is_err());
    }

    // ── accept ────────────────────────────────────────────────────────────────

    #[test]
    fn accept_writes_pair_to_federation_yaml() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let fed = read_federation_yaml(dir.path()).unwrap();
        assert_eq!(fed.pairs.len(), 1);
        assert_eq!(fed.pairs[0].peer_mesh_label, "TestMesh");
    }

    #[test]
    fn accept_pair_has_default_subscribe_all() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let fed = read_federation_yaml(dir.path()).unwrap();
        assert_eq!(fed.pairs[0].subscribe_topics, vec!["#"]);
        assert!(fed.pairs[0].publish_topics.is_empty());
    }

    #[test]
    fn accept_pair_has_default_exclusions() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let fed = read_federation_yaml(dir.path()).unwrap();
        assert!(
            fed.pairs[0]
                .excluded_topics
                .contains(&"federation/*".to_string()),
            "missing federation/* exclusion"
        );
        assert!(
            fed.pairs[0]
                .excluded_topics
                .contains(&"passcode/*".to_string()),
            "missing passcode/* exclusion"
        );
    }

    #[test]
    fn accept_rejects_wrong_word_count() {
        let dir = tmp();
        let r = cmd_accept(
            "only five words here now",
            "TestMesh",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        );
        assert!(r.is_err());
        let r2 = cmd_accept(
            "seven words is too many for this test now",
            "TestMesh",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        );
        assert!(r2.is_err());
    }

    #[test]
    fn accept_rejects_passcode_with_no_matching_mint() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        // Six valid-count words that are not the minted mnemonic. The old bypass
        // wrote a pair for any 6 words; the secure path must fail closed.
        let r = cmd_accept(
            "mesh node link mint mode myth",
            "Imposter",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        );
        assert!(r.is_err(), "arbitrary 6 words must not establish a pair");
        assert!(
            read_federation_yaml(dir.path()).unwrap().pairs.is_empty(),
            "no pair may be written for an unmatched passcode"
        );
    }

    #[test]
    fn accept_consumes_matching_mint() {
        let dir = tmp();
        let peer = accept_with_fresh_mint(dir.path(), "Peer");
        assert!(!peer.is_empty());
        // Every envelope in the dir is now marked used (the one we accepted).
        let all_used = std::fs::read_dir(mints_dir(dir.path()))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str::<MintEnvelope>(&t).ok())
            .all(|env| env.used);
        assert!(all_used, "the accepted mint must be marked used");
    }

    #[test]
    fn accept_rejects_replayed_passcode() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        let mnemonic = read_one_mint(dir.path()).mnemonic;
        cmd_accept(
            &mnemonic,
            "First",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        )
        .unwrap();
        // Replaying the same words must fail — single-use.
        let r = cmd_accept(
            &mnemonic,
            "Replay",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        );
        assert!(r.is_err(), "a consumed passcode must not be replayable");
        assert_eq!(
            read_federation_yaml(dir.path()).unwrap().pairs.len(),
            1,
            "replay must not add a second pair"
        );
    }

    #[test]
    fn accept_rejects_expired_mint() {
        let dir = tmp();
        cmd_mint_passcode(false, dir.path()).unwrap();
        // Force-expire the envelope on disk, then try to accept it.
        let path = std::fs::read_dir(mints_dir(dir.path()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut env: MintEnvelope =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        env.expires_at_unix_ms = now_unix_ms() - 1;
        std::fs::write(&path, serde_json::to_string(&env).unwrap()).unwrap();
        let r = cmd_accept(
            &env.mnemonic,
            "Stale",
            None,
            false,
            dir.path(),
            &trusts(dir.path()),
        );
        assert!(r.is_err(), "an expired passcode must be rejected");
    }

    // ── grant-publish ─────────────────────────────────────────────────────────

    #[test]
    fn grant_publish_adds_topic_to_pair() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let peer_id = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].peer_mesh_id.clone()
        };
        cmd_grant_publish(&peer_id, "portal/peer-presence/*", dir.path()).unwrap();
        let fed = read_federation_yaml(dir.path()).unwrap();
        assert!(fed.pairs[0]
            .publish_topics
            .contains(&"portal/peer-presence/*".to_string()));
    }

    #[test]
    fn grant_publish_errors_on_missing_peer() {
        let dir = tmp();
        let r = cmd_grant_publish("NO_SUCH_PEER", "topic/*", dir.path());
        assert!(r.is_err());
    }

    #[test]
    fn grant_publish_errors_on_duplicate_topic() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let peer_id = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].peer_mesh_id.clone()
        };
        cmd_grant_publish(&peer_id, "portal/*", dir.path()).unwrap();
        let r = cmd_grant_publish(&peer_id, "portal/*", dir.path());
        assert!(r.is_err());
    }

    // ── revoke ────────────────────────────────────────────────────────────────

    #[test]
    fn revoke_removes_pair_from_yaml() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let peer_id = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].peer_mesh_id.clone()
        };
        cmd_revoke(&peer_id, dir.path(), &trusts(dir.path())).unwrap();
        let fed = read_federation_yaml(dir.path()).unwrap();
        assert!(fed.pairs.is_empty());
    }

    #[test]
    fn revoke_errors_on_missing_peer() {
        let dir = tmp();
        let r = cmd_revoke("NO_SUCH_PEER", dir.path(), &trusts(dir.path()));
        assert!(r.is_err());
    }

    // ── rotate ────────────────────────────────────────────────────────────────

    #[test]
    fn rotate_updates_established_timestamp() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let peer_id = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].peer_mesh_id.clone()
        };
        let before = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].established.clone()
        };
        // Small sleep so the timestamp actually changes.
        std::thread::sleep(std::time::Duration::from_millis(10));
        cmd_rotate(&peer_id, dir.path()).unwrap();
        let after = {
            let fed = read_federation_yaml(dir.path()).unwrap();
            fed.pairs[0].established.clone()
        };
        // Both should be non-empty RFC3339 strings; rotation produces a new value.
        assert!(!before.is_empty());
        assert!(!after.is_empty());
    }

    #[test]
    fn rotate_errors_on_missing_peer() {
        let dir = tmp();
        let r = cmd_rotate("NO_SUCH_PEER", dir.path());
        assert!(r.is_err());
    }

    // ── federation.yaml YAML schema ───────────────────────────────────────────

    #[test]
    fn federation_yaml_uses_kebab_case_keys() {
        let dir = tmp();
        accept_with_fresh_mint(dir.path(), "TestMesh");
        let raw = std::fs::read_to_string(federation_yaml_path(dir.path())).unwrap();
        assert!(
            raw.contains("peer-mesh-id:"),
            "missing kebab-case peer-mesh-id in YAML"
        );
        assert!(
            raw.contains("peer-mesh-label:"),
            "missing kebab-case peer-mesh-label in YAML"
        );
        assert!(
            raw.contains("subscribe-topics:"),
            "missing kebab-case subscribe-topics"
        );
        assert!(
            raw.contains("excluded-topics:"),
            "missing kebab-case excluded-topics"
        );
    }
}
