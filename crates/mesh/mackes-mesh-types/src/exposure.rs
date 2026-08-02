//! CONNECT-1 — the unified connectivity / **exposure policy** model + state
//! (design: `docs/design/connect.md`). Per-service records declaring how each
//! host/VM/container service is reached: mesh-only (overlay) or published to the
//! public through the lighthouse reverse-proxy ingress. The one-state doctrine
//! (§9 W88): durable state is TOML on the shared substrate; `mackesd`'s
//! `action/connect/*` responders are the typed surface over it; GUIs render it.
//!
//! Naming: this is **exposure** (the public boundary), distinct from the
//! KDE-Connect device facts in [`crate::connect`]. CONNECT governs ONLY the
//! public boundary — intra-mesh trust stays flat / open-mesh (`AI_GOVERNANCE`
//! §8 unchanged).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a service may be reached from. (Tier 1 — Public = Nebula + SSH only —
/// is the foundational layer and not a per-service choice; a service is either
/// overlay-only or additionally published via the ingress.)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// Reachable only over the Nebula overlay (the default — no public surface).
    #[default]
    MeshOnly,
    /// Additionally published to the public internet via the lighthouse
    /// reverse-proxy ingress.
    PublicViaIngress,
}

/// How the ingress carries an exposed service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProtoMode {
    /// Reverse-proxied HTTP/HTTPS (auto-TLS for the DDNS name).
    #[default]
    Http,
    /// Allowlisted raw TCP stream (layer-4 proxy).
    Tcp,
    /// Allowlisted raw UDP stream.
    Udp,
}

/// What kind of workload hosts a service (for display + discovery).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// A host-level service (runs directly on the node).
    #[default]
    Host,
    /// A libvirt/KVM virtual machine.
    Vm,
    /// A Podman container.
    Container,
}

/// The service being exposed: which node hosts it + its overlay-side endpoint.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceSource {
    /// Hostname of the node that hosts the service.
    pub node: String,
    /// Host / VM / container.
    pub kind: SourceKind,
    /// The service's port on its overlay-reachable endpoint.
    pub port: u16,
    /// `tcp` or `udp` (the L4 protocol the service listens on).
    #[serde(default = "default_proto")]
    pub proto: String,
}

fn default_proto() -> String {
    "tcp".to_string()
}

/// Where a public-via-ingress service terminates: which lighthouse + the public
/// hostname it's published under (a DDNS name, see DDNS-EGRESS).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressBinding {
    /// Hostname of the lighthouse acting as the public reverse-proxy ingress.
    pub lighthouse: String,
    /// Public hostname the service is published under (e.g.
    /// `grafana.services.matthewmackes.com`).
    pub hostname: String,
}

/// One service's exposure policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposurePolicy {
    /// Stable service id (operator-chosen; unique within the config).
    pub id: String,
    /// The hosting node + endpoint.
    pub source: ServiceSource,
    /// Mesh-only or public-via-ingress.
    #[serde(default)]
    pub tier: Tier,
    /// The ingress binding — required when `tier == PublicViaIngress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressBinding>,
    /// HTTP / raw TCP / raw UDP (only meaningful when published).
    #[serde(default)]
    pub mode: ProtoMode,
    /// Optional group template this policy was applied from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl ExposurePolicy {
    /// Validate the policy is internally consistent: a public-via-ingress service
    /// MUST carry an ingress binding (lighthouse + hostname), else the renderer
    /// has nothing to publish. Returns the reason on failure.
    ///
    /// # Errors
    /// A human-readable reason when the policy is inconsistent.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("service id is empty".into());
        }
        if self.tier == Tier::PublicViaIngress {
            match &self.ingress {
                None => {
                    return Err(format!(
                        "'{}' is public-via-ingress but has no ingress binding",
                        self.id
                    ))
                }
                Some(b) if b.lighthouse.trim().is_empty() || b.hostname.trim().is_empty() => {
                    return Err(format!(
                        "'{}' ingress binding is missing a lighthouse or hostname",
                        self.id
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    /// True when this service faces the public internet (drives the firewalld +
    /// Caddy render + the [`AI_GOVERNANCE`] §8 "public, no ingress auth" flag).
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.tier == Tier::PublicViaIngress
    }
}

/// A reusable exposure template applied across many services (CONNECT-8).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureTemplate {
    /// Template name (referenced by [`ExposurePolicy::template`]).
    pub name: String,
    /// Tier this template confers.
    #[serde(default)]
    pub tier: Tier,
    /// Protocol mode this template confers.
    #[serde(default)]
    pub mode: ProtoMode,
    /// The lighthouse public-via-ingress services land on under this template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighthouse: Option<String>,
}

/// The whole exposure config — the `[connect]` durable state.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposureConfig {
    /// Per-service exposure policies.
    #[serde(default)]
    pub service: Vec<ExposurePolicy>,
    /// Reusable group templates.
    #[serde(default)]
    pub template: Vec<ExposureTemplate>,
}

impl ExposureConfig {
    /// Parse from a TOML string (missing sections default to empty).
    ///
    /// # Errors
    /// A TOML parse error.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to a TOML string.
    ///
    /// # Errors
    /// A TOML serialize error.
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Validate every service policy.
    ///
    /// # Errors
    /// The first inconsistent policy's reason.
    pub fn validate(&self) -> Result<(), String> {
        for s in &self.service {
            s.validate()?;
        }
        Ok(())
    }

    /// Look up a service policy by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ExposurePolicy> {
        self.service.iter().find(|s| s.id == id)
    }

    /// Insert or replace a service policy (keyed by id).
    pub fn upsert(&mut self, policy: ExposurePolicy) {
        if let Some(existing) = self.service.iter_mut().find(|s| s.id == policy.id) {
            *existing = policy;
        } else {
            self.service.push(policy);
        }
    }

    /// Remove a service policy by id. Returns `true` if one was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.service.len();
        self.service.retain(|s| s.id != id);
        self.service.len() != before
    }

    /// Every public-via-ingress service (drives the firewalld + Caddy render).
    #[must_use]
    pub fn public_services(&self) -> Vec<&ExposurePolicy> {
        self.service.iter().filter(|s| s.is_public()).collect()
    }
}

/// Marker line so the Caddy ingress render is recognizably MCNF-owned.
pub const CADDY_MANAGED_HEADER: &str =
    "# Managed by MCNF CONNECT — do not edit (rendered from connect/policy.toml).";

/// CONNECT-4 — render the Caddy ingress config for `lighthouse` from the policy:
/// one auto-HTTPS site per public **HTTP** service bound to this lighthouse,
/// reverse-proxying the public hostname to the hosting node over the mesh
/// (`<node>.mesh:<port>`, resolved by the mesh DNS). Caddy obtains a Let's
/// Encrypt cert for each hostname automatically. Raw TCP/UDP stream services are
/// **not** emitted here — they need the `caddy-l4` layer-4 app (CONNECT-5).
/// Deterministic (sorted by hostname) for stable writes/tests. Pure.
#[must_use]
pub fn render_caddyfile(cfg: &ExposureConfig, lighthouse: &str) -> String {
    let mut sites: Vec<(&str, &str, u16)> = cfg
        .public_services()
        .into_iter()
        .filter(|s| s.mode == ProtoMode::Http)
        .filter_map(|s| {
            let b = s.ingress.as_ref()?;
            (b.lighthouse == lighthouse).then_some((
                b.hostname.as_str(),
                s.source.node.as_str(),
                s.source.port,
            ))
        })
        .collect();
    sites.sort_unstable();
    sites.dedup();
    let mut out = String::from(CADDY_MANAGED_HEADER);
    out.push('\n');
    for (hostname, node, port) in sites {
        out.push_str(&format!(
            "{hostname} {{\n\treverse_proxy {node}.mesh:{port}\n}}\n"
        ));
    }
    out
}

/// Durable path for the exposure config: `<workgroup_root>/connect/policy.toml`.
#[must_use]
pub fn config_path(workgroup_root: &Path) -> PathBuf {
    workgroup_root.join("connect").join("policy.toml")
}

/// Load the exposure config from the shared substrate. A missing/empty/malformed
/// file yields the default (empty) config — the panels render an honest empty
/// state, never an error.
#[must_use]
pub fn load(workgroup_root: &Path) -> ExposureConfig {
    std::fs::read_to_string(config_path(workgroup_root))
        .ok()
        .and_then(|raw| ExposureConfig::from_toml_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the exposure config to the shared substrate (atomic write-through:
/// temp + rename). Validates before writing.
///
/// # Errors
/// Validation failure, or an I/O / serialize error.
pub fn save(workgroup_root: &Path, cfg: &ExposureConfig) -> Result<PathBuf, String> {
    cfg.validate()?;
    let path = config_path(workgroup_root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let toml = cfg.to_toml_string().map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web() -> ExposurePolicy {
        ExposurePolicy {
            id: "grafana".into(),
            source: ServiceSource {
                node: "eagle".into(),
                kind: SourceKind::Container,
                port: 3000,
                proto: "tcp".into(),
            },
            tier: Tier::PublicViaIngress,
            ingress: Some(IngressBinding {
                lighthouse: "Lighthouse-01".into(),
                hostname: "grafana.services.matthewmackes.com".into(),
            }),
            mode: ProtoMode::Http,
            template: Some("web-apps".into()),
        }
    }

    #[test]
    fn toml_round_trip_preserves_policy() {
        let mut cfg = ExposureConfig::default();
        cfg.upsert(web());
        cfg.upsert(ExposurePolicy {
            id: "db".into(),
            source: ServiceSource {
                node: "eagle".into(),
                kind: SourceKind::Host,
                port: 5432,
                proto: "tcp".into(),
            },
            tier: Tier::MeshOnly,
            ..Default::default()
        });
        let s = cfg.to_toml_string().unwrap();
        let back = ExposureConfig::from_toml_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn public_service_requires_ingress_binding() {
        let mut p = web();
        p.ingress = None;
        assert!(p.validate().is_err(), "public-via-ingress needs a binding");
        // Mesh-only needs none.
        p.tier = Tier::MeshOnly;
        assert!(p.validate().is_ok());
    }

    #[test]
    fn upsert_replaces_and_remove_works() {
        let mut cfg = ExposureConfig::default();
        cfg.upsert(web());
        let mut updated = web();
        updated.tier = Tier::MeshOnly;
        cfg.upsert(updated); // same id → replace, not append
        assert_eq!(cfg.service.len(), 1);
        assert_eq!(cfg.get("grafana").unwrap().tier, Tier::MeshOnly);
        assert!(cfg.remove("grafana"));
        assert!(!cfg.remove("grafana"));
        assert!(cfg.service.is_empty());
    }

    #[test]
    fn public_services_filters() {
        let mut cfg = ExposureConfig::default();
        cfg.upsert(web()); // public
        cfg.upsert(ExposurePolicy {
            id: "internal".into(),
            tier: Tier::MeshOnly,
            ..Default::default()
        });
        let pub_ids: Vec<&str> = cfg
            .public_services()
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(pub_ids, vec!["grafana"]);
    }

    #[test]
    fn load_save_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = ExposureConfig::default();
        cfg.upsert(web());
        save(tmp.path(), &cfg).unwrap();
        let loaded = load(tmp.path());
        assert_eq!(loaded, cfg);
        // A missing file → default empty.
        let empty = load(tmp.path().join("nope").as_path());
        assert_eq!(empty, ExposureConfig::default());
    }

    #[test]
    fn caddyfile_render_emits_http_sites_for_this_lighthouse_only() {
        let mut cfg = ExposureConfig::default();
        cfg.upsert(web()); // grafana, http, LH-01, port 3000, node "eagle"
                           // A second http service on a different lighthouse + a tcp service (skipped).
        cfg.upsert(ExposurePolicy {
            id: "other".into(),
            source: ServiceSource {
                node: "pine".into(),
                port: 8080,
                ..Default::default()
            },
            tier: Tier::PublicViaIngress,
            ingress: Some(IngressBinding {
                lighthouse: "LH-02".into(),
                hostname: "other.example".into(),
            }),
            mode: ProtoMode::Http,
            ..Default::default()
        });
        cfg.upsert(ExposurePolicy {
            id: "game".into(),
            source: ServiceSource {
                node: "eagle".into(),
                port: 25565,
                ..Default::default()
            },
            tier: Tier::PublicViaIngress,
            ingress: Some(IngressBinding {
                lighthouse: "Lighthouse-01".into(),
                hostname: "game.example".into(),
            }),
            mode: ProtoMode::Tcp, // raw stream — not in the Caddy HTTP render
            ..Default::default()
        });
        let cf = render_caddyfile(&cfg, "Lighthouse-01");
        assert!(cf.contains("grafana.services.matthewmackes.com {"), "{cf}");
        assert!(cf.contains("reverse_proxy eagle.mesh:3000"), "{cf}");
        assert!(!cf.contains("other.example"), "other lighthouse excluded");
        assert!(
            !cf.contains("game.example"),
            "tcp stream not in HTTP render"
        );
        assert!(cf.starts_with(CADDY_MANAGED_HEADER));
    }

    #[test]
    fn save_rejects_inconsistent_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = ExposureConfig::default();
        let mut bad = web();
        bad.ingress = None; // public but no binding
        cfg.service.push(bad);
        assert!(save(tmp.path(), &cfg).is_err());
    }
}
