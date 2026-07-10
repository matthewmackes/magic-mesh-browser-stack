# BROWSER-DD-9 re-scope — media + mesh conferencing (WebRTC)

**Status:** DESIGN — options + recommendation, 2026-07-10. Written because a
reconciliation pass found BROWSER-DD-9 sitting in `docs/WORKLIST.md` as a single
unexpanded acceptance line with **zero code evidence anywhere in the repo**: no
worker, no doc-comment, no test, no struct — nothing under any spelling of
`webrtc`/`rtcpeerconnection`/`getusermedia`/`conferenc` in `crates/` traces back to
this item. That is unusual among its BROWSER-DD-* siblings (DD-1 CEF engine,
DD-2 tabs/omnibox, DD-3 ad-blocking, DD-5/6/7/8/11/12 all have farm-evidence
sub-bullets this session) and matches this session's other "big unscoped area"
precedent, `docs/design/e12-9-10-libvirt-rescope.md`, which this doc mirrors in
format. **Not implemented here — design/options only.**
**Companions:** `docs/design/browser-daily-driver.md` (the 100-Q survey that
locked "full WebRTC" + "a full built-in MESH CONFERENCING app" as part of the
Q33–36/46/60/69 media-and-real-time theme, and already flagged the TURN/signaling
risk in its own Risks section — this doc makes that risk concrete), `docs/design/
mesh-chat-icq.md` (the closest shipped precedent for a real-time, peer-addressed
mesh feature riding the Bus), the shipped VOIP-GW epic (the mesh's existing,
unrelated-protocol calling stack — see §"Two deliverables hiding under one line"
below).

**Update 2026-07-10 (later same day):** the open verification question this
doc flags below — whether `--disable-webrtc` is functionally real — has been
resolved, independent of DD-9 itself. It is confirmed **not real** (verified
against the live Chromium source, not just the community switches reference
this doc cites — see `docs/THREAT_MODEL.md` §8 and the doc comment on
`chromium_privacy_switches()` in `cef_init.rs` for the full evidence trail)
and has been fixed: the dead switch is removed, `--force-webrtc-ip-handling-
policy` is confirmed-real and kept, and a renderer-level JS shim
(`cef_browser::webrtc_block_script`) now actually removes the WebRTC surface
pending DD-9. This does **not** change DD-9's own scope or recommendation
below (CEF WebRTC is still off by design, not yet a shipped feature) — it
only means the engine-choice table's "may not even be real" framing and the
Rollup's "unverified... worth checking" line are now historical, not current.

## What DD-9 actually asks for

The full, unexpanded WORKLIST line (`docs/WORKLIST.md`, BROWSER-DD-9):

> media + mesh conferencing. **Acceptance:** full codecs (incl. H.264/H.265) +
> full WebRTC + PiP + background audio + media keys + GPU/HW video decode;
> block-all autoplay; a built-in MULTI-PARTY mesh conferencing app over WebRTC
> on the Nebula overlay (screen-share, peer-directory participants, phone ties).

The richer source is `browser-daily-driver.md`'s Q33–36/46/60/69 theme block,
which adds: WebGL + WebGPU, HW video decode named explicitly as VA-API, and this
line already in the epic's own Risks section — "Mesh conferencing — WebRTC needs
signaling + (often) a TURN relay; doing it purely over Nebula (overlay = the
relay) needs care for multi-party." That sentence turns out to be the single
most load-bearing intuition in the whole item, and §"Mesh-native signaling"
below confirms it's directionally right — with more nuance once checked against
the code that actually exists.

This is nine distinct capabilities bundled into one WORKLIST line (codecs, WebRTC,
PiP, background audio, media keys, HW decode, autoplay blocking, and a bespoke
multi-party conferencing app). This doc does not try to size all nine — it
focuses on the two hardest, most architecturally consequential ones (real
WebRTC, and what "on the Nebula overlay" should actually mean), because
everything else is either a smaller, more conventional browser feature (media
keys, autoplay blocking, background audio — normal Chromium/Servo preference
flags) or depends entirely on the WebRTC answer (PiP, screen-share, the
conferencing app).

## Current architecture (what this would build on)

Per `browser-daily-driver.md`'s locked dual-engine decision, the browser has two
engines behind one shm/texture bridge into `mde-shell-egui`'s `Surface::Browser`:

- **`mde-web-cef`** — a prebuilt-CEF (Chromium) offscreen-render helper.
- **`mde-web-preview`** / **`mde-web-preview-client`** — a sandboxed Servo
  offscreen-render helper, the older of the two engines (BOOKMARKS-5/6).

Both speak the same shm-frame/IPC seam (`wire.rs`) so the shell is engine-agnostic
per tab; the engine choice is a per-tab user selection (DD-1, shipped). Neither
engine has any WebRTC-related code today beyond the two hardening switches found
below — this is a from-scratch feature on either path, not a partial one.

## Engine choice: verified against the actual pinned sources

Following the same discipline the DD-2 Servo cancel-load investigation used
(read the real pinned source, don't assume from general Chromium/Servo
reputation) — both engines turn out to have **more going on than a flat "yes" or
"no,"** and the two findings are asymmetric in an important way: one engine's
gap is a one-line policy reversal, the other's is a dependency-graph change plus
a still-incomplete upstream feature.

### CEF / Chromium — the switch is the whole story, and it may not even be real

`mde-web-cef/src/cef_init.rs`'s `chromium_privacy_switches()` (used by every CEF
launch) includes, among a broader privacy/telemetry-hardening bundle:

```rust
// crates/desktop/mde-web-cef/src/cef_init.rs (existing, unmodified)
"--disable-background-networking",
"--disable-breakpad",
"--disable-client-side-phishing-detection",
"--disable-component-update",
"--disable-default-apps",
"--disable-device-discovery-notifications",
"--disable-domain-reliability",
"--disable-metrics",
"--disable-metrics-reporting",
"--disable-notifications",
"--disable-speech-api",
"--disable-sync",
"--disable-webrtc",
"--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
"--disable-features=AutofillServerCommunication,DevicePosture,InterestCohort,
   MediaRouter,PaymentRequest,PrivacySandboxAdsAPIs,Translate,WebBluetooth,
   WebGPU,WebUSB",
```

A unit test in the same file pins `--disable-webrtc` and the IP-handling-policy
switch as present in the launch argv. Two things follow from this, and they cut
in different directions:

1. **The intent is unambiguous and easy to reverse.** This is one string in one
   `Vec` literal plus its test assertion — not a capability that needs building.
   Also confirmed real and worth *keeping*: `--force-webrtc-ip-handling-policy=
   disable_non_proxied_udp` is a documented, genuine Chromium enterprise-policy
   switch that restricts ICE gathering to proxied/relayed candidates (no direct
   local-IP-revealing UDP) — the *correct* mechanism for WebRTC privacy, better
   than an outright kill switch, and worth carrying forward once WebRTC is
   re-enabled (§"Mesh-native signaling" below argues the mesh case wants
   something even more specific than this stock policy value).
2. **Whether `--disable-webrtc` is functionally real is unverified, and the
   evidence leans toward "maybe not."** Checked a comprehensive, community-
   maintained scrape of real Chromium command-line switches (peter.sh's
   Chromium switches list) for anything named exactly `disable-webrtc` (not
   `-hw-decoding`/`-hw-encoding`/`-encryption`, which *are* real, narrower
   switches) — not found. Also found a Google Chrome Enterprise support-forum
   thread from an administrator specifically reporting `--disable-webrtc`
   appearing to be ignored. Chromium is well-documented to silently no-op any
   `--` switch it doesn't recognize — it does not error. **This means it is a
   real open question, independent of DD-9, whether this "hardening" line is
   doing anything at all today**, which cuts two ways: either WebRTC needs a
   real removal of a working block (the switch is honored), or CEF's WebRTC may
   already be reachable today despite this codebase's stated intent (the switch
   is decorative) — itself a privacy-posture gap worth a runtime check
   (`chrome://webrtc-internals` or a live `getUserMedia` probe against a built
   CEF helper) regardless of whether DD-9 proceeds. This doc doesn't resolve
   which is true — it flags it as the first verification step of any real
   slice, the same "verify, don't assume" instinct DD-2 applied to Servo.

The pinned CEF itself (`packaging/browser/cef-linux64-minimal.env`) is
`149.0.6+g0d0eeb6+chromium-149.0.7827.201`, selected 2026-06-27 as "the newest
stable Linux64 branch" — a fully current, production Chromium. Its WebRTC stack
is the same mature, standards-complete implementation every modern Chrome/Edge/
Brave ships (full codec negotiation incl. H.264 hardware paths, VP8/VP9/AV1,
Opus, real ICE/DTLS-SRTP, `getDisplayMedia` for screen-share). Nothing about the
*engine* is a gap here — only this codebase's own launch-time policy is.

CEF also runs with `windowless_no_sandbox()` (`cef_init.rs`) — Chromium's own
internal multi-process sandbox is explicitly **off**, and there is no CEF
equivalent of Servo's custom namespace/seccomp module (`mde-web-preview/src/
sandbox.rs` — see below). This is a pre-existing, orthogonal security posture,
not something DD-9 creates, but it means camera/mic access for CEF is presently
an OS-device-permission question (whatever launches the helper process today),
not a "punch one new hole in an already-tight sandbox" question the way it is
for Servo.

### Servo — real DOM code, off by default, missing its own media backend here

Checked the actual pinned `servo` 0.3.0 release (published 2026-07-01 per
docs.rs — nine days before this doc) by fetching the real source, the same way
DD-2 did for the cancel-load finding:

- `components/script/dom/webrtc/` (Servo's `main` branch, which the recently-cut
  0.3.0 release tracks) has real, substantial implementations: `rtcpeerconnection
  .rs` (30,534 bytes), `rtcdatachannel.rs` (14,833 bytes), plus
  `rtcicecandidate.rs`, `rtcsessiondescription.rs`, `rtctrackevent.rs`,
  `rtcrtptransceiver.rs`, `rtcrtpsender.rs`, `rtcerror(event).rs` — this is not a
  stub surface.
- `components/script/dom/media/mediadevices.rs` has a real `GetUserMedia`
  implementation — it calls `ServoMedia::get().create_audioinput_stream(...)` /
  `create_videoinput_stream(...)` and resolves a real `MediaStream` with
  whatever tracks the backend actually produced.
- **But it's off by default.** `servo-config` 0.3.0's `prefs.rs`:
  ```rust
  // feature: WebRTC | #41396 | Web/API/WebRTC_API
  pub dom_webrtc_enabled: bool,          // default: false
  // feature: WebRTC Transceiver | #41396 | Web/API/RTCRtpTransceiver
  pub dom_webrtc_transceiver_enabled: bool,  // default: false
  ```
  Upstream tracking issue **servo/servo#41396, "Enable WebRTC by default,"
  is still OPEN** (created 2025-12-19, zero comments as of this research), body:
  "Known missing pieces: #26860 …to be continued." Issue **#26860**
  ("Implement `RTCPeerConnection.addTransceiver` and `RTCRtpTransceiver`") has
  been open since **2020** — six years — and is cited there as needed by
  real-world SDKs (`mediasoup-client`). `addTransceiver`/Unified Plan
  negotiation is close to load-bearing for how essentially every modern WebRTC
  stack negotiates media, so this isn't a cosmetic gap.
- **The production media backend isn't in this workspace's dependency
  resolution at all.** `RTCPeerConnection`'s real work (ICE, DTLS, SRTP, codec
  I/O) is delegated to the `servo-media` crate family via `ServoMedia`. Two
  backends exist upstream: `servo-media-dummy` (no-op) and
  `servo-media-gstreamer` (real — depends on `gstreamer-webrtc 0.25`,
  `gstreamer-sdp`, `gstreamer-app`, `gstreamer-audio`/`-video`, genuine
  ICE/DTLS/SRTP-capable GStreamer `webrtcbin` machinery). Checked
  `crates/desktop/mde-web-preview/Cargo.lock`: it resolves **`servo-media-dummy`
  0.3.0**, not `servo-media-gstreamer`. So even flipping `dom_webrtc_enabled` to
  `true` today would expose the JS API surface wired to a backend that does
  nothing — calls would not carry real audio/video. Getting real Servo WebRTC
  needs (a) re-pointing the Cargo feature/dependency selection to
  `servo-media-gstreamer`, a real dependency-graph change, plus (b) a new native
  system dependency on every node running the Servo engine: a GStreamer runtime
  with its webrtc/sdp/app/audio/video plugins present — a smaller-in-degree but
  same-*kind* of vendoring cost this codebase already treats seriously for CEF
  (see [[rpm-cut-needs-servo-build]] for how much weight "a second heavy
  excluded engine's asset story" already carries here).
- **This codebase already deliberately disables the flag anyway.**
  `mde-web-preview/src/engine.rs`:
  ```rust
  //! ... the security defaults (a generic UA, no persistent storage APIs, no HTTP
  //! disk cache, no WebRTC/WebGPU, denied permission prompts) are applied ...
  pub fn secure_preferences() -> Preferences {
      Preferences {
          dom_webrtc_enabled: false, // no local-IP leak
          ...
  ```
  with a dedicated regression test asserting `!prefs.dom_webrtc_enabled` with
  the message "WebRTC local-IP leaks are disabled." Enabling Servo WebRTC means
  reversing a decision this codebase already made on purpose, not just flipping
  an upstream default.

**This is a real, meaningfully different finding from the DD-2 cancel-load
investigation, and the difference matters.** DD-2 found Servo has *zero*
cancel-load surface at *every* reachable layer — a flat wall. Here, Servo has
substantial, real, spec-shaped DOM code — the wall is upstream completeness
(a 6-year-old core-negotiation gap, an off-by-default flag with an open,
stalled tracking issue) plus this workspace's own dependency and policy
choices, not an absent API. Worth stating precisely rather than reaching for
the DD-2 framing by pattern-matching alone.

Servo's sandbox (`mde-web-preview/src/sandbox.rs` — thorough: user/mount/IPC/
UTS/cgroup/PID namespaces, a read-only tmpfs rootfs, seccomp-bpf) currently
bind-mounts only `/dev/{null,zero,full,random,urandom}` plus `/dev/dri` (GPU).
No `/dev/video*` or audio device path is in `readonly_binds()`/`dev_binds()`
today — camera/mic access needs an explicit, reviewed addition there, same as
it would for CEF's (currently nonexistent) equivalent.

### Recommendation: CEF, not Servo, for WebRTC

| | CEF | Servo |
|---|---|---|
| DOM/API completeness | Full production Chromium WebRTC | Real but incomplete; 6-yr-old core-negotiation gap upstream |
| What's blocking it here | One switch string + its test (`--disable-webrtc`) | An off-by-default pref **and** the wrong media backend resolved **and** this codebase's own deliberate disable |
| New native dependency | None (already vendored) | GStreamer + webrtc/sdp/app/audio/video plugins, new to every Servo-running node |
| Codec/HW-decode story | Already the same engine DD-9 wants for H.264/H.265/VA-API generally | Would need its own, separate verification |
| Camera/mic sandbox work | New (CEF has no dedicated sandbox module at all today) | New (existing sandbox's allowlist needs video/audio device paths added) |

CEF is the dramatically smaller lift — a policy reversal plus a runtime
verification step, versus a dependency-graph change plus riding an upstream
feature its own maintainers call incomplete. This also fits the daily-driver
epic's existing engine split cleanly: CEF is already "the compat path," Servo
"the light/private option" (browser-daily-driver.md's pivotal decision) — WebRTC
is a compat-tier feature (real sites like Meet/Discord-web need it to work at
all), so it belongs on the compat engine. **Recommendation: land WebRTC CEF-only,
leave Servo explicitly gated pending upstream (`dom_webrtc_enabled`/
`addTransceiver`) and a backend decision, exactly the shape DD-2's Stop-button
work already used for a different Servo gap** (CEF got the real feature; Servo
kept the honest not-yet-implemented path, distinguished from genuine upstream
absence in its own match arm).

## Mesh-native signaling: does Nebula simplify or complicate the ICE/STUN/TURN story?

`browser-daily-driver.md`'s own Risks section already flagged this as the crux
risk: "WebRTC needs signaling + (often) a TURN relay; doing it purely over
Nebula (overlay = the relay) needs care for multi-party." Checked what the mesh
already has built for exactly this class of problem, and the honest answer is:
**it simplifies the connectivity half substantially, but does not remove the
protocol's own mandatory pieces, and multi-party is still hard regardless.**

### What the mesh already has

- **A real STUN client**, RFC 5389/5780/8489, already shipped:
  `crates/mesh/mackesd/src/stun.rs` (`encode_binding_request`/
  `parse_binding_response`/`gather_endpoint`, fully unit-tested including
  XOR-MAPPED-ADDRESS round-trips for IPv4 and IPv6-decode). It exists to seed
  **Nebula's own** endpoint advertisement (Phase 12.17, "so symmetric-NAT edges
  hole-punch before falling through to the lighthouse relay") — not for WebRTC —
  but it's the same wire protocol WebRTC's own ICE gathering would use, already
  proven against this fleet's NAT conditions.
- **A real multi-transport router that already does ICE's job for the mesh's
  own traffic**: `crates/mesh/mackes-transport/src/peer_path.rs`'s `PeerPath`/
  `TransportKind` tracks, per peer, a primary transport (`NebulaDirect`), a
  fallback, health-scored automatic switching, and — critically — an
  `HttpsFallbackState` machine that activates an HTTPS-tunnel transport
  (`NebulaHttps443`) when direct and relay UDP both fail three cycles running.
  That is a direct-then-relay-then-last-resort-tunnel cascade — architecturally
  the same shape as ICE-then-TURN, already built, already tested, already
  running in production for the mesh's own KDC/Nebula traffic.
- **Every paired node already has a stable, routable Nebula overlay IP**,
  assigned once at mesh-join via Nebula certs. Nebula's own lighthouse-mediated
  NAT traversal (direct hole-punch, falling back to lighthouse relay) already
  solves "can peer A reach peer B" *underneath* that IP — a WebRTC stack running
  on top never has to know or care whether the packets are going direct or
  relayed through a lighthouse.

### What this means for WebRTC's own ICE story

Because that Nebula IP is already reachable to every other paired node (with
NAT traversal and relay fallback already solved one layer down), **a
mesh-scoped call does not need to run the standard public-internet WebRTC
playbook at all.** The straightforward design: scope ICE candidate gathering to
the local Nebula tun interface only — offer a single host candidate at the
Nebula IP, no STUN server round-trip, no TURN relay candidate. No externally
operated STUN/TURN infrastructure (e.g. a self-hosted coturn, which is what most
WebRTC deployments have to stand up and pay for) needs to exist anywhere on this
mesh. This is a genuine simplification, not just an assumption — it follows
directly from Nebula already being a full mesh-wide L3 overlay with its own
NAT-traversal-and-relay story solved beneath the IP layer.

It also has a nice second-order effect: it **sidesteps the exact privacy concern
that led to today's hardening** (`disable_non_proxied_udp`, `dom_webrtc_enabled:
false // no local-IP leak`). The usual WebRTC local-IP-leak risk is that ICE
host candidates expose a machine's real LAN/public IP to a remote web page. If
ICE is scoped to *only* the Nebula interface, the one address ever offered is
the mesh-private Nebula IP — information every other paired mesh node already
has by virtue of mesh membership itself, not a new leak. This should be an
explicit design constraint of the first slice (see below), not an incidental
detail: it changes the privacy calculus enough that it's worth stating as a
reason the mesh case is different from the generic web-facing case the current
hardening switches were written against.

**Signaling transport** (exchanging the SDP offer/answer and — if any real ICE
negotiation is kept — candidates) has a direct, precedented home: the mesh Bus's
existing pub/sub topic pattern, exactly as `mesh-chat-icq.md`'s shipped
architecture already uses it (a worker on every node draining `action/chat/*`,
publishing `event/chat/message`, presence gossip, peer-directory-addressed, no
external server). A new `action/webrtc/signal`-shaped topic (or folded into the
existing chat/notify transport, since a call invite is conceptually similar to
`Message::CallAction`, already a variant in `mde-chat`'s message-kind enum per
that doc) is the natural fit — no separate signaling server (normally a
required piece of WebRTC infrastructure, e.g. a websocket service) needs
building.

### What does NOT get simplified away

- **DTLS-SRTP is mandatory by spec regardless of transport trust.** WebRTC
  requires a DTLS handshake and SRTP media encryption even when the underlying
  transport (here, the Nebula tunnel) is already encrypted — this can't be
  skipped just because Nebula already secures the link. The real DTLS/SRTP
  work still has to happen inside whichever engine's stack is used.
- **Multi-party is not solved by any of the above.** Nebula's transport-layer
  relay-on-demand answers "can two peers reach each other," not "how do N
  peers' media get mixed or forwarded." Grepped the mesh's existing real-time
  media code (`mde-voice-hud`'s `sip.rs`/`media.rs`, `mde-voice-config`) for any
  conference/mixer/SFU/B2BUA capability — none exists anywhere in this
  codebase today. A full mesh (every peer connects to every peer) scales
  quadratically in bandwidth/CPU and is the traditional pain point past
  roughly 4–6 participants; a real SFU (selective forwarding) or a mixing
  bridge is a substantial, separate piece of work either way. DD-9's acceptance
  text says MULTI-PARTY explicitly — this doc does not have a sized answer for
  that beyond "defer it past the first slice," and flags it as the single
  biggest open scope item in the whole feature.
- **Interop with non-mesh WebRTC peers is an unresolved, unstated product
  question.** If the built-in conferencing app should ever let a mesh member
  join a call with someone outside the mesh (or the browser's generic WebRTC
  support needs to work with real external sites like Meet/Discord-web — which
  the daily-driver epic's Q2 compatibility-target lock clearly wants for the
  *browser* case), then real ICE/STUN/TURN and standard signaling can't be
  skipped for that path — the Nebula-only shortcut above only applies to the
  mesh-internal conferencing app, not to generic web-page WebRTC. **This is a
  fork this doc surfaces rather than resolves**: "full WebRTC" (browser compat,
  needs the real thing, CEF already has it once unblocked) and "mesh
  conferencing app" (mesh-internal, can use the Nebula shortcut) are different
  enough in their connectivity requirements that treating them as one
  deliverable risks under-scoping the browser-compat half or over-building the
  mesh-app half. See next section.

## Two deliverables hiding under one WORKLIST line

Netting out what already exists elsewhere in this codebase changes DD-9's
*incremental* scope more than the raw acceptance text suggests:

1. **"Full WebRTC" as a browser-compat feature** (real websites — Meet, Discord
   web, Jitsi, etc. — should just work) is purely a CEF-engine capability
   question, answered above: reverse the disable switch, verify it was doing
   anything, ship it. This has nothing to do with the mesh.
2. **"A built-in MULTI-PARTY mesh conferencing app... phone ties"** overlaps
   substantially with an **already-shipped, mature, different-protocol** calling
   system: the **VOIP-GW epic (7/7 closed)** — `mde-voice-hud` (a real pure-Rust
   SIP stack: REGISTER/INVITE/ACK/BYE, RTP/G.711 media, TLS-preferred with an
   honest UDP-downgrade signal) plus `mde-voice-egui`, already doing P2P
   intra-mesh calls over Nebula **and** real PSTN phone bridging via a
   commercial SIP trunk (Vitelity) with a per-node inbound DID. That is,
   functionally, already-shipped "phone ties" and 1:1 peer-directory calling —
   just over SIP/RTP, not WebRTC. It has no conference/mixer/multi-party
   capability today (verified: no hits for `conference`/`mixer`/`b2bua`/`SFU`
   in `mde-voice-config`, `sip.rs`, or `media.rs`), so multi-party is still a
   real gap — but the more natural home for it may well be **extending this
   existing, mature SIP/RTP stack with a conference-bridge module** (kamailio
   has real conferencing/mixer modules; rtpengine already does N-way RTP relay)
   rather than building a parallel WebRTC SFU from nothing. That's a genuine,
   non-obvious fork worth an explicit call before committing to "conferencing
   app == new WebRTC app": it could instead become "conferencing app == a
   multi-party UI over the calling system that already exists," with WebRTC
   staying scoped to the browser-compat half only.

This doc does not resolve which path the "conferencing app" should take — it's
a real product decision (reuse the mature SIP/RTP stack vs. build a new WebRTC
one, or some hybrid) — but flags it because building a from-scratch WebRTC SFU
without first weighing the already-shipped SIP conferencing option would be
easy to do by just following DD-9's literal "over WebRTC" phrasing, and would
be substantially more work than necessary if the SIP path turns out to satisfy
the same acceptance bullet.

## What's already real and reusable (beyond the SIP/RTP stack above)

- **`mde-seat/src/mixer.rs`'s `StripOrigin::MeshRemote(String)`, keyed off the
  PipeWire property `mde.mesh.peer`, already exists, is fully classified and
  tested — and has zero producer anywhere in the codebase** (grep-verified,
  same shape as the `mde.vm.name` gap `e12-9-10-libvirt-rescope.md` found for VM
  audio in this same mixer file). A WebRTC (or SIP) call's received remote
  audio, once decoded and played out through a PipeWire stream tagged
  `mde.mesh.peer=<remote-peer-id>`, slots directly into the existing DAW-style
  mixer UI with **zero mixer-side changes** — the consumer is done, only the
  producer is missing, exactly the E12-16/E12-9-audio pattern.
- **`mde-media-core`** (the mpv-backed player, freshly landed real decode via
  BUG-VIDEO-1) is **not** directly reusable for WebRTC's live bidirectional
  encode/decode path — it's explicitly a playback/decode engine for files and
  streams ("wraps mpv, never reimplements decode"; mpv has no live
  camera-capture-to-RTP-encode pipeline). Its architectural *pattern* is worth
  copying though: an injectable engine trait, a `Fake` backend for headless
  testing, a feature-gated real backend so the default build stays airgap-safe
  (`mde-media-core`'s `mpv` feature is exactly this shape) — a good template for
  whatever WebRTC media-engine seam gets designed.
- **`mde-media-core/src/capture.rs` (MEDIA-13)** already enumerates and opens
  v4l2 webcam devices (`v4l2-ctl --list-devices` behind an injectable
  `CaptureEnumerator`, `av://v4l2:/dev/videoN` play URL) — reusable for camera
  *enumeration* and a local self-preview view, not for the actual
  encode/RTP-packetize pipeline a call needs.
- **No screen-share infrastructure exists at all** — grepped for
  `wlr-screencopy`/`xdg-desktop-portal`/PipeWire-screencast, zero hits. This
  matters more here than on a typical desktop: this shell is **DRM-native with
  no Wayland compositor** (`mde-shell-egui` owns DRM directly — no compositor
  exposes a standard screen-capture portal to hook into). The usual mechanism
  most WebRTC screen-share implementations rely on (a compositor-mediated
  `getDisplayMedia` backend) doesn't architecturally exist here; a DRM
  framebuffer-level capture path would need to be built from scratch. Real,
  new, shell-specific work — not a WebRTC-library gap.
- **PiP has zero existing infrastructure.** `mde-shell-egui/src/dock.rs`'s
  `Surface` enum (`Workbench`, `MeshView`, `Desktop`, ..., `Browser`, ...,
  `Chat`, ...) is a single-active-surface model — one panel visible at a time,
  switched via the dock. Grepped `web.rs`/`dock.rs`/`main.rs` for any
  floating/always-on-top/overlay-window concept — none exists. Real PiP (a
  video that stays visible while the user switches to, say, Chat) needs a new
  compositing capability this shell doesn't have today: something rendered on
  top of whichever `Surface` is active, not gated by the dock's single-panel
  switch. A materially smaller substitute — a floating video confined to
  *within* the Browser surface's own UI only, disappearing when you leave
  Browser entirely — is much closer to what already exists architecturally and
  is a reasonable first-cut scope reduction, explicitly short of "real" OS-level
  PiP.

## Recommended first slice

Matching the task framing this doc was scoped against: **one-to-one audio+video
call over the mesh between two paired nodes**, nothing past it.

1. **CEF only.** No Servo WebRTC work in this slice (leave it honestly gated,
   `dom_webrtc_enabled: false`, pointing at this doc + servo/servo#41396).
2. **Verify, then narrowly reverse, the disable switch.** First confirm at
   runtime whether `--disable-webrtc` is actually load-bearing today (§ above);
   remove it either way once confirmed, but keep
   `--force-webrtc-ip-handling-policy` — tightened, if practical, to a mesh-aware
   policy that only ever offers the Nebula interface rather than the stock
   "any non-proxied UDP" exclusion.
3. **A minimal, security-reviewed camera/mic device grant** for the CEF helper
   process (today undocumented/unmanaged, unlike Servo's explicit allowlist) —
   audit what device access CEF's helper actually has before adding to it.
4. **Signaling over the existing Bus**, peer-directory-addressed, new
   `action/webrtc/*`-shaped topic (or folded into `mde-chat`'s existing
   `CallAction` message kind) — no external signaling server.
5. **ICE scoped to the Nebula tun interface only** — one host candidate, no
   STUN round-trip, no TURN relay candidate, no externally operated STUN/TURN
   infrastructure. Real DTLS-SRTP still happens (spec-mandatory, not skippable).
6. **A minimal call UI** — a "Call" action against one named peer chosen from
   the existing peer-directory (reusing `peer_probe.rs`'s existing
   connectivity/roster concepts where it makes sense for a call-quality
   readout), `getUserMedia` for local mic+camera, one `RTCPeerConnection` to
   exactly that peer. Explicitly **1:1 only**.
7. **Received remote audio tagged `mde.mesh.peer=<id>`** on its PipeWire output
   so it appears for free in the already-shipped DAW mixer — zero mixer changes.

**Explicitly deferred past this slice** (each is independently large, per the
findings above): multi-party/SFU-or-conference-bridge, screen-share (needs new
DRM-capture code), real PiP (needs new shell compositing), background audio +
media keys, block-all autoplay, GPU/HW-decode tuning, and the "conferencing
app == extend VOIP-GW's SIP stack vs. new WebRTC" product decision.

## Rollup: what changes about how big/risky this really is

- **The engine question has a clear, cheap answer.** CEF's gap is a policy
  string (of uncertain real effect) to reverse; Servo's is a dependency-graph
  change plus riding a feature its own maintainers still call incomplete
  (a 6-year-open core-negotiation issue). This isn't close — CEF is the path.
- **The mesh materially simplifies WebRTC's usual hardest infrastructure
  problem.** No STUN/TURN server needs to be stood up anywhere; Nebula's
  already-shipped, already-tested direct-then-relay-then-tunnel cascade
  (`PeerPath`/`TransportKind`, the STUN client that already exists to seed it)
  does that job one layer down, and the mesh Bus already has the exact
  peer-addressed pub/sub shape a signaling channel needs. This is the one place
  this doc found the item is *smaller* than the raw acceptance text implies.
- **Multi-party is the one place it's clearly *not* smaller.** No conference/
  mixer/SFU capability exists anywhere in this codebase today, on either the
  WebRTC or the SIP/RTP side. DD-9's acceptance text asks for MULTI-PARTY
  explicitly and in capitals; this doc has no sized first-slice answer for it
  beyond "defer it," which is the single biggest gap between what DD-9 asks for
  and what a reasonable first slice delivers.
- **DD-9 is really two deliverables wearing one WORKLIST line** — browser-page
  WebRTC compat (CEF-engine, mesh-independent) and a mesh conferencing app
  (which may not need WebRTC at all, given the already-shipped SIP/RTP calling
  stack). Conflating them risks either under-scoping browser compat or
  over-building a parallel calling system next to one that already works.
- **Screen-share and PiP both need genuinely new, shell-specific capabilities**
  (DRM-level capture with no compositor portal to lean on; a compositing model
  that can show something over an inactive `Surface`) that have nothing to do
  with WebRTC itself and would still be required even if WebRTC were free.
- **A real local-IP-leak privacy question is open independent of DD-9** — it's
  unverified whether `--disable-webrtc` currently does anything, which is worth
  checking regardless of whether this feature ever ships.

## Out of scope (this doc)

- Implementing any of the above in workspace crates (design/options only).
- Sizing or designing the multi-party mechanism (SFU vs. SIP conference bridge
  vs. something else) — flagged as the biggest open item, not solved here.
- Resolving "should the mesh conferencing app be SIP/RTP (extend VOIP-GW) or
  WebRTC" — a product call, not an engineering one.
- Screen-share's DRM-capture design, PiP's shell-compositing design, media-keys/
  background-audio/autoplay-blocking (conventional, smaller, engine-preference-flag
  work not investigated in depth here).
- Verifying whether `--disable-webrtc` is functionally real in the pinned CEF
  149 build — flagged as the first concrete step of any real slice, not
  performed here (would need a live CEF build + a runtime probe, not source
  reading).
