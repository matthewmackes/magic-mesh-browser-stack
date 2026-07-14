# Browser — industry-grade gap backlog

Source: 6-lane multi-agent gap analysis vs Chrome/Edge/Firefox (2026-07-13, wf_ad423a0c-204;
73 raw gaps). The workflow's auto-synthesis returned a stub — this is the hand-synthesized
prioritized backlog recovered from the per-lane journal. Chrome = `crates/desktop/mde-shell-egui/src/web/`,
engine = `crates/desktop/mde-web-cef/`.

## Reconciled status — 2026-07-13 (7-lane code-evidence audit, wf_e7147257-7ed)

A second, deeper audit mapped every item below to its actual code with `file:line` evidence:
**29 shipped · 23 partial · 13 missing · 1 non-goal** (66 features). Whole browser stack is
farm-green (`mde-shell-egui` 1158→1159/0, `mde-web-cef`, `mde-adblock`+`mde-bookmarks`,
`mde-web-preview-client`+`mde-web-wire`). CEF display+load handler callbacks are VERIFIED firing on
live hardware end-to-end (nav/title/favicon/loading + paint) via the `cef-verify` wire-harness — see
memory `cef-handler-lookup-peer-null-bug`.

- **Achievable next (non-gated), highest-value first. DELIVERED 2026-07-13 (each farm-green + a
  unit test, `mde-shell-egui` 1158→1161/0, 0 style-leaks):**
  - ✅ *star-reflects-bookmarked-state* — filled ★ (`Style::ACCENT`) when the page is bookmarked in
    ANY folder; recursive `all_bookmarks` walk + trailing-slash-normalized membership.
  - ✅ *omnibox bookmark autocomplete* — `matching_bookmarks` ranks title-prefix > url-prefix >
    substring; rendered as a ★ "Bookmarks" row above History in the dropdown.
  - ✅ *IDN/punycode homograph warning* — wired the previously-orphaned `mde_adblock::confusable_reason`
    detector into the site-info panel (⚠ WARN line for punycode / mixed-script / look-alike hosts).
  - ✅ *kb-nav of omnibox suggestions* — Up/Down highlight (wrapping, seeded from none) across
    bookmarks→history→search, accent fill on the selected row, Enter commits the highlight; pure
    `next_selection` + `ordered_commit_values` unit-tested.
  - ✅ *private-mode UX* — front-door privacy explainer on the new-tab dashboard (`PRIVATE_MODE_EXPLAINER`).
  - ✅ *unified Clear All Browsing Data* — Privacy-menu action composing history + downloads +
    reopen-stack + active-session clears (previously scattered across 3 drawers); menu-driven test.
  - ✅ *tab hover thumbnail* — a scaled page-frame preview on tab hover (`thumbnail_size` aspect-fit,
    unit-tested); falls back to text for a not-yet-rendered tab.
  - ✅ *omnibox focus ring* — the branded 2px `mde_egui::focus` accent ring on the primary keyboard
    target (a11y), matching the dock/Console/Start idiom.

- **Follow-on delivered 2026-07-14:** completed `browser_download` ledger rows now expose
  Open + Show actions in the Browser downloads drawer. The shell resolves daemon-written output
  paths for single-file media, HLS/DASH packages, and materialized browser outputs, mirrors the
  worker filename sanitizer, and launches through an injectable `xdg-open` seam covered by unit
  tests. No new `mackesd` verb was needed.

- **Follow-on delivered 2026-07-14:** the focused omnibox now paints the visible grey
  inline-completion tail for a genuine top-hit completion, while preserving the existing
  TextEdit buffer/cursor mapping and Enter-to-accept preselect behavior.

- **Follow-on delivered 2026-07-14:** JavaScript dialogs now complete their engine-to-shell
  loop: CEF handles `alert`/`confirm`/`prompt` without blocking the page, the client keeps a
  drainable bounded notice queue, and Browser chrome surfaces the origin + auto-resolution
  decision as a dismissible passive notice.

- **Follow-on delivered 2026-07-14:** page `beforeunload` handlers now use a real
  leave/stay round-trip instead of falling through the null CEF slot. The CEF bridge registers
  `on_before_unload_dialog`, retains the `cef_jsdialog_callback_t` by a bridge-minted id, the
  preview client queues bounded prompts with safe stay-on-overflow behavior, and Browser chrome
  answers the active prompt with Leave/Reload or Stay.

- **Follow-on delivered 2026-07-14:** per-tab autoplay blocking is now a first-class Browser tool.
  The shell exposes Block/Allow Autoplay in the View menu and both tab context menus, syncs the bit
  through session JSON, and sends `ControlMsg::SetAutoplayBlocked` to both CEF and Servo helpers.
  Helpers install a bounded page shim that strips autoplay media and rejects page-initiated
  `HTMLMediaElement.play()` until trusted user activation, then cleanly restore the original media
  API when disabled. CEF also remembers the tab policy and reapplies the blocker from its existing
  navigation shim injector for fresh documents.

- **Follow-on delivered 2026-07-14:** the credential wire protocol is now fail-closed for
  mixed-version helpers. Host-bound `FillLogin` and origin-bound `LoginSubmitted` moved to fresh
  tags, legacy payloads decode with no trusted host/origin, and the wire decoder rejects trailing
  bytes so added fields cannot be silently reinterpreted by older payload shapes.

- **Follow-on delivered 2026-07-14:** Browser now has an operator-managed URL policy gate.
  `browser/managed-url-policy.txt` under the workgroup root accepts host suffix and URL-prefix
  rules, the shell blocks omnibox/bookmark/send-tab/session-restore/new-tab opens before helper
  load, and the shared request filter blocks helper-originated top-level document navigations
  and subresources before network. Blocks paint a full-page managed-policy interstitial naming
  the matched rule, publish `event/browser/policy-block` audit records carrying trigger, engine,
  URL, host, rule, node, and timestamp metadata, and the Privacy menu reports the active managed
  rule count. URL-prefix policy matching canonicalizes default HTTP(S) ports, strips authority
  userinfo, and keeps authority-only prefixes host-bound, so
  `https://portal.example:443/admin/` and `https://alice@portal.example/admin/` cannot bypass
  `https://portal.example/admin/`, while `https://docs.example` does not overmatch
  `https://docs.example.evil/`. The same managed policy is rechecked before Browser-created
  download manifests enter the daemon Transfers lane, including CEF-intercepted downloads and
  Power Mode observed-media downloads, with `event/browser/policy-block` records using
  `trigger: "download"`.

- **Follow-on delivered 2026-07-14:** CEF one-shot callback retention is bounded for favicon and
  Save PDF flows. Completed `download_image` and `print_to_pdf` callbacks are removed from the
  bridge registry on completion, finished callback boxes are purged on the next lifecycle touch,
  and the `cef_binary_value_t` returned by `get_as_png` is released after its bytes are copied.
  Resource-verdict, permission-prompt, and beforeunload callbacks are also capped; if the shell
  stops answering, the engine denies/cancels new requests fail-closed instead of growing the held
  CEF callback set without bound.

- **Follow-on delivered 2026-07-14:** current-tab site-data clears now have an operator-consumable
  audit event. `Clear Current Tab Data` records the pre-clear URL, title, host, engine, scope, node,
  and timestamp to `event/browser/site-data-clear` while preserving the private-by-default,
  session-memory-only cookie/site-data model. `Clear All Browsing Data` also publishes
  `event/browser/browsing-data-clear` with all-session scope plus before-clear counts for history,
  downloads, reopen entries, saved logins, and permission grants. Runtime page permission decisions
  now publish `event/browser/permission-decision` for explicit allow/deny prompts and session-grant
  reuse auto-allows, including origin, permission kind, grant scope, active-tab context, node, and
  timestamp metadata. `Forget Site Permissions` now revokes matching session runtime grants instead
  of only clearing prompt history, and publishes `event/browser/permission-revoke` with active-site
  context plus revoked-grant/prompt counts. Dangerous download gates now publish
  `event/browser/download-danger` for the warning prompt and the user's eventual Keep/Discard
  decision, with source URL, host, filename, helper download id, node, and timestamp metadata.
  Dangerous download classification now evaluates the suggested filename and the source URL path leaf
  after percent-decoding, trailing Windows dot/space normalization, and ADS-style suffix handling, so
  a benign suggested name cannot hide an executable URL leaf. Safe-browsing host blocks are also
  rechecked before Browser-created download manifests enter the daemon Transfers lane, preserve the
  same mesh/overlay exemption as page requests, and publish `event/browser/safe-browsing-block`
  records for hard download blocks. Public plain-HTTP downloads are also blocked before transfer
  manifest creation, while mesh/overlay HTTP endpoints remain trusted, and the shell publishes
  `event/browser/insecure-download-block` records for those transport hard-blocks. Secure pages now
  also hard-block public plain-HTTP subresources in the shared helper request filter as
  `mixed-content:http`, preserving mesh/overlay HTTP exemptions and leaving top-level HTTP
  navigations with the existing shell navigation prompt; those subresource transport blocks publish
  `event/browser/mixed-content-block` audit records and carry their `blocked_by` reason through
  offline-cache/media resource manifests. Top-level public plain-HTTP navigation decisions now publish
  `event/browser/insecure-navigation` records for prompt, Continue, Upgrade, Cancel, and session-HSTS
  auto-upgrade decisions; new-tab URL opens also pause at the same HTTPS prompt instead of bypassing
  it before helper spawn. The site-info/security popup now also surfaces live managed-policy,
  safe-browsing, mixed-content, and tracker/filter block counts plus representative rules/hosts
  from the active page's observed resource log, and shows current-site permission posture
  (default-deny, session grants, denied prompts, and forgotten-site state). Per-site
  privacy/adfilter toggles now publish
  `event/browser/site-blocking` with the active URL/title/host, engine, enable/disable decision,
  node, and timestamp so local user overrides are
  auditable separately from synced filter policy. Session-only credential actions now publish
  redacted `event/browser/credential` records for save, update, delete, fill, and prompt-dismiss
  decisions with host, active URL/title, engine, trigger, per-host count, node, and timestamp
  metadata, but never username or password material. Auto-captured "Save password?" prompts are
  also scoped to the tab that submitted the login, so a background-tab capture cannot be accepted
  from an unrelated active page; closing the source tab drops its pending prompt. The shell now
  revalidates the capture origin against the source tab's committed host before staging the prompt,
  even though the CEF bridge already performs the same host check.

- **AUDIT CORRECTION:** *middle-click-to-close-tab* was marked "missing" by the audit but is in fact
  SHIPPED (`mod.rs` `tab_response.middle_clicked() => close`). The audit (subagents over a 13K-line
  god-module) carries false-negatives, so the raw "gap" count is an OVER-count — re-verify a
  "missing" item against code before treating it as unbuilt.

- **Genuinely remaining, by category (NOT "a few more clean gaps" — categorically different work):**
  - *Engine / CEF-handler (ABI-offset risk, needs live-CEF verify):* audible indicator, HTML5
    fullscreen (`on_fullscreen_mode_change`), IME preedit, UA HTTP-header override.
  - *Threat-model-GATED (do NOT do autonomously — see [[browser-privacy-locks]]):* per-site permission
    ALLOW path, safe-browsing host population, HSTS, B5 password manager, B4 extensions-runtime proof.
  - *Large (multi-session each):* tab groups, configurable search engines, PiP, real userscript
    engine, in-shell PDF UI, print-preview options.
  - *Data-plumbing / OS-coupled:* ✅ adblock breakdown DELIVERED 2026-07-13 (filter accumulates a
    `BlockTally` → `session.block_tally()` → shield hover shows top blocked domains; filter test +
    client 48/0 + shell 1165/0) — the plumbing regime is deliverable, not just display. Browser
    download Open/Show and omnibox inline grey top-hit were delivered 2026-07-14.

### Wave B STARTED — first engine round-trip live-verified 2026-07-13

**#18 HTML5 page-fullscreen — DELIVERED + LIVE-VERIFIED on .15.** Full 4-crate round-trip:
`EventMsg::Fullscreen` (mde-web-wire tag 17 + roundtrip test) ← `on_fullscreen_mode_change`
(mde-web-cef display block @ offset 64 — EXTENDS the already-verified block, ~zero NULL-handler
risk) → `WebSession::fullscreen()` (client field + poll test) → shell render gate hides chrome
(reuses the #17 F11 immersive path). Deployed shell+engine+renderer to .15, rebooted; `cef-verify`
confirms `CEF_INITIALIZE_OK` + display block resolves/fires + `paints=1` (no regression). Only the
`requestFullscreen()` gesture-trigger is eyes-on. **TEMPLATE for the rest of Wave B: extend a
verified vtable block (display/load/request) rather than mint a new handler struct.**

### FINAL STATE — 2026-07-13 (drive 3 close): pure-shell + engine round-trips DONE; 4 remain, each with a named blocker

**Completed this session (all farm-green + pushed):** safe-browsing interstitial, UA HTTP-header override,
audible-tab round-trip, HTML5 fullscreen, F11 fullscreen, HSTS, safe-browsing mesh source, pinned tabs,
Duplicate/Close-others/Close-right, tab-search, per-site permission ALLOW path (engine+shell), **IME preedit
(full 4-layer round-trip)**, configurable search engines, print options, tab groups, site styles, adblock
breakdown, Clear-All, plus 18 adversarial stress-tests. **Plus a real bug fix**: `print`/`print_to_pdf` host
offsets were stale (504/512 → 192/200) so PrintPage/SavePdf hit the wrong live-CEF methods — fixed `801331e9`.

**DEFINITIVE re-analysis (2026-07-13, header-evidenced): 3 of the 4 are architecturally PRECLUDED by
the pinned CEF-Alloy + OSR design — non-goals in the WebRTC category, not unfinished work — and only
1 (password) has a real path.**
- **B4 WebExtensions runtime — CONFIRMED NON-GOAL.** CEF 149's CAPI has NO WebExtensions load API
  (`cef_request_context_capi.h` has no `load_extension`/`get_extension`; only `cef_register_extension`
  for V8 native bindings + `cef_get_extensions_for_mime_type` remain). Extensions CANNOT execute on
  this build — it needs a CEF Chrome-runtime build. Gate made honest: `CEF_EXTENSIONS_NO_RUNTIME` (`c0c0b772`).
- **In-shell PDF viewer — architecturally precluded on this runtime.** Chromium's PDF viewer is itself
  an internal extension; with no extension runtime (above), CEF Alloy can't render PDFs inline. Needs a
  standalone PDFium/Rust-PDF renderer, or a CEF Chrome-runtime switch.
- **PiP — architecturally precluded by OSR.** `requestPictureInPicture` needs a native video window;
  the egui-texture OSR pipeline has none. Would need a second OSR surface + a floating shell window.
- **Password manager — ✅ COMPLETE (session-only, industry-standard).** Store + user-initiated autofill
  (🔑 menu) + **auto-capture-on-submit** + manage + Clear-All integration. The capture half was NOT built
  via the render-process bridge I first assumed — instead it REUSES the proven passkey beacon channel:
  the page-side `login_capture_script` beacons `{origin=<location.origin>, body={username,password}}` to
  `https://mde-login.invalid/capture/`, which the resource-request handler intercepts + CANCELS before the
  network. The engine verifies that origin's host against CEF's cached top-level URL before the shell offers
  to save, so page-supplied JSON cannot spoof another saved-login host (the credential never leaves the
  sandbox — identical safety to passkey ceremony data). Persistence + auto-fill-on-load stay deliberate
  non-goals (private-by-default). Only on-glass e2e verification (real form submit) is F44-deploy-gated, like
  the other engine round-trips.

**Operator decisions that would change this:** switch to the CEF **Chrome runtime** (unblocks B4 +
native PDF in one move); authorize password persistence or auto-fill-on-load beyond the current
session-only/private-by-default model; decide PiP (build the OSR video-surface or accept non-goal).
None are "finish the feature" — they are platform-architecture calls.

**(Earlier framing, superseded by the above; kept for history.) The 4 genuinely-remaining features are NOT clean gaps — each is blocked on infrastructure, architecture, or an
operator-gated resource (not effort I'm choosing to skip):**
1. **Password manager (auto-capture)** — BLOCKED on missing infrastructure. There is NO JS↔native bridge in the
   crate (no `cefQuery`/message-router/`on_process_message` wiring). Capturing a submitted login needs a
   render-process handler + `send_process_message` → `on_process_message_received` (client slot 184 exists but is
   unwired) routing, then a credential store. Autofill-FILL alone (JS-injection via a new `FillCredential`
   ControlMsg) is buildable now, but a fill-only, manual-save manager is not industry-grade, and credential
   handling is security-critical — this is a deliberate multi-session build, not a rush.
2. **Picture-in-Picture** — architecturally constrained by the OSR/egui-texture model (the engine renders
   offscreen; there is no native video window for `requestPictureInPicture`). Like WebRTC-removal, this needs an
   operator decision on whether to add a second OSR surface + floating shell window, or declare it a non-goal.
3. **B4 WebExtensions runtime smoke** — the registry validates + allowlists, but the runtime is gated
   (`CEF_EXTENSIONS_UNPROVEN … reason=live_extension_runtime_smoke_pending`). Proving it is a live-verify on the
   `.15` seat, which needs an F44 rebuild+deploy — the F44 builder (`.131`) is an operator-gated RAM juggle.
4. **In-shell PDF viewer** — no shell viewer exists (only save-to-PDF, now fixed). Chromium/CEF ships a native
   PDFium viewer, so navigating to a `.pdf` most likely already renders inline via the engine — but that is
   unverified on the current build and needs an eyes-on `.15` check (same F44-deploy gate) to confirm before
   deciding whether any shell work is even needed.

**Unblock asks for the operator:** (a) authorize the multi-session password JS↔native bridge build (or scope it
to fill-only for now); (b) decide PiP: build the OSR video-surface path or declare non-goal; (c) bring up the
F44 builder so I can deploy current master to `.15` and live-verify B4 + PDF + the audible/UA/permission/IME
handlers on glass (closing every on-glass caveat at once).

### Wave-B engine round-trips COMPLETE + tab-management sweep — 2026-07-13 (drive 3)

Nine more features, all farm-green + pushed (`mde-shell-egui` 1174→1186/0, `mde-web-cef`
116→117/0, `mde-web-preview-client` 50→52/0, wire 7/0, 0 style-leaks). The engine round-trips
used a **collision-free file-partition**: the coordinator freezes the wire seam (EventMsg/ControlMsg
tag + client accessor) and pushes it, then a farm agent owns `cef_browser/mod.rs` (engine) while the
coordinator owns `web/mod.rs` (shell) — disjoint files, clean parallel merge. Each agent derived its
CEF ABI offsets from in-repo anchors (never the SDK header, absent airgapped) with a fake-vtable unit
test and dedicated cached ptrs (no `lookup_peer`).

- **safe-browsing interstitial (#19)** — a top-level Document block whose filter label starts
  `safe-browsing` paints a full-page "unsafe site blocked" interstitial (mirrors the cert spine).
- **UA HTTP-header override (engine)** — `SetUserAgent` now also stamps the real `User-Agent:`
  request header (was JS-`navigator.userAgent`-only); request-handler `on_before_resource_load`,
  offset 144 cross-checked. Server-side sniffers now see the spoof too.
- **audible tab indicator (full round-trip)** — `cef_audio_handler` (client getter offset 40,
  9-anchor cross-check) publishes `EventMsg::AudioState` → shell renders 🔊 / 🔇 (mute wins),
  click-to-mute. The 🔇 mute half is live now; 🔊 lights on real playback (on-glass gesture).
- **pinned tabs** — compact favicon-only pills clustered front via a stable partition; drag snaps
  back across the boundary; both strips; no ×, close via middle-click/menu.
- **Duplicate tab / Close other tabs / Close tabs to the right** — both strips, pinned-sparing.
- **tab-search dropdown** — Chrome's 🔍 "Search tabs" over a live title/URL filter, both strips.
- **permission ALLOW path (wire seam + client answer landed; engine handler in flight)** —
  `EventMsg::PermissionRequest`/`ControlMsg::PermissionDecision` (tags 19/28) + `answer_permission`;
  the `cef_permission_handler` engine leg (holds the async prompt callback open across the round-trip,
  ref-counted) is a farm agent in progress. Shell prompt gated on engine success.

**Honest remaining tail (genuinely engine-coupled / on-glass / multi-session, NOT clean gaps):**
per-site permission engine leg (in flight) + its shell prompt, IME preedit (host-method round-trip),
password manager (session store + engine form fill/capture), PiP (video-element engine detection),
in-shell PDF viewer (needs a PDF renderer). The pure-shell industry surface is now essentially
COMPLETE — verified this drive: bookmarks bar, tab groups, drag-reorder, reopen-closed, configurable
search engines, print options, site styles, find-in-page (match tally), zoom controls all already
shipped.

### Session tally 2026-07-13 — POST-UNBLOCK addendum

Operator lifted the gated set via a 7-Q survey (see [[browser-gated-features-unblocked]]). Delivered
3 MORE features after the unblock, all farm-green: **#15 session HSTS** (remember + auto-upgrade
user-upgraded hosts, no persistence), **#16 safe-browsing mesh source** (dead-code setter now wired
to `browser/safe-browsing-hosts.txt` under the workgroup root; block mechanism activates on
population), **#17 manual fullscreen (F11)** (immersive body-only view, Esc exits). **17 features
total this session**, `mde-shell-egui` 1158→1174/0, ~28 new tests, 0 style-leaks. ~46/66 (~70%).
Remaining = engine round-trips (permission handler, safe-browsing interstitial, audible, HTML5
page-fullscreen, UA HTTP-header, IME, password autofill, B4 smoke) + big builds (PDF UI, PiP) —
each a wire-protocol + CEF-ABI + 4-crate + build→deploy→.15-reboot→verify cycle. Authorized,
multi-session.

### Session tally 2026-07-13 (this drive)

FOURTEEN features delivered to green in one session — star-state, bookmark autocomplete, IDN warning,
omnibox kb-nav, private-mode explainer, Clear-All-Browsing-Data, hover thumbnail, omnibox focus ring,
adblock breakdown, inline top-hit preselect, **configurable search engines + keyword shortcuts**,
**print-preview options**, **tab groups** (BOTH strips — horizontal band + vertical left-edge),
**user-authored CSS site styles** (safe userscript slice — CSS only, arbitrary JS stays gated;
editor drawer + menu). Spanning display, interactive,
a11y, privacy, data-plumbing, omnibox-completion, AND the "large" regime (search engines, print
options, tab groups). 21 new unit tests; `mde-shell-egui` 1158→1170/0, `mde-web-preview-client`
47→48/0, 0 style-leaks. THREE items I'd earlier mis-labeled "multi-session epics" (search engines,
print options, tab groups) were delivered autonomously this session — "large" was never a real blocker.

**Honest boundary on the rest (why it is not a one-session job):**
- *Threat-model-gated (~5: permission-allow, safe-browsing, HSTS, password-mgr, extensions-runtime):*
  NOT autonomously deliverable — [[browser-privacy-locks]] forbids changing the permission/persistence
  posture without the operator. These are operator decisions, permanently.
- *Engine-handlers firing on user-gesture events (HTML5 fullscreen, audible indicator):* the callback
  fires only on a gesture-gated page action (requestFullscreen / page media playback), which cannot be triggered
  headlessly — so even the `cef-verify` harness can't confirm them; they need an on-glass seat.
- *Large multi-session epics (tab groups, configurable search engines, PiP, userscript engine, in-shell
  PDF UI, print-preview options):* each is a feature program, not a gap.
- The audit also carries false-negatives (confirmed: middle-click-close is SHIPPED), so the true
  shipped count is higher than the raw audit tally.
- **Gated (need a CEF handler, security review, or are precluded by the no-persistent-profile
  non-goal):** per-site permission ALLOW path (deny-only today), safe-browsing host population +
  interstitial, HSTS, B4 WebExtensions runtime proof, B5 password manager, page-AT bridge, audio
  routing, HTML5 fullscreen (`on_fullscreen_mode_change` unregistered), IME preedit, UA HTTP-header.
- **Large (multi-session):** tab groups, configurable search engines, PiP, real userscript engine,
  in-shell PDF UI, print-preview options.

Below is the ORIGINAL prioritized backlog (pre-reconciliation) for context.

Engine (Track E) is now un-blocked: CEF paints real frames on the F44 seat after the
`/dev/shm` + `/etc/alternatives` sandbox fixes (`c27de915`, `c11c0ff4`).

## Tier 0 — blockers (a working browser needs these)
- **B1 CEF NavState → chrome.** CEF never emits nav/load state → omnibox URL, Back/Forward
  enable, and the loading indicator are all dead. *(engine lane; web-cef bridge + web/mod.rs)*
- **B2 Real downloads.** CEF download handler unwired — no browser-initiated downloads. *(downloads lane)*
- **B3 Browsing history absent.** History menu is only Back/Forward. *(downloads lane)*
- **B4 WebExtensions never run.** Validated + allowlisted but no runtime. *(extensions lane)*
- **B5 No password manager / autofill.** *(extensions + security lanes)*

## Tier 1 — high (industry-grade essentials)
- **Tab strip:** drag-reorder, keyboard shortcuts (Ctrl+T/W/Tab/Shift+T/1-9), restore-closed-tab,
  pinned tabs, audible/mute indicator, favicons. *(tabstrip lane; web/mod.rs, menubar.rs)*
- **Omnibox:** local history/bookmark autocomplete, inline autocomplete + top-hit preselect +
  keyboard nav of suggestions, clickable in-bar security indicator. *(omnibox lane)*
- **Engine input/paint:** page title from CEF, mouse button-state on move (drag-select/scrollbar),
  modifiers+click-count on click (ctrl/shift/double-click), `<select>`/datalist popups (PET_POPUP),
  in-page right-click context menu, window.open/target=_blank opens a visible tab. *(engine lane)*
- **Security:** cert-error interstitial, clickable page-info/site-info panel, per-site permission
  grants (camera/mic/location), safe-browsing blocked-page gating navigation. *(security lane)*
- **Bookmarks:** a bookmarks bar in the chrome; the star reflects state + is a toggle + add editor. *(downloads lane)*
- **Downloads UX:** Open / Show-in-folder on completed items and dangerous-file warnings are now
  shipped in the reconciled browser code; this line is retained only as original backlog context.
  *(downloads lane)*

## Tier 2 — medium (polish toward parity)
- Omnibox: robust search-vs-navigate router, scheme elision + domain emphasis, configurable search
  engines + keywords/tab-to-search, IDN/punycode homograph handling.
- Tabs: favicons, shrink+scroll overflow (not stacked rows), middle-click close, tab groups,
  HTML5 page fullscreen, hover thumbnail.
- Engine: native page zoom (not CSS `zoom`), find-in-page match count + highlight-all, IME preedit,
  engine-driven cursor updates, JS dialogs/file chooser, sad-tab on renderer crash, favicons.
- Security/privacy: HSTS + persistent HTTPS-only, cookie viewer/controls, global clear-browsing-data
  dialog, adblock blocked-item breakdown + in-chrome filter toggle, explicit private-mode UX.
- Extensions/advanced: real userscript engine, inline page translate, readability reader mode,
  print preview + options (range/size/orientation), richer UA emulation, account sync, in-shell PDF UI.

## Deliberate non-goals (privacy threat model — confirm before "fixing")
- WebRTC removed → video conferencing/casting/screen-share impossible *(extensions lane flagged as
  blocker, but this is a deliberate design choice — re-confirm with operator before restoring)*.
- No persistent profile/cookies by design (sandbox has no writable $HOME) — private-by-default.

## Strengths (already above default Chrome)
- Every engine runs in a robust OS sandbox (userns + seccomp + dropped caps + pivot_root RO rootfs).
- Rich tab context menu (mute, autoplay blocking, force-dark, reader, containers, display target).
