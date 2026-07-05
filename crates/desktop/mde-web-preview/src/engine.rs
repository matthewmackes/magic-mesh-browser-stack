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
}

/// One embedded, sandboxed Servo tab rendering offscreen.
pub struct Engine {
    servo: Servo,
    webview: WebView,
    rendering_context: Rc<SoftwareRenderingContext>,
    shared: Rc<Shared>,
    width: u32,
    height: u32,
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
        })
    }

    /// Spin the engine until a frame is painted, publish it to `channel`, and
    /// return. Bounded by `timeout`.
    ///
    /// # Errors
    /// Fails if no frame is produced before `timeout`, or the read-back /
    /// publish fails.
    pub fn pump_until_frame(&self, channel: &FrameChannel, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            self.servo.spin_event_loop();

            if self.shared.frame_ready.replace(false) {
                self.webview.paint();
                // Read back BEFORE present(): read_to_image reads the context's
                // BOUND (back) buffer, and present() swaps in an unpreserved one
                // (PreserveBuffer::No) — a post-present read returns the empty /
                // one-frame-stale buffer, never the frame paint() just rendered
                // (the live "browser renders all-black" bug, 2026-07-05).
                let pixels = self.read_back()?;
                self.rendering_context.present();
                if let Some(pixels) = pixels {
                    channel
                        .emit(self.width, self.height, PixelFormat::Rgba8, &pixels)
                        .context("publish frame to shm")?;
                    return Ok(());
                }
            }

            if Instant::now() >= deadline {
                anyhow::bail!("timed out after {timeout:?} waiting for the first frame");
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
            self.webview.paint();
            // Read back BEFORE present() — see `pump_until_frame`.
            let pixels = self.read_back()?;
            self.rendering_context.present();
            if let Some(pixels) = pixels {
                channel
                    .emit(self.width, self.height, PixelFormat::Rgba8, &pixels)
                    .context("publish frame to shm")?;
                return Ok(true);
            }
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
        self.webview.paint();
        // Read back BEFORE present() — see `pump_until_frame`.
        let pixels = self.read_back()?;
        self.rendering_context.present();
        if let Some(pixels) = pixels {
            channel
                .emit(self.width, self.height, PixelFormat::Rgba8, &pixels)
                .context("publish forced frame to shm")?;
            return Ok(true);
        }
        Ok(false)
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

    /// Whether the initial load has reported completion.
    #[must_use]
    pub fn load_complete(&self) -> bool {
        self.shared.load_complete.get()
    }
}
