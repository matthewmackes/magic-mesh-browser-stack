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

    /// Apply or remove tab audio mute in the Servo tab. Servo does not expose the
    /// CEF-style browser-host mute slot, so this uses the page seam to mute every
    /// HTML media element already present and any media element inserted later.
    pub fn set_audio_muted(&self, muted: bool) {
        self.evaluate_page_script(audio_mute_script(muted));
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

const fn print_page_script() -> &'static str {
    "(function(){if(window.print)window.print();})();"
}

const fn audio_mute_script(muted: bool) -> &'static str {
    if muted {
        "(function(){var key='mdeServoAudioMuted';var apply=function(root){var list=(root||document).querySelectorAll? (root||document).querySelectorAll('audio,video') : [];for(var i=0;i<list.length;i++){list[i].muted=true;list[i].defaultMuted=true;}};document.documentElement.dataset[key]='true';apply(document);if(window.__mdeServoAudioMuteObserver)window.__mdeServoAudioMuteObserver.disconnect();window.__mdeServoAudioMuteObserver=new MutationObserver(function(records){for(var r=0;r<records.length;r++){for(var n=0;n<records[r].addedNodes.length;n++){var node=records[r].addedNodes[n];if(node&&node.matches&&node.matches('audio,video')){node.muted=true;node.defaultMuted=true;}apply(node);}}});window.__mdeServoAudioMuteObserver.observe(document.documentElement,{childList:true,subtree:true});})();"
    } else {
        "(function(){var key='mdeServoAudioMuted';delete document.documentElement.dataset[key];if(window.__mdeServoAudioMuteObserver){window.__mdeServoAudioMuteObserver.disconnect();window.__mdeServoAudioMuteObserver=null;}var list=document.querySelectorAll?document.querySelectorAll('audio,video'):[];for(var i=0;i<list.length;i++){list[i].muted=false;list[i].defaultMuted=false;}})();"
    }
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

#[cfg(test)]
mod tests {
    use super::{
        audio_mute_script, clear_find_script, find_in_page_script, force_dark_script,
        page_zoom_script, print_page_script, reader_mode_script, secure_preferences,
        GENERIC_USER_AGENT,
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
    fn servo_print_script_uses_the_page_print_hook() {
        assert_eq!(
            print_page_script(),
            "(function(){if(window.print)window.print();})();"
        );
    }
}
