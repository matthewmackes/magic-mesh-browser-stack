# Browser — native-grade performance

**Goal (operator, 2026-07-19):** the platform web browser must match a native
Xorg/Wayland Chromium: **perfect frame rate, smooth, perfect audio.** The
canonical stress case is **5 video tabs playing with no system impact.**

Engine = CEF **OSR (offscreen rendering), CPU-readback** (`mde-web-cef`, publishes
BGRA8 into an `MWP1` shm channel). This doc is the bottleneck map + the phased plan
+ how we *measure* the gap. Feature-completeness lives in
`browser-industry-grade-backlog.md`; this doc is strictly the **performance axis**,
which that backlog never addressed. Tracked as WL-PERF-003.

Base branch: `agent/browser-enterprise-hardening` (the real seat browser, 137
browser commits ahead of origin/master). File:line citations below are against
that branch.

## Why the current path is slow (verified 2026-07-19)

The architecture reads every rendered frame back to CPU, ships it over shm,
reconverts it, and re-uploads it to the GPU — with three multipliers that make it
expensive:

1. **No occlusion signal — the flagship blocker.** There was *no* visibility
   message on `mde-web-wire`; every CEF helper paints at ~30 fps whether its tab is
   foreground or buried. `on_paint` publishes every frame; idle-suspend only fires
   after **30 min** and skips audible tabs. So N background video tabs = N ×
   (decode + readback + memcpy + socket) forever. **This is the root cause of
   "5 video tabs tanks the system."**
2. **Redundant frame copies.** Per 1080p BGRA frame the old path did: shm→`Vec`
   (mandatory seqlock snapshot), then `clone()`, then an in-place B↔R swap pass,
   then a `Color32` build — 3 full-frame allocations + 2 linear passes before the
   GPU. (**Fixed** — fused to one pass, see Phase 2.)
3. **No dirty-rect.** CEF hands `on_paint` the changed rectangles; they are
   ignored. The whole frame is always republished, reconverted, and re-uploaded
   even if one pixel changed.
4. **Full-`Context` repaint.** When media plays, the shell calls
   `request_repaint_after(16ms)` on the whole egui `Context`, re-tessellating
   dock + taskbar + chrome every frame, not just the browser texture rect.
5. **Audio is muted by irony.** `get_audio_parameters` returns `1` to opt into
   CEF's PCM stream *only* to detect the audible bit for the 🔊 pip, then
   `on_audio_stream_packet` discards every sample (a no-op). Opting in **diverts
   audio away from the OS output** → silence.
6. **Software video decode only.** `--ozone-platform=headless`, no
   `VaapiVideoDecoder`. GPU is *not* disabled, but the headless ozone backend has
   no HW-decode path.

## Measurement — how we prove "native"

"Perfect frame rate" is unfalsifiable without instrumentation. **Phase 0 builds an
env-gated metrics harness** (idiom mirrors `MDE_CEF_TRACE_NAV`):

- `MDE_WEB_PERF=1` → the shell logs, per second, per live tab: published-frame
  rate (from `PaintReady` seq deltas), convert time (shm→`ColorImage`), GPU-upload
  time, egui full-frame paint time, and dropped/coalesced frames.
- A headless bench in `mde-web-preview-client` (extends the `cef-verify` harness
  pattern) drives a known animated page and reports steady-state fps + p99
  frame-time with **no shell**, so we can A/B a change in ~15 s without a seat.
- **Targets:** foreground tab sustains 60 fps at p99 ≤ 16.6 ms; each *hidden* tab
  drops to ≈0 published fps; 5 simultaneous video tabs (1 visible) keep total
  engine+shell CPU within a small multiple of one native Chromium tab. Baseline is
  captured *before* Phase 1 and re-measured after each phase.

## Phased plan

Wire seam is frozen first and committed so parallel work branches from it; then
engine (`cef_browser/mod.rs`), shell (`web/mod.rs`), and client (`frame.rs`)
proceed in parallel on disjoint files (the proven collision-free partition).

- **Phase 0 — Instrumentation.** The metrics above. Pure shell + client; no ABI.
- **Phase 1 — Occlusion signal (flagship).** `ControlMsg::SetHidden { hidden }`
  (wire tag **38**, **frozen**) → CEF `CefBrowserHost::WasHidden()` at vtable
  **offset 312** (field 34, derived between `was_resized`=304 and `invalidate`=328;
  cross-check on-seat header at `/opt/mde/cef/include/capi/` before ship) → shell
  computes per-tab visibility (`hidden = !(surface_visible && tab==active) &&
  !pipped`) and sends the edge on every activation/deactivation, PiP change, and
  Browser-surface foreground change. Audible background tabs still hide (audio
  continues under `WasHidden`; this is exactly native `document.hidden` behavior).
- **Phase 2 — Frame path.** (a) **DONE:** fuse the BGRA swizzle into the `Color32`
  build (`frame.rs`, one pass, unit-tested). (b) Honor `on_paint` dirty rects:
  publish only changed regions and `set_partial` the texture sub-rect instead of a
  full re-upload. (c) Scope the repaint to the browser texture rect where egui
  allows, so playback stops re-tessellating unrelated panels.
- **Phase 3 — Native audio.** Move the 🔊 audible bit to
  `CefDisplayHandler::on_audio_state_changed` (extends the already-verified display
  block — low NULL-handler risk), return `0` from `get_audio_parameters` so CEF
  routes audio to the OS itself (native A/V sync), and verify the sandbox `/dev/snd`
  ALSA bind carries output (add a PipeWire socket to `mde-web-sandbox` only if ALSA
  alone is insufficient). No PCM ever crosses the wire.
- **Phase 4 — HW decode (stretch / operator-gated).** Requires a non-headless GL
  ozone backend + `--enable-features=VaapiVideoDecoder`; the shell owns the DRM
  seat, so this is an architecture call, not a flag flip. Shared boundary with
  WL-FUNC-001's GPU-decode line. Measure whether Phases 1-3 already hit the
  5-video target before spending here.

## Definition of done

Each phase: farm-green build + unit test + `0` style-leaks + committed. The overall
goal closes only on **eyes-on-glass at seat .15/.138**: 5 video tabs, 1 visible,
smooth 60 fps foreground, audible audio in sync, background tabs quiescent — with
the `MDE_WEB_PERF` numbers to back it. (Compiling ≠ shipping-correct; live-verify
per `deploy-shell-needs-drm-features`.)
