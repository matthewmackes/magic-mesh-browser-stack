//! VPN-GW-1 — the VPN tunnel definition model + pure helpers (design:
//! `docs/design/vpn-gateway.md`).
//!
//! A node runs N named **tunnels**, each an internet-egress layer on top of the
//! mesh. This crate holds the durable model (TOML on the shared substrate), the
//! `mvpn-<id>` interface-name derivation (bounded to Linux's 15-char `IFNAMSIZ`),
//! and the `wg-quick` / `openvpn` argv builders — all pure + unit-tested. The
//! `mackesd` `vpn_gateway` worker brings tunnels up/down by spawning these argv
//! and serves `action/vpn/*`; the secret material (keys/.ovpn) is age-encrypted
//! in the mesh secret store, never in this config.

use serde::{Deserialize, Serialize};

/// Linux `IFNAMSIZ` is 16 incl. the NUL → 15 usable chars for an interface name.
pub const IFNAME_MAX: usize = 15;

/// How a tunnel is brought up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    /// `WireGuard` via `wg-quick` on a rendered config (the primary path).
    #[default]
    Wg,
    /// `OpenVPN` via `openvpn` on an imported `.ovpn`.
    Ovpn,
    /// A provider CLI (`mullvad`/`protonvpn-cli`/`nordvpn`).
    Cli,
    /// A provider API/config-generator (mints a WG config / picks a server).
    Api,
}

/// One named tunnel definition. Secret material is referenced by `creds_ref`
/// (an age-encrypted blob in the mesh secret store), never inlined here.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelDef {
    /// Operator-chosen id, unique within the node (drives `mvpn-<id>`).
    pub id: String,
    /// Provider label (`mullvad`/`proton`/…/`generic-wg`/`generic-ovpn`).
    pub provider: String,
    /// How it's brought up.
    #[serde(default)]
    pub method: Method,
    /// Server/region selector (provider-specific; may be empty for generic).
    #[serde(default)]
    pub server: String,
    /// Transport hint (`udp`/`tcp`); `OpenVPN` obfuscation → tcp.
    #[serde(default)]
    pub protocol: String,
    /// Reference to the age-encrypted creds in the mesh secret store.
    #[serde(default)]
    pub creds_ref: String,
}

impl TunnelDef {
    /// The dedicated interface name `mvpn-<id>`, sanitized + bounded to
    /// [`IFNAME_MAX`] (Linux refuses longer names). Non-alphanumeric id chars
    /// collapse to nothing; the `mvpn-` prefix is always kept. Pure + stable.
    #[must_use]
    pub fn ifname(&self) -> String {
        const PREFIX: &str = "mvpn-";
        let body: String = self
            .id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(IFNAME_MAX - PREFIX.len())
            .collect();
        format!("{PREFIX}{body}")
    }

    /// Validate the definition is usable: non-empty id whose `ifname` body isn't
    /// empty after sanitizing (else two ids could collide on the bare prefix).
    ///
    /// # Errors
    /// A human-readable reason.
    pub fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("tunnel id is empty".into());
        }
        if self.ifname() == "mvpn-" {
            return Err(format!(
                "tunnel id '{}' has no alphanumeric chars for the interface name",
                self.id
            ));
        }
        Ok(())
    }
}

/// The node's VPN config — the durable set of tunnel definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnConfig {
    /// Per-node tunnel definitions.
    #[serde(default)]
    pub tunnel: Vec<TunnelDef>,
}

impl VpnConfig {
    /// Parse from TOML (missing sections → empty).
    ///
    /// # Errors
    /// A TOML parse error.
    pub fn from_toml_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Serialize to TOML.
    ///
    /// # Errors
    /// A TOML serialize error.
    pub fn to_toml_string(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Look up a tunnel by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&TunnelDef> {
        self.tunnel.iter().find(|t| t.id == id)
    }

    /// Insert or replace a tunnel (keyed by id).
    pub fn upsert(&mut self, t: TunnelDef) {
        if let Some(e) = self.tunnel.iter_mut().find(|x| x.id == t.id) {
            *e = t;
        } else {
            self.tunnel.push(t);
        }
    }

    /// Remove a tunnel by id; `true` if one was removed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tunnel.len();
        self.tunnel.retain(|t| t.id != id);
        self.tunnel.len() != before
    }

    /// Validate every tunnel + that interface names don't collide (two ids that
    /// sanitize to the same `mvpn-<body>` can't run concurrently).
    ///
    /// # Errors
    /// The first inconsistency's reason.
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for t in &self.tunnel {
            t.validate()?;
            let ifn = t.ifname();
            if !seen.insert(ifn.clone()) {
                return Err(format!("interface name collision: {ifn}"));
            }
        }
        Ok(())
    }
}

/// Durable path for the VPN config: `<workgroup_root>/vpn/tunnels.toml`.
#[must_use]
pub fn config_path(workgroup_root: &std::path::Path) -> std::path::PathBuf {
    workgroup_root.join("vpn").join("tunnels.toml")
}

/// Load the VPN config (missing/malformed → default empty).
#[must_use]
pub fn load(workgroup_root: &std::path::Path) -> VpnConfig {
    std::fs::read_to_string(config_path(workgroup_root))
        .ok()
        .and_then(|raw| VpnConfig::from_toml_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the VPN config (validate → atomic temp+rename).
///
/// # Errors
/// Validation failure, or an I/O / serialize error.
pub fn save(
    workgroup_root: &std::path::Path,
    cfg: &VpnConfig,
) -> Result<std::path::PathBuf, String> {
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

/// The `wg-quick up <ifname>` argv (the config is written to
/// `/etc/wireguard/<ifname>.conf` by the worker from the decrypted creds).
#[must_use]
pub fn wg_quick_argv(t: &TunnelDef, up: bool) -> Vec<String> {
    vec![
        "wg-quick".into(),
        if up { "up".into() } else { "down".into() },
        t.ifname(),
    ]
}

/// The `openvpn` argv to bring a tunnel up against its `.ovpn` at `config_path`,
/// naming the device `mvpn-<id>` so it matches the egress policy routing.
#[must_use]
pub fn openvpn_argv(t: &TunnelDef, config_path: &str) -> Vec<String> {
    vec![
        "openvpn".into(),
        "--config".into(),
        config_path.into(),
        "--dev".into(),
        t.ifname(),
        "--daemon".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tun(id: &str, method: Method) -> TunnelDef {
        TunnelDef {
            id: id.into(),
            provider: "generic-wg".into(),
            method,
            ..Default::default()
        }
    }

    #[test]
    fn ifname_is_prefixed_sanitized_and_bounded() {
        assert_eq!(tun("mullvad1", Method::Wg).ifname(), "mvpn-mullvad1");
        // Non-alnum collapses.
        assert_eq!(tun("proton-uk_2", Method::Wg).ifname(), "mvpn-protonuk2");
        // Bounded to 15 chars total (10 body chars after the 5-char prefix).
        let long = tun("abcdefghijklmnop", Method::Wg).ifname();
        assert_eq!(long, "mvpn-abcdefghij");
        assert!(long.len() <= IFNAME_MAX);
    }

    #[test]
    fn validate_rejects_empty_and_non_alnum_ids() {
        assert!(tun("", Method::Wg).validate().is_err());
        assert!(tun("___", Method::Wg).validate().is_err()); // ifname body empty
        assert!(tun("ok", Method::Wg).validate().is_ok());
    }

    #[test]
    fn config_round_trips_and_detects_ifname_collision() {
        let mut cfg = VpnConfig::default();
        cfg.upsert(tun("mullvad1", Method::Wg));
        cfg.upsert(tun("mullvad2", Method::Ovpn));
        let s = cfg.to_toml_string().unwrap();
        assert_eq!(VpnConfig::from_toml_str(&s).unwrap(), cfg);
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.tunnel.len(), 2);
        // Two ids sanitizing to the same ifname collide.
        cfg.upsert(tun("mull-vad1", Method::Wg)); // → mvpn-mullvad1, same as "mullvad1"
        assert!(cfg.validate().unwrap_err().contains("collision"));
    }

    #[test]
    fn upsert_replaces_and_remove_works() {
        let mut cfg = VpnConfig::default();
        cfg.upsert(tun("a", Method::Wg));
        let mut updated = tun("a", Method::Ovpn);
        updated.server = "us-nyc".into();
        cfg.upsert(updated);
        assert_eq!(cfg.tunnel.len(), 1);
        assert_eq!(cfg.get("a").unwrap().method, Method::Ovpn);
        assert!(cfg.remove("a"));
        assert!(!cfg.remove("a"));
    }

    #[test]
    fn load_save_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = VpnConfig::default();
        cfg.upsert(tun("mullvad1", Method::Wg));
        save(tmp.path(), &cfg).unwrap();
        assert_eq!(load(tmp.path()), cfg);
        // Missing → default empty.
        assert_eq!(
            load(tmp.path().join("nope").as_path()),
            VpnConfig::default()
        );
    }

    #[test]
    fn argv_builders() {
        let t = tun("mullvad1", Method::Wg);
        assert_eq!(
            wg_quick_argv(&t, true),
            vec!["wg-quick", "up", "mvpn-mullvad1"]
        );
        assert_eq!(wg_quick_argv(&t, false)[1], "down");
        assert_eq!(
            openvpn_argv(&t, "/run/mvpn/mullvad1.ovpn"),
            vec![
                "openvpn",
                "--config",
                "/run/mvpn/mullvad1.ovpn",
                "--dev",
                "mvpn-mullvad1",
                "--daemon"
            ]
        );
    }
}
