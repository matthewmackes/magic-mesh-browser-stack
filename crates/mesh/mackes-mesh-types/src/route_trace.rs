//! ROUTE-TRACE-1 — the typed **PathGraph** result of `action/route/trace`
//! (design: `docs/design/route-trace.md`).
//!
//! A PathGraph is the logical path between two endpoints — a topology of typed
//! **nodes** (endpoints + waypoints: host/VM/container, overlay peers, the VPN
//! gateway + exit, the lighthouse ingress, the internet cloud, the service) and
//! **edges** (links carrying the per-segment layer + transport + RTT/loss + an
//! optional firewall/control verdict). The mackesd responder assembles it from
//! real state (compute/service inventory, routing/netstate, VPN-GW, the CONNECT
//! ingress mapping) and the GUI renders it; this crate is the **shared model +
//! the pure assembly/derivation logic** both sides agree on.
//!
//! Pure + serde — no I/O. The state-gathering lives in `mackesd`; here we keep
//! the types, the builder, and the derivations (`blocked_at`, edge validation)
//! that are identical regardless of how the segments were measured.

use serde::{Deserialize, Serialize};

/// Which way the path is traced (lock #2 — a selectable perspective).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// A mesh node → an external destination, through gateway + tunnel.
    #[default]
    Egress,
    /// An external client → a published service, through the lighthouse ingress.
    Ingress,
}

/// What a path node represents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    /// A host-level endpoint (a mesh node).
    #[default]
    Host,
    /// A libvirt/KVM virtual machine.
    Vm,
    /// A Podman container.
    Container,
    /// Another node on the Nebula overlay (a waypoint).
    OverlayPeer,
    /// The VPN egress gateway node.
    Gateway,
    /// The VPN provider's exit (the public egress IP).
    VpnExit,
    /// The lighthouse reverse-proxy ingress.
    Ingress,
    /// The public internet cloud (an opaque waypoint).
    Internet,
    /// A published service (the destination of an ingress trace / a mesh service).
    Service,
}

/// The network layer an edge crosses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layer {
    /// Local to a host (loopback / host↔its-VM/container).
    #[default]
    Host,
    /// The Nebula overlay (peer↔peer).
    Mesh,
    /// A VPN tunnel (gateway↔exit).
    Vpn,
    /// The public internet.
    Public,
}

/// How an edge's traffic is carried.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// A direct Nebula overlay tunnel (hole-punched).
    #[default]
    DirectOverlay,
    /// A relayed Nebula overlay path (via a lighthouse).
    RelayOverlay,
    /// A WireGuard/OpenVPN tunnel.
    VpnTunnel,
    /// Plain public-internet routing.
    Public,
    /// On-host loopback (host ↔ its own VM/container/service).
    Loopback,
}

/// A firewall/control verdict on an edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The control point permits this segment.
    #[default]
    Allow,
    /// The control point denies this segment (the path is blocked here).
    Block,
    /// The control point's rule set could not be statically resolved (e.g. a
    /// remote host's firewalld we can't read) — the verdict is unknown and is
    /// **never guessed**. An indeterminate edge does NOT set `blocked_at`.
    Indeterminate,
}

/// A control point an edge crosses (a firewall / kill-switch) + its verdict.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlPoint {
    /// Which firewall/control evaluated this segment (e.g. `firewalld:public`).
    pub firewall: String,
    /// Allow or block.
    pub verdict: Verdict,
    /// The matching rule, cited (e.g. `--add-port 443/tcp` / `default deny`).
    pub rule: String,
}

/// One node (endpoint or waypoint) in the path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathNode {
    /// Stable id, referenced by edges' `from`/`to`.
    pub id: String,
    /// What this node is.
    pub kind: NodeKind,
    /// Human label (hostname / service name / "Internet" / the exit region).
    pub label: String,
    /// LAN/host IP, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ip: Option<String>,
    /// Nebula overlay IP, if on the overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlay_ip: Option<String>,
    /// Public IP, if applicable (the VPN exit, the ingress lighthouse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_ip: Option<String>,
    /// Public DNS name, if published (DDNS / the ingress hostname).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns_name: Option<String>,
    /// The node hosting this endpoint (for a VM/container/service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosting_node: Option<String>,
}

/// One edge (segment) linking two nodes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PathEdge {
    /// Source node id.
    pub from: String,
    /// Destination node id.
    pub to: String,
    /// The layer this segment crosses.
    pub layer: Layer,
    /// How the segment is carried.
    pub transport: Transport,
    /// Measured round-trip time (ms), when a probe ran for this segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Measured loss fraction 0.0..=1.0, when probed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<f64>,
    /// The firewall/control verdict on this segment, if it crosses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ControlPoint>,
}

impl PathEdge {
    /// True when a control point on this edge denies the segment.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|c| c.verdict == Verdict::Block)
    }

    /// A stable edge id (`<from>-><to>`) for `blocked_at` references.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}->{}", self.from, self.to)
    }
}

/// The typed path between two endpoints.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PathGraph {
    /// Which perspective this trace was built from.
    pub direction: Direction,
    /// Endpoints + waypoints.
    pub nodes: Vec<PathNode>,
    /// Segments linking them, source→dest order.
    pub edges: Vec<PathEdge>,
    /// The id (`<from>-><to>`) of the first denying control point, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_at: Option<String>,
}

impl PathGraph {
    /// Start an empty graph for `direction`.
    #[must_use]
    pub fn new(direction: Direction) -> Self {
        Self {
            direction,
            ..Default::default()
        }
    }

    /// Append a node (builder style).
    #[must_use]
    pub fn with_node(mut self, node: PathNode) -> Self {
        self.nodes.push(node);
        self
    }

    /// Append an edge (builder style).
    #[must_use]
    pub fn with_edge(mut self, edge: PathEdge) -> Self {
        self.edges.push(edge);
        self
    }

    /// Re-derive [`Self::blocked_at`] from the edges — the id of the FIRST edge
    /// (source→dest order) whose control point blocks. Call after assembling all
    /// edges. Returns the blocked edge id (also stored).
    pub fn recompute_blocked_at(&mut self) -> Option<String> {
        self.blocked_at = self.edges.iter().find(|e| e.is_blocked()).map(PathEdge::id);
        self.blocked_at.clone()
    }

    /// True when the path reaches the destination unblocked.
    #[must_use]
    pub const fn is_reachable(&self) -> bool {
        self.blocked_at.is_none()
    }

    /// Validate that every edge references nodes that exist — a self-consistency
    /// check the assembler must satisfy before returning a graph (a dangling
    /// edge would render as a floating segment). Returns the first offending
    /// `(edge-id, missing-node-id)`.
    ///
    /// # Errors
    /// `(edge_id, missing_node_id)` for the first edge with an unknown endpoint.
    pub fn validate(&self) -> Result<(), (String, String)> {
        let ids: std::collections::HashSet<&str> =
            self.nodes.iter().map(|n| n.id.as_str()).collect();
        for e in &self.edges {
            if !ids.contains(e.from.as_str()) {
                return Err((e.id(), e.from.clone()));
            }
            if !ids.contains(e.to.as_str()) {
                return Err((e.id(), e.to.clone()));
            }
        }
        Ok(())
    }

    /// Serialize to a JSON string (the `action/route/trace` reply body).
    ///
    /// # Errors
    /// A serde JSON error.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

// ---- Assembly from real mesh state (ROUTE-TRACE-1) -------------------------
//
// Pure assemblers: given the already-resolved state (an exposure policy + the
// hosting peer's overlay IP + the ingress lighthouse's public IP), build the
// PathGraph. The `mackesd` responder gathers that state (exposure::load,
// read_peers, the nebula roster) and calls these; keeping the assembly here
// makes it unit-testable without the daemon + lets the GUI reason about the same
// shapes. VPN-GW segments layer in once that epic exists (no placeholder now).

use crate::exposure::ExposurePolicy;

/// Build the **ingress** path for a published service from its CONNECT exposure
/// policy: `Internet → Ingress(lighthouse) → <hosting node> → Service`. The
/// control point on the internet→ingress edge is the public boundary: a
/// public-via-ingress service is `Allow` (the firewalld profile + Caddy open it),
/// a mesh-only service is `Block` ("not published — mesh-only"), so a trace to an
/// unexposed service shows exactly where it's refused. `peer_overlay_ip` /
/// `lighthouse_public_ip` are looked up by the caller (the peer directory /
/// nebula roster); `None` is rendered as an unknown address, never a guess.
#[must_use]
pub fn assemble_ingress(
    svc: &ExposurePolicy,
    peer_overlay_ip: Option<&str>,
    lighthouse_public_ip: Option<&str>,
) -> PathGraph {
    let public = svc.is_public();
    let (lh_label, hostname) = match &svc.ingress {
        Some(b) => (b.lighthouse.clone(), Some(b.hostname.clone())),
        None => ("(no ingress)".to_string(), None),
    };
    let host_node = &svc.source.node;

    let net = PathNode {
        id: "internet".into(),
        kind: NodeKind::Internet,
        label: "Internet".into(),
        ..Default::default()
    };
    let ingress = PathNode {
        id: "ingress".into(),
        kind: NodeKind::Ingress,
        label: lh_label,
        public_ip: lighthouse_public_ip.map(str::to_string),
        dns_name: hostname,
        ..Default::default()
    };
    let host = PathNode {
        id: "host".into(),
        kind: NodeKind::OverlayPeer,
        label: host_node.clone(),
        overlay_ip: peer_overlay_ip.map(str::to_string),
        ..Default::default()
    };
    let service = PathNode {
        id: "service".into(),
        kind: NodeKind::Service,
        label: format!("{} ({}/{})", svc.id, svc.source.port, svc.source.proto),
        hosting_node: Some(host_node.clone()),
        ..Default::default()
    };

    let boundary = ControlPoint {
        firewall: "firewalld:public + caddy".into(),
        verdict: if public {
            Verdict::Allow
        } else {
            Verdict::Block
        },
        rule: if public {
            format!("published via {} (CONNECT exposure)", svc.source.proto)
        } else {
            "not published — mesh-only (no CONNECT exposure)".into()
        },
    };

    let mut g = PathGraph::new(Direction::Ingress)
        .with_node(net)
        .with_node(ingress)
        .with_node(host)
        .with_node(service)
        .with_edge(PathEdge {
            from: "internet".into(),
            to: "ingress".into(),
            layer: Layer::Public,
            transport: Transport::Public,
            control: Some(boundary),
            ..Default::default()
        })
        .with_edge(PathEdge {
            from: "ingress".into(),
            to: "host".into(),
            layer: Layer::Mesh,
            transport: Transport::DirectOverlay,
            ..Default::default()
        })
        .with_edge(PathEdge {
            from: "host".into(),
            to: "service".into(),
            layer: Layer::Host,
            transport: Transport::Loopback,
            ..Default::default()
        });
    g.recompute_blocked_at();
    g
}

/// Build the **egress** path from a mesh node to an external destination. Without
/// VPN-GW this is the plain WAN egress: `Host(source) → Internet(dest)`; once
/// VPN-GW exists the gateway + tunnel + exit nodes splice between them. `source`
/// is the originating node's label + overlay IP; `dest_label` is the destination
/// (a hostname/IP). Pure — no VPN placeholder is invented.
#[must_use]
pub fn assemble_egress(
    source_label: &str,
    source_overlay_ip: Option<&str>,
    dest_label: &str,
) -> PathGraph {
    let mut g = PathGraph::new(Direction::Egress)
        .with_node(PathNode {
            id: "source".into(),
            kind: NodeKind::Host,
            label: source_label.to_string(),
            overlay_ip: source_overlay_ip.map(str::to_string),
            ..Default::default()
        })
        .with_node(PathNode {
            id: "internet".into(),
            kind: NodeKind::Internet,
            label: dest_label.to_string(),
            ..Default::default()
        })
        .with_edge(PathEdge {
            from: "source".into(),
            to: "internet".into(),
            layer: Layer::Public,
            transport: Transport::Public,
            ..Default::default()
        });
    g.recompute_blocked_at();
    g
}

// ---------------------------------------------------------------------------
// ROUTE-TRACE-2: control-point evaluation.
//
// Given the assembled PathGraph and the actual firewall rule sets each segment
// crosses, decide Allow / Block / Indeterminate per edge — citing the matching
// rule — and let `recompute_blocked_at` find the FIRST denying point. The rule
// sets are gathered by the `mackesd` responder (the destination host's firewalld
// public zone, the Nebula overlay firewall, a VPN kill-switch) and fed in here so
// the decision is pure + unit-testable and the GUI reasons about the same shapes.
// A rule set we can't read is `Indeterminate` — never guessed (the §7 no-fake rule
// applied to verdicts).
// ---------------------------------------------------------------------------

/// One ordered firewall rule: match a `(port, proto)` flow, take an action.
///
/// Evaluated **first-match-wins** (nftables/iptables order; firewalld allows are
/// modelled as leading `Allow` rules over the zone's default `Block`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirewallRule {
    /// The port this rule matches, or `None` for "any port".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The protocol this rule matches (`tcp`/`udp`), or `None` for "any proto".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proto: Option<String>,
    /// What this rule does when it matches (`Allow` or `Block`).
    pub action: Verdict,
    /// Human-cited form of the rule (e.g. `--add-port 4242/udp`, `default deny`).
    pub cite: String,
}

impl FirewallRule {
    /// True when this rule matches the given flow (`None` fields are wildcards).
    #[must_use]
    pub fn matches(&self, port: u16, proto: &str) -> bool {
        self.port.is_none_or(|p| p == port)
            && self
                .proto
                .as_deref()
                .is_none_or(|pr| pr.eq_ignore_ascii_case(proto))
    }
}

/// A firewall as ROUTE-TRACE evaluates it.
///
/// Either an ordered, readable rule set (with a default verdict for "no rule
/// matched"), or `Indeterminate` when the real rules could not be resolved (a
/// remote host we can't read) — in which case every flow is `Indeterminate`,
/// never guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FirewallProfile {
    /// A resolved, ordered rule set evaluated first-match-wins.
    Rules {
        /// The control's name (e.g. `firewalld:public`).
        name: String,
        /// Ordered rules; the first to match decides.
        rules: Vec<FirewallRule>,
        /// The verdict when no rule matches (firewalld public zone = `Block`).
        default: Verdict,
    },
    /// The rule set could not be read — every flow is `Indeterminate`.
    Indeterminate {
        /// The control's name + why it's unresolved (e.g. `firewalld:public (remote, unread)`).
        name: String,
    },
}

impl FirewallProfile {
    /// Evaluate a `(port, proto)` flow into a [`ControlPoint`], first-match-wins.
    /// An `Indeterminate` profile always yields an `Indeterminate` verdict.
    #[must_use]
    pub fn evaluate(&self, port: u16, proto: &str) -> ControlPoint {
        match self {
            Self::Rules {
                name,
                rules,
                default,
            } => rules.iter().find(|r| r.matches(port, proto)).map_or_else(
                || ControlPoint {
                    firewall: name.clone(),
                    verdict: *default,
                    rule: match default {
                        Verdict::Block => "default deny (no matching rule)".into(),
                        Verdict::Allow => "default allow (no matching rule)".into(),
                        Verdict::Indeterminate => "default indeterminate".into(),
                    },
                },
                |r| ControlPoint {
                    firewall: name.clone(),
                    verdict: r.action,
                    rule: r.cite.clone(),
                },
            ),
            Self::Indeterminate { name } => ControlPoint {
                firewall: name.clone(),
                verdict: Verdict::Indeterminate,
                rule: "host firewall not statically resolvable — not guessed".into(),
            },
        }
    }

    /// The platform **public-boundary baseline** (CONNECT §3 lock): the public
    /// zone denies all inbound except the foundational ports — Nebula UDP/4242,
    /// SSH/22, enroll TCP/4243 — plus the covert TCP/443 fallback on lighthouses.
    /// This is the rule set every non-ingress node enforces; ROUTE-TRACE evaluates
    /// an inbound public flow against it to show why a port is or isn't reachable.
    #[must_use]
    pub fn public_baseline(is_lighthouse: bool) -> Self {
        let mut rules = vec![
            FirewallRule {
                port: Some(4242),
                proto: Some("udp".into()),
                action: Verdict::Allow,
                cite: "--add-port 4242/udp (Nebula)".into(),
            },
            FirewallRule {
                port: Some(22),
                proto: Some("tcp".into()),
                action: Verdict::Allow,
                cite: "--add-service ssh (22/tcp)".into(),
            },
            FirewallRule {
                port: Some(4243),
                proto: Some("tcp".into()),
                action: Verdict::Allow,
                cite: "--add-port 4243/tcp (enroll)".into(),
            },
        ];
        if is_lighthouse {
            rules.push(FirewallRule {
                port: Some(443),
                proto: Some("tcp".into()),
                action: Verdict::Allow,
                cite: "--add-port 443/tcp (covert fallback, lighthouse)".into(),
            });
        }
        Self::Rules {
            name: "firewalld:public".into(),
            rules,
            default: Verdict::Block,
        }
    }

    /// The **Nebula overlay firewall** as the platform pins it: hard-coded
    /// open-mesh (any peer with a valid mesh cert reaches any other — §8 flat-trust
    /// lock). Every overlay segment crosses this control point and is `Allow`d; the
    /// rule is cited so a trace shows the intra-mesh trust is intentional, not absent.
    #[must_use]
    pub fn nebula_open_mesh() -> Self {
        Self::Rules {
            name: "nebula:overlay".into(),
            rules: vec![FirewallRule {
                port: None,
                proto: None,
                action: Verdict::Allow,
                cite: "open-mesh: valid cert ⇒ any-to-any (§8 flat-trust)".into(),
            }],
            default: Verdict::Allow,
        }
    }
}

impl PathGraph {
    /// Evaluate the firewall a single edge crosses, write its [`ControlPoint`],
    /// and re-derive [`Self::blocked_at`]. The first `Block` edge (source→dest
    /// order) wins; `Indeterminate` never blocks. No-op if the edge id is absent.
    pub fn evaluate_edge(&mut self, edge_id: &str, fw: &FirewallProfile, port: u16, proto: &str) {
        if let Some(e) = self.edges.iter_mut().find(|e| e.id() == edge_id) {
            e.control = Some(fw.evaluate(port, proto));
        }
        self.recompute_blocked_at();
    }

    /// True when any edge's verdict is [`Verdict::Indeterminate`] — the trace is
    /// reachable-so-far but a control point couldn't be resolved, so the caller
    /// should surface "indeterminate" rather than a confident "reachable".
    #[must_use]
    pub fn has_indeterminate(&self) -> bool {
        self.edges.iter().any(|e| {
            e.control
                .as_ref()
                .is_some_and(|c| c.verdict == Verdict::Indeterminate)
        })
    }
}

// ---------------------------------------------------------------------------
// ROUTE-TRACE-3: live, best-effort measurement.
//
// The model so far is the *modeled* path; this layer enriches it with real
// numbers where they're measurable, and falls back to the modeled segment with
// no number (and never a panic — §2) where they aren't:
//
//   * per-overlay-link RTT/loss from the existing path classifier / netstate
//     (the `mesh-latency` snapshot keyed by peer name) onto the Mesh edges;
//   * a real public hop list (RTT) from a `traceroute`/`mtr` run beyond the exit
//     onto the Public edge.
//
// The *gathering* (reading the latency cache, shelling out to traceroute) lives
// in `mackesd`; here we keep the typed shapes + the pure application logic so
// both the daemon and the GUI agree on them and it's unit-testable. An
// unmeasured segment is left exactly as the model built it (`rtt_ms`/`loss`
// stay `None`) — a missing measurement is never invented.
// ---------------------------------------------------------------------------

/// True when an RTT reading is a real measurement worth rendering: finite and
/// strictly positive. A `0.0` (the traceroute parser's fallback for an answered
/// hop whose `… ms` token it couldn't read), a negative, or a NaN/inf is **not**
/// a measurement — the segment degrades to modeled rather than showing "0 ms".
#[must_use]
fn is_usable_rtt(rtt_ms: f64) -> bool {
    rtt_ms.is_finite() && rtt_ms > 0.0
}

/// True when a loss reading is a real measurement: finite and in `0.0..=1.0`.
#[must_use]
fn is_usable_loss(loss: f64) -> bool {
    loss.is_finite() && (0.0..=1.0).contains(&loss)
}

impl Layer {
    /// True for the Nebula overlay layer — the one whose per-link RTT/loss the
    /// path classifier / netstate measures.
    #[must_use]
    pub const fn is_overlay(self) -> bool {
        matches!(self, Self::Mesh)
    }
}

impl PathEdge {
    /// True when this edge rides the Nebula overlay (a `Mesh`-layer link) — the
    /// segment whose RTT/loss the per-link path classifier can fill in.
    #[must_use]
    pub const fn is_overlay(&self) -> bool {
        self.layer.is_overlay()
    }
}

/// One overlay peer's live link measurement from the path classifier / netstate.
///
/// As the `mesh-latency` snapshot's per-peer reading reports it. `rtt_ms`/`loss`
/// are `None` when the probe didn't land — an unmeasurable link, never guessed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkMeasurement {
    /// Measured round-trip time (ms), `None` when the probe timed out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<f64>,
    /// Measured loss fraction 0.0..=1.0, `None` when unmeasured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loss: Option<f64>,
}

impl LinkMeasurement {
    /// A reading with just an RTT (the common path-classifier case — the
    /// transport probe times an RTT but doesn't sample loss).
    #[must_use]
    pub const fn from_rtt(rtt_ms: Option<f64>) -> Self {
        Self { rtt_ms, loss: None }
    }
}

impl PathGraph {
    /// Populate per-overlay-link **RTT/loss** onto the Mesh edges from the path
    /// classifier / netstate, keyed by the **peer-node label** at the overlay end
    /// of the edge (the peer name the `mesh-latency` snapshot uses). For each
    /// overlay edge the measurement is looked up by the label of the endpoint
    /// that is an actual overlay **peer node** ([`NodeKind::OverlayPeer`] /
    /// [`NodeKind::Host`]) — never the lighthouse/internet end, so a lighthouse
    /// label can't collide with a real peer name and apply the wrong reading.
    /// When both ends are peer nodes, either matching reading applies. An edge
    /// with no reading — or only a non-finite/zero RTT and out-of-range loss — is
    /// **left modeled** (`rtt_ms`/`loss` untouched): the degrade-to-modeled path,
    /// no panic.
    ///
    /// Returns the number of overlay edges that got at least one measured value.
    pub fn apply_overlay_latency(
        &mut self,
        by_peer: &std::collections::HashMap<String, LinkMeasurement>,
    ) -> usize {
        // id -> (label, is the endpoint an overlay peer node?), so we resolve an
        // edge's *peer* endpoint to its mesh-latency key and skip the lighthouse /
        // internet ends (whose labels could otherwise shadow a real peer name).
        let node_of: std::collections::HashMap<&str, (&str, bool)> = self
            .nodes
            .iter()
            .map(|n| {
                let is_peer = matches!(n.kind, NodeKind::OverlayPeer | NodeKind::Host);
                (n.id.as_str(), (n.label.as_str(), is_peer))
            })
            .collect();
        let mut applied = 0usize;
        for e in &mut self.edges {
            if !e.is_overlay() {
                continue;
            }
            // Look the reading up by the peer-node end's label only.
            let peer_label = |id: &str| -> Option<&str> {
                node_of
                    .get(id)
                    .and_then(|(label, is_peer)| is_peer.then_some(*label))
            };
            let m = peer_label(e.from.as_str())
                .and_then(|l| by_peer.get(l))
                .or_else(|| peer_label(e.to.as_str()).and_then(|l| by_peer.get(l)));
            if let Some(m) = m {
                // Apply only the *usable* fields; an all-`None` (probe timed out)
                // or junk reading touches nothing — degrade to modeled.
                let rtt = m.rtt_ms.filter(|v| is_usable_rtt(*v));
                let loss = m.loss.filter(|v| is_usable_loss(*v));
                if rtt.is_some() {
                    e.rtt_ms = rtt;
                }
                if loss.is_some() {
                    e.loss = loss;
                }
                if rtt.is_some() || loss.is_some() {
                    applied += 1;
                }
            }
            // No reading ⇒ leave the modeled edge as-is.
        }
        applied
    }
}

/// One hop of a live public-internet `traceroute`/`mtr` run (beyond the exit).
///
/// `ip` is `*` for an unanswered hop (ICMP-filtered / rate-limited); its
/// `rtt_ms` is then meaningless and the gap is shown, never filled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicHop {
    /// Hop number (TTL, 1-based).
    pub ttl: u8,
    /// Hop IP, or `*` when the hop didn't answer.
    pub ip: String,
    /// Round-trip ms for the hop; `0.0` (and `ip == "*"`) when unanswered.
    pub rtt_ms: f64,
}

impl PublicHop {
    /// True when this hop answered (has a real IP, not the `*` gap marker).
    #[must_use]
    pub fn answered(&self) -> bool {
        self.ip != "*"
    }
}

/// The typed result of the `action/route/traceroute` verb.
///
/// The best-effort public hop list from the relevant node to `target`.
/// `available` is `false` when the trace couldn't run at all (no
/// `traceroute`/`mtr` binary, or it failed) — the caller then degrades to the
/// modeled public hop, no error.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PublicTrace {
    /// The destination the hops lead to.
    pub target: String,
    /// The tool that produced the hops (`traceroute` / `mtr`), for provenance.
    pub tool: String,
    /// Whether a live trace ran (a tool was present and produced output).
    pub available: bool,
    /// The hops, TTL order. Empty when `!available`.
    pub hops: Vec<PublicHop>,
}

impl PublicTrace {
    /// An empty, unavailable result — the degrade-to-modeled case (no tool / no
    /// output). Carries the target + the tool that *would* have run, never errors.
    #[must_use]
    pub fn unavailable(target: &str, tool: &str) -> Self {
        Self {
            target: target.to_string(),
            tool: tool.to_string(),
            available: false,
            hops: Vec::new(),
        }
    }

    /// The RTT to attribute to the modeled public edge: the last *answered* hop
    /// with a **usable** RTT (the closest-to-destination measurement we got).
    /// `None` when no hop answered with a real RTT — the public segment then
    /// stays modeled (no number). A hop that answered but whose RTT the tool
    /// didn't report (parsed as a non-positive `0.0` fallback) is **not** treated
    /// as a measurement — a missing number is never rendered as "0 ms".
    #[must_use]
    pub fn edge_rtt_ms(&self) -> Option<f64> {
        self.hops
            .iter()
            .rev()
            .find(|h| h.answered() && is_usable_rtt(h.rtt_ms))
            .map(|h| h.rtt_ms)
    }
}

impl PathGraph {
    /// Splice a best-effort public `traceroute` result onto the **Public** edge
    /// (the segment beyond the exit). The edge's `rtt_ms` becomes the last
    /// answered hop's RTT; an unavailable trace (or one where no hop answered)
    /// leaves the modeled edge untouched. No-op when there's no Public edge.
    ///
    /// Returns `true` when a measured RTT was applied to a Public edge.
    pub fn apply_public_trace(&mut self, trace: &PublicTrace) -> bool {
        let Some(rtt) = trace.edge_rtt_ms() else {
            return false; // unmeasurable ⇒ degrade to the modeled hop.
        };
        if let Some(e) = self
            .edges
            .iter_mut()
            .find(|e| matches!(e.layer, Layer::Public))
        {
            e.rtt_ms = Some(rtt);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exposure::{IngressBinding, ServiceSource, Tier};

    fn node(id: &str, kind: NodeKind) -> PathNode {
        PathNode {
            id: id.into(),
            kind,
            label: id.into(),
            ..Default::default()
        }
    }

    fn edge(from: &str, to: &str, layer: Layer, transport: Transport) -> PathEdge {
        PathEdge {
            from: from.into(),
            to: to.into(),
            layer,
            transport,
            ..Default::default()
        }
    }

    #[test]
    fn egress_path_round_trips_through_json() {
        let g = PathGraph::new(Direction::Egress)
            .with_node(node("eagle", NodeKind::Host))
            .with_node(node("gw", NodeKind::Gateway))
            .with_node(node("exit", NodeKind::VpnExit))
            .with_node(node("net", NodeKind::Internet))
            .with_edge(edge("eagle", "gw", Layer::Mesh, Transport::DirectOverlay))
            .with_edge(edge("gw", "exit", Layer::Vpn, Transport::VpnTunnel))
            .with_edge(edge("exit", "net", Layer::Public, Transport::Public));
        let json = g.to_json().unwrap();
        let back: PathGraph = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
        assert_eq!(back.direction, Direction::Egress);
        assert_eq!(back.nodes.len(), 4);
        assert_eq!(back.edges.len(), 3);
        // kebab-case on the wire.
        assert!(json.contains("\"direct-overlay\""));
        assert!(json.contains("\"vpn-tunnel\""));
    }

    #[test]
    fn blocked_at_is_the_first_denying_edge() {
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(node("net", NodeKind::Internet))
            .with_node(node("lh", NodeKind::Ingress))
            .with_node(node("svc", NodeKind::Service))
            .with_edge(edge("net", "lh", Layer::Public, Transport::Public))
            .with_edge(edge("lh", "svc", Layer::Mesh, Transport::DirectOverlay));
        // No control points yet → reachable.
        assert_eq!(g.recompute_blocked_at(), None);
        assert!(g.is_reachable());
        // Deny the ingress edge.
        g.edges[0].control = Some(ControlPoint {
            firewall: "firewalld:public".into(),
            verdict: Verdict::Block,
            rule: "default deny (no exposure policy)".into(),
        });
        assert_eq!(g.recompute_blocked_at().as_deref(), Some("net->lh"));
        assert!(!g.is_reachable());
    }

    #[test]
    fn blocked_at_picks_the_earliest_of_multiple_blocks() {
        let mut g = PathGraph::new(Direction::Egress)
            .with_node(node("a", NodeKind::Host))
            .with_node(node("b", NodeKind::Gateway))
            .with_node(node("c", NodeKind::VpnExit))
            .with_edge(edge("a", "b", Layer::Mesh, Transport::DirectOverlay))
            .with_edge(edge("b", "c", Layer::Vpn, Transport::VpnTunnel));
        let block = |rule: &str| {
            Some(ControlPoint {
                firewall: "fw".into(),
                verdict: Verdict::Block,
                rule: rule.into(),
            })
        };
        g.edges[0].control = block("first");
        g.edges[1].control = block("second");
        assert_eq!(g.recompute_blocked_at().as_deref(), Some("a->b"));
    }

    #[test]
    fn validate_catches_a_dangling_edge() {
        let g = PathGraph::new(Direction::Egress)
            .with_node(node("a", NodeKind::Host))
            .with_edge(edge("a", "ghost", Layer::Mesh, Transport::DirectOverlay));
        let err = g.validate().unwrap_err();
        assert_eq!(err, ("a->ghost".to_string(), "ghost".to_string()));
        // A consistent graph validates.
        let ok = PathGraph::new(Direction::Egress)
            .with_node(node("a", NodeKind::Host))
            .with_node(node("b", NodeKind::Host))
            .with_edge(edge("a", "b", Layer::Mesh, Transport::DirectOverlay));
        assert!(ok.validate().is_ok());
    }

    fn web_policy(public: bool) -> ExposurePolicy {
        ExposurePolicy {
            id: "grafana".into(),
            source: ServiceSource {
                node: "eagle".into(),
                port: 3000,
                proto: "tcp".into(),
                ..Default::default()
            },
            tier: if public {
                Tier::PublicViaIngress
            } else {
                Tier::MeshOnly
            },
            ingress: public.then(|| IngressBinding {
                lighthouse: "Lighthouse-01".into(),
                hostname: "grafana.services.example".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn assemble_ingress_public_service_is_reachable() {
        let g = assemble_ingress(&web_policy(true), Some("10.42.0.2"), Some("45.55.33.179"));
        assert_eq!(g.direction, Direction::Ingress);
        // Internet → Ingress → host(overlay peer) → Service.
        assert_eq!(g.nodes.len(), 4);
        assert_eq!(g.nodes[1].kind, NodeKind::Ingress);
        assert_eq!(
            g.nodes[1].dns_name.as_deref(),
            Some("grafana.services.example")
        );
        assert_eq!(g.nodes[1].public_ip.as_deref(), Some("45.55.33.179"));
        assert_eq!(g.nodes[2].overlay_ip.as_deref(), Some("10.42.0.2"));
        assert_eq!(g.nodes[3].hosting_node.as_deref(), Some("eagle"));
        // The public boundary allows it → reachable.
        assert!(g.is_reachable());
        assert_eq!(g.edges[0].control.as_ref().unwrap().verdict, Verdict::Allow);
        assert!(g.validate().is_ok());
    }

    #[test]
    fn assemble_ingress_mesh_only_service_is_blocked_at_the_boundary() {
        let g = assemble_ingress(&web_policy(false), Some("10.42.0.2"), None);
        // mesh-only ⇒ the internet→ingress edge blocks.
        assert!(!g.is_reachable());
        assert_eq!(g.blocked_at.as_deref(), Some("internet->ingress"));
        let ctrl = g.edges[0].control.as_ref().unwrap();
        assert_eq!(ctrl.verdict, Verdict::Block);
        assert!(ctrl.rule.contains("mesh-only"));
    }

    #[test]
    fn assemble_egress_is_host_to_internet() {
        let g = assemble_egress("eagle", Some("10.42.0.2"), "1.1.1.1");
        assert_eq!(g.direction, Direction::Egress);
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].kind, NodeKind::Host);
        assert_eq!(g.nodes[1].label, "1.1.1.1");
        assert!(g.is_reachable());
        assert!(g.validate().is_ok());
    }

    #[test]
    fn edge_id_and_is_blocked() {
        let mut e = edge("x", "y", Layer::Public, Transport::Public);
        assert_eq!(e.id(), "x->y");
        assert!(!e.is_blocked());
        e.control = Some(ControlPoint {
            firewall: "f".into(),
            verdict: Verdict::Allow,
            rule: "ok".into(),
        });
        assert!(!e.is_blocked());
        e.control.as_mut().unwrap().verdict = Verdict::Block;
        assert!(e.is_blocked());
    }

    // --- ROUTE-TRACE-2: control-point evaluation -------------------------------

    #[test]
    fn public_baseline_allows_foundation_ports_denies_the_rest() {
        let fw = FirewallProfile::public_baseline(false);
        // Nebula, SSH, enroll are allowed, citing the real rule.
        let neb = fw.evaluate(4242, "udp");
        assert_eq!(neb.verdict, Verdict::Allow);
        assert!(neb.rule.contains("4242/udp"));
        assert_eq!(fw.evaluate(22, "tcp").verdict, Verdict::Allow);
        assert_eq!(fw.evaluate(4243, "tcp").verdict, Verdict::Allow);
        // An arbitrary service port is denied by the default.
        let web = fw.evaluate(8080, "tcp");
        assert_eq!(web.verdict, Verdict::Block);
        assert_eq!(web.rule, "default deny (no matching rule)");
        // 443/tcp is NOT open on a non-lighthouse...
        assert_eq!(fw.evaluate(443, "tcp").verdict, Verdict::Block);
        // ...but IS on a lighthouse (covert fallback).
        let lh = FirewallProfile::public_baseline(true);
        let covert = lh.evaluate(443, "tcp");
        assert_eq!(covert.verdict, Verdict::Allow);
        assert!(covert.rule.contains("443/tcp"));
    }

    #[test]
    fn proto_must_match_not_just_port() {
        let fw = FirewallProfile::public_baseline(false);
        // 4242 is open for UDP only — the same port over TCP falls through to deny.
        assert_eq!(fw.evaluate(4242, "udp").verdict, Verdict::Allow);
        assert_eq!(fw.evaluate(4242, "tcp").verdict, Verdict::Block);
    }

    #[test]
    fn first_matching_rule_wins() {
        let fw = FirewallProfile::Rules {
            name: "ordered".into(),
            rules: vec![
                FirewallRule {
                    port: Some(8080),
                    proto: Some("tcp".into()),
                    action: Verdict::Block,
                    cite: "drop 8080/tcp (explicit)".into(),
                },
                FirewallRule {
                    port: None,
                    proto: None,
                    action: Verdict::Allow,
                    cite: "allow any".into(),
                },
            ],
            default: Verdict::Block,
        };
        // The earlier explicit Block wins over the later allow-any.
        let v = fw.evaluate(8080, "tcp");
        assert_eq!(v.verdict, Verdict::Block);
        assert_eq!(v.rule, "drop 8080/tcp (explicit)");
        // A different port hits the allow-any rule.
        assert_eq!(fw.evaluate(9090, "tcp").verdict, Verdict::Allow);
    }

    #[test]
    fn indeterminate_profile_never_guesses() {
        let fw = FirewallProfile::Indeterminate {
            name: "firewalld:public (remote, unread)".into(),
        };
        let cp = fw.evaluate(8080, "tcp");
        assert_eq!(cp.verdict, Verdict::Indeterminate);
        assert!(cp.rule.contains("not guessed"));
    }

    #[test]
    fn evaluate_edge_blocks_at_the_destination_host_firewall() {
        // ingress: internet -> host -> service; the host's public firewall denies
        // an unexposed service port, so blocked_at lands on host->service.
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(node("internet", NodeKind::Internet))
            .with_node(node("host", NodeKind::OverlayPeer))
            .with_node(node("service", NodeKind::Service))
            .with_edge(edge("internet", "host", Layer::Public, Transport::Public))
            .with_edge(edge("host", "service", Layer::Host, Transport::Loopback));
        let fw = FirewallProfile::public_baseline(false);
        g.evaluate_edge("host->service", &fw, 8080, "tcp");
        assert_eq!(g.blocked_at.as_deref(), Some("host->service"));
        assert!(!g.is_reachable());
        let cp = g.edges[1].control.as_ref().unwrap();
        assert_eq!(cp.verdict, Verdict::Block);
    }

    #[test]
    fn evaluate_edge_indeterminate_does_not_set_blocked_at() {
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(node("host", NodeKind::OverlayPeer))
            .with_node(node("service", NodeKind::Service))
            .with_edge(edge("host", "service", Layer::Host, Transport::Loopback));
        let fw = FirewallProfile::Indeterminate {
            name: "firewalld:public (remote, unread)".into(),
        };
        g.evaluate_edge("host->service", &fw, 8080, "tcp");
        // Not guessed → not blocked, but surfaced as indeterminate.
        assert_eq!(g.blocked_at, None);
        assert!(g.is_reachable());
        assert!(g.has_indeterminate());
    }

    // --- ROUTE-TRACE-3: live, best-effort measurement --------------------------

    /// A node whose id and human label differ (the label is the peer name the
    /// path classifier keys its measurements by).
    fn labeled(id: &str, label: &str, kind: NodeKind) -> PathNode {
        PathNode {
            id: id.into(),
            kind,
            label: label.into(),
            ..Default::default()
        }
    }

    #[test]
    fn overlay_edge_helpers_only_match_the_mesh_layer() {
        assert!(Layer::Mesh.is_overlay());
        assert!(!Layer::Public.is_overlay());
        assert!(!Layer::Vpn.is_overlay());
        assert!(!Layer::Host.is_overlay());
        assert!(edge("a", "b", Layer::Mesh, Transport::DirectOverlay).is_overlay());
        assert!(!edge("a", "b", Layer::Public, Transport::Public).is_overlay());
    }

    #[test]
    fn apply_overlay_latency_populates_rtt_and_loss_on_the_mesh_edge() {
        // ingress: internet -> ingress -> host(peer "eagle") -> service.
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(node("internet", NodeKind::Internet))
            .with_node(labeled("ingress", "Lighthouse-01", NodeKind::Ingress))
            .with_node(labeled("host", "eagle", NodeKind::OverlayPeer))
            .with_node(labeled("service", "grafana", NodeKind::Service))
            .with_edge(edge(
                "internet",
                "ingress",
                Layer::Public,
                Transport::Public,
            ))
            .with_edge(edge(
                "ingress",
                "host",
                Layer::Mesh,
                Transport::DirectOverlay,
            ))
            .with_edge(edge("host", "service", Layer::Host, Transport::Loopback));
        let mut by_peer = std::collections::HashMap::new();
        by_peer.insert(
            "eagle".to_string(),
            LinkMeasurement {
                rtt_ms: Some(14.3),
                loss: Some(0.02),
            },
        );
        let applied = g.apply_overlay_latency(&by_peer);
        assert_eq!(applied, 1);
        // Only the overlay (Mesh) edge picked up the numbers.
        let mesh = &g.edges[1];
        assert_eq!(mesh.rtt_ms, Some(14.3));
        assert_eq!(mesh.loss, Some(0.02));
        // The Public + Host edges stay modeled (no number).
        assert_eq!(g.edges[0].rtt_ms, None);
        assert_eq!(g.edges[2].rtt_ms, None);
    }

    #[test]
    fn apply_overlay_latency_degrades_to_modeled_when_unmeasurable() {
        // No reading for "eagle" at all ⇒ the overlay edge stays exactly modeled,
        // no panic, no invented number (§2 graceful degrade).
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(labeled("ingress", "Lighthouse-01", NodeKind::Ingress))
            .with_node(labeled("host", "eagle", NodeKind::OverlayPeer))
            .with_edge(edge(
                "ingress",
                "host",
                Layer::Mesh,
                Transport::DirectOverlay,
            ));
        // (a) empty snapshot.
        let empty = std::collections::HashMap::new();
        assert_eq!(g.apply_overlay_latency(&empty), 0);
        assert_eq!(g.edges[0].rtt_ms, None);
        assert_eq!(g.edges[0].loss, None);
        // (b) a reading exists for the peer but it's an all-`None` (probe timed
        // out) measurement — still leaves the edge modeled, no panic.
        let mut by_peer = std::collections::HashMap::new();
        by_peer.insert("eagle".to_string(), LinkMeasurement::from_rtt(None));
        assert_eq!(g.apply_overlay_latency(&by_peer), 0);
        assert_eq!(g.edges[0].rtt_ms, None);
        assert_eq!(g.edges[0].loss, None);
    }

    #[test]
    fn apply_overlay_latency_matches_either_endpoint_label() {
        // The measurement is keyed on the `to` end here (the `from` end is the
        // lighthouse, which the classifier has no peer reading for).
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(labeled("ingress", "Lighthouse-01", NodeKind::Ingress))
            .with_node(labeled("host", "anvil", NodeKind::OverlayPeer))
            .with_edge(edge(
                "ingress",
                "host",
                Layer::Mesh,
                Transport::RelayOverlay,
            ));
        let mut by_peer = std::collections::HashMap::new();
        by_peer.insert("anvil".to_string(), LinkMeasurement::from_rtt(Some(31.7)));
        assert_eq!(g.apply_overlay_latency(&by_peer), 1);
        assert_eq!(g.edges[0].rtt_ms, Some(31.7));
        assert_eq!(g.edges[0].loss, None);
    }

    #[test]
    fn public_trace_edge_rtt_is_the_last_answered_hop() {
        let trace = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![
                PublicHop {
                    ttl: 1,
                    ip: "10.0.0.1".into(),
                    rtt_ms: 1.2,
                },
                PublicHop {
                    ttl: 2,
                    ip: "*".into(),
                    rtt_ms: 0.0,
                },
                PublicHop {
                    ttl: 3,
                    ip: "1.1.1.1".into(),
                    rtt_ms: 18.9,
                },
            ],
        };
        // The last *answered* hop (ttl 3), not the trailing gap.
        assert_eq!(trace.edge_rtt_ms(), Some(18.9));
    }

    #[test]
    fn apply_public_trace_splices_rtt_onto_the_public_edge() {
        let mut g = assemble_egress("eagle", Some("10.42.0.2"), "1.1.1.1");
        // egress is source -> internet over a Public edge.
        assert!(matches!(g.edges[0].layer, Layer::Public));
        let trace = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![PublicHop {
                ttl: 1,
                ip: "1.1.1.1".into(),
                rtt_ms: 9.4,
            }],
        };
        assert!(g.apply_public_trace(&trace));
        assert_eq!(g.edges[0].rtt_ms, Some(9.4));
    }

    #[test]
    fn apply_public_trace_unavailable_leaves_the_modeled_edge() {
        let mut g = assemble_egress("eagle", Some("10.42.0.2"), "1.1.1.1");
        // (a) an explicitly unavailable trace (no tool).
        let none = PublicTrace::unavailable("1.1.1.1", "traceroute");
        assert!(!none.available);
        assert!(!g.apply_public_trace(&none));
        assert_eq!(g.edges[0].rtt_ms, None);
        // (b) a trace that ran but every hop was an ICMP-filtered gap.
        let all_gaps = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![
                PublicHop {
                    ttl: 1,
                    ip: "*".into(),
                    rtt_ms: 0.0,
                },
                PublicHop {
                    ttl: 2,
                    ip: "*".into(),
                    rtt_ms: 0.0,
                },
            ],
        };
        assert_eq!(all_gaps.edge_rtt_ms(), None);
        assert!(!g.apply_public_trace(&all_gaps));
        assert_eq!(g.edges[0].rtt_ms, None);
    }

    #[test]
    fn public_trace_round_trips_through_json() {
        let trace = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![PublicHop {
                ttl: 1,
                ip: "10.0.0.1".into(),
                rtt_ms: 1.2,
            }],
        };
        let json = serde_json::to_string(&trace).unwrap();
        let back: PublicTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(back, trace);
    }

    #[test]
    fn edge_rtt_ms_rejects_an_answered_hop_with_a_bogus_zero_rtt() {
        // The traceroute parser falls back to rtt 0.0 for an answered hop whose
        // `… ms` token it couldn't read — that's "RTT unknown", not "0 ms". The
        // edge then degrades to modeled rather than rendering an impossible 0 ms.
        let trace = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![
                PublicHop {
                    ttl: 1,
                    ip: "10.0.0.1".into(),
                    rtt_ms: 2.1,
                },
                // last *answered* hop, but no usable RTT (0.0 fallback).
                PublicHop {
                    ttl: 2,
                    ip: "1.1.1.1".into(),
                    rtt_ms: 0.0,
                },
            ],
        };
        // Falls back to the earlier hop that DID have a real RTT, not the 0.0 one.
        assert_eq!(trace.edge_rtt_ms(), Some(2.1));
        // And when the ONLY answered hop has a 0.0 fallback ⇒ no measurement.
        let only_zero = PublicTrace {
            target: "1.1.1.1".into(),
            tool: "traceroute".into(),
            available: true,
            hops: vec![PublicHop {
                ttl: 1,
                ip: "1.1.1.1".into(),
                rtt_ms: 0.0,
            }],
        };
        assert_eq!(only_zero.edge_rtt_ms(), None);
    }

    #[test]
    fn apply_overlay_latency_keys_on_the_peer_node_not_the_lighthouse() {
        // The lighthouse/ingress label could collide with a real peer name; the
        // overlay reading must come from the PEER end ("eagle"), never the
        // lighthouse end — even if a (wrong) reading exists under the lighthouse
        // label, it must not be applied.
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(labeled("ingress", "anvil", NodeKind::Ingress)) // collides!
            .with_node(labeled("host", "eagle", NodeKind::OverlayPeer))
            .with_edge(edge(
                "ingress",
                "host",
                Layer::Mesh,
                Transport::DirectOverlay,
            ));
        let mut by_peer = std::collections::HashMap::new();
        // A bogus reading under the lighthouse-collision label + the real one.
        by_peer.insert("anvil".to_string(), LinkMeasurement::from_rtt(Some(999.0)));
        by_peer.insert("eagle".to_string(), LinkMeasurement::from_rtt(Some(12.0)));
        assert_eq!(g.apply_overlay_latency(&by_peer), 1);
        // The PEER end ("eagle") wins, never the colliding lighthouse label.
        assert_eq!(g.edges[0].rtt_ms, Some(12.0));
    }

    #[test]
    fn apply_overlay_latency_ignores_zero_and_out_of_range_readings() {
        let mut g = PathGraph::new(Direction::Ingress)
            .with_node(labeled("ingress", "Lighthouse-01", NodeKind::Ingress))
            .with_node(labeled("host", "eagle", NodeKind::OverlayPeer))
            .with_edge(edge(
                "ingress",
                "host",
                Layer::Mesh,
                Transport::DirectOverlay,
            ));
        let mut by_peer = std::collections::HashMap::new();
        // rtt 0.0 (not a real measurement) + loss 1.5 (out of 0..=1 range).
        by_peer.insert(
            "eagle".to_string(),
            LinkMeasurement {
                rtt_ms: Some(0.0),
                loss: Some(1.5),
            },
        );
        assert_eq!(g.apply_overlay_latency(&by_peer), 0);
        assert_eq!(g.edges[0].rtt_ms, None);
        assert_eq!(g.edges[0].loss, None);
    }

    #[test]
    fn usable_rtt_and_loss_predicates() {
        assert!(is_usable_rtt(0.1));
        assert!(!is_usable_rtt(0.0));
        assert!(!is_usable_rtt(-1.0));
        assert!(!is_usable_rtt(f64::NAN));
        assert!(!is_usable_rtt(f64::INFINITY));
        assert!(is_usable_loss(0.0));
        assert!(is_usable_loss(1.0));
        assert!(!is_usable_loss(-0.1));
        assert!(!is_usable_loss(1.01));
        assert!(!is_usable_loss(f64::NAN));
    }
}
