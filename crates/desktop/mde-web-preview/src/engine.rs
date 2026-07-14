//! The interactive Servo web engine, embedded headless-first (BOOKMARKS-5).
//!
//! One [`Engine`] drives one Servo instance rendering one tab into an offscreen
//! [`SoftwareRenderingContext`]. Each finished frame is read back and published
//! to a [`FrameChannel`] (the shm seam BOOKMARKS-6 consumes). JavaScript is on;
//! the security defaults (a generic UA, no persistent storage APIs, no HTTP disk
//! cache, no WebRTC/WebGPU, denied permission prompts) are applied through
//! [`Preferences`] + the delegates below — real, not TODO. Persistence is *also*
//! prevented structurally by the sandbox (read-only rootfs + tmpfs + no `$HOME`),
//! so "no history / cookies cleared on close" cannot be bypassed by the content.
//!
//! Navigation (`load`, `reload`, `go_back`, `go_forward`) and input forwarding
//! are exposed for BOOKMARKS-6 to drive over IPC; this unit ships the engine +
//! the headless "about:blank -> a frame arrives on the shm channel" path.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use embedder_traits::JSValue;
use euclid::{Box2D, Point2D};
use servo::{
    EventLoopWaker, LoadStatus, PermissionRequest, Preferences, RenderingContext, Servo,
    ServoBuilder, ServoDelegate, SoftwareRenderingContext, WebView, WebViewBuilder,
    WebViewDelegate,
};

use crate::shm::{FrameChannel, PixelFormat};

/// A fixed, common, non-identifying desktop User-Agent. Reveals nothing about
/// the mesh, the node, or the operator; blends into the general Firefox-on-Linux
/// population rather than announcing "Servo".
pub const GENERIC_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

/// How long [`Engine::pump_until_content_frame`] keeps pumping after
/// `LoadStatus::Complete` before force-compositing the final capture. The
/// page's display list reaches WebRender asynchronously (scene-builder
/// thread), so the composite that actually CONTAINS the page trails the DOM
/// load event by a scene-build + frame-generation hop; this window lets the
/// natural content frame-ready arrive, and the forced final composite then
/// renders whatever newest frame WebRender holds.
const POST_LOAD_SETTLE: Duration = Duration::from_millis(500);

/// Whether `MDE_WEB_DEBUG` per-capture tracing is enabled (read once).
fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("MDE_WEB_DEBUG").is_some())
}

/// Cheap content stats over an RGBA frame: how many distinct byte values appear,
/// and the mean luma (Rec. 601). A blank/white frame reads as `distinct` ~1–2 and
/// `mean_luma` ~255; a real render spreads both. Used by the `render-once`
/// `FRAME_OK` report and the `MDE_WEB_DEBUG` per-capture trace.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::suboptimal_flops,
    reason = "a pixel count fits an f64 mantissa exactly at these frame sizes, and the \
              readable weighted-sum form is fine for a diagnostic luma stat (no mul_add)"
)]
pub fn frame_stats(pixels: &[u8]) -> (usize, f64) {
    let mut seen = [false; 256];
    for &b in pixels {
        seen[b as usize] = true;
    }
    let distinct = seen.iter().filter(|&&s| s).count();
    let mut luma_sum = 0.0f64;
    let pixel_count = pixels.len() / 4;
    for px in pixels.chunks_exact(4) {
        luma_sum += 0.299 * f64::from(px[0]) + 0.587 * f64::from(px[1]) + 0.114 * f64::from(px[2]);
    }
    let mean_luma = if pixel_count == 0 {
        0.0
    } else {
        luma_sum / pixel_count as f64
    };
    (distinct, mean_luma)
}

/// The security-hardened Servo preferences for a sandboxed preview tab.
///
/// JavaScript stays ON (the `js_jit` feature + Servo defaults). Everything that
/// persists to disk, fingerprints, or leaks the network path is turned OFF.
///
/// Cross-engine posture (browser-5): the shell runs two interchangeable browser
/// engines on the same seat — CEF is the preferred default when its runtime is
/// present and this Servo helper is the fallback (see
/// `mde-shell-egui::web::engine_runtime::preferred_default_engine`) — so the
/// same user may render a page under either, and their security guarantees must
/// be reasoned about together:
/// * WebRTC / local-IP leak: Servo turns it off at the *engine* level here
///   (`dom_webrtc_enabled = false` — `RTCPeerConnection` never exists, no bypass).
///   CEF has no equivalent hard off switch on its prebuilt binary, so it pairs an
///   engine-level `--force-webrtc-ip-handling-policy=disable_non_proxied_udp`
///   switch (blocks the actual raw-local-IP leak — parity with Servo for the
///   *harm*) with a best-effort JS shim that removes the API surface. The one
///   residual gap is CEF-only: a page's own inline script can touch the WebRTC
///   *API surface* in the sub-tick before the shim's first injection lands
///   (documented in `mde-web-cef::cef_browser::webrtc_block_script`); the IP
///   leak itself stays engine-blocked on both. Do not weaken this Servo hard-off
///   to "match" CEF — CEF is meant to reach up to Servo, not the reverse.
/// * Passkeys / WebAuthn: at parity — this Servo helper bridges ceremonies to
///   the daemon-owned passkey worker via `poll_passkey_request` /
///   `complete_passkey` exactly as CEF's `passkey_bridge_script` does. (An
///   earlier "passkeys are CEF-only" belief is stale as of this landing.)
#[must_use]
pub fn secure_preferences() -> Preferences {
    Preferences {
        user_agent: GENERIC_USER_AGENT.to_owned(),
        // No persistent storage surface (also structurally blocked by the sandbox).
        dom_indexeddb_enabled: false,
        dom_cookiestore_enabled: false,
        dom_storage_manager_api_enabled: false,
        network_http_cache_disabled: true,
        // Deny-all sensitive web features.
        dom_webrtc_enabled: false, // no local-IP leak
        dom_webgpu_enabled: false,
        media_glvideo_enabled: false,
        ..Default::default()
    }
}

/// State shared between the [`WebViewDelegate`] and the render loop. Servo runs
/// single-threaded, so `Rc<Cell<_>>` is sufficient and lock-free.
#[derive(Default)]
struct Shared {
    frame_ready: Cell<bool>,
    load_complete: Cell<bool>,
}

/// A no-op event-loop waker: the headless driver spins the loop itself, so there
/// is nothing to wake.
#[derive(Clone)]
struct HeadlessWaker;

impl EventLoopWaker for HeadlessWaker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {}
}

/// Servo-instance-level delegate: refuse devtools, surface engine errors.
#[derive(Default)]
struct HardenedServoDelegate;

impl ServoDelegate for HardenedServoDelegate {
    fn notify_error(&self, error: servo::ServoError) {
        eprintln!("mde-web-preview: engine error: {error:?}");
    }
}

/// Per-webview delegate: capture frame-ready + load-complete, deny every
/// permission prompt (geolocation, notifications, camera/mic, …).
struct TabDelegate {
    shared: Rc<Shared>,
}

impl WebViewDelegate for TabDelegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.shared.frame_ready.set(true);
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if matches!(status, LoadStatus::Complete) {
            self.shared.load_complete.set(true);
        }
    }

    fn request_permission(&self, _webview: WebView, request: PermissionRequest) {
        // Deny-all sensitive web permissions (acceptance + design lock).
        request.deny();
    }

    fn notify_crashed(&self, _webview: WebView, reason: String, backtrace: Option<String>) {
        // A crashed content pipeline otherwise renders as a silent blank frame
        // (the BUG-BROWSER-6 white-screen class) — always say so on stderr.
        eprintln!("mde-web-preview: content pipeline CRASHED: {reason}");
        if let Some(backtrace) = backtrace {
            eprintln!("{backtrace}");
        }
    }

    fn show_console_message(
        &self,
        _webview: WebView,
        level: servo::ConsoleLogLevel,
        message: String,
    ) {
        if debug_enabled() {
            eprintln!("mde-web-preview[debug]: console {level:?}: {message}");
        }
    }
}

/// One embedded, sandboxed Servo tab rendering offscreen.
pub struct Engine {
    servo: Servo,
    webview: WebView,
    rendering_context: Rc<SoftwareRenderingContext>,
    shared: Rc<Shared>,
    width: u32,
    height: u32,
    /// When the engine booted — the `MDE_WEB_DEBUG` trace timebase.
    booted: Instant,
}

impl Engine {
    /// Boot a headless engine at `width` x `height` and begin loading `url`.
    ///
    /// # Errors
    /// Fails if the offscreen software rendering context cannot be created
    /// (e.g. no software GL available) or the URL is unparseable.
    pub fn new_headless(width: u32, height: u32, url: &str) -> Result<Self> {
        let size = dpi::PhysicalSize::new(width, height);
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(size)
                .map_err(|e| anyhow::anyhow!("software rendering context: {e:?}"))?,
        );

        let servo = ServoBuilder::default()
            .preferences(secure_preferences())
            .event_loop_waker(Box::new(HeadlessWaker))
            .build();
        servo.set_delegate(Rc::new(HardenedServoDelegate));
        if debug_enabled() {
            // Route Servo's internal `log` records (constellation / paint /
            // layout) to stderr, filtered by RUST_LOG — the deep-debug seam.
            servo.setup_logging();
        }

        let shared = Rc::new(Shared::default());
        let target = url::Url::parse(url).with_context(|| format!("parse url {url}"))?;
        let rc_dyn: Rc<dyn RenderingContext> = rendering_context.clone();
        let webview = WebViewBuilder::new(&servo, rc_dyn)
            .delegate(Rc::new(TabDelegate {
                shared: shared.clone(),
            }))
            .url(target)
            .build();
        webview.focus();

        Ok(Self {
            servo,
            webview,
            rendering_context,
            shared,
            width,
            height,
            booted: Instant::now(),
        })
    }

    /// Spin the engine until the FIRST frame is composited, publish it to
    /// `channel`, and return. Bounded by `timeout`.
    ///
    /// The first composite is generally the PRE-CONTENT one: registering the
    /// webview sends the root-pipeline display list immediately, and WebRender
    /// generates a frame of that still-empty scene (the shell-background clear)
    /// before the page's own display list exists. So this is a liveness /
    /// warm-up primitive ("the pipeline produces frames"), NOT a content
    /// capture — for "the page is actually visible in the frame", use
    /// [`Self::pump_until_content_frame`] (BUG-BROWSER-6).
    ///
    /// # Errors
    /// Fails if no frame is produced before `timeout`, or the read-back /
    /// publish fails.
    pub fn pump_until_frame(&self, channel: &FrameChannel, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.servo.spin_event_loop();

            if self.shared.frame_ready.replace(false) && self.capture_frame(channel, "first")? {
                return Ok(());
            }

            if Instant::now() >= deadline {
                anyhow::bail!("timed out after {timeout:?} waiting for the first frame");
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// Spin the engine until the page has finished loading AND its content has
    /// composited, publishing every captured frame to `channel` (the newest
    /// frame wins on the seqlock channel). Bounded by `timeout`.
    ///
    /// Why not the first frame (BUG-BROWSER-6): the FIRST
    /// `notify_new_frame_ready` predates layout — it announces the frame
    /// WebRender generated for the initial, still-EMPTY root scene
    /// (`Painter::clear_background()` + no content pipeline), which reads back
    /// as a uniform shell-background frame no matter what the page contains.
    /// Content arrives on a LATER frame-ready once script/layout ship the
    /// page's display list. So: pump (publishing frames as they come) until
    /// `LoadStatus::Complete`, keep pumping through a short settle window (the
    /// content scene is built asynchronously), then force one final composite —
    /// `paint()` renders the newest frame WebRender holds, so even a missed
    /// frame-ready cannot leave a stale capture as the channel's latest.
    ///
    /// # Errors
    /// Fails if nothing could be captured before `timeout`, or the read-back /
    /// publish fails. If frames were published but the load never completed
    /// (heavy pages that never report `Complete`), returns `Ok` — the newest
    /// frame is on the channel, the same degrade the tab serve loop uses.
    pub fn pump_until_content_frame(
        &self,
        channel: &FrameChannel,
        timeout: Duration,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut published = false;
        let mut load_seen_at: Option<Instant> = None;
        loop {
            self.servo.spin_event_loop();

            if self.shared.frame_ready.replace(false) {
                published |= self.capture_frame(channel, "content-pump")?;
            }

            if load_seen_at.is_none() && self.shared.load_complete.get() {
                load_seen_at = Some(Instant::now());
            }

            if let Some(at) = load_seen_at {
                if at.elapsed() >= POST_LOAD_SETTLE {
                    self.shared.frame_ready.set(false);
                    published |= self.capture_frame(channel, "content-final")?;
                    if published {
                        return Ok(());
                    }
                    anyhow::bail!("no frame could be read back after load completion");
                }
            }

            if Instant::now() >= deadline {
                if published {
                    return Ok(());
                }
                anyhow::bail!("timed out after {timeout:?} waiting for a content frame");
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// Spin the engine once and, if a fresh frame was painted, publish it to
    /// `channel`. Returns whether a frame was published. Used by the continuous
    /// per-tab serve loop (an idle page simply produces no new frames).
    ///
    /// # Errors
    /// Fails only if the read-back / publish fails.
    pub fn pump_step(&self, channel: &FrameChannel) -> Result<bool> {
        self.servo.spin_event_loop();
        if self.shared.frame_ready.replace(false) {
            return self.capture_frame(channel, "step");
        }
        Ok(false)
    }

    /// Force a frame onto `channel` WITHOUT waiting for a fresh
    /// `notify_new_frame_ready` — the tab serve loop's first-frame watchdog.
    ///
    /// Some pages (heavy SPAs, ad-laden sites) paint but are slow to signal a
    /// frame-ready / never report `LoadStatus::Complete`, so a delivery keyed
    /// purely on `notify_new_frame_ready` can leave the shell stuck on "Loading
    /// the page…" indefinitely. This spins the loop once, then paints, presents,
    /// reads the framebuffer back, and publishes it regardless of the ready flag,
    /// so the shell always gets *a* frame (and goes Live). Returns whether a frame
    /// was actually published (read-back can still be empty before the very first
    /// paint). Keyed on a delivered frame, never on load completion.
    ///
    /// # Errors
    /// Fails only if the read-back / publish fails.
    pub fn force_emit(&self, channel: &FrameChannel) -> Result<bool> {
        self.servo.spin_event_loop();
        // Consume any pending ready flag so a follow-up `pump_step` does not
        // re-publish this same frame as a "new" paint.
        self.shared.frame_ready.set(false);
        self.capture_frame(channel, "forced")
    }

    /// Composite the current WebRender frame and publish it to `channel`:
    /// `paint()`, read the pixels back, `present()`, emit. Returns whether a
    /// frame was actually published (read-back can still be empty before the
    /// very first paint). All pump paths funnel through here so the ordering
    /// invariant and the `MDE_WEB_DEBUG` trace live in ONE place.
    ///
    /// Read back BEFORE `present()`: `read_to_image` reads the context's BOUND
    /// (back) buffer, and `present()` swaps in an unpreserved one
    /// (`PreserveBuffer::No`) — a post-present read returns the empty /
    /// one-frame-stale buffer, never the frame `paint()` just rendered (the
    /// live "browser renders all-black" bug, 2026-07-05).
    fn capture_frame(&self, channel: &FrameChannel, tag: &str) -> Result<bool> {
        self.webview.paint();
        let pixels = self.read_back()?;
        self.rendering_context.present();
        let Some(pixels) = pixels else {
            self.debug_trace(tag, None, channel);
            return Ok(false);
        };
        channel
            .emit(self.width, self.height, PixelFormat::Rgba8, &pixels)
            .context("publish frame to shm")?;
        self.debug_trace(tag, Some(&pixels), channel);
        Ok(true)
    }

    /// `MDE_WEB_DEBUG` per-capture trace: when the capture happened, which pump
    /// path produced it, whether the load had completed, and whether the pixels
    /// carry content (distinct byte values + mean luma). Stats are computed
    /// only when tracing is on.
    fn debug_trace(&self, tag: &str, pixels: Option<&[u8]>, channel: &FrameChannel) {
        if !debug_enabled() {
            return;
        }
        let elapsed = self.booted.elapsed().as_millis();
        let load = self.shared.load_complete.get();
        if let Some(px) = pixels {
            let (distinct, mean_luma) = frame_stats(px);
            eprintln!(
                "mde-web-preview[debug]: +{elapsed}ms {tag} seq={} load_complete={load} \
                 distinct={distinct} mean_luma={mean_luma:.1}",
                channel.sequence(),
            );
        } else {
            eprintln!(
                "mde-web-preview[debug]: +{elapsed}ms {tag} read_back=empty load_complete={load}"
            );
        }
    }

    /// BUG-BROWSER-6 debug probes (`MDE_WEB_DEBUG` only, no-op otherwise):
    /// interrogate the live page and Servo's own screenshot pipeline to locate
    /// where content stops flowing.
    ///
    /// * The JS probe proves whether script is alive and what the page THINKS
    ///   its state is (`visibilityState`, viewport, body geometry, computed
    ///   background) — a hidden/zero-sized document explains a skipped render.
    /// * The screenshot probe drives Servo's own readiness pipeline (load +
    ///   render-blocking + fonts + display lists uploaded + frame ready) and
    ///   reads the composite back independently of our capture path: content
    ///   here but not in `capture_frame` = our read is broken; a timeout here =
    ///   the pipelines never produced render-ready display lists at all.
    pub fn debug_content_probe(&self, timeout: Duration) {
        if !debug_enabled() {
            return;
        }
        let booted = self.booted;
        self.webview.evaluate_javascript(
            "[document.visibilityState, document.hidden, \
              window.innerWidth+'x'+window.innerHeight, \
              document.body && document.body.getBoundingClientRect().width+'x'+\
              document.body.getBoundingClientRect().height, \
              getComputedStyle(document.body).backgroundColor].join(' | ')",
            move |result| {
                eprintln!(
                    "mde-web-preview[debug]: +{}ms js-probe: {result:?}",
                    booted.elapsed().as_millis()
                );
            },
        );

        let screenshot_done: Rc<Cell<bool>> = Rc::default();
        let done = screenshot_done.clone();
        self.webview.take_screenshot(None, move |result| {
            let elapsed = booted.elapsed().as_millis();
            match result {
                Ok(image) => {
                    let pixels = image.into_raw();
                    let (distinct, mean_luma) = frame_stats(&pixels);
                    eprintln!(
                        "mde-web-preview[debug]: +{elapsed}ms screenshot \
                         distinct={distinct} mean_luma={mean_luma:.1}"
                    );
                }
                Err(error) => {
                    eprintln!("mde-web-preview[debug]: +{elapsed}ms screenshot FAILED: {error:?}");
                }
            }
            done.set(true);
        });

        let deadline = Instant::now() + timeout;
        while !screenshot_done.get() && Instant::now() < deadline {
            self.servo.spin_event_loop();
            if self.shared.frame_ready.replace(false) {
                // The screenshot pipeline needs composites to drain its pending
                // frames — paint like the serve loop would.
                self.webview.paint();
                let _ = self.read_back();
                self.rendering_context.present();
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        if !screenshot_done.get() {
            eprintln!(
                "mde-web-preview[debug]: screenshot probe TIMED OUT after {timeout:?} — \
                 the pipelines never reported render-ready display lists"
            );
        }
    }

    /// Read the whole framebuffer back into an RGBA byte buffer, if painted.
    ///
    /// MUST be called after `webview.paint()` and BEFORE
    /// `rendering_context.present()`: `SoftwareRenderingContext::read_to_image`
    /// reads the context's currently BOUND (back) surface, and `present()` is a
    /// surfman swap-chain `swap_buffers(PreserveBuffer::No)` — after it, the bound
    /// surface is an unpreserved recycled/new buffer (all zeros on the first swap,
    /// one frame stale after), so a post-present read can never observe the frame
    /// that was just painted.
    fn read_back(&self) -> Result<Option<Vec<u8>>> {
        let rect = Box2D::new(
            Point2D::new(0, 0),
            Point2D::new(i32::try_from(self.width)?, i32::try_from(self.height)?),
        );
        self.rendering_context
            .read_to_image(rect)
            .map_or_else(|| Ok(None), |img| Ok(Some(img.into_raw())))
    }

    /// Navigate the tab to `url` (BOOKMARKS-6 drives this over IPC).
    ///
    /// # Errors
    /// Fails if `url` is unparseable.
    pub fn load(&self, url: &str) -> Result<()> {
        let target = url::Url::parse(url).with_context(|| format!("parse url {url}"))?;
        self.webview.load(target);
        Ok(())
    }

    /// Reload the current page.
    pub fn reload(&self) {
        self.webview.reload();
    }

    /// Go back `amount` history entries (address-bar / back-button intent).
    pub fn go_back(&self, amount: usize) {
        let _ = self.webview.go_back(amount);
    }

    /// Go forward `amount` history entries.
    pub fn go_forward(&self, amount: usize) {
        let _ = self.webview.go_forward(amount);
    }

    /// Set page zoom through Servo's page script seam. This is intentionally the
    /// same bounded DOM transform CEF uses until Servo exposes a native zoom API.
    pub fn set_zoom(&self, percent: u16) {
        self.evaluate_page_script(&page_zoom_script(percent));
    }

    /// Find text on the current page through Servo's page script seam.
    pub fn find_in_page(&self, query: &str, backwards: bool) {
        if query.trim().is_empty() {
            self.clear_find();
        } else {
            self.evaluate_page_script(&find_in_page_script(query, backwards));
        }
    }

    /// Clear the current page selection/highlight where the DOM supports it.
    pub fn clear_find(&self) {
        self.evaluate_page_script(clear_find_script());
    }

    /// Apply or remove Quasar forced-dark styling in the Servo tab.
    pub fn set_force_dark(&self, enabled: bool) {
        self.evaluate_page_script(&force_dark_script(enabled));
    }

    /// Apply or remove reader-mode styling in the Servo tab.
    pub fn set_reader_mode(&self, enabled: bool) {
        self.evaluate_page_script(&reader_mode_script(enabled));
    }

    /// Apply or remove the shell-curated userscript bundle in the Servo tab.
    pub fn set_user_scripts(&self, enabled: bool, bundle: &str) {
        self.evaluate_page_script(&userscript_library_script(enabled, bundle));
    }

    /// Override page-visible User-Agent metadata in the Servo tab. Network-stack
    /// header rewriting remains a native helper hook.
    pub fn set_user_agent(&self, user_agent: &str) {
        self.evaluate_page_script(&user_agent_override_script(user_agent));
    }

    /// Override page-visible device metadata in the Servo tab. Native compositor
    /// viewport/device emulation remains a deeper helper hook.
    pub fn set_device_profile(
        &self,
        profile: &str,
        width: u16,
        height: u16,
        scale_percent: u16,
        touch: bool,
    ) {
        self.evaluate_page_script(&device_profile_script(
            profile,
            width,
            height,
            scale_percent,
            touch,
        ));
    }

    /// Apply or remove shell-owned spellcheck highlights in the Servo tab.
    pub fn set_spellcheck_highlights(&self, words: &[String]) {
        self.evaluate_page_script(&spellcheck_highlight_script(words));
    }

    /// Apply one shell-selected spellcheck replacement in the Servo tab.
    pub fn apply_spellcheck_correction(&self, word: &str, replacement: &str) {
        self.evaluate_page_script(&spellcheck_correction_script(word, replacement));
    }

    /// Apply one shell-selected spellcheck replacement to every visible match in
    /// the Servo tab.
    pub fn apply_spellcheck_correction_all(&self, word: &str, replacement: &str) {
        self.evaluate_page_script(&spellcheck_correction_all_script(word, replacement));
    }

    /// Apply one shell-selected spellcheck replacement to an indexed visible
    /// match in the Servo tab.
    pub fn apply_spellcheck_correction_at(&self, word: &str, replacement: &str, occurrence: u16) {
        self.evaluate_page_script(&spellcheck_correction_at_script(
            word,
            replacement,
            occurrence,
        ));
    }

    /// Extract bounded visible page text for shell-owned spellcheck/TTS.
    pub fn request_page_text<F>(&self, id: u64, max_bytes: u32, publish: F)
    where
        F: FnOnce(u64, String) + 'static,
    {
        let max_bytes = max_bytes.clamp(1, 64 * 1024);
        self.webview
            .evaluate_javascript(&page_text_script(max_bytes), move |result| {
                let text = match result {
                    Ok(JSValue::String(text)) => clamp_utf8(&text, max_bytes as usize),
                    Err(_) => String::new(),
                    Ok(_) => String::new(),
                };
                publish(id, text);
            });
    }

    /// Extract bounded active-page scrape data for shell-owned export.
    pub fn request_page_scrape<F>(
        &self,
        id: u64,
        max_bytes: u32,
        max_links: u16,
        max_headings: u16,
        publish: F,
    ) where
        F: FnOnce(u64, String) + 'static,
    {
        self.webview.evaluate_javascript(
            &page_scrape_script(max_bytes, max_links, max_headings),
            move |result| {
                let body = match result {
                    Ok(JSValue::String(body)) => clamp_utf8(&body, 256 * 1024),
                    Err(_) => String::new(),
                    Ok(_) => String::new(),
                };
                publish(id, body);
            },
        );
    }

    /// Install/drain the page WebAuthn interception bridge. The bridge extracts
    /// public ceremony metadata and keeps the page promise pending until the
    /// daemon-owned passkey worker returns a matching completion.
    pub fn poll_passkey_request<F>(&self, publish: F)
    where
        F: FnOnce(String) + 'static,
    {
        self.webview
            .evaluate_javascript(passkey_bridge_drain_script(), move |result| {
                let body = match result {
                    Ok(JSValue::String(body)) => body,
                    _ => String::new(),
                };
                let body = body.trim();
                if body.starts_with('{') {
                    publish(clamp_utf8(body, 8 * 1024));
                }
            });
    }

    /// Resolve or reject one pending page WebAuthn/passkey request.
    pub fn complete_passkey(&self, body: &str) {
        self.evaluate_page_script(&passkey_complete_script(body));
    }

    /// Apply or remove tab audio mute in the Servo tab. Servo does not expose the
    /// CEF-style browser-host mute slot, so this uses the page seam to mute every
    /// HTML media element already present and any media element inserted later.
    pub fn set_audio_muted(&self, muted: bool) {
        self.evaluate_page_script(audio_mute_script(muted));
    }

    /// Apply or remove autoplay blocking in the Servo tab until user activation.
    pub fn set_autoplay_blocked(&self, blocked: bool) {
        self.evaluate_page_script(&autoplay_block_script(blocked));
    }

    /// Ask the page to invoke its print flow. Servo does not expose a native
    /// print/PDF backend here yet, so the helper uses the browser-standard DOM
    /// print hook and leaves save-as-PDF to CEF.
    pub fn print_page(&self) {
        self.evaluate_page_script(print_page_script());
    }

    fn evaluate_page_script(&self, script: &str) {
        self.webview.evaluate_javascript(script, |_| {});
    }

    /// Whether the initial load has reported completion.
    #[must_use]
    pub fn load_complete(&self) -> bool {
        self.shared.load_complete.get()
    }
}

fn page_zoom_script(percent: u16) -> String {
    let percent = percent.clamp(25, 500);
    format!("(function(){{document.documentElement.style.zoom='{percent}%';}})();")
}

fn passkey_bridge_drain_script() -> &'static str {
    r#"(function(){
try{
  if(!window.__mdeBrowserPasskeyQueue)window.__mdeBrowserPasskeyQueue=[];
  if(!window.__mdeBrowserPasskeyPending)window.__mdeBrowserPasskeyPending={};
  if(!window.__mdeBrowserPasskeyDrain){
    window.__mdeBrowserPasskeyDrain=function(){
      var item=window.__mdeBrowserPasskeyQueue.shift();
      return item?JSON.stringify(item).slice(0,8192):'';
    };
  }
  if(!window.__mdeBrowserPasskeyComplete){
    window.__mdeBrowserPasskeyComplete=function(event){
      try{
        event=event||{};
        var id=String(event.client_request_id||'');
        var pending=window.__mdeBrowserPasskeyPending&&window.__mdeBrowserPasskeyPending[id];
        if(!pending)return false;
        delete window.__mdeBrowserPasskeyPending[id];
        function ab(v){
          try{
            v=String(v||'').replace(/-/g,'+').replace(/_/g,'/');
            while(v.length%4)v+='=';
            var s=atob(v),out=new Uint8Array(s.length);
            for(var i=0;i<s.length;i++)out[i]=s.charCodeAt(i);
            return out.buffer;
          }catch(_){return new ArrayBuffer(0);}
        }
        if(event.error||event.state==='error'){
          pending.reject(new DOMException(String(event.error||'Passkey ceremony failed'),'NotAllowedError'));
          return true;
        }
        function setProto(obj,ctor){try{if(ctor&&ctor.prototype)Object.setPrototypeOf(obj,ctor.prototype);}catch(_){}return obj;}
        function b64(v){return String(v||'');}
        var credentialId=String(event.credential_id_b64url||'');
        var response={};
        if(event.op==='browser_passkey_assertion'||event.ceremony==='get'){
          response.authenticatorData=ab(event.authenticator_data_b64url);
          response.clientDataJSON=ab(event.client_data_json_b64url);
          response.signature=ab(event.signature_b64url);
          response.userHandle=ab(event.user_handle_b64url);
          response.toJSON=function(){return {authenticatorData:b64(event.authenticator_data_b64url),clientDataJSON:b64(event.client_data_json_b64url),signature:b64(event.signature_b64url),userHandle:b64(event.user_handle_b64url)};};
          setProto(response,window.AuthenticatorAssertionResponse);
        }else{
          response.clientDataJSON=ab(event.client_data_json_b64url);
          response.attestationObject=ab(event.attestation_object_b64url);
          response.getPublicKey=function(){return ab(event.public_key_spki_der_b64url||event.public_key_sec1_b64url);};
          response.getPublicKeyAlgorithm=function(){return Number(event.cose_alg||-7);};
          response.getTransports=function(){return ['internal'];};
          response.getAuthenticatorData=function(){return ab(event.authenticator_data_b64url);};
          response.toJSON=function(){return {attestationObject:b64(event.attestation_object_b64url),clientDataJSON:b64(event.client_data_json_b64url),publicKey:b64(event.public_key_spki_der_b64url||event.public_key_sec1_b64url),publicKeyAlgorithm:Number(event.cose_alg||-7),authenticatorData:b64(event.authenticator_data_b64url),transports:['internal']};};
          setProto(response,window.AuthenticatorAttestationResponse);
        }
        var credential={id:credentialId,rawId:ab(credentialId),type:'public-key',authenticatorAttachment:'platform',response:response};
        credential.getClientExtensionResults=function(){return {};};
        credential.toJSON=function(){return {id:credentialId,rawId:credentialId,type:'public-key',authenticatorAttachment:'platform',response:response.toJSON?response.toJSON():{},clientExtensionResults:{}};};
        pending.resolve(setProto(credential,window.PublicKeyCredential));
        return true;
      }catch(err){return false;}
    };
  }
  if(!window.__mdeBrowserPasskeyBridgeInstalled){
    window.__mdeBrowserPasskeyBridgeInstalled=true;
    window.__mdeBrowserPasskeySeq=window.__mdeBrowserPasskeySeq||0;
    try{
      if(!window.PublicKeyCredential)window.PublicKeyCredential=function PublicKeyCredential(){};
      if(!window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable)window.PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable=function(){return Promise.resolve(true);};
      if(!window.PublicKeyCredential.isConditionalMediationAvailable)window.PublicKeyCredential.isConditionalMediationAvailable=function(){return Promise.resolve(false);};
      if(!window.AuthenticatorAttestationResponse)window.AuthenticatorAttestationResponse=function AuthenticatorAttestationResponse(){};
      if(!window.AuthenticatorAssertionResponse)window.AuthenticatorAssertionResponse=function AuthenticatorAssertionResponse(){};
    }catch(_){}
    function trim(v,n){v=String(v||'').trim();return v.length>n?v.slice(0,n):v;}
    function b64url(value){
      try{
        if(value==null)return '';
        if(typeof value==='string')return value.replace(/=+$/,'').replace(/\+/g,'-').replace(/\//g,'_');
        var bytes=null;
        if(value instanceof ArrayBuffer)bytes=new Uint8Array(value);
        else if(ArrayBuffer.isView(value))bytes=new Uint8Array(value.buffer,value.byteOffset,value.byteLength);
        if(!bytes)return '';
        var s='',max=Math.min(bytes.length,1536);
        for(var i=0;i<max;i++)s+=String.fromCharCode(bytes[i]);
        return btoa(s).replace(/=+$/,'').replace(/\+/g,'-').replace(/\//g,'_');
      }catch(_){return '';}
    }
    function ceremony(kind,options){
      var pk=(options&&options.publicKey)||{};
      var rp=(pk.rp&&pk.rp.id)||location.hostname;
      var out={ceremony:kind,origin:String(location.href||''),rp_id:trim(rp,253),challenge_b64url:b64url(pk.challenge)};
      if(kind==='create'&&pk.user){
        out.user_handle_b64url=b64url(pk.user.id);
        out.user_name=trim(pk.user.displayName||pk.user.name||'',256);
      }
      if(kind==='get'&&Array.isArray(pk.allowCredentials)){
        out.allow_credentials=pk.allowCredentials.slice(0,64).map(function(c){return b64url(c&&c.id);}).filter(Boolean);
      }
      if(typeof pk.timeout==='number')out.timeout_ms=Math.max(0,Math.floor(pk.timeout));
      return out;
    }
    function enqueue(kind,options){
      var item=ceremony(kind,options);
      if(!item.challenge_b64url)return Promise.reject(new DOMException('Passkey challenge missing','NotAllowedError'));
      item.client_request_id='mde-pk-'+Date.now().toString(36)+'-'+(++window.__mdeBrowserPasskeySeq).toString(36);
      return new Promise(function(resolve,reject){
        window.__mdeBrowserPasskeyPending[item.client_request_id]={resolve:resolve,reject:reject,ceremony:kind};
        var q=window.__mdeBrowserPasskeyQueue;
        q.push(item);
        while(q.length>16)q.shift();
      });
    }
    var creds=navigator.credentials||(navigator.credentials={});
    creds.create=function(options){return enqueue('create',options);};
    creds.get=function(options){return enqueue('get',options);};
  }
  return window.__mdeBrowserPasskeyDrain();
}catch(_){return '';}
})()"#
}

fn passkey_complete_script(body: &str) -> String {
    let body = js_string_literal(body);
    format!(
        "(function(){{try{{var event=JSON.parse({body});if(window.__mdeBrowserPasskeyComplete)window.__mdeBrowserPasskeyComplete(event);}}catch(_){{}}}})();"
    )
}

fn find_in_page_script(query: &str, backwards: bool) -> String {
    let query = js_string_literal(query);
    let backwards = if backwards { "true" } else { "false" };
    format!("(function(){{window.find({query},false,{backwards},true,false,false,false);}})();")
}

const fn clear_find_script() -> &'static str {
    "(function(){var s=window.getSelection&&window.getSelection();if(s)s.removeAllRanges();})();"
}

fn force_dark_script(enabled: bool) -> String {
    if !enabled {
        return "(function(){var id='mde-servo-force-dark-style';var el=document.getElementById(id);if(el)el.remove();document.documentElement.style.colorScheme='';})();".to_owned();
    }
    let css = r#"
:root { color-scheme: dark !important; background: #0f1419 !important; }
html, body { background: #0f1419 !important; color: #f2f4f8 !important; }
body, main, article, section, nav, aside, header, footer, div {
  background-color: color-mix(in srgb, currentColor 0%, #0f1419 100%) !important;
}
p, span, li, td, th, label, input, textarea, select, button, a, h1, h2, h3, h4, h5, h6 {
  color: #f2f4f8 !important;
}
a { color: #78a9ff !important; }
img, video, canvas, picture, svg, iframe { filter: none !important; }
input, textarea, select, button { background: #202830 !important; border-color: #525c66 !important; }
"#;
    let css = js_string_literal(css);
    format!(
        "(function(){{var id='mde-servo-force-dark-style';var root=document.head||document.documentElement;if(!root)return;var el=document.getElementById(id);if(!el){{el=document.createElement('style');el.id=id;root.appendChild(el);}}document.documentElement.style.colorScheme='dark';el.textContent={css};}})();"
    )
}

fn reader_mode_script(enabled: bool) -> String {
    if !enabled {
        return "(function(){var id='mde-servo-reader-style';var el=document.getElementById(id);if(el)el.remove();document.documentElement.classList.remove('mde-reader-mode');})();".to_owned();
    }
    let css = r#"
html.mde-reader-mode body {
  max-width: 76ch !important;
  margin: 0 auto !important;
  padding: 3rem 2rem !important;
  line-height: 1.65 !important;
  font-size: 18px !important;
  font-family: Inter, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif !important;
}
html.mde-reader-mode article, html.mde-reader-mode main {
  max-width: 76ch !important;
  margin-left: auto !important;
  margin-right: auto !important;
}
html.mde-reader-mode nav, html.mde-reader-mode aside, html.mde-reader-mode footer,
html.mde-reader-mode [role="navigation"], html.mde-reader-mode [aria-label*="advert"],
html.mde-reader-mode iframe, html.mde-reader-mode embed {
  display: none !important;
}
html.mde-reader-mode p, html.mde-reader-mode li {
  margin-block: 0.85em !important;
}
html.mde-reader-mode img, html.mde-reader-mode video {
  max-width: 100% !important;
  height: auto !important;
}
"#;
    let css = js_string_literal(css);
    format!(
        "(function(){{var id='mde-servo-reader-style';var root=document.head||document.documentElement;if(!root)return;var el=document.getElementById(id);if(!el){{el=document.createElement('style');el.id=id;root.appendChild(el);}}el.textContent={css};document.documentElement.classList.add('mde-reader-mode');}})();"
    )
}

fn user_agent_override_script(user_agent: &str) -> String {
    if user_agent.trim().is_empty() {
        return "(function(){delete window.__mdeUserAgentOverride;})();".to_owned();
    }
    let ua = js_string_literal(&clamp_utf8(user_agent, 512));
    format!(
        "(function(){{var ua={ua};window.__mdeUserAgentOverride=ua;try{{Object.defineProperty(Navigator.prototype,'userAgent',{{get:function(){{return window.__mdeUserAgentOverride||ua;}},configurable:true}});Object.defineProperty(Navigator.prototype,'appVersion',{{get:function(){{return window.__mdeUserAgentOverride||ua;}},configurable:true}});Object.defineProperty(Navigator.prototype,'platform',{{get:function(){{return /Android|Mobile|iPhone|iPad/.test(window.__mdeUserAgentOverride||ua)?'Linux armv8l':'Linux x86_64';}},configurable:true}});}}catch(_e){{}}}})();"
    )
}

fn device_profile_script(
    profile: &str,
    width: u16,
    height: u16,
    scale_percent: u16,
    touch: bool,
) -> String {
    if profile == "default" || width == 0 || height == 0 {
        return "(function(){delete window.__mdeDeviceProfile;try{delete window.innerWidth;delete window.innerHeight;delete window.devicePixelRatio;}catch(_e){}var meta=document.getElementById('mde-device-profile-viewport');if(meta)meta.remove();delete document.documentElement.dataset.mdeDeviceProfile;})();".to_owned();
    }
    let profile = js_string_literal(&clamp_utf8(profile, 32));
    let width = width.clamp(240, 7680);
    let height = height.clamp(240, 7680);
    let scale = scale_percent.clamp(50, 600);
    let touch_points = if touch { 5 } else { 0 };
    format!(
        "(function(){{var p={{profile:{profile},width:{width},height:{height},dpr:{scale}/100,touch:{touch},touchPoints:{touch_points}}};window.__mdeDeviceProfile=p;document.documentElement.dataset.mdeDeviceProfile=p.profile;var meta=document.getElementById('mde-device-profile-viewport');if(!meta){{meta=document.createElement('meta');meta.id='mde-device-profile-viewport';meta.name='viewport';(document.head||document.documentElement).appendChild(meta);}}meta.content='width='+p.width+', initial-scale=1';function def(o,n,g){{try{{Object.defineProperty(o,n,{{get:g,configurable:true}});}}catch(_e){{}}}}def(window,'innerWidth',function(){{return p.width;}});def(window,'innerHeight',function(){{return p.height;}});def(window,'devicePixelRatio',function(){{return p.dpr;}});if(window.Screen&&Screen.prototype){{def(Screen.prototype,'width',function(){{return p.width;}});def(Screen.prototype,'height',function(){{return p.height;}});def(Screen.prototype,'availWidth',function(){{return p.width;}});def(Screen.prototype,'availHeight',function(){{return p.height;}});}}if(window.Navigator&&Navigator.prototype){{def(Navigator.prototype,'maxTouchPoints',function(){{return p.touchPoints;}});}}}})();"
    )
}

const fn print_page_script() -> &'static str {
    "(function(){if(window.print)window.print();})();"
}

fn page_text_script(max_bytes: u32) -> String {
    let max_bytes = max_bytes.clamp(1, 64 * 1024);
    format!(
        "(function(){{var cap={max_bytes};var root=document.body||document.documentElement;\
var text=root?String(root.innerText||root.textContent||''):'';\
text=text.replace(/\\s+/g,' ').trim();return text.length>cap?text.slice(0,cap):text;}})();"
    )
}

fn page_scrape_script(max_bytes: u32, max_links: u16, max_headings: u16) -> String {
    let max_bytes = max_bytes.clamp(1, 64 * 1024);
    let max_links = max_links.min(256);
    let max_headings = max_headings.min(128);
    format!(
        r#"(function(){{
var textCap={max_bytes},linkCap={max_links},headingCap={max_headings},articleCap=16384;
function trim(v,n){{v=String(v||'').replace(/\s+/g,' ').trim();return v.length>n?v.slice(0,n):v;}}
function visible(el){{try{{if(!el||!el.getClientRects||!el.getClientRects().length)return false;var s=getComputedStyle(el);return s.visibility!=='hidden'&&s.display!=='none';}}catch(_){{return true;}}}}
var root=document.body||document.documentElement;
var text=root?trim(root.innerText||root.textContent||'',textCap):'';
var articleNode=null,articleSelector='';
var candidates=document.querySelectorAll?document.querySelectorAll('article,main,[role=main]'):[];
for(var c=0;c<candidates.length;c++){{if(visible(candidates[c])){{articleNode=candidates[c];articleSelector=(articleNode.tagName||'').toLowerCase();if(articleNode.getAttribute&&articleNode.getAttribute('role'))articleSelector+='[role='+articleNode.getAttribute('role')+']';break;}}}}
var articleRaw=articleNode?String(articleNode.innerText||articleNode.textContent||''):'';
var articleText=trim(articleRaw,articleCap);
var links=[];
var anchors=document.querySelectorAll?document.querySelectorAll('a[href]'):[];
for(var i=0;i<anchors.length&&links.length<linkCap;i++){{
  var a=anchors[i];if(!visible(a))continue;
  var href=trim(a.href||a.getAttribute('href')||'',2048);if(!href)continue;
  links.push({{url:href,text:trim(a.innerText||a.textContent||a.getAttribute('aria-label')||'',160),rel:trim(a.getAttribute('rel')||'',80),target:trim(a.getAttribute('target')||'',40)}});
}}
var headings=[];
var hs=document.querySelectorAll?document.querySelectorAll('h1,h2,h3,h4,h5,h6'):[];
for(var h=0;h<hs.length&&headings.length<headingCap;h++){{
  var el=hs[h];if(!visible(el))continue;
  var label=trim(el.innerText||el.textContent||'',240);if(!label)continue;
  headings.push({{level:Number(String(el.tagName||'H0').slice(1))||0,text:label}});
}}
var canonicalEl=document.querySelector?document.querySelector('link[rel~="canonical"][href]'):null;
var descriptionEl=document.querySelector?document.querySelector('meta[name="description" i][content],meta[property="og:description"][content]'):null;
return JSON.stringify({{text:text,text_truncated:(root?String(root.innerText||root.textContent||'').replace(/\s+/g,' ').trim().length:0)>textCap,article_text:articleText,article_text_truncated:trim(articleRaw,2147483647).length>articleCap,article_selector:articleSelector,canonical_url:canonicalEl?trim(canonicalEl.href||canonicalEl.getAttribute('href')||'',2048):'',meta_description:descriptionEl?trim(descriptionEl.getAttribute('content')||'',512):'',document_lang:trim((document.documentElement&&document.documentElement.lang)||'',64),links:links,headings:headings}});
}})();"#
    )
}

fn spellcheck_highlight_script(words: &[String]) -> String {
    let words: Vec<String> = words
        .iter()
        .filter_map(|word| {
            let trimmed = word.trim();
            if trimmed.len() < 2 || trimmed.len() > 64 {
                None
            } else {
                Some(trimmed.to_owned())
            }
        })
        .take(64)
        .collect();
    let words = js_string_array_literal(&words);
    r#"(function(){
var cls='mde-browser-spell-miss';
var old=document.querySelectorAll('span.'+cls);
for(var i=old.length-1;i>=0;i--){var n=old[i];n.replaceWith(document.createTextNode(n.textContent||''));}
if(!document.body){return;}
document.body.normalize();
var words=__WORDS__;
if(!words.length){delete document.documentElement.dataset.mdeBrowserSpellcheck;return;}
var style=document.getElementById('mde-browser-spellcheck-style');
if(!style){style=document.createElement('style');style.id='mde-browser-spellcheck-style';(document.head||document.documentElement).appendChild(style);}
style.textContent='span.'+cls+'{text-decoration: underline wavy #d13438; text-decoration-thickness: 1.5px; text-underline-offset: 0.12em;}';
var escaped=words.map(function(w){return String(w).replace(/[.*+?^${}()|[\]\\]/g,'\\$&');}).filter(Boolean);
if(!escaped.length){return;}
var re=new RegExp('\\b('+escaped.join('|')+')\\b','gi');
var walker=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,{acceptNode:function(node){
  var p=node.parentElement;
  if(!p||p.closest('script,style,textarea,input,select,span.'+cls))return NodeFilter.FILTER_REJECT;
  re.lastIndex=0;
  return re.test(node.nodeValue||'')?NodeFilter.FILTER_ACCEPT:NodeFilter.FILTER_REJECT;
}});
var nodes=[];
while(nodes.length<256){var node=walker.nextNode();if(!node)break;nodes.push(node);}
for(var n=0;n<nodes.length;n++){
  var text=nodes[n].nodeValue||'';re.lastIndex=0;
  var frag=document.createDocumentFragment();var last=0;var m;
  while((m=re.exec(text))&&frag.childNodes.length<512){
    if(m.index>last)frag.appendChild(document.createTextNode(text.slice(last,m.index)));
    var span=document.createElement('span');span.className=cls;span.dataset.mdeBrowserSpellcheck='miss';span.textContent=m[0];frag.appendChild(span);
    last=m.index+m[0].length;
  }
  if(last<text.length)frag.appendChild(document.createTextNode(text.slice(last)));
  nodes[n].replaceWith(frag);
}
document.documentElement.dataset.mdeBrowserSpellcheck=String(words.length);
})();"#
        .replace("__WORDS__", &words)
}

fn spellcheck_correction_script(word: &str, replacement: &str) -> String {
    spellcheck_correction_script_with_target(word, replacement, Some(0))
}

fn spellcheck_correction_all_script(word: &str, replacement: &str) -> String {
    spellcheck_correction_script_with_target(word, replacement, None)
}

fn spellcheck_correction_at_script(word: &str, replacement: &str, occurrence: u16) -> String {
    spellcheck_correction_script_with_target(word, replacement, Some(occurrence))
}

fn spellcheck_correction_script_with_target(
    word: &str,
    replacement: &str,
    target_occurrence: Option<u16>,
) -> String {
    let word = word.trim();
    let replacement = replacement.trim();
    if word.is_empty() || replacement.is_empty() || word.len() > 64 || replacement.len() > 128 {
        return "()=>{}".to_owned();
    }
    let word = js_string_literal(word);
    let replacement = js_string_literal(replacement);
    let target_occurrence = target_occurrence.map_or(-1, i32::from);
    format!(
        r#"(function(){{
var word={word};
var replacement={replacement};
var targetOccurrence={target_occurrence};
var replaceAll=targetOccurrence<0;
var cls='mde-browser-spell-miss';
function same(value){{return String(value||'').toLocaleLowerCase()===word.toLocaleLowerCase();}}
var marks=document.querySelectorAll('span.'+cls);
var changed=0;var seen=0;var markMatches=0;
for(var i=0;i<marks.length;i++){{
  if(same(marks[i].textContent)){{
    markMatches++;
    if(!replaceAll&&seen!==targetOccurrence){{seen++;continue;}}
    marks[i].replaceWith(document.createTextNode(replacement));
    changed++;
    if(!replaceAll){{
      document.body&&document.body.normalize();
      return;
    }}
    seen++;
  }}
}}
if(markMatches>0&&!replaceAll)return;
if(changed>0){{
  document.body&&document.body.normalize();
  return;
}}
if(!document.body)return;
var escaped=word.replace(/[.*+?^${{}}()|[\]\\]/g,'\\$&');
var re=new RegExp('\\b'+escaped+'\\b','gi');
var walker=document.createTreeWalker(document.body,NodeFilter.SHOW_TEXT,{{acceptNode:function(node){{
  var p=node.parentElement;
  if(!p||p.closest('script,style,textarea,input,select'))return NodeFilter.FILTER_REJECT;
  re.lastIndex=0;
  return re.test(node.nodeValue||'')?NodeFilter.FILTER_ACCEPT:NodeFilter.FILTER_REJECT;
}}}});
var node;var total=0;
while((node=walker.nextNode())&&total<512){{
  var text=node.nodeValue||'';
  re.lastIndex=0;
  if(!replaceAll){{
    var m;
    while((m=re.exec(text))&&total<512){{
      if(total===targetOccurrence){{
        node.nodeValue=text.slice(0,m.index)+replacement+text.slice(m.index+m[0].length);
        return;
      }}
      total++;
    }}
    continue;
  }}
  var next=text.replace(re,function(m){{total++;return total<=512?replacement:m;}});
  if(next!==text)node.nodeValue=next;
}}
}})();"#
    )
}

fn clamp_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

const fn audio_mute_script(muted: bool) -> &'static str {
    if muted {
        "(function(){var key='mdeServoAudioMuted';var apply=function(root){var list=(root||document).querySelectorAll? (root||document).querySelectorAll('audio,video') : [];for(var i=0;i<list.length;i++){list[i].muted=true;list[i].defaultMuted=true;}};document.documentElement.dataset[key]='true';apply(document);if(window.__mdeServoAudioMuteObserver)window.__mdeServoAudioMuteObserver.disconnect();window.__mdeServoAudioMuteObserver=new MutationObserver(function(records){for(var r=0;r<records.length;r++){for(var n=0;n<records[r].addedNodes.length;n++){var node=records[r].addedNodes[n];if(node&&node.matches&&node.matches('audio,video')){node.muted=true;node.defaultMuted=true;}apply(node);}}});window.__mdeServoAudioMuteObserver.observe(document.documentElement,{childList:true,subtree:true});})();"
    } else {
        "(function(){var key='mdeServoAudioMuted';delete document.documentElement.dataset[key];if(window.__mdeServoAudioMuteObserver){window.__mdeServoAudioMuteObserver.disconnect();window.__mdeServoAudioMuteObserver=null;}var list=document.querySelectorAll?document.querySelectorAll('audio,video'):[];for(var i=0;i<list.length;i++){list[i].muted=false;list[i].defaultMuted=false;}})();"
    }
}

fn autoplay_block_script(blocked: bool) -> String {
    if !blocked {
        return r#"(function(){var s=window.__mdeServoAutoplayBlocker;if(s){try{if(s.observer)s.observer.disconnect();}catch(_e){}try{document.removeEventListener('pointerdown',s.allow,true);document.removeEventListener('keydown',s.allow,true);document.removeEventListener('touchstart',s.allow,true);document.removeEventListener('click',s.allow,true);}catch(_e){}try{if(s.originalPlay&&s.patchedPlay&&window.HTMLMediaElement&&HTMLMediaElement.prototype.play===s.patchedPlay){HTMLMediaElement.prototype.play=s.originalPlay;}}catch(_e){}}delete window.__mdeServoAutoplayBlocker;delete document.documentElement.dataset.mdeServoAutoplayBlocked;})();"#.to_owned();
    }
    r#"(function(){var root=document.documentElement;if(!root)return;root.dataset.mdeServoAutoplayBlocked='true';var s=window.__mdeServoAutoplayBlocker;if(s&&s.sweep){s.sweep(document);return;}s={allowed:false};window.__mdeServoAutoplayBlocker=s;s.allow=function(e){if(e&&e.isTrusted===false)return;s.allowed=true;};document.addEventListener('pointerdown',s.allow,true);document.addEventListener('keydown',s.allow,true);document.addEventListener('touchstart',s.allow,true);document.addEventListener('click',s.allow,true);s.blockedError=function(){try{return new DOMException('Autoplay blocked by MDE Browser','NotAllowedError');}catch(_e){var err=new Error('Autoplay blocked by MDE Browser');err.name='NotAllowedError';return err;}};s.sweep=function(scope){try{var base=scope&&scope.querySelectorAll?scope:document;var media=base.querySelectorAll('audio[autoplay],video[autoplay]');for(var i=0;i<media.length;i++){var el=media[i];if(s.allowed||el.dataset.mdeAutoplayAllowed==='true')continue;el.autoplay=false;el.removeAttribute('autoplay');try{el.pause();}catch(_e){}}}catch(_e){}};try{var proto=window.HTMLMediaElement&&HTMLMediaElement.prototype;if(proto&&proto.play&&!s.originalPlay){s.originalPlay=proto.play;s.patchedPlay=function(){if(s.allowed||this.dataset.mdeAutoplayAllowed==='true'||!document.documentElement.dataset.mdeServoAutoplayBlocked){return s.originalPlay.apply(this,arguments);}try{this.pause();}catch(_e){}return Promise.reject(s.blockedError());};try{Object.defineProperty(proto,'play',{value:s.patchedPlay,writable:true,configurable:true});}catch(_e){proto.play=s.patchedPlay;}}}catch(_e){}if(window.MutationObserver){s.observer=new MutationObserver(function(records){for(var i=0;i<records.length;i++){for(var j=0;j<records[i].addedNodes.length;j++){var n=records[i].addedNodes[j];if(n&&n.nodeType===1)s.sweep(n);}}});s.observer.observe(document.documentElement,{childList:true,subtree:true});}s.sweep(document);})();"#.to_owned()
}

fn userscript_library_script(enabled: bool, bundle: &str) -> String {
    if !enabled {
        return "(function(){var style=document.getElementById('mde-browser-userscript-style');if(style)style.remove();if(window.__mdeBrowserUserScriptsObserver){window.__mdeBrowserUserScriptsObserver.disconnect();window.__mdeBrowserUserScriptsObserver=null;}delete document.documentElement.dataset.mdeBrowserUserscripts;})();".to_owned();
    }
    format!(
        "(function(){{try{{document.documentElement.dataset.mdeBrowserUserscripts='true';\n{bundle}\n}}catch(err){{console.warn('mde userscript bundle failed',err);}}}})();"
    )
}

fn js_string_literal(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if ch <= '\u{1f}' => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn js_string_array_literal(values: &[String]) -> String {
    let mut out = String::from("[");
    for (idx, value) in values.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&js_string_literal(value));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::{
        audio_mute_script, autoplay_block_script, clamp_utf8, clear_find_script,
        device_profile_script, find_in_page_script, force_dark_script, page_scrape_script,
        page_text_script, page_zoom_script, passkey_bridge_drain_script, passkey_complete_script,
        print_page_script, reader_mode_script, secure_preferences,
        spellcheck_correction_all_script, spellcheck_correction_at_script,
        spellcheck_correction_script, spellcheck_highlight_script, user_agent_override_script,
        userscript_library_script, GENERIC_USER_AGENT,
    };

    #[test]
    fn secure_preferences_disable_cookie_storage_and_disk_cache() {
        let prefs = secure_preferences();
        assert_eq!(prefs.user_agent, GENERIC_USER_AGENT);
        assert!(
            !prefs.dom_cookiestore_enabled,
            "cookie store is disabled, so third-party cookies have no persistence surface"
        );
        assert!(
            !prefs.dom_indexeddb_enabled,
            "IndexedDB persistence is disabled"
        );
        assert!(
            !prefs.dom_storage_manager_api_enabled,
            "StorageManager persistence is disabled"
        );
        assert!(
            prefs.network_http_cache_disabled,
            "HTTP disk cache is disabled"
        );
        assert!(
            !prefs.dom_webrtc_enabled,
            "WebRTC local-IP leaks are disabled"
        );
        assert!(!prefs.dom_webgpu_enabled, "WebGPU is disabled");
    }

    #[test]
    fn servo_page_tool_scripts_are_bounded_and_escaped() {
        assert!(page_zoom_script(125).contains("zoom='125%'"));
        assert!(page_zoom_script(5).contains("zoom='25%'"));
        assert!(page_zoom_script(900).contains("zoom='500%'"));

        let forward = find_in_page_script("mesh \"ops\"", false);
        assert!(forward.contains(r#"window.find("mesh \"ops\"",false,false"#));
        let backward = find_in_page_script("mesh", true);
        assert!(backward.contains(r#"window.find("mesh",false,true"#));
        assert!(clear_find_script().contains("removeAllRanges"));
    }

    #[test]
    fn servo_passkey_bridge_intercepts_webauthn_without_credentials() {
        let script = passkey_bridge_drain_script();
        assert!(script.contains("navigator.credentials"));
        assert!(script.contains("creds.create=function"));
        assert!(script.contains("creds.get=function"));
        assert!(script.contains("challenge_b64url"));
        assert!(script.contains("allow_credentials"));
        assert!(script.contains("client_request_id"));
        assert!(script.contains("__mdeBrowserPasskeyComplete"));
        assert!(script.contains("pending.resolve"));
        assert!(script.contains("public_key_spki_der_b64url"));
        assert!(script.contains("getClientExtensionResults"));
        assert!(script.contains("isUserVerifyingPlatformAuthenticatorAvailable"));
        assert!(script.contains("isConditionalMediationAvailable"));
        assert!(script.contains("Object.setPrototypeOf"));
        assert!(script.contains("getTransports"));
        assert!(script.contains("toJSON"));

        let complete = passkey_complete_script(r#"{"client_request_id":"pk-1"}"#);
        assert!(complete.contains("__mdeBrowserPasskeyComplete"));
        assert!(complete.contains("JSON.parse"));
    }

    #[test]
    fn servo_force_dark_script_installs_and_clears_bounded_style() {
        let enable = force_dark_script(true);
        assert!(enable.contains("mde-servo-force-dark-style"));
        assert!(enable.contains("color-scheme: dark"));
        assert!(
            !enable.contains("</style>"),
            "force-dark is injected as style text only"
        );

        let disable = force_dark_script(false);
        assert!(disable.contains("remove()"));
        assert!(disable.contains("colorScheme=''"));
    }

    #[test]
    fn servo_reader_mode_script_installs_and_clears_bounded_style() {
        let enable = reader_mode_script(true);
        assert!(enable.contains("mde-servo-reader-style"));
        assert!(enable.contains("mde-reader-mode"));
        assert!(enable.contains("max-width: 76ch"));
        assert!(
            !enable.contains("<script"),
            "reader mode is injected as style text only"
        );

        let disable = reader_mode_script(false);
        assert!(disable.contains("if(el)el.remove()"));
        assert!(disable.contains("classList.remove"));
    }

    #[test]
    fn servo_user_agent_override_script_installs_and_clears_page_visible_ua() {
        let enable = user_agent_override_script("Mozilla/5.0 MDE-Test");
        assert!(enable.contains("Navigator.prototype"));
        assert!(enable.contains("userAgent"));
        assert!(enable.contains("MDE-Test"));
        assert!(
            !enable.contains("</script>"),
            "UA override is injected as bounded script text only"
        );

        let disable = user_agent_override_script("");
        assert!(disable.contains("__mdeUserAgentOverride"));
        assert!(disable.contains("delete"));
    }

    #[test]
    fn servo_device_profile_script_installs_and_clears_page_visible_device_metadata() {
        let enable = device_profile_script("phone", 390, 844, 300, true);
        assert!(enable.contains("mdeDeviceProfile"));
        assert!(enable.contains("innerWidth"));
        assert!(enable.contains("maxTouchPoints"));
        assert!(enable.contains("mde-device-profile-viewport"));
        assert!(enable.contains("width:390"));
        assert!(
            !enable.contains("</script>"),
            "device profile is injected as bounded script text only"
        );

        let disable = device_profile_script("default", 0, 0, 100, false);
        assert!(disable.contains("__mdeDeviceProfile"));
        assert!(disable.contains("delete"));
    }

    #[test]
    fn servo_audio_mute_script_mutes_existing_and_future_media() {
        let enable = audio_mute_script(true);
        assert!(enable.contains("querySelectorAll('audio,video')"));
        assert!(enable.contains("muted=true"));
        assert!(enable.contains("defaultMuted=true"));
        assert!(enable.contains("MutationObserver"));
        assert!(enable.contains("__mdeServoAudioMuteObserver.observe"));

        let disable = audio_mute_script(false);
        assert!(disable.contains("__mdeServoAudioMuteObserver.disconnect"));
        assert!(disable.contains("muted=false"));
        assert!(disable.contains("defaultMuted=false"));
        assert!(disable.contains("delete document.documentElement.dataset"));
    }

    #[test]
    fn servo_autoplay_block_script_patches_media_play_and_cleans_up() {
        let enable = autoplay_block_script(true);
        assert!(enable.contains("__mdeServoAutoplayBlocker"));
        assert!(enable.contains("mdeServoAutoplayBlocked"));
        assert!(enable.contains("HTMLMediaElement.prototype"));
        assert!(enable.contains("MutationObserver"));
        assert!(enable.contains("removeAttribute('autoplay')"));
        assert!(enable.contains("Promise.reject"));
        assert!(!enable.contains("</script>"));

        let disable = autoplay_block_script(false);
        assert!(disable.contains("observer.disconnect"));
        assert!(disable.contains("HTMLMediaElement.prototype.play=s.originalPlay"));
        assert!(disable.contains("delete window.__mdeServoAutoplayBlocker"));
        assert!(disable.contains("delete document.documentElement.dataset.mdeServoAutoplayBlocked"));
    }

    #[test]
    fn servo_page_text_script_is_bounded_and_utf8_clamp_is_safe() {
        let script = page_text_script(200_000);
        assert!(
            script.contains("cap=65536"),
            "page text extraction is capped before it reaches the shell"
        );
        assert!(script.contains("innerText||root.textContent"));
        assert!(script.contains("replace(/\\s+/g"));

        assert_eq!(clamp_utf8("hello", 64), "hello");
        assert_eq!(clamp_utf8("abé", 3), "ab");
    }

    #[test]
    fn servo_page_scrape_script_collects_bounded_dom_links_and_headings() {
        let script = page_scrape_script(200_000, 400, 300);
        assert!(script.contains("textCap=65536"));
        assert!(script.contains("articleCap=16384"));
        assert!(script.contains("linkCap=256"));
        assert!(script.contains("headingCap=128"));
        assert!(script.contains("querySelectorAll('a[href]')"));
        assert!(script.contains("querySelectorAll('h1,h2,h3,h4,h5,h6')"));
        assert!(script.contains("querySelectorAll('article,main,[role=main]')"));
        assert!(script.contains("link[rel~=\"canonical\"][href]"));
        assert!(script.contains("meta[name=\"description\" i][content]"));
        assert!(script.contains("JSON.stringify"));
    }

    #[test]
    fn servo_userscript_library_script_runs_and_cleans_the_bundle() {
        let enable = userscript_library_script(
            true,
            "document.documentElement.dataset.curatedUserscript='youtube';",
        );
        assert!(enable.contains("mdeBrowserUserscripts"));
        assert!(enable.contains("curatedUserscript='youtube'"));
        assert!(enable.contains("try{"));

        let disable = userscript_library_script(false, "");
        assert!(disable.contains("mde-browser-userscript-style"));
        assert!(disable.contains("__mdeBrowserUserScriptsObserver"));
        assert!(disable.contains("delete document.documentElement.dataset.mdeBrowserUserscripts"));
    }

    #[test]
    fn servo_spellcheck_highlight_script_marks_and_clears_words() {
        let script =
            spellcheck_highlight_script(&["wrold".to_owned(), "mesh?".to_owned(), "x".to_owned()]);
        assert!(script.contains("mde-browser-spell-miss"));
        assert!(script.contains("mde-browser-spellcheck-style"));
        assert!(script.contains("\"wrold\""));
        assert!(script.contains("\"mesh?\""));
        assert!(!script.contains("\"x\""));

        let clear = spellcheck_highlight_script(&[]);
        assert!(clear.contains("delete document.documentElement.dataset.mdeBrowserSpellcheck"));
    }

    #[test]
    fn servo_spellcheck_correction_script_replaces_mark_or_text() {
        let script = spellcheck_correction_script("wrold", "world");
        assert!(script.contains("mde-browser-spell-miss"));
        assert!(script.contains(r#"word="wrold""#));
        assert!(script.contains(r#"replacement="world""#));
        assert!(script.contains("replaceWith(document.createTextNode(replacement))"));
        assert!(script.contains("createTreeWalker"));
        assert!(script.contains("var targetOccurrence=0"));
        assert!(script.contains("var replaceAll=targetOccurrence<0"));

        let all = spellcheck_correction_all_script("wrold", "world");
        assert!(all.contains("var targetOccurrence=-1"));
        assert!(all.contains("while((node=walker.nextNode())&&total<512)"));
        assert!(all.contains("text.replace(re,function(m)"));

        let indexed = spellcheck_correction_at_script("wrold", "world", 3);
        assert!(indexed.contains("var targetOccurrence=3"));
        assert!(indexed.contains("if(total===targetOccurrence)"));
        assert!(indexed.contains("if(markMatches>0&&!replaceAll)return"));

        assert_eq!(spellcheck_correction_script("", "world"), "()=>{}");
        assert_eq!(spellcheck_correction_script("wrold", ""), "()=>{}");
        assert_eq!(spellcheck_correction_all_script("", "world"), "()=>{}");
        assert_eq!(spellcheck_correction_at_script("", "world", 1), "()=>{}");
    }

    #[test]
    fn servo_print_script_uses_the_page_print_hook() {
        assert_eq!(
            print_page_script(),
            "(function(){if(window.print)window.print();})();"
        );
    }
}
