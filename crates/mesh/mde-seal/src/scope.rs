//! WL-SEC-003 — role/scope-targeted sealing (scoped decryption roots).
//!
//! The mesh secret store (`automation/secrets/mcnf-secret.sh`) `age`-encrypts a
//! secret to a SET of recipients — every registered node's public `age1…` key —
//! so any node holding one of those identities decrypts the same ciphertext. That
//! is *whole-mesh* sealing: the blast radius of any one secret is the entire mesh.
//!
//! This module narrows that radius. A secret can be sealed to a **role** (e.g.
//! `role:lighthouse`) or a **capability/scope** (e.g. `scope:media`) — the
//! recipient set is then *exactly* the nodes whose published role / scope tags
//! match the selector, and no others. Because `age` wraps the per-file key ONLY
//! to the recipients in the set, a non-matching node's key is simply never a
//! recipient: it holds valid mesh credentials yet still **cannot decrypt** the
//! scoped ciphertext. That is the scoped decryption root — the cryptographic
//! boundary is the recipient set, not a policy check.
//!
//! ## The model is pure resolution
//!
//! The asymmetric multi-recipient seal itself is `age`'s job (it lives in the
//! shell helper's `_seal_to_set`). What lives *here*, in the leaf crate, is the
//! **resolver**: given a [`SealScope`] selector and a roster of
//! [`NodeRecipient`]s (each node's public recipient + its published role/scope
//! tags), [`recipients_for`] returns the exact subset of `age1…` recipients the
//! secret must be sealed to. The shell's `recipient_set_scoped` mirrors the same
//! rule over etcd, and `mcnf-secret.sh put --scope <selector>` feeds the result
//! to `age`.
//!
//! ## Where the roster comes from
//!
//! Each node publishes its own tags at `mcnf-secret.sh init-self` time (an
//! untrusted node can only ever advertise ITS OWN public key + tags): the role
//! from `mde-role` (`lighthouse` / `workstation`) and any capability/scope tags
//! (the `WORKER_CAPABILITIES` vocabulary — e.g. `media`). A node with no tags
//! only ever matches [`SealScope::WholeMesh`], so scoped seals never leak to an
//! untagged legacy recipient. Whole-mesh (no `--scope`) stays byte-for-byte the
//! old behavior — every registered recipient — so the change is backward
//! compatible.

use std::collections::BTreeSet;

/// The selector prefix naming a deployment **role** — `role:lighthouse`.
pub const ROLE_PREFIX: &str = "role:";

/// The selector prefix naming a capability / **scope** tag — `scope:media`.
pub const SCOPE_PREFIX: &str = "scope:";

/// A sealing target: which subset of the mesh a secret is sealed to.
///
/// The default ([`SealScope::WholeMesh`]) reproduces the pre-WL-SEC-003 behavior
/// exactly — every registered recipient — so an unscoped `put` is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealScope {
    /// Seal to EVERY registered recipient (the backward-compatible default).
    WholeMesh,
    /// Seal only to nodes whose published role tags contain this role
    /// (`role:<role>`, e.g. `role:lighthouse`).
    Role(String),
    /// Seal only to nodes whose published capability/scope tags contain this
    /// scope (`scope:<scope>`, e.g. `scope:media`).
    Capability(String),
}

/// A malformed [`SealScope`] selector string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// The selector had no recognized `role:` / `scope:` prefix.
    #[error(
        "bad --scope selector {0:?}: expected 'role:<role>' or 'scope:<scope>' \
         (e.g. role:lighthouse, scope:media)"
    )]
    UnknownPrefix(String),
    /// The prefix was present but the value after the colon was empty.
    #[error("bad --scope selector {0:?}: empty value after the prefix")]
    EmptyValue(String),
}

impl SealScope {
    /// Parse a `--scope` selector into a [`SealScope`].
    ///
    /// `role:<role>` → [`SealScope::Role`]; `scope:<scope>` →
    /// [`SealScope::Capability`]. The role/scope value is trimmed and lowercased
    /// so `role:Lighthouse` and `role:lighthouse` resolve identically (tags are
    /// canonicalized lowercase, matching `mde-role`'s `as_str`).
    ///
    /// # Errors
    ///
    /// [`ScopeError::UnknownPrefix`] when the selector names neither dimension;
    /// [`ScopeError::EmptyValue`] when the value after the prefix is blank.
    pub fn parse(selector: &str) -> Result<Self, ScopeError> {
        let s = selector.trim();
        if let Some(role) = s.strip_prefix(ROLE_PREFIX) {
            let role = role.trim().to_ascii_lowercase();
            if role.is_empty() {
                return Err(ScopeError::EmptyValue(selector.to_string()));
            }
            return Ok(Self::Role(role));
        }
        if let Some(scope) = s.strip_prefix(SCOPE_PREFIX) {
            let scope = scope.trim().to_ascii_lowercase();
            if scope.is_empty() {
                return Err(ScopeError::EmptyValue(selector.to_string()));
            }
            return Ok(Self::Capability(scope));
        }
        Err(ScopeError::UnknownPrefix(selector.to_string()))
    }

    /// Parse an OPTIONAL selector: `None` (no `--scope` given) resolves to the
    /// backward-compatible [`SealScope::WholeMesh`]; `Some(sel)` parses per
    /// [`SealScope::parse`].
    ///
    /// # Errors
    ///
    /// Per [`SealScope::parse`] when `selector` is `Some` and malformed.
    pub fn parse_opt(selector: Option<&str>) -> Result<Self, ScopeError> {
        match selector {
            None => Ok(Self::WholeMesh),
            Some(s) => Self::parse(s),
        }
    }

    /// The canonical selector string this scope round-trips through
    /// ([`SealScope::parse`] of it yields `self`), or `None` for
    /// [`SealScope::WholeMesh`] (which is spelled by the *absence* of `--scope`).
    #[must_use]
    pub fn selector(&self) -> Option<String> {
        match self {
            Self::WholeMesh => None,
            Self::Role(role) => Some(format!("{ROLE_PREFIX}{role}")),
            Self::Capability(scope) => Some(format!("{SCOPE_PREFIX}{scope}")),
        }
    }

    /// Whether `node` is in this scope's recipient set — i.e. whether the secret
    /// should be sealed so `node` can decrypt it.
    ///
    /// [`SealScope::WholeMesh`] matches every node. [`SealScope::Role`] matches a
    /// node carrying that role tag; [`SealScope::Capability`] a node carrying that
    /// scope tag. A node that matches NOTHING (an untagged legacy recipient under
    /// a scoped seal) is excluded — its key never becomes an `age` recipient, so
    /// it cannot decrypt the scoped ciphertext.
    #[must_use]
    pub fn matches(&self, node: &NodeRecipient) -> bool {
        match self {
            Self::WholeMesh => true,
            Self::Role(role) => node.has_role(role),
            Self::Capability(scope) => node.has_scope(scope),
        }
    }
}

/// One node's published entry in the recipient roster: its public `age` recipient
/// plus the role / capability-scope tags it advertised at `init-self`.
///
/// Only PUBLIC data — the `age1…` recipient (never the private identity) and the
/// node's own role/scope tags. The resolver never sees a private key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecipient {
    /// The node's stable id (the `/mcnf/age-recipients/<node_id>` key).
    pub node_id: String,
    /// The node's public `age1…` recipient — what a secret is sealed to.
    pub recipient: String,
    /// The node's deployment role tags (canonical lowercase, e.g.
    /// `["lighthouse"]`).
    pub roles: Vec<String>,
    /// The node's capability / scope tags (canonical lowercase, e.g.
    /// `["media"]`).
    pub scopes: Vec<String>,
}

impl NodeRecipient {
    /// Build a roster entry, canonicalizing every tag to trimmed lowercase so a
    /// selector (also canonicalized) matches regardless of the published casing.
    #[must_use]
    pub fn new(
        node_id: impl Into<String>,
        recipient: impl Into<String>,
        roles: &[&str],
        scopes: &[&str],
    ) -> Self {
        Self {
            node_id: node_id.into(),
            recipient: recipient.into(),
            roles: roles
                .iter()
                .map(|r| r.trim().to_ascii_lowercase())
                .collect(),
            scopes: scopes
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect(),
        }
    }

    /// Whether this node carries `role` (case-insensitive).
    #[must_use]
    pub fn has_role(&self, role: &str) -> bool {
        let want = role.trim().to_ascii_lowercase();
        self.roles.iter().any(|r| *r == want)
    }

    /// Whether this node carries capability/scope `scope` (case-insensitive).
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        let want = scope.trim().to_ascii_lowercase();
        self.scopes.iter().any(|s| *s == want)
    }
}

/// Resolve the exact set of `age` recipients a secret sealed to `scope` must go
/// to, from the published `roster`.
///
/// Returns each matching node's public recipient, DEDUPED and SORTED so the
/// result is deterministic (two nodes may share a key; the order must not depend
/// on roster iteration). A non-matching node's recipient is simply absent — the
/// scoped decryption root: `age` wraps the file key only to these recipients, so
/// only these nodes can decrypt.
///
/// An empty result means the selector matched no registered node; the caller
/// (the shell `put`) must refuse to seal to nothing rather than silently produce
/// an undecryptable secret.
#[must_use]
pub fn recipients_for(scope: &SealScope, roster: &[NodeRecipient]) -> Vec<String> {
    roster
        .iter()
        .filter(|node| scope.matches(node))
        .map(|node| node.recipient.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A three-node fixture: a thin lighthouse, an explicitly provisioned
    /// non-lighthouse media host, and a workstation. The recipient strings stand
    /// in for the real `age1…` public keys — the resolver treats them opaquely.
    fn roster() -> Vec<NodeRecipient> {
        vec![
            NodeRecipient::new("lh1", "age1-lighthouse-one", &["lighthouse"], &[]),
            NodeRecipient::new(
                "media-host",
                "age1-media-host",
                &["workstation"],
                &["media"],
            ),
            NodeRecipient::new("ws1", "age1-workstation-one", &["workstation"], &[]),
        ]
    }

    // ── selector grammar ──

    #[test]
    fn parse_role_and_scope_selectors() {
        assert_eq!(
            SealScope::parse("role:lighthouse").unwrap(),
            SealScope::Role("lighthouse".into())
        );
        assert_eq!(
            SealScope::parse("scope:media").unwrap(),
            SealScope::Capability("media".into())
        );
    }

    #[test]
    fn parse_canonicalizes_case_and_whitespace() {
        assert_eq!(
            SealScope::parse("  role:Lighthouse ").unwrap(),
            SealScope::Role("lighthouse".into())
        );
        assert_eq!(
            SealScope::parse("scope:MEDIA").unwrap(),
            SealScope::Capability("media".into())
        );
    }

    #[test]
    fn parse_opt_none_is_whole_mesh() {
        assert_eq!(SealScope::parse_opt(None).unwrap(), SealScope::WholeMesh);
    }

    #[test]
    fn parse_rejects_unknown_prefix_and_empty_value() {
        assert!(matches!(
            SealScope::parse("lighthouse"),
            Err(ScopeError::UnknownPrefix(_))
        ));
        assert!(matches!(
            SealScope::parse("group:media"),
            Err(ScopeError::UnknownPrefix(_))
        ));
        assert!(matches!(
            SealScope::parse("role:"),
            Err(ScopeError::EmptyValue(_))
        ));
        assert!(matches!(
            SealScope::parse("scope:  "),
            Err(ScopeError::EmptyValue(_))
        ));
    }

    #[test]
    fn selector_round_trips() {
        for sel in ["role:lighthouse", "scope:media", "role:workstation"] {
            let scope = SealScope::parse(sel).unwrap();
            assert_eq!(scope.selector().as_deref(), Some(sel));
            // And re-parsing the emitted selector is a fixed point.
            assert_eq!(SealScope::parse(&scope.selector().unwrap()).unwrap(), scope);
        }
        assert_eq!(SealScope::WholeMesh.selector(), None);
    }

    // ── the three acceptance resolutions (mirrored by the shell selftest's
    //    real-`age` role-match-decrypts / role-mismatch-fails / whole-mesh) ──

    #[test]
    fn role_scope_selects_only_matching_role_and_excludes_others() {
        // `role:lighthouse` seals only to the thin lighthouse and NEVER to the
        // non-lighthouse media host or workstation.
        let got = recipients_for(&SealScope::Role("lighthouse".into()), &roster());
        assert_eq!(
            got,
            vec!["age1-lighthouse-one".to_string()],
            "role:lighthouse must resolve only to the thin lighthouse recipient"
        );
        assert!(
            !got.contains(&"age1-workstation-one".to_string()),
            "the workstation-only recipient must be EXCLUDED — it cannot decrypt a \
             role:lighthouse secret"
        );
    }

    #[test]
    fn capability_scope_selects_only_matching_scope() {
        // `scope:media` seals ONLY to the explicitly provisioned non-lighthouse
        // node carrying the media scope tag.
        let got = recipients_for(&SealScope::Capability("media".into()), &roster());
        assert_eq!(got, vec!["age1-media-host".to_string()]);
    }

    #[test]
    fn whole_mesh_default_selects_every_recipient() {
        // The backward-compatible default: every registered recipient, unchanged.
        let got = recipients_for(&SealScope::WholeMesh, &roster());
        assert_eq!(
            got,
            vec![
                "age1-lighthouse-one".to_string(),
                "age1-media-host".to_string(),
                "age1-workstation-one".to_string(),
            ]
        );
    }

    #[test]
    fn recipients_are_deduped_and_sorted() {
        // Two node-ids sharing one physical key collapse to a single recipient,
        // and the output order is deterministic (sorted), not roster order.
        let dup = vec![
            NodeRecipient::new("b", "age1-shared", &["lighthouse"], &[]),
            NodeRecipient::new("a", "age1-shared", &["lighthouse"], &[]),
            NodeRecipient::new("c", "age1-other", &["lighthouse"], &[]),
        ];
        assert_eq!(
            recipients_for(&SealScope::Role("lighthouse".into()), &dup),
            vec!["age1-other".to_string(), "age1-shared".to_string()]
        );
    }

    #[test]
    fn no_match_resolves_to_empty_set() {
        // A selector no registered node satisfies resolves to the empty set — the
        // shell `put` turns this into a hard refusal (never seal to nobody).
        assert!(recipients_for(&SealScope::Role("relay".into()), &roster()).is_empty());
        assert!(recipients_for(&SealScope::Capability("voice".into()), &roster()).is_empty());
    }

    #[test]
    fn untagged_recipient_only_matches_whole_mesh() {
        // A legacy recipient with no published tags takes part in whole-mesh seals
        // but NEVER in a scoped seal (so scoped secrets can't leak to it).
        let legacy = vec![NodeRecipient::new("legacy", "age1-legacy", &[], &[])];
        assert_eq!(
            recipients_for(&SealScope::WholeMesh, &legacy),
            vec!["age1-legacy".to_string()]
        );
        assert!(recipients_for(&SealScope::Role("lighthouse".into()), &legacy).is_empty());
        assert!(recipients_for(&SealScope::Capability("media".into()), &legacy).is_empty());
    }
}
