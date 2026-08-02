//! `mde-seal` — the passphrase-sealed byte envelope, extracted verbatim from
//! `mackesd`'s `ca::backup` module (arch-7) so the three consumers share ONE
//! audited crypto path: CA disaster-recovery bundles, the VPN secret store's
//! local-AEAD fallback, and `browser_passkeys`.
//!
//! The binary envelope [`seal_bytes`] produces / [`unseal_bytes`] consumes is:
//!
//!   [0..4]   Magic   `MNCA` ("Mackes Nebula CA Archive")
//!   [4]      Version `0x01`
//!   [5..21]  Salt    16 random bytes — Argon2id input
//!   [21..45] Nonce   24 random bytes — XChaCha20-Poly1305
//!   [45..]   Ciphertext (XChaCha20-Poly1305 over the plaintext bytes)
//!
//! Crypto choices (best-choice per iteration skill standing authorizations,
//! locked 2026-05-24; unchanged by this extraction):
//!
//!   * **KDF:** Argon2id, default params (t=2, m=19456 KiB, p=1). Picks the
//!     OWASP 2023 baseline; trades off ~1 s of derivation time on a desktop for
//!     memory-hard resistance.
//!   * **AEAD:** XChaCha20-Poly1305 (24-byte nonce). The wider nonce eliminates
//!     birthday-bound concerns even under random-nonce-per-message policy.
//!   * **Versioned envelope:** future swaps (AES-GCM, libsodium, etc.) ship as a
//!     new version byte without breaking old sealed blobs. Today only `0x01`
//!     exists.
//!
//! Threat model: an adversary with stolen sealed bytes, offline-attacker
//! compute, no online oracle. They need to brute-force the passphrase to recover
//! the plaintext. Argon2id's memory hardness raises the per-guess cost well past
//! commodity-GPU brute-force feasibility for any passphrase ≥ 8 random chars.

use std::path::PathBuf;

use rand::RngCore;

// WL-SEC-003 — role/scope-targeted sealing: the pure resolver that narrows a
// secret's recipient set from the whole mesh down to the nodes whose published
// role / capability-scope tags match a `--scope` selector. The asymmetric
// multi-recipient seal itself stays `age`'s job (in `mcnf-secret.sh`); this crate
// owns the selector grammar + the recipient-set resolution both the shell helper
// and any Rust caller share.
pub mod scope;
pub use scope::{recipients_for, NodeRecipient, ScopeError, SealScope};

/// Bundle magic — distinguishes our envelope from generic
/// base64 blobs. ASCII "MNCA".
pub const BUNDLE_MAGIC: &[u8; 4] = b"MNCA";

/// Current bundle version. New crypto swaps bump this byte +
/// add a new arm in the unseal path.
pub const BUNDLE_VERSION: u8 = 0x01;

/// Argon2id salt length (16 bytes — OWASP minimum).
pub const SALT_LEN: usize = 16;

/// XChaCha20-Poly1305 nonce length.
pub const NONCE_LEN: usize = 24;

/// Header length before the ciphertext starts:
/// magic + version + salt + nonce.
pub const HEADER_LEN: usize = 4 + 1 + SALT_LEN + NONCE_LEN;

/// The mesh age **identity** (private) — the only host-local artifact of the
/// secret store, distributed to leader-eligible nodes like the mesh SSH key.
/// Overridable via `MCNF_AGE_KEY` to match the script's own default. Used by the
/// local-AEAD fallback to derive its key (the mesh store proper uses the key via
/// the `age` CLI inside the script).
const DEFAULT_AGE_KEY: &str = "/root/.mcnf-age-key";

/// Errors the seal/unseal path can hit. Each variant carries
/// operator-actionable copy so the CLI doesn't have to assemble
/// hint strings.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    /// Argon2id KDF failure — almost always
    /// "bad parameter shape" from a malformed bundle header.
    #[error("kdf: {0}")]
    Kdf(String),
    /// AEAD seal/unseal failure. On unseal: usually wrong
    /// passphrase OR tampered ciphertext (both surface the same
    /// AEAD-tag-mismatch error from the underlying crate).
    #[error("aead: {0} (wrong passphrase, or bundle tampered)")]
    Aead(String),
    /// Bundle header didn't parse (magic mismatch, unknown
    /// version, truncated bytes).
    #[error("bundle format: {0}")]
    Format(String),
    /// Plaintext JSON didn't deserialize. Symptom of a corrupt
    /// or version-mismatched export.
    #[error("plaintext json: {0}")]
    Json(String),
    /// ASCII-armor decode failure.
    #[error("ascii armor: {0}")]
    Armor(String),
    /// Caller passed an empty passphrase. We reject early
    /// rather than letting Argon2 derive a weak key.
    #[error("empty passphrase")]
    EmptyPassphrase,
}

/// Encrypt an arbitrary byte payload under the versioned envelope
/// (magic + version + salt + nonce + ciphertext).
///
/// Exposed so every passphrase-sealed-blob caller (the CA disaster-recovery
/// bundle, the VPN secret store's local-AEAD fallback, and the per-seat
/// passkey seed) reuses the one audited Argon2id + XChaCha20-Poly1305 path
/// rather than re-rolling AEAD.
///
/// # Errors
///
/// Per [`BackupError`] (empty passphrase, KDF, or AEAD failure).
pub fn seal_bytes(passphrase: &str, plaintext: &[u8]) -> Result<Vec<u8>, BackupError> {
    if passphrase.is_empty() {
        return Err(BackupError::EmptyPassphrase);
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce);

    let key = derive_key(passphrase.as_bytes(), &salt)?;
    let ciphertext = aead_seal(&key, &nonce, plaintext)?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(BUNDLE_MAGIC);
    out.push(BUNDLE_VERSION);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a payload sealed by [`seal_bytes`], returning the raw
/// plaintext bytes.
///
/// # Errors
///
/// Per [`BackupError`]. Wrong passphrase + tampered ciphertext
/// both surface as `Aead` (intentional — the AEAD-tag-mismatch
/// error is indistinguishable, and exposing the distinction
/// would help an attacker confirm a tamper attempt).
///
/// # Panics
///
/// Never in practice: the salt/nonce fixed-slice conversions are
/// guarded by the `sealed.len() >= HEADER_LEN` check above, so the
/// `try_into` always has exactly the bytes it needs.
pub fn unseal_bytes(passphrase: &str, sealed: &[u8]) -> Result<Vec<u8>, BackupError> {
    if passphrase.is_empty() {
        return Err(BackupError::EmptyPassphrase);
    }
    if sealed.len() < HEADER_LEN {
        return Err(BackupError::Format(format!(
            "bundle too short: {} bytes (header alone needs {})",
            sealed.len(),
            HEADER_LEN
        )));
    }
    if &sealed[..4] != BUNDLE_MAGIC {
        return Err(BackupError::Format(
            "magic mismatch — not a Mackes Nebula CA bundle".to_string(),
        ));
    }
    let version = sealed[4];
    if version != BUNDLE_VERSION {
        return Err(BackupError::Format(format!(
            "unknown version {version}; this build expects {BUNDLE_VERSION}",
        )));
    }
    let salt: [u8; SALT_LEN] = sealed[5..21].try_into().expect("16 bytes");
    let nonce: [u8; NONCE_LEN] = sealed[21..45].try_into().expect("24 bytes");
    let ciphertext = &sealed[HEADER_LEN..];

    let key = derive_key(passphrase.as_bytes(), &salt)?;
    aead_unseal(&key, &nonce, ciphertext)
}

/// The mesh age identity path (`MCNF_AGE_KEY` env, else [`DEFAULT_AGE_KEY`]) —
/// matches the script's own default so both secret-store backends key off the
/// same artifact. Lives here so consumers that seal against the mesh age
/// identity (the VPN local-AEAD store, browser passkeys) reach it without
/// depending on `mackesd`.
#[must_use]
pub fn age_key_path() -> PathBuf {
    std::env::var_os("MCNF_AGE_KEY").map_or_else(|| PathBuf::from(DEFAULT_AGE_KEY), PathBuf::from)
}

// ----- internals --------------------------------------------

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; 32], BackupError> {
    use argon2::{Algorithm, Argon2, Params, Version};
    // OWASP 2023 baseline for Argon2id (t=2, m=19456 KiB, p=1).
    let params = Params::new(19_456, 2, 1, Some(32))
        .map_err(|e| BackupError::Kdf(format!("params: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|e| BackupError::Kdf(e.to_string()))?;
    Ok(key)
}

fn aead_seal(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
) -> Result<Vec<u8>, BackupError> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::XChaCha20Poly1305;
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .encrypt(nonce.into(), plaintext)
        .map_err(|e| BackupError::Aead(e.to_string()))
}

fn aead_unseal(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>, BackupError> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::XChaCha20Poly1305;
    let cipher = XChaCha20Poly1305::new(key.into());
    cipher
        .decrypt(nonce.into(), ciphertext)
        .map_err(|e| BackupError::Aead(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex pair"))
            .collect()
    }

    // Fixed inputs the pinned test-vectors below are generated from. Changing
    // these WITHOUT regenerating the vectors is a test bug; changing the crypto
    // (KDF params, AEAD algorithm, framing) makes the pinned vectors fail —
    // which is exactly the silent-format-drift guard this extraction requires.
    const PIN_PASSPHRASE: &str = "mde-seal-pinned-passphrase-v1";
    const PIN_PLAINTEXT: &[u8] = b"mde-seal pinned plaintext \x00\x01\xfe\xff round-trip";

    #[test]
    fn seal_then_unseal_round_trips_bytes() {
        let pt = b"the quick brown fox \x00 jumps";
        let sealed = seal_bytes("correct horse battery staple", pt).expect("seal");
        let back = unseal_bytes("correct horse battery staple", &sealed).expect("unseal");
        assert_eq!(back, pt, "round-trip must recover the exact bytes");
    }

    #[test]
    fn seal_frames_magic_version_salt_nonce_then_ciphertext() {
        // Pins the on-wire framing offsets + the Poly1305 tag overhead so a
        // future change to the envelope layout can't slip through silently.
        let pt = b"payload";
        let sealed = seal_bytes("pp", pt).expect("seal");
        assert_eq!(&sealed[0..4], BUNDLE_MAGIC, "magic at [0..4]");
        assert_eq!(sealed[4], BUNDLE_VERSION, "version byte at [4]");
        assert_eq!(HEADER_LEN, 45, "magic(4)+version(1)+salt(16)+nonce(24)");
        // XChaCha20-Poly1305 appends a 16-byte tag; ciphertext len == pt len.
        assert_eq!(
            sealed.len(),
            HEADER_LEN + pt.len() + 16,
            "header + ciphertext(plaintext len) + 16-byte AEAD tag"
        );
    }

    #[test]
    fn seal_bytes_rejects_empty_passphrase() {
        assert!(matches!(
            seal_bytes("", b"x"),
            Err(BackupError::EmptyPassphrase)
        ));
    }

    #[test]
    fn unseal_bytes_rejects_empty_passphrase() {
        assert!(matches!(
            unseal_bytes("", &[0u8; 200]),
            Err(BackupError::EmptyPassphrase)
        ));
    }

    #[test]
    fn unseal_rejects_wrong_passphrase_as_aead() {
        let sealed = seal_bytes("right", b"secret").expect("seal");
        assert!(matches!(
            unseal_bytes("wrong", &sealed),
            Err(BackupError::Aead(_))
        ));
    }

    #[test]
    fn unseal_rejects_truncated_bundle() {
        match unseal_bytes("any", &[0u8; 10]) {
            Err(BackupError::Format(msg)) => assert!(msg.contains("too short")),
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn unseal_rejects_bad_magic() {
        let mut bad = vec![0u8; HEADER_LEN + 10];
        bad[..4].copy_from_slice(b"NOPE");
        match unseal_bytes("any", &bad) {
            Err(BackupError::Format(msg)) => assert!(msg.contains("magic mismatch")),
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn unseal_rejects_unknown_version() {
        let mut bad = vec![0u8; HEADER_LEN + 10];
        bad[..4].copy_from_slice(BUNDLE_MAGIC);
        bad[4] = 0xFF;
        match unseal_bytes("any", &bad) {
            Err(BackupError::Format(msg)) => assert!(msg.contains("unknown version")),
            other => panic!("expected Format, got {other:?}"),
        }
    }

    #[test]
    fn unseal_rejects_tampered_ciphertext() {
        let mut sealed = seal_bytes("right", b"secret payload here").expect("seal");
        sealed[HEADER_LEN + 5] ^= 0x01;
        assert!(matches!(
            unseal_bytes("right", &sealed),
            Err(BackupError::Aead(_))
        ));
    }

    #[test]
    fn binary_safe_over_all_byte_values() {
        let blob: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let sealed = seal_bytes("pp-binary", &blob).expect("seal");
        let back = unseal_bytes("pp-binary", &sealed).expect("unseal");
        assert_eq!(back, blob);
    }

    // ---- pinned crypto test-vectors (silent-format-drift guard) ----
    //
    // A sealed blob generated ONCE from (PIN_PASSPHRASE, PIN_PLAINTEXT) and
    // hardcoded. Because the random salt + nonce are embedded IN the blob, the
    // decrypt is deterministic across builds: if the Argon2id params, the AEAD
    // algorithm, or the header framing ever change, unseal of this exact vector
    // stops recovering PIN_PLAINTEXT and this test fails loudly.

    const PIN_SEALED_HEX: &str = "4d4e4341013eac451679bc60968d49d3869cbb6b4337776bc477d0747de28b26371f9cde2c906082d4a4fceaafee49989a4fef1e18a17e76b45a59e8c8dbf524521994b3c4e862646e1d2a2a9e6f0ee8437e1e9b1afffb96908e5842f8e15f07d4c0d3ddbed5";

    /// Emitter — run with `--nocapture` to (re)generate the pinned vectors,
    /// then paste the hex into the consts above. `#[ignore]` so it never runs
    /// in the normal suite (it prints, it doesn't assert).
    #[test]
    #[ignore = "vector emitter — run manually with --nocapture to regenerate"]
    fn emit_pinned_vectors() {
        let sealed = seal_bytes(PIN_PASSPHRASE, PIN_PLAINTEXT).expect("seal");
        let salt = [0x42u8; SALT_LEN];
        let key = derive_key(PIN_PASSPHRASE.as_bytes(), &salt).expect("derive");
        println!("PIN_SEALED_HEX = \"{}\";", to_hex(&sealed));
        println!("PIN_DERIVE_KEY_HEX = \"{}\";", to_hex(&key));
    }

    #[test]
    fn pinned_vector_still_unseals_to_known_plaintext() {
        if PIN_SEALED_HEX == "__EMIT__" {
            return; // placeholder until the emitter has been run
        }
        let sealed = from_hex(PIN_SEALED_HEX);
        let back = unseal_bytes(PIN_PASSPHRASE, &sealed).expect("pinned vector must unseal");
        assert_eq!(
            back, PIN_PLAINTEXT,
            "pinned crypto vector drifted — KDF params / AEAD / framing changed"
        );
    }

    const PIN_DERIVE_KEY_HEX: &str =
        "5c5b7c81ad9cd85833c4e51464a284164c2d82550edc4cf6dc4c6b225d01b690";

    #[test]
    fn derive_key_pins_argon2id_params() {
        if PIN_DERIVE_KEY_HEX == "__EMIT__" {
            return; // placeholder until the emitter has been run
        }
        // Fixed salt so the derived key is deterministic — pins Argon2id
        // (algorithm, version V0x13, m=19456, t=2, p=1, 32-byte output).
        let salt = [0x42u8; SALT_LEN];
        let key = derive_key(PIN_PASSPHRASE.as_bytes(), &salt).expect("derive");
        assert_eq!(
            to_hex(&key),
            PIN_DERIVE_KEY_HEX,
            "Argon2id params drifted from the OWASP baseline (t=2, m=19456, p=1)"
        );
    }

    #[test]
    fn age_key_path_default_and_override() {
        // Default when the env var is unset.
        std::env::remove_var("MCNF_AGE_KEY");
        assert_eq!(age_key_path(), PathBuf::from("/root/.mcnf-age-key"));
        // Honors the override.
        std::env::set_var("MCNF_AGE_KEY", "/custom/age.key");
        assert_eq!(age_key_path(), PathBuf::from("/custom/age.key"));
        std::env::remove_var("MCNF_AGE_KEY");
    }
}
