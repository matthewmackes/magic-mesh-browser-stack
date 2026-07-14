# BROWSER-DD — the daily-driver browser (100-Q survey, operator-locked 2026-07-04)

Turn the platform's browser (`mde-web-preview` / the BOOKMARKS-5/6 helper) from an
experimental Servo preview into a **daily-driver browser matching current browsers** —
while staying mesh-native, privacy-forward, and Quasar-styled. Locked via a 100-question
`/plan` survey. This is a large multi-phase epic; the survey is the durable spec.

## The pivotal decision: a DUAL-engine browser

- **Add a CEF (Chromium Embedded Framework) engine** for daily-driver compatibility
  (Widevine DRM, WebExtensions, full web platform, DevTools, WebGL/WebGPU, WebRTC), AND
- **Keep the (now-fixed, sandboxed) Servo engine** as the light/private option.
- The two are **user-selectable** (Q93/Q1). CEF is the compat path; Servo the lean one.
- Both reuse the existing **out-of-process helper architecture** (BOOKMARKS-5/6): render
  offscreen → shared-memory frame → the egui shell uploads to a texture; input forwarded
  back. CEF runs in offscreen mode inside the sandboxed helper (Q6/Q7).
- CEF ships as **prebuilt binaries** vendored on the farm (NOT built from source); Widevine
  is **fetched at first run** (Firefox-style, not redistributed) (Q5/Q7/Q8).
- **Engine/browser updates** ride an **independent fast updater** (security-driven, faster
  than the platform release cadence) (Q89).

## Locked decisions (100), by theme

### Engine & architecture (Q1–8, 93)
Dual engine (Servo + CEF, selectable) · CEF embedding · reuse BOOKMARKS-5/6 offscreen→egui
helper · prebuilt CEF binaries · Widevine fetched first-run · Servo kept as light/private,
CEF added for compat · transition = ship both, cut nothing.

### Compatibility target (Q2)
Everything: Google suite+mail · streaming+media (incl. DRM) · banking/shopping/social ·
dev/self-hosted (GitHub, Horizon, Grafana, routers).

### Tabs, windows, session (Q9–12, 32, 56, 71, 85)
Standard tabs (open/close/switch/reorder) · single window + tabs · **vertical tabs option**
(matches the vertical dock) · scroll + favicons + tab-list · **session/tabs synced across
the mesh** · **follow-me tabs** (your live tab set on any node) · **split/tile tabs** ·
global mute · bookmarks in a menu (no bar).

### Privacy & security (Q24–28, 57, 58, 82, 87)
HTTPS-only with prompt · block 3rd-party cookies + clear-on-close · **no browsing history** ·
explicit private mode · full per-site permission prompts + manager (default-deny) ·
**anti-fingerprinting hardening** · per-tab process isolation · **mesh-hosted** safe-browsing
blocklists (no Google phone-home) · full per-site data manager + "forget this site" ·
**zero telemetry** (crashes local/mesh only) · generic UA + per-site override.

### Built-in features (extensions replaced by natives, then extended) (Q3, 21, 22, 23, 53, 77)
Full **uBlock-class ad/tracker blocking** (mesh-synced subscribable lists + custom rules,
Q83) · reader mode (+ TTS) · **no** built-in password manager (external/OS store) → instead
**WebAuthn/passkeys + hardware keys + phone-as-authenticator** (Q77) · no form autofill ·
force-dark on light sites (Quasar-tuned) (Q54).

### Extensions (v1 decision, Q97–99 revised 2026-07-14)
V1 skips WebExtensions in production. Native ad/tracker blocking, reader mode, userscripts,
site styles, and WebAuthn/passkeys are the supported first-cut path; the CEF extension
registry remains a lab/future probe behind `MDE_CEF_WEBEXTENSIONS_LAB` until a Chrome-runtime
CEF build can prove extension execution and permission semantics. No general extension store ships.

### Search & address bar (Q17–20)
Unified URL+search omnibox · live suggestions · default **mesh-hosted SearXNG** + a
user-configurable engine list incl. privacy engines + keyword shortcuts · new-tab =
**Quasar-dark dashboard** (SearXNG box + mesh-service quick links, Q91).

### Media & real-time (Q33–36, 46, 60, 69)
Full codecs incl. **H.264/H.265** · **full WebRTC** · fullscreen + **picture-in-picture** +
background audio + media keys · block-all autoplay · WebGL + **WebGPU** · GPU accel + HW
video decode (VA-API) · **a full built-in MESH CONFERENCING app** (multi-party WebRTC over
the Nebula overlay — the mesh is signaling+relay, no external server — screen-share,
peer-directory participants, phone ties).

### Power tools (governed by a single Power-mode toggle, Q49) (Q37, 39, 41, 42, 66, 79)
**Power mode**: off = clean consumer browser; on = a Power menu with all dev/power tools
first-class. Full Chromium **DevTools** · view-source · file:// · **native media downloader**
(HLS/DASH video, all-images, media sniffer, ignore-blocking, **auto-rename + metadata
fixup**) · **full site scraper** (extract/crawl/export CSV·JSON·MD, scriptable selectors) ·
device APIs (WebUSB/Serial/HID/MIDI/BT, prompted) · **userscripts** (per-site JS/CSS,
Tampermonkey-style) **+ a bundled library of ~100 curated scripts for major sites**
(YouTube, NPR, Spotify, news…).

### Files, downloads, print, capture (Q14–16, 40, 39, 67)
Full download manager, fixed folder · built-in **PDF viewer** · full **print + save-as-PDF**
(CUPS) · full capture suite (full-page screenshot, region, PDF/MHTML, annotate).

### Accessibility & input (Q23, 44–45, 50–51, 72, 86)
Reader mode · screen-reader (accesskit) + forced-dark + full keyboard nav + **a best-of-breed
open-source TTS engine** (Piper/Kokoro-class) · global zoom · full customizable
Chrome-compatible shortcuts · **mouse gestures + rocker nav** · **voice commands + dictation**
(on-device STT) · offline-hunspell spell-check.

### Web platform (Q47, 48, 65, 66, 80)
Full storage + service workers **cleared on close** · **manual/spoofable geolocation** ·
clipboard prompt-per-site · handle mailto/tel/magnet protocols · no PWA-install (tabs only,
Q38) · no RSS (Q78) · no reading list / command palette (Q73/Q74).

### Mesh integration (Q29–32, 61, 62, 75, 94)
Sync over the existing **Nebula + Syncthing** substrate (no external account): tabs/session,
bookmarks, settings/speed-dial, downloads list · **send-tab to nodes AND paired phones** ·
cast tab · phone-as-remote · open-on/from-phone · offline **mesh peer cache** (pull from a
peer when a site's unreachable, Q81) · **private offline/mesh translation** · share to
peer/Chat/phone/**email** · download/capture events → the mesh notify feed · web
notifications → the mesh notify feed (Q43).

### Profiles, presentation, platform (Q68, 84, 90, 95, 96, 92, 63, 64, 76, 33, 55)
**Container tabs** (per-tab isolated identity) · single browser surface (no per-site app
pin) · F11 + present-to-node · full multi-display · English-first (multi-lang later) ·
minimal onboarding · import bookmarks · startup restores the synced follow-me session ·
rich bookmarks (folders + tags + search + dead-link check, mesh-synced) · fixed Quasar
toolbar.

### Phase 1 MVP (Q100 — the operator marked ALL of these for the first cut)
1. **CEF engine + tabs + omnibox + real-site compat** (the foundation).
2. **Ad-block + the privacy posture + Widevine** (daily-driver essentials).
3. **Native login path + passkeys; WebExtensions deferred for v1** (native blockers/reader/userscripts/site styles plus WebAuthn/passkeys work without LastPass extension runtime).
4. **Mesh sync + follow-me tabs + send-tab** (the mesh differentiators).

## Architecture notes

- The browser stays a **surface in the one DRM egui shell** (`mde-shell-egui`), driven via
  the BOOKMARKS-6 client. The engine choice is per-session; CEF and Servo both implement the
  same shm-frame/IPC seam so the shell is engine-agnostic.
- **CEF integration** (new crate, e.g. `mde-web-cef`, workspace-excluded like `mde-web-preview`
  for the same native-lib reasons): wraps prebuilt CEF in offscreen render mode, exposes the
  BOOKMARKS-6 seam. The `mde-web-preview-client` `live-helper` spawn learns to launch either
  helper.
- **Mesh sync** rides the existing substrate — a new `browser-sync` topic on Syncthing/etcd,
  E2E over Nebula; the session/bookmarks/settings state is CRDT-merged (reuse the platform's
  sync patterns).
- **Ad-block / filter lists / safe-browsing / extension registry / userscript library / TTS
  models** are **mesh-hosted assets** replicated to nodes (no external fetch) — one on-brand
  mechanism.
- **Widevine** fetched first-run into a per-user store; the browser is fully functional
  (non-DRM) without it.

## Risks
- **CEF on the airgapped Fedora farm** — vendoring ~200MB prebuilt CEF + Rust bindings
  (cef-rs) reproducibly; the release/RPM story (cf. [[rpm-cut-needs-servo-build]] — a second
  excluded heavy engine).
- **CEF WebExtensions support is not a v1 dependency** — the pinned CEF CAPI lacks the needed Chrome-runtime extension host, so production skips WebExtensions and keeps the registry only as a lab/future probe behind `MDE_CEF_WEBEXTENSIONS_LAB`.
- **Widevine + our sandbox** — the CDM has its own process/sandbox expectations; reconcile
  with the out-of-process helper confinement.
- **Mesh conferencing** — WebRTC needs signaling + (often) a TURN relay; doing it purely over
  Nebula (overlay = the relay) needs care for multi-party.
- **Scope** — this is effectively "build a browser." Phase 1 (the four MVP pillars) is itself
  large; everything else phases behind it.

## Out of scope (this epic)
- **Email** — a separate `/plan` (the operator raised Thunderbird; lean toward a native Rust
  mail client reusing CEF for HTML bodies + the mesh-sync/notify/WebAuthn foundations here).
- A from-source Chromium build (prebuilt CEF only).
- General unrestricted extension store (curated allowlist only).

## Tasks → `docs/WORKLIST.md` BROWSER-DD-1..N (Phase 1 first).
