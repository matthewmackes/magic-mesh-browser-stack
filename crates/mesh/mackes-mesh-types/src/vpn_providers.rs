//! VPN-GW-5 — first-class commercial-VPN **provider adapters** + the generic
//! "paste WG config" / "import .ovpn" paths (design: `docs/design/vpn-gateway.md`,
//! locked decisions 1 + 2).
//!
//! Each adapter takes operator-supplied inputs (a provider account / a server
//! pick / pasted key material) and produces a **verifiable tunnel config** — a
//! rendered WireGuard `.conf` (or an imported `.ovpn`) plus the matching
//! [`TunnelDef`] — that the EXISTING VPN-GW tunnel machinery (VPN-GW-1..4:
//! `wg_quick_argv` / `openvpn_argv`, the `mvpn-<id>` interface, the responder)
//! stands up unchanged. This module is **pure** (no I/O, no process spawn, no
//! network) so every adapter's config generation is fully unit-testable; the
//! live exit-IP verification is wired daemon-side (best-effort — it needs a real
//! account) and only the *expected* check target is derived here.
//!
//! The five first-class providers — **Mullvad, ProtonVPN, IVPN, NordVPN,
//! Surfshark** — each map to a [`Provider`] with its preferred [`Method`], its
//! WireGuard endpoint port, an optional verification host, and (where relevant)
//! a provider CLI. Anything not first-class still works via [`Provider::GenericWg`]
//! ("paste any WG config") or [`Provider::GenericOvpn`] ("import any .ovpn").

use serde::{Deserialize, Serialize};

use crate::vpn::{Method, TunnelDef};

/// A first-class provider (the five locked in decision 2) plus the two generic
/// escape hatches so ANY provider works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    /// Mullvad — WireGuard-first (account-number auth, `mullvad` CLI available).
    Mullvad,
    /// `ProtonVPN` — `WireGuard` configs from the dashboard/API (`protonvpn-cli`).
    Proton,
    /// IVPN — `WireGuard` configs/API (`ivpn` CLI).
    Ivpn,
    /// `NordVPN` — `NordLynx` (`WireGuard`) primarily via the `nordvpn` CLI; manual
    /// `WireGuard` via the access-token API; `OpenVPN` configs also published.
    Nord,
    /// Surfshark — `WireGuard` + `OpenVPN` configs from the dashboard.
    Surfshark,
    /// Generic "paste any `WireGuard` config".
    GenericWg,
    /// Generic "import any .ovpn".
    GenericOvpn,
}

impl Provider {
    /// The stable provider label stored in [`TunnelDef::provider`] / shown in the
    /// UI. Matches the `kebab-case` serde rename so config round-trips.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mullvad => "mullvad",
            Self::Proton => "proton",
            Self::Ivpn => "ivpn",
            Self::Nord => "nord",
            Self::Surfshark => "surfshark",
            Self::GenericWg => "generic-wg",
            Self::GenericOvpn => "generic-ovpn",
        }
    }

    /// Parse a provider from its [`label`](Provider::label) (case-insensitive,
    /// with a few common aliases). `None` for anything unrecognized.
    #[must_use]
    pub fn from_label(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mullvad" => Some(Self::Mullvad),
            "proton" | "protonvpn" => Some(Self::Proton),
            "ivpn" => Some(Self::Ivpn),
            "nord" | "nordvpn" => Some(Self::Nord),
            "surfshark" => Some(Self::Surfshark),
            "generic-wg" | "generic" | "wg" => Some(Self::GenericWg),
            "generic-ovpn" | "ovpn" | "openvpn" => Some(Self::GenericOvpn),
            _ => None,
        }
    }

    /// The bring-up [`Method`] this provider prefers. `WireGuard`-first per
    /// decision 4; the `generic-ovpn` path is `OpenVPN`.
    #[must_use]
    pub const fn method(self) -> Method {
        match self {
            Self::GenericOvpn => Method::Ovpn,
            // All five first-class providers ship a WireGuard path; generic-wg too.
            _ => Method::Wg,
        }
    }

    /// The provider CLI binary, where one is chosen for this provider. `None`
    /// means config/API-only (no first-class CLI integration). Informational —
    /// the always-works path is the rendered `WireGuard` config.
    #[must_use]
    pub const fn cli(self) -> Option<&'static str> {
        match self {
            Self::Mullvad => Some("mullvad"),
            Self::Proton => Some("protonvpn-cli"),
            Self::Ivpn => Some("ivpn"),
            Self::Nord => Some("nordvpn"),
            // Surfshark ships no first-class Linux CLI — WG/OVPN config only.
            Self::Surfshark | Self::GenericWg | Self::GenericOvpn => None,
        }
    }

    /// The default `WireGuard` endpoint UDP port for this provider. Used when the
    /// operator supplies a server hostname/IP without an explicit port. All five
    /// providers (and `generic-wg`) use 51820; the `OpenVPN`-only generic path has
    /// no WG port (`0`).
    #[must_use]
    pub const fn default_wg_port(self) -> u16 {
        match self {
            Self::GenericOvpn => 0,
            _ => 51820,
        }
    }

    /// A best-effort HTTPS host that, fetched *through the tunnel*, reports the
    /// observed exit IP — used by the daemon-side verifier to confirm egress is
    /// the provider's, not the WAN. `Some` only where the provider runs a
    /// first-party check endpoint; otherwise the verifier falls back to a
    /// neutral reflector (`ipinfo.io`). Pure — only the target is chosen here.
    #[must_use]
    pub const fn exit_check_host(self) -> Option<&'static str> {
        match self {
            // Mullvad's first-party "am I Mullvad" reflector.
            Self::Mullvad => Some("https://am.i.mullvad.net/json"),
            // The rest have no public first-party reflector → neutral fallback.
            _ => None,
        }
    }

    /// Whether the provider's ToS/keys allow the same account to run multiple
    /// concurrent tunnels (multi-instance). The model allows distinct
    /// `mvpn-<id>` interfaces regardless; this drives the UI's per-provider
    /// guidance. All current providers permit it (`NordVPN` caps the *count* per
    /// account, which the route assignment — not this flag — enforces).
    #[must_use]
    pub const fn allows_multi_instance(self) -> bool {
        true
    }
}

/// The neutral exit-IP reflector used when a provider has no first-party check
/// host. Fetched *through the tunnel*, it reports the observed public IP.
pub const NEUTRAL_EXIT_CHECK_HOST: &str = "https://ipinfo.io/json";

/// The operator-supplied inputs for a WireGuard-based provider setup. The
/// secret material (the private key) is referenced, not inlined into the durable
/// [`TunnelDef`]; the rendered config (which DOES contain the key) is handed to
/// the secret store for age-encryption + distribution (VPN-GW-2/3).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WgSetup {
    /// Operator-chosen tunnel id (drives `mvpn-<id>`). Required.
    pub id: String,
    /// The client's `WireGuard` private key (base64). Required.
    pub private_key: String,
    /// The client's tunnel address(es), e.g. `10.64.0.2/32` (+ an optional v6).
    /// Provider-issued; required — `WireGuard` won't come up without it.
    pub address: String,
    /// The server's `WireGuard` public key (base64). Required.
    pub peer_public_key: String,
    /// The server endpoint host (hostname or IP), e.g. `us-nyc-wg-301.relays...`.
    /// Required. A `:port` suffix overrides [`Provider::default_wg_port`].
    pub endpoint: String,
    /// DNS servers to set inside the tunnel (provider-issued); optional but
    /// strongly recommended to avoid a DNS leak. Comma/space separated.
    #[serde(default)]
    pub dns: String,
    /// Server/region label for the UI + [`TunnelDef::server`] (informational).
    #[serde(default)]
    pub server: String,
    /// Optional pre-shared key (base64) for an extra symmetric layer.
    #[serde(default)]
    pub preshared_key: String,
}

/// A produced tunnel: the [`TunnelDef`] for the durable config + the rendered
/// secret material (a `WireGuard` `.conf` body or an `.ovpn` body) to hand to the
/// secret store. The secret is kept OUT of the `TunnelDef` (which only carries a
/// `creds_ref`); callers age-encrypt `secret` and set `def.creds_ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProducedTunnel {
    /// The durable definition (no secret material).
    pub def: TunnelDef,
    /// The rendered config body containing the secret (WG `.conf` or `.ovpn`).
    pub secret: String,
    /// Which kind of secret body `secret` is (`wg-quick` conf vs. `.ovpn`).
    pub secret_kind: SecretKind,
}

/// What kind of config body a [`ProducedTunnel::secret`] holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    /// A `wg-quick(8)` interface config (`[Interface]`/`[Peer]`), written to
    /// `/etc/wireguard/<ifname>.conf`.
    WgQuick,
    /// An `OpenVPN` client config (`.ovpn`), written to the openvpn config dir.
    Ovpn,
}

/// Errors a provider adapter can return while validating inputs / rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// A required field was empty.
    Missing(&'static str),
    /// A field failed a structural check (e.g. a non-base64 key, a bad CIDR).
    Invalid {
        /// The field name.
        field: &'static str,
        /// Why it's invalid.
        reason: String,
    },
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(field) => write!(f, "missing required field: {field}"),
            Self::Invalid { field, reason } => {
                write!(f, "invalid {field}: {reason}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}

/// Is `s` plausibly a base64 `WireGuard` key? A WG key is 32 bytes → exactly 44
/// base64 chars: 43 from the alphabet `[A-Za-z0-9+/]` followed by a single `=`
/// of padding (the `=` only ever appears as the final char). We don't decode
/// (no extra dep); we check the length + charset + padding shape so an
/// obviously-wrong paste is caught early. Pure.
fn looks_like_wg_key(s: &str) -> bool {
    let s = s.trim();
    let Some(body) = s.strip_suffix('=') else {
        return false;
    };
    // 43 body chars + the one `=` = 44; the body carries no further `=`.
    body.len() == 43
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Validate + normalize a server endpoint into `host:port`, applying the
/// provider default port when none is given. Rejects an empty host.
fn normalize_endpoint(endpoint: &str, default_port: u16) -> Result<String, AdapterError> {
    let ep = endpoint.trim();
    if ep.is_empty() {
        return Err(AdapterError::Missing("endpoint"));
    }
    let no_port_no_default = || AdapterError::Invalid {
        field: "endpoint",
        reason: "no port given and the provider has no default WireGuard port".into(),
    };

    // Bracketed IPv6 `[addr]` or `[addr]:port`. Require a closing `]` (an
    // unclosed bracket like `[2001:db8::1` is malformed → reject rather than
    // mangle). `]:port` is complete; a bare `[addr]` gets the default port.
    if let Some(rest) = ep.strip_prefix('[') {
        let Some((addr, after)) = rest.split_once(']') else {
            return Err(AdapterError::Invalid {
                field: "endpoint",
                reason: "unclosed IPv6 bracket in endpoint".into(),
            });
        };
        if addr.is_empty() {
            return Err(AdapterError::Invalid {
                field: "endpoint",
                reason: "empty IPv6 literal in endpoint".into(),
            });
        }
        return match after.strip_prefix(':') {
            // `[addr]:port` — keep as-is if the port is numeric, else reject.
            Some(port) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                Ok(ep.to_string())
            }
            Some(_) => Err(AdapterError::Invalid {
                field: "endpoint",
                reason: "non-numeric port after IPv6 literal".into(),
            }),
            // `[addr]` with no port — append the default.
            None if after.is_empty() => {
                if default_port == 0 {
                    Err(no_port_no_default())
                } else {
                    Ok(format!("{ep}:{default_port}"))
                }
            }
            // `[addr]junk` — malformed.
            None => Err(AdapterError::Invalid {
                field: "endpoint",
                reason: "trailing characters after IPv6 literal".into(),
            }),
        };
    }

    // A bare (unbracketed) host. More than one colon means a bare IPv6 literal —
    // it MUST be bracketed to carry a port, so reject it.
    let colons = ep.matches(':').count();
    if colons > 1 {
        return Err(AdapterError::Invalid {
            field: "endpoint",
            reason: "bare IPv6 literal must be bracketed, e.g. [2001:db8::1]:51820".into(),
        });
    }
    if let Some((host, port)) = ep.split_once(':') {
        // A trailing/garbage port (`host:`, `host:abc`) is malformed — don't
        // append a default after it, which would yield `host::port`.
        if host.is_empty() {
            return Err(AdapterError::Missing("endpoint host"));
        }
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AdapterError::Invalid {
                field: "endpoint",
                reason: format!("invalid port in endpoint '{ep}'"),
            });
        }
        Ok(ep.to_string())
    } else if default_port == 0 {
        Err(no_port_no_default())
    } else {
        Ok(format!("{ep}:{default_port}"))
    }
}

/// Build a `WireGuard` adapter setup for a first-class provider (or generic-wg),
/// producing a `wg-quick` config + the [`TunnelDef`]. The five providers share
/// the `WireGuard` config shape; only the defaults (port, label, verification
/// host) differ — captured by [`Provider`]. Pure + deterministic.
///
/// # Errors
/// [`AdapterError`] if a required field is missing or structurally invalid.
pub fn build_wg(provider: Provider, setup: &WgSetup) -> Result<ProducedTunnel, AdapterError> {
    if setup.id.trim().is_empty() {
        return Err(AdapterError::Missing("id"));
    }
    if setup.private_key.trim().is_empty() {
        return Err(AdapterError::Missing("private_key"));
    }
    if !looks_like_wg_key(&setup.private_key) {
        return Err(AdapterError::Invalid {
            field: "private_key",
            reason: "not a 44-char base64 WireGuard key".into(),
        });
    }
    if setup.peer_public_key.trim().is_empty() {
        return Err(AdapterError::Missing("peer_public_key"));
    }
    if !looks_like_wg_key(&setup.peer_public_key) {
        return Err(AdapterError::Invalid {
            field: "peer_public_key",
            reason: "not a 44-char base64 WireGuard key".into(),
        });
    }
    if setup.address.trim().is_empty() {
        return Err(AdapterError::Missing("address"));
    }
    if !setup.preshared_key.trim().is_empty() && !looks_like_wg_key(&setup.preshared_key) {
        return Err(AdapterError::Invalid {
            field: "preshared_key",
            reason: "not a 44-char base64 WireGuard key".into(),
        });
    }

    let endpoint = normalize_endpoint(&setup.endpoint, provider.default_wg_port())?;

    let def = TunnelDef {
        id: setup.id.trim().to_string(),
        provider: provider.label().to_string(),
        method: Method::Wg,
        server: if setup.server.trim().is_empty() {
            setup.endpoint.trim().to_string()
        } else {
            setup.server.trim().to_string()
        },
        protocol: "udp".to_string(),
        creds_ref: String::new(), // set by the caller after age-encrypting `secret`
    };
    // The produced def must satisfy the existing model's validation (non-empty
    // id with a usable ifname body) — fail loudly here rather than at save.
    def.validate().map_err(|reason| AdapterError::Invalid {
        field: "id",
        reason,
    })?;

    let secret = render_wg_conf(setup, &endpoint);
    Ok(ProducedTunnel {
        def,
        secret,
        secret_kind: SecretKind::WgQuick,
    })
}

/// Render a `wg-quick(8)` interface config from a [`WgSetup`]. `endpoint` is the
/// already-normalized `host:port`. `AllowedIPs = 0.0.0.0/0, ::/0` routes ALL
/// egress through the tunnel (the gateway carves out Nebula's overlay subnet via
/// policy-routing — VPN-GW-3 — not here). Pure string rendering.
#[must_use]
pub fn render_wg_conf(setup: &WgSetup, endpoint: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    s.push_str("[Interface]\n");
    let _ = writeln!(s, "PrivateKey = {}", setup.private_key.trim());
    let _ = writeln!(s, "Address = {}", setup.address.trim());
    let dns = normalize_dns(&setup.dns);
    if !dns.is_empty() {
        let _ = writeln!(s, "DNS = {dns}");
    }
    s.push('\n');
    s.push_str("[Peer]\n");
    let _ = writeln!(s, "PublicKey = {}", setup.peer_public_key.trim());
    if !setup.preshared_key.trim().is_empty() {
        let _ = writeln!(s, "PresharedKey = {}", setup.preshared_key.trim());
    }
    s.push_str("AllowedIPs = 0.0.0.0/0, ::/0\n");
    let _ = writeln!(s, "Endpoint = {endpoint}");
    // A keepalive keeps the NAT mapping alive behind the mesh's own NAT.
    s.push_str("PersistentKeepalive = 25\n");
    s
}

/// Normalize a comma/space separated DNS list into `a, b` form (deduped, order
/// preserved). Empty in → empty out.
fn normalize_dns(dns: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for tok in dns.split([',', ' ', '\t', '\n']) {
        let tok = tok.trim();
        if !tok.is_empty() && !out.contains(&tok) {
            out.push(tok);
        }
    }
    out.join(", ")
}

/// A parsed `WireGuard` config (the "paste any WG config" generic path). Captures
/// just the fields the adapter needs to (a) sanity-check the paste and (b)
/// derive a [`WgSetup`] so the same render/standup path is reused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedWgConf {
    /// `[Interface] PrivateKey`.
    pub private_key: String,
    /// `[Interface] Address`.
    pub address: String,
    /// `[Interface] DNS` (joined as given).
    pub dns: String,
    /// `[Peer] PublicKey`.
    pub peer_public_key: String,
    /// `[Peer] PresharedKey` (optional).
    pub preshared_key: String,
    /// `[Peer] Endpoint`.
    pub endpoint: String,
    /// `[Peer] AllowedIPs` (kept for inspection; standup re-derives full-tunnel).
    pub allowed_ips: String,
}

/// Parse a pasted `WireGuard` `.conf` (the generic path). Tolerant of comments
/// (`#`/`;`), blank lines, and `key=value` with surrounding spaces; section
/// headers (`[Interface]`/`[Peer]`) are honored so a `PublicKey` under `[Peer]`
/// isn't confused with an interface key. Pure.
///
/// # Errors
/// [`AdapterError`] if the required `WireGuard` fields (`PrivateKey`, Address, the
/// peer `PublicKey`, Endpoint) are absent — an unusable paste is caught here.
pub fn parse_wg_conf(text: &str) -> Result<ParsedWgConf, AdapterError> {
    #[derive(PartialEq)]
    enum Section {
        None,
        Interface,
        Peer,
    }
    let mut section = Section::None;
    let mut p = ParsedWgConf::default();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = match name.trim().to_ascii_lowercase().as_str() {
                "interface" => Section::Interface,
                "peer" => Section::Peer,
                _ => Section::None,
            };
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        // A value may itself contain `=` (base64 padding) — rejoin via the first
        // `=` only, then keep the rest.
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim().to_string();
        match (&section, key.as_str()) {
            (Section::Interface, "privatekey") => p.private_key = val,
            (Section::Interface, "address") => p.address = val,
            (Section::Interface, "dns") => p.dns = val,
            (Section::Peer, "publickey") => p.peer_public_key = val,
            (Section::Peer, "presharedkey") => p.preshared_key = val,
            (Section::Peer, "endpoint") => p.endpoint = val,
            (Section::Peer, "allowedips") => p.allowed_ips = val,
            _ => {}
        }
    }

    if p.private_key.is_empty() {
        return Err(AdapterError::Missing("[Interface] PrivateKey"));
    }
    if p.address.is_empty() {
        return Err(AdapterError::Missing("[Interface] Address"));
    }
    if p.peer_public_key.is_empty() {
        return Err(AdapterError::Missing("[Peer] PublicKey"));
    }
    if p.endpoint.is_empty() {
        return Err(AdapterError::Missing("[Peer] Endpoint"));
    }
    Ok(p)
}

/// The "paste a WG config" path: parse the pasted config, then run it through
/// the same `WireGuard` adapter so the produced tunnel is identical in shape to a
/// first-class one. `id`/`server` are operator-supplied (the paste has no tunnel
/// id). `provider` tags the result — usually [`Provider::GenericWg`], but a
/// named provider's dashboard-exported `.conf` keeps that provider's label (and
/// thus its exit-IP check host). Only WireGuard-method providers are accepted;
/// an `.ovpn`-only provider is rejected (paste an `.ovpn` via [`import_ovpn`]).
///
/// # Errors
/// [`AdapterError`] from parsing, from [`build_wg`], or if `provider` is not a
/// `WireGuard` provider.
pub fn import_wg_paste(
    provider: Provider,
    id: &str,
    server: &str,
    pasted: &str,
) -> Result<ProducedTunnel, AdapterError> {
    if provider.method() != Method::Wg {
        return Err(AdapterError::Invalid {
            field: "provider",
            reason: format!(
                "{} is not a WireGuard provider — paste an .ovpn instead",
                provider.label()
            ),
        });
    }
    let parsed = parse_wg_conf(pasted)?;
    let setup = WgSetup {
        id: id.to_string(),
        private_key: parsed.private_key,
        address: parsed.address,
        peer_public_key: parsed.peer_public_key,
        endpoint: parsed.endpoint,
        dns: parsed.dns,
        server: server.to_string(),
        preshared_key: parsed.preshared_key,
    };
    build_wg(provider, &setup)
}

/// A minimal parse of an `OpenVPN` `.ovpn` (the "import any .ovpn" generic path).
///
/// `.ovpn` is verbatim config — we don't re-render it (it's handed to `openvpn`
/// as-is), so we only extract what's needed to build a [`TunnelDef`] + to catch
/// an obviously-broken file before it's stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedOvpn {
    /// The `remote <host> <port> [proto]` server endpoint (first `remote`).
    pub remote_host: String,
    /// The remote port (from the `remote` line or a `port` directive).
    pub remote_port: u16,
    /// `udp` / `tcp` (from `proto`, the `remote` line, or defaulted to `udp`).
    pub protocol: String,
    /// Whether inline `<ca>`/`<cert>`/`<key>` or `auth-user-pass` is present —
    /// i.e. the file carries (or expects) credentials.
    pub has_auth: bool,
}

/// Parse the essentials out of an `.ovpn`. Tolerant of comments (`#`/`;`),
/// blank lines, and inline cert blocks. Pure.
///
/// # Errors
/// [`AdapterError`] if no usable `remote` directive is present (an `.ovpn` with
/// no server can't stand up a tunnel).
pub fn parse_ovpn(text: &str) -> Result<ParsedOvpn, AdapterError> {
    let mut p = ParsedOvpn {
        protocol: "udp".to_string(),
        ..Default::default()
    };
    let mut explicit_proto: Option<String> = None;
    let mut explicit_port: Option<u16> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut toks = line.split_whitespace();
        let Some(directive) = toks.next() else {
            continue;
        };
        match directive.to_ascii_lowercase().as_str() {
            "remote" if p.remote_host.is_empty() => {
                // `remote <host> [port] [proto]` — port + proto are both
                // optional and proto may appear in the port's slot when the
                // port is omitted (`remote host tcp`). Classify each remaining
                // token by shape rather than position so a port-less proto
                // token isn't swallowed by the port parse.
                if let Some(host) = toks.next() {
                    p.remote_host = host.to_string();
                }
                for tok in toks {
                    if let Ok(port) = tok.parse::<u16>() {
                        p.remote_port = port;
                    } else {
                        let proto = tok.to_ascii_lowercase();
                        if proto.starts_with("tcp") {
                            p.protocol = "tcp".to_string();
                        } else if proto.starts_with("udp") {
                            p.protocol = "udp".to_string();
                        }
                    }
                }
            }
            "proto" => {
                if let Some(proto) = toks.next() {
                    let proto = proto.to_ascii_lowercase();
                    explicit_proto = Some(if proto.starts_with("tcp") {
                        "tcp".to_string()
                    } else {
                        "udp".to_string()
                    });
                }
            }
            "port" => {
                explicit_port = toks.next().and_then(|s| s.parse::<u16>().ok());
            }
            // `auth-user-pass` or an inline cert block signals embedded creds.
            "auth-user-pass" | "<ca>" | "<cert>" | "<key>" | "<tls-auth>" | "<tls-crypt>" => {
                p.has_auth = true;
            }
            _ => {}
        }
    }

    if p.remote_host.is_empty() {
        return Err(AdapterError::Missing("remote (no OpenVPN server in .ovpn)"));
    }
    // An explicit `proto`/`port` directive overrides the `remote` line tokens.
    if let Some(proto) = explicit_proto {
        p.protocol = proto;
    }
    if let Some(port) = explicit_port {
        p.remote_port = port;
    }
    if p.remote_port == 0 {
        p.remote_port = if p.protocol == "tcp" { 443 } else { 1194 };
    }
    Ok(p)
}

/// The generic "import any .ovpn" path: parse the `.ovpn`, then build the
/// [`TunnelDef`] + carry the verbatim `.ovpn` body as the secret. `id`/`server`
/// operator-supplied. Tagged `provider = "generic-ovpn"`, method `Ovpn`.
///
/// # Errors
/// [`AdapterError`] from [`parse_ovpn`] or from the produced def's validation.
pub fn import_ovpn(id: &str, server: &str, ovpn: &str) -> Result<ProducedTunnel, AdapterError> {
    let parsed = parse_ovpn(ovpn)?;
    let def = TunnelDef {
        id: id.trim().to_string(),
        provider: Provider::GenericOvpn.label().to_string(),
        method: Method::Ovpn,
        server: if server.trim().is_empty() {
            format!("{}:{}", parsed.remote_host, parsed.remote_port)
        } else {
            server.trim().to_string()
        },
        protocol: parsed.protocol,
        creds_ref: String::new(),
    };
    def.validate().map_err(|reason| AdapterError::Invalid {
        field: "id",
        reason,
    })?;
    Ok(ProducedTunnel {
        def,
        // The .ovpn is handed to `openvpn` verbatim — don't re-render it.
        secret: ovpn.to_string(),
        secret_kind: SecretKind::Ovpn,
    })
}

/// The exit-IP check target for a produced tunnel's provider: the provider's
/// first-party reflector where one exists, else the neutral reflector. The
/// daemon-side verifier curls this *through the tunnel* and compares the
/// reported IP to the WAN IP (different ⇒ egress really goes out the tunnel).
#[must_use]
pub fn exit_check_target(provider: Provider) -> &'static str {
    provider
        .exit_check_host()
        .unwrap_or(NEUTRAL_EXIT_CHECK_HOST)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid-looking 44-char base64 WG key (32 zero bytes → "AAA…A=").
    const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const KEY_B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB=";

    fn wg_setup(id: &str) -> WgSetup {
        WgSetup {
            id: id.into(),
            private_key: KEY_A.into(),
            address: "10.64.0.2/32".into(),
            peer_public_key: KEY_B.into(),
            endpoint: "us-nyc-wg-301.relays.example".into(),
            dns: "10.64.0.1".into(),
            server: "us-nyc".into(),
            preshared_key: String::new(),
        }
    }

    #[test]
    fn provider_label_round_trips() {
        for p in [
            Provider::Mullvad,
            Provider::Proton,
            Provider::Ivpn,
            Provider::Nord,
            Provider::Surfshark,
            Provider::GenericWg,
            Provider::GenericOvpn,
        ] {
            assert_eq!(Provider::from_label(p.label()), Some(p), "{}", p.label());
        }
        // Aliases.
        assert_eq!(Provider::from_label("protonvpn"), Some(Provider::Proton));
        assert_eq!(Provider::from_label("NordVPN"), Some(Provider::Nord));
        assert_eq!(Provider::from_label("openvpn"), Some(Provider::GenericOvpn));
        assert_eq!(Provider::from_label("nonesuch"), None);
    }

    #[test]
    fn provider_method_and_cli_lock() {
        assert_eq!(Provider::Mullvad.method(), Method::Wg);
        assert_eq!(Provider::GenericOvpn.method(), Method::Ovpn);
        assert_eq!(Provider::Mullvad.cli(), Some("mullvad"));
        assert_eq!(Provider::Nord.cli(), Some("nordvpn"));
        assert_eq!(Provider::Surfshark.cli(), None);
        assert_eq!(Provider::GenericWg.cli(), None);
    }

    // ── per-provider WG config generation (the 5 named + generic) ──

    #[test]
    fn build_wg_mullvad_renders_full_tunnel_conf() {
        let t = build_wg(Provider::Mullvad, &wg_setup("mullvad1")).unwrap();
        assert_eq!(t.def.provider, "mullvad");
        assert_eq!(t.def.method, Method::Wg);
        assert_eq!(t.def.protocol, "udp");
        assert_eq!(t.def.ifname(), "mvpn-mullvad1");
        assert_eq!(t.secret_kind, SecretKind::WgQuick);
        // The rendered conf is a full-tunnel wg-quick config.
        assert!(t.secret.contains("[Interface]"));
        assert!(t.secret.contains(&format!("PrivateKey = {KEY_A}")));
        assert!(t.secret.contains("Address = 10.64.0.2/32"));
        assert!(t.secret.contains("DNS = 10.64.0.1"));
        assert!(t.secret.contains("[Peer]"));
        assert!(t.secret.contains(&format!("PublicKey = {KEY_B}")));
        assert!(t.secret.contains("AllowedIPs = 0.0.0.0/0, ::/0"));
        // The default WG port was appended.
        assert!(
            t.secret
                .contains("Endpoint = us-nyc-wg-301.relays.example:51820"),
            "{}",
            t.secret
        );
        assert!(t.secret.contains("PersistentKeepalive = 25"));
        // No creds inline in the durable def.
        assert!(t.def.creds_ref.is_empty());
    }

    #[test]
    fn build_wg_each_named_provider_tags_its_label() {
        for (p, label) in [
            (Provider::Mullvad, "mullvad"),
            (Provider::Proton, "proton"),
            (Provider::Ivpn, "ivpn"),
            (Provider::Nord, "nord"),
            (Provider::Surfshark, "surfshark"),
        ] {
            let t = build_wg(p, &wg_setup("t1")).unwrap();
            assert_eq!(t.def.provider, label);
            assert_eq!(t.secret_kind, SecretKind::WgQuick);
            assert!(t.secret.contains("AllowedIPs = 0.0.0.0/0, ::/0"));
        }
    }

    #[test]
    fn build_wg_preserves_explicit_endpoint_port() {
        let mut s = wg_setup("t1");
        s.endpoint = "1.2.3.4:53".into();
        let t = build_wg(Provider::Proton, &s).unwrap();
        assert!(t.secret.contains("Endpoint = 1.2.3.4:53"), "{}", t.secret);
    }

    #[test]
    fn build_wg_renders_preshared_key_when_given() {
        let mut s = wg_setup("t1");
        s.preshared_key = KEY_B.into();
        let t = build_wg(Provider::Ivpn, &s).unwrap();
        assert!(t.secret.contains(&format!("PresharedKey = {KEY_B}")));
    }

    #[test]
    fn build_wg_rejects_missing_and_malformed_fields() {
        // Missing id.
        let mut s = wg_setup("");
        assert_eq!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Missing("id"))
        );
        // Malformed key.
        s = wg_setup("t1");
        s.private_key = "not-a-key".into();
        assert!(matches!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Invalid {
                field: "private_key",
                ..
            })
        ));
        // Missing address.
        s = wg_setup("t1");
        s.address.clear();
        assert_eq!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Missing("address"))
        );
        // Missing endpoint.
        s = wg_setup("t1");
        s.endpoint.clear();
        assert_eq!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Missing("endpoint"))
        );
        // An id with no alnum body fails the def validation.
        s = wg_setup("___");
        assert!(matches!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Invalid { field: "id", .. })
        ));
    }

    #[test]
    fn dns_is_normalized_and_deduped() {
        let mut s = wg_setup("t1");
        s.dns = "1.1.1.1, 1.1.1.1  8.8.8.8".into();
        let t = build_wg(Provider::Surfshark, &s).unwrap();
        assert!(t.secret.contains("DNS = 1.1.1.1, 8.8.8.8"), "{}", t.secret);
        // No DNS line when none given.
        s.dns.clear();
        let t = build_wg(Provider::Surfshark, &s).unwrap();
        assert!(!t.secret.contains("DNS ="), "{}", t.secret);
    }

    // ── generic "paste WG config" import + parse ──

    #[test]
    fn parse_wg_conf_reads_interface_and_peer() {
        let conf = format!(
            "# my config\n\
             [Interface]\n\
             PrivateKey = {KEY_A}\n\
             Address = 10.64.0.2/32, fc00::2/128\n\
             DNS = 10.64.0.1\n\
             \n\
             [Peer]\n\
             PublicKey = {KEY_B}\n\
             AllowedIPs = 0.0.0.0/0, ::/0\n\
             Endpoint = 198.51.100.7:51820\n"
        );
        let p = parse_wg_conf(&conf).unwrap();
        assert_eq!(p.private_key, KEY_A);
        assert_eq!(p.address, "10.64.0.2/32, fc00::2/128");
        assert_eq!(p.dns, "10.64.0.1");
        assert_eq!(p.peer_public_key, KEY_B);
        assert_eq!(p.endpoint, "198.51.100.7:51820");
        assert_eq!(p.allowed_ips, "0.0.0.0/0, ::/0");
    }

    #[test]
    fn parse_wg_conf_section_scoping_and_errors() {
        // A PublicKey under [Interface] must NOT fill the peer key.
        let bad = format!(
            "[Interface]\nPrivateKey = {KEY_A}\nPublicKey = {KEY_B}\nAddress = 10.0.0.2/32\n"
        );
        assert_eq!(
            parse_wg_conf(&bad),
            Err(AdapterError::Missing("[Peer] PublicKey"))
        );
        // Missing endpoint.
        let no_ep = format!("[Interface]\nPrivateKey = {KEY_A}\nAddress = 10.0.0.2/32\n[Peer]\nPublicKey = {KEY_B}\n");
        assert_eq!(
            parse_wg_conf(&no_ep),
            Err(AdapterError::Missing("[Peer] Endpoint"))
        );
    }

    #[test]
    fn import_wg_paste_produces_standable_tunnel() {
        let conf = format!(
            "[Interface]\nPrivateKey = {KEY_A}\nAddress = 10.64.0.2/32\nDNS = 10.64.0.1\n[Peer]\nPublicKey = {KEY_B}\nAllowedIPs = 0.0.0.0/0\nEndpoint = vpn.example.net:51820\n"
        );
        let t = import_wg_paste(Provider::GenericWg, "paste1", "frankfurt", &conf).unwrap();
        assert_eq!(t.def.provider, "generic-wg");
        assert_eq!(t.def.method, Method::Wg);
        assert_eq!(t.def.server, "frankfurt");
        assert_eq!(t.def.ifname(), "mvpn-paste1");
        assert!(t.secret.contains("Endpoint = vpn.example.net:51820"));
        // Standup re-derives the full-tunnel AllowedIPs regardless of the paste.
        assert!(t.secret.contains("AllowedIPs = 0.0.0.0/0, ::/0"));
        // A named provider's dashboard-exported .conf keeps that provider's
        // label (and its exit-check host), not the generic one.
        let m = import_wg_paste(Provider::Mullvad, "m1", "", &conf).unwrap();
        assert_eq!(m.def.provider, "mullvad");
        // An .ovpn-only provider can't take a WG paste.
        assert!(matches!(
            import_wg_paste(Provider::GenericOvpn, "x", "", &conf),
            Err(AdapterError::Invalid {
                field: "provider",
                ..
            })
        ));
    }

    // ── generic "import .ovpn" parse ──

    #[test]
    fn parse_ovpn_extracts_remote_proto_port() {
        let ovpn = "client\n\
                    dev tun\n\
                    remote us-nyc.example.com 1194 udp\n\
                    auth-user-pass\n\
                    <ca>\n-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n</ca>\n";
        let p = parse_ovpn(ovpn).unwrap();
        assert_eq!(p.remote_host, "us-nyc.example.com");
        assert_eq!(p.remote_port, 1194);
        assert_eq!(p.protocol, "udp");
        assert!(p.has_auth);
    }

    #[test]
    fn parse_ovpn_honors_explicit_proto_and_port_directives() {
        let ovpn = "remote vpn.example.com\nproto tcp\nport 443\n";
        let p = parse_ovpn(ovpn).unwrap();
        assert_eq!(p.remote_host, "vpn.example.com");
        assert_eq!(p.protocol, "tcp");
        assert_eq!(p.remote_port, 443);
    }

    #[test]
    fn parse_ovpn_defaults_port_by_proto_when_absent() {
        let udp = parse_ovpn("remote a.example\n").unwrap();
        assert_eq!(udp.remote_port, 1194);
        let tcp = parse_ovpn("remote a.example\nproto tcp\n").unwrap();
        assert_eq!(tcp.remote_port, 443);
    }

    #[test]
    fn parse_ovpn_remote_line_proto_without_port_is_not_swallowed() {
        // `remote <host> <proto>` (no port) — the proto must survive and the
        // port default by proto, not be eaten by the port slot.
        let tcp = parse_ovpn("remote vpn.example.com tcp\n").unwrap();
        assert_eq!(tcp.remote_host, "vpn.example.com");
        assert_eq!(tcp.protocol, "tcp");
        assert_eq!(tcp.remote_port, 443);
        // `remote <host> <port>` (no proto) still reads the port.
        let p = parse_ovpn("remote h.example 51820\n").unwrap();
        assert_eq!(p.remote_port, 51820);
        assert_eq!(p.protocol, "udp");
    }

    #[test]
    fn parse_ovpn_rejects_no_remote() {
        assert_eq!(
            parse_ovpn("client\ndev tun\n"),
            Err(AdapterError::Missing("remote (no OpenVPN server in .ovpn)"))
        );
    }

    #[test]
    fn import_ovpn_carries_verbatim_body_and_tags_generic() {
        let ovpn = "client\nremote nl-ams.example.com 1194 udp\nauth-user-pass\n";
        let t = import_ovpn("ovpn1", "", ovpn).unwrap();
        assert_eq!(t.def.provider, "generic-ovpn");
        assert_eq!(t.def.method, Method::Ovpn);
        assert_eq!(t.def.protocol, "udp");
        // server defaulted to host:port.
        assert_eq!(t.def.server, "nl-ams.example.com:1194");
        assert_eq!(t.secret_kind, SecretKind::Ovpn);
        // The .ovpn is carried verbatim (handed to `openvpn` as-is).
        assert_eq!(t.secret, ovpn);
    }

    // ── exit-IP verification target derivation ──

    #[test]
    fn exit_check_target_is_provider_first_party_or_neutral() {
        assert_eq!(
            exit_check_target(Provider::Mullvad),
            "https://am.i.mullvad.net/json"
        );
        // No first-party reflector → neutral.
        assert_eq!(exit_check_target(Provider::Proton), NEUTRAL_EXIT_CHECK_HOST);
        assert_eq!(
            exit_check_target(Provider::Surfshark),
            NEUTRAL_EXIT_CHECK_HOST
        );
    }

    #[test]
    fn endpoint_normalization_ipv6_and_default_port() {
        // Bracketed IPv6 with a port is preserved.
        let mut s = wg_setup("t1");
        s.endpoint = "[2001:db8::1]:51820".into();
        let t = build_wg(Provider::Mullvad, &s).unwrap();
        assert!(
            t.secret.contains("Endpoint = [2001:db8::1]:51820"),
            "{}",
            t.secret
        );
        // A bare hostname gets the default port.
        s.endpoint = "host.example".into();
        let t = build_wg(Provider::Mullvad, &s).unwrap();
        assert!(
            t.secret.contains("Endpoint = host.example:51820"),
            "{}",
            t.secret
        );
        // A bare (unbracketed) IPv6 literal is rejected — it must be bracketed.
        s.endpoint = "2001:db8::1".into();
        assert!(matches!(
            build_wg(Provider::Mullvad, &s),
            Err(AdapterError::Invalid {
                field: "endpoint",
                ..
            })
        ));
    }

    #[test]
    fn endpoint_normalization_rejects_malformed_not_mangle() {
        let bad = |ep: &str| {
            let mut s = wg_setup("t1");
            s.endpoint = ep.into();
            build_wg(Provider::Mullvad, &s)
        };
        // Trailing colon must NOT become `host::51820`.
        assert!(matches!(
            bad("host.example:"),
            Err(AdapterError::Invalid {
                field: "endpoint",
                ..
            })
        ));
        // Non-numeric port.
        assert!(matches!(
            bad("host.example:https"),
            Err(AdapterError::Invalid {
                field: "endpoint",
                ..
            })
        ));
        // Unclosed IPv6 bracket must NOT become `[2001:db8::1:51820`.
        assert!(matches!(
            bad("[2001:db8::1"),
            Err(AdapterError::Invalid {
                field: "endpoint",
                ..
            })
        ));
        // Non-numeric port after a bracketed IPv6.
        assert!(matches!(
            bad("[2001:db8::1]:https"),
            Err(AdapterError::Invalid {
                field: "endpoint",
                ..
            })
        ));
        // A bracketed IPv6 with no port gets the default appended.
        let mut s = wg_setup("t1");
        s.endpoint = "[2001:db8::1]".into();
        let t = build_wg(Provider::Mullvad, &s).unwrap();
        assert!(
            t.secret.contains("Endpoint = [2001:db8::1]:51820"),
            "{}",
            t.secret
        );
    }

    #[test]
    fn looks_like_wg_key_rejects_interior_padding() {
        // A real key: 43 body chars + a single trailing '='.
        assert!(looks_like_wg_key(KEY_A));
        assert_eq!(KEY_A.len(), 44);
        // Interior '=' (still 44 chars, still ends in '=') must be rejected:
        // 21 'A's + '=' + 21 'B's + '=' = 44 chars with an interior '='.
        let interior44 = "AAAAAAAAAAAAAAAAAAAAA=BBBBBBBBBBBBBBBBBBBBB=";
        assert_eq!(interior44.len(), 44);
        assert!(interior44[..43].contains('='));
        assert!(!looks_like_wg_key(interior44));
        // No trailing '=' → reject (44 chars, all alnum).
        assert!(!looks_like_wg_key(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
        // Wrong length → reject.
        assert!(!looks_like_wg_key("AAA="));
    }
}
