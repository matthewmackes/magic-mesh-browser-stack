//! [`WebSession`] — one sandboxed-browser session the shell drives and displays.
//!
//! It owns the per-session socket (+ the helper child in a live build), the
//! read-only mapping of the helper's shm frame region, and the decoded-but-not-yet
//! uploaded frame. The shell:
//!
//! 1. [`poll`](WebSession::poll)s each frame — drains helper events without
//!    blocking, mapping the frame fd on `AttachFrame`, decoding a **new** frame on
//!    `PaintReady` (only when the sequence advanced — frames are not streamed), and
//!    tracking title / nav-state; a socket EOF or a dead child becomes a typed
//!    [`SessionState::Crashed`].
//! 2. [`take_frame`](WebSession::take_frame)s the pending [`egui::ColorImage`] on a
//!    paint-ready and uploads it to its `TextureHandle` (the panel does this only
//!    when a frame is actually present — never a per-frame re-upload).
//! 3. Forwards input with [`send_input`](WebSession::send_input) (pointer
//!    positions pre-mapped by the shell into frame device pixels; `pixels_per_point`
//!    scales only wheel scroll) and drives navigation.
//!
//! Sessions are independent: one session's crash never touches another's socket,
//! mapping, or state (per-tab isolation).

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::time::{Duration, Instant};

use crate::egui::{self, ColorImage};
use crate::filter::{self, RequestFilter};
use crate::frame::FrameReader;
use crate::scm::{self, RecvOutcome};
use crate::wire::{ControlMsg, CursorKind, EventMsg, MediaTransportAction};
use crate::{input, wire};

/// How many `recvmsg` batches one [`WebSession::poll`] drains before yielding
/// (a bound so a flooding helper can't spin the UI thread).
const MAX_RECV_PER_POLL: usize = 64;
const MAX_RECENT_RESOURCE_REQUESTS: usize = 128;
const MAX_PENDING_JS_DIALOGS: usize = 16;
const MAX_PENDING_BEFORE_UNLOADS: usize = 16;
/// A generous cap on queued, not-yet-answered permission prompts. Real pages raise
/// a handful at most; the bound stops a hostile page from growing the queue (and the
/// engine's held-callback set) without limit. An overflow auto-denies the oldest.
const MAX_PENDING_PERMISSIONS: usize = 16;
const HELPER_GRACEFUL_SHUTDOWN: Duration = Duration::from_millis(250);
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// A session's live status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Spawned/connected; no frame painted yet.
    Loading,
    /// At least one frame has been received — the page is displaying.
    Live,
    /// The helper died or the protocol broke; `reason` is a short human string.
    Crashed {
        /// Why the session crashed.
        reason: String,
    },
}

/// The navigation state the chrome renders (back/forward/reload + address bar).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NavState {
    /// The committed URL.
    pub url: String,
    /// A back-history entry exists.
    pub can_back: bool,
    /// A forward-history entry exists.
    pub can_forward: bool,
    /// A load is in progress.
    pub loading: bool,
}

/// Result of a helper save-as-PDF request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfSaveStatus {
    /// The requested output path.
    pub path: String,
    /// Whether the helper reported success.
    pub ok: bool,
}

/// One page-text extraction result from the helper.
#[derive(Clone, PartialEq, Eq)]
pub struct PageTextStatus {
    /// Request id originally supplied by the shell.
    pub id: u64,
    /// Bounded visible text extracted by the helper.
    pub text: String,
}

impl std::fmt::Debug for PageTextStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageTextStatus")
            .field("id", &self.id)
            .field("text", &"<redacted>")
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

/// One structured active-page scrape result from the helper.
#[derive(Clone, PartialEq, Eq)]
pub struct PageScrapeStatus {
    /// Request id originally supplied by the shell.
    pub id: u64,
    /// Bounded helper JSON body with visible text plus DOM links/headings.
    pub body: String,
}

impl std::fmt::Debug for PageScrapeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageScrapeStatus")
            .field("id", &self.id)
            .field("body", &"<redacted>")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// One helper-observed passkey/WebAuthn ceremony request.
#[derive(Clone, PartialEq, Eq)]
pub struct PasskeyRequestStatus {
    /// Bounded helper JSON body with ceremony metadata and user/credential hints.
    pub body: String,
}

impl std::fmt::Debug for PasskeyRequestStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasskeyRequestStatus")
            .field("body", &"<redacted>")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// One page/media-session now-playing metadata update from the helper.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaMetadataStatus {
    /// Bounded helper JSON body with page-provided now-playing metadata.
    pub body: String,
}

impl std::fmt::Debug for MediaMetadataStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaMetadataStatus")
            .field("body", &"<redacted>")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A page-initiated request to open a new window/tab (window.open, target=_blank).
/// The helper cancels the native popup; the shell opens the URL as a regular tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PopupRequestStatus {
    /// The popup's target URL.
    pub url: String,
}

/// A browser-initiated download's latest state (B2). Re-reported on progress and
/// completion, keyed by `id`; the shell folds these into its downloads drawer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadStatus {
    /// Helper-minted download id (stable across the download's lifetime).
    pub id: u64,
    /// Source URL.
    pub url: String,
    /// Chosen/suggested file name (basename).
    pub filename: String,
    /// Bytes received so far.
    pub received: u64,
    /// Total bytes expected (0 if unknown).
    pub total: u64,
    /// Finished writing successfully.
    pub done: bool,
    /// Canceled or interrupted.
    pub canceled: bool,
}

/// A TLS/certificate error that blocked the top-level load. The engine cancelled
/// the navigation (blocking-by-default); the shell paints a "Not secure —
/// blocked" interstitial from this state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertError {
    /// The URL whose certificate failed validation.
    pub url: String,
    /// The Chromium `net::Error` code (`cef_errorcode_t`, e.g. -202
    /// CERT_AUTHORITY_INVALID).
    pub code: i32,
    /// A short human-readable description of the failure.
    pub message: String,
}

/// A JavaScript dialog (`alert`/`confirm`/`prompt`) a page raised. The engine
/// auto-resolves it synchronously (alert accepted, confirm/prompt cancelled) so
/// the page never blocks; the shell may surface this as a passive notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsDialog {
    /// `0` = alert, `1` = confirm, `2` = prompt.
    pub kind: u8,
    /// The dialog's message text.
    pub message: String,
    /// The origin URL that raised the dialog.
    pub origin: String,
}

/// A page `beforeunload` prompt waiting for the user's leave/stay decision. The
/// engine holds the CEF JS dialog callback open and awaits
/// [`crate::ControlMsg::BeforeUnloadDecision`] carrying the same `id`.
#[derive(Clone, PartialEq, Eq)]
pub struct BeforeUnloadDialog {
    /// Correlates the prompt with its decision.
    pub id: u64,
    /// Page-provided prompt text (possibly empty on modern browsers).
    pub message: String,
    /// The top-level page URL/origin available to the engine.
    pub origin: String,
    /// Whether the unload is caused by reload rather than leaving/closing.
    pub is_reload: bool,
}

impl std::fmt::Debug for BeforeUnloadDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BeforeUnloadDialog")
            .field("id", &self.id)
            .field("message", &"<redacted>")
            .field("message_bytes", &self.message.len())
            .field("origin", &self.origin)
            .field("is_reload", &self.is_reload)
            .finish()
    }
}

/// A page's pending request for a powerful capability (geolocation / notifications
/// / clipboard / camera / microphone). The engine holds the CEF permission
/// callback open and awaits the shell's [`crate::ControlMsg::PermissionDecision`]
/// carrying the same `id`; the shell prompts the user, then calls
/// [`WebSession::answer_permission`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Correlates the request with its decision.
    pub id: u64,
    /// Engine-neutral kind: `0` geolocation, `1` notifications, `2` clipboard, `3`
    /// camera, `4` microphone, `5` camera + microphone.
    pub kind: u8,
    /// The requesting page's origin (scheme + host).
    pub origin: String,
}

/// A submitted login reported by the engine after top-level origin binding.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginCaptureStatus {
    /// Engine-derived origin that owns the submitted login.
    pub origin: String,
    /// Bounded JSON body carrying username/password fields.
    pub body: String,
}

impl std::fmt::Debug for LoginCaptureStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginCaptureStatus")
            .field("origin", &self.origin)
            .field("body", &"<redacted>")
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// One subresource request observed by the shell-side request filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRequestStatus {
    /// Shell-local monotonically increasing observation sequence for this session.
    /// This is not part of the helper wire protocol; it lets the shell audit only
    /// newly observed requests from the bounded recent-resource window.
    pub seq: u64,
    /// Requested subresource URL.
    pub url: String,
    /// Compact resource-type discriminant from [`crate::resource_to_wire`].
    pub resource: u8,
    /// Whether the shell allowed the request to continue.
    pub allowed: bool,
    /// The filter/policy that blocked the request, when [`Self::allowed`] is false.
    pub blocked_by: Option<String>,
}

/// One driven browser session.
pub struct WebSession {
    stream: UnixStream,
    /// The helper child (live spawn); `None` for a test/fake-helper session. Read
    /// each poll so a crashed process surfaces even without a socket EOF.
    child: Option<Child>,
    reader: Option<FrameReader>,
    rbuf: Vec<u8>,
    fd_queue: VecDeque<OwnedFd>,
    state: SessionState,
    nav: NavState,
    title: String,
    cursor: CursorKind,
    /// The page is in HTML5 fullscreen (the shell hides its chrome while true).
    fullscreen: bool,
    /// The page is currently producing audio (Chromium's audible state). Drives the
    /// tab-strip 🔊 indicator. Independent of mute — a muted-but-playing tab stays true.
    audible: bool,
    /// Latest bounded page/media-session now-playing metadata observed by the helper.
    media_metadata: Option<MediaMetadataStatus>,
    /// The latest engine-fetched favicon (PNG bytes), if the page reported one.
    /// The shell uploads it as the tab-strip icon.
    favicon: Option<Vec<u8>>,
    /// Latest find-in-page tally `(active_ordinal, total_count)`.
    find_result: Option<(u32, u32)>,
    /// The latest TLS/certificate error that blocked a load (if any). Cleared on
    /// a fresh navigation; drives the shell's "Not secure — blocked" interstitial.
    cert_error: Option<CertError>,
    /// A top-level navigation blocked by the mesh safe-browsing list — the URL drives
    /// a full-page "unsafe site" interstitial (mirrors [`Self::cert_error`]).
    safe_browsing_block: Option<String>,
    /// A top-level navigation blocked by operator-managed Browser policy.
    /// The shell paints a managed-policy interstitial from this state.
    managed_policy_block: Option<String>,
    /// The latest JS dialog (alert/confirm/prompt) the page raised. The engine
    /// already auto-resolved it; this is a passive notice for the shell.
    pending_js_dialog: Option<JsDialog>,
    /// Drainable JS dialog notices. The latest accessor above keeps compatibility
    /// with existing callers; this queue lets the shell surface each event once.
    js_dialog_events: VecDeque<JsDialog>,
    /// Page beforeunload prompts awaiting a leave/stay decision, oldest first.
    /// Bounded so a hostile page cannot grow the queue or CEF callback set without
    /// limit; overflow answers the oldest with `proceed=false` (stay/cancel).
    pending_before_unloads: VecDeque<BeforeUnloadDialog>,
    /// Page permission requests awaiting the user's allow/block, oldest first (the
    /// shell prompts for the FRONT). A queue, not a single slot: a page can raise
    /// several prompts in quick succession (e.g. geolocation + notifications), and
    /// the engine holds a distinct CEF callback open for EACH — dropping one would
    /// orphan its callback. Bounded ([`MAX_PENDING_PERMISSIONS`]); an overflow
    /// auto-denies the oldest so its callback still resolves.
    pending_permissions: VecDeque<PermissionRequest>,
    last_seq: u64,
    pending: Option<ColorImage>,
    pdf_events: VecDeque<PdfSaveStatus>,
    page_text_events: VecDeque<PageTextStatus>,
    page_scrape_events: VecDeque<PageScrapeStatus>,
    passkey_events: VecDeque<PasskeyRequestStatus>,
    /// Bounded JSON bodies from submitted login forms (auto-capture); the shell
    /// drains these and offers to save the credential (session-only).
    login_captures: VecDeque<LoginCaptureStatus>,
    download_events: VecDeque<DownloadStatus>,
    popup_requests: VecDeque<PopupRequestStatus>,
    /// Shell-local sequence assigned to observed resource requests.
    resource_seq: u64,
    recent_resource_requests: VecDeque<ResourceRequestStatus>,
    /// BOOKMARKS-7 — the ad-filter engine judging each helper subresource query +
    /// the per-page blocked count. Defaults to a blocks-nothing filter; the shell
    /// injects a compiled one from the mackesd `adfilter` blob via [`Self::set_filter`].
    filter: RequestFilter,
}

impl WebSession {
    /// Build a session over an already-connected session socket (+ optional helper
    /// child). The socket is switched to non-blocking so [`Self::poll`] never
    /// stalls the UI thread. This is the seam both the live spawn and the test
    /// fake-helper build on.
    ///
    /// # Errors
    /// Fails if the socket cannot be set non-blocking.
    pub fn from_stream(stream: UnixStream, child: Option<Child>) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            child,
            reader: None,
            rbuf: Vec::new(),
            fd_queue: VecDeque::new(),
            state: SessionState::Loading,
            nav: NavState::default(),
            title: String::new(),
            cursor: CursorKind::default(),
            fullscreen: false,
            audible: false,
            media_metadata: None,
            favicon: None,
            find_result: None,
            cert_error: None,
            safe_browsing_block: None,
            managed_policy_block: None,
            pending_js_dialog: None,
            js_dialog_events: VecDeque::new(),
            pending_before_unloads: VecDeque::new(),
            pending_permissions: VecDeque::new(),
            last_seq: 0,
            pending: None,
            pdf_events: VecDeque::new(),
            page_text_events: VecDeque::new(),
            page_scrape_events: VecDeque::new(),
            passkey_events: VecDeque::new(),
            login_captures: VecDeque::new(),
            download_events: VecDeque::new(),
            popup_requests: VecDeque::new(),
            resource_seq: 0,
            recent_resource_requests: VecDeque::new(),
            filter: RequestFilter::empty(),
        })
    }

    /// Attach a compiled ad-filter engine (builder form) — the shell compiles it
    /// from the mackesd `adfilter` worker's replicated blob (BOOKMARKS-7).
    #[must_use]
    pub fn with_filter(mut self, filter: RequestFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Swap in a compiled ad-filter engine (e.g. after a fresh `adfilter` blob
    /// syncs). Leaves the per-page counter of the new filter at its own state.
    pub fn set_filter(&mut self, filter: RequestFilter) {
        self.filter = filter;
    }

    /// Requests blocked by the ad-filter on the active page — the Browser
    /// surface's "N blocked" indicator (BOOKMARKS-7).
    #[must_use]
    pub const fn blocked_count(&self) -> u32 {
        self.filter.blocked_count()
    }

    /// The per-page ad-filter block breakdown (by domain / by filter) behind the
    /// "N blocked" shield's detail hover.
    #[must_use]
    pub const fn block_tally(&self) -> &mde_adblock::BlockTally {
        self.filter.tally()
    }

    /// Drain pending helper events without blocking. Maps the frame fd on
    /// `AttachFrame`, decodes a fresh frame on a new `PaintReady`, and folds
    /// title/nav-state; a dead helper or broken stream becomes
    /// [`SessionState::Crashed`].
    pub fn poll(&mut self) {
        if self.is_crashed() {
            return;
        }
        if let Some(child) = self.child.as_mut() {
            if let Ok(Some(status)) = child.try_wait() {
                self.mark_crashed(format!("helper process exited ({status})"));
                return;
            }
        }
        for _ in 0..MAX_RECV_PER_POLL {
            match scm::recv(&self.stream) {
                Ok(RecvOutcome::Data { bytes, fds }) => {
                    self.rbuf.extend_from_slice(&bytes);
                    self.fd_queue.extend(fds);
                    if let Err(reason) = self.drain_frames() {
                        self.mark_crashed(reason);
                        return;
                    }
                    if self.is_crashed() {
                        return;
                    }
                }
                Ok(RecvOutcome::WouldBlock) => break,
                Ok(RecvOutcome::Eof) => {
                    self.mark_crashed("helper closed the session socket".to_owned());
                    return;
                }
                Err(e) => {
                    self.mark_crashed(format!("session socket error: {e}"));
                    return;
                }
            }
        }
        // Layer-A shm fallback (belt + self-heal): promote a fresh frame straight
        // from the mapped region's seqlock sequence, independent of a `PaintReady`
        // signal. This decouples going Live from paint-ready timing and self-heals
        // the ordering hazard where a `PaintReady` arrived BEFORE `AttachFrame`
        // mapped the reader (so its upload was skipped). Without it, a helper whose
        // paint-ready is early, dropped, or slow would leave the surface stuck on
        // "Loading the page…" forever even though a real frame is already published.
        self.promote_from_shm_sequence();
    }

    /// If the mapped reader's published sequence advanced past what we last
    /// uploaded, take that frame — the shm-sequence path to Live that does not
    /// depend on a `PaintReady`. A no-op before the fd is attached, before the
    /// first published frame, or when the last `PaintReady` already uploaded it.
    fn promote_from_shm_sequence(&mut self) {
        let Some(reader) = self.reader.as_ref() else {
            return;
        };
        let seq = reader.sequence();
        // Zero = nothing published yet; odd = writer mid-frame; equal = already
        // uploaded (by a `PaintReady` or an earlier fallback).
        if seq == 0 || seq % 2 != 0 || seq == self.last_seq {
            return;
        }
        if let Some(snap) = reader.snapshot() {
            self.pending = Some(snap.to_color_image());
            self.last_seq = seq;
            self.state = SessionState::Live;
        }
    }

    /// Parse and dispatch every complete frame buffered so far.
    fn drain_frames(&mut self) -> Result<(), String> {
        loop {
            let payload = match wire::take_frame(&mut self.rbuf) {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(()),
                Err(e) => return Err(format!("bad wire framing: {e}")),
            };
            let msg = EventMsg::decode(&payload).map_err(|e| format!("bad helper event: {e}"))?;
            self.handle_event(msg)?;
            if self.is_crashed() {
                return Ok(());
            }
        }
    }

    fn handle_event(&mut self, msg: EventMsg) -> Result<(), String> {
        match msg {
            EventMsg::AttachFrame => {
                let fd = self
                    .fd_queue
                    .pop_front()
                    .ok_or_else(|| "AttachFrame carried no descriptor".to_owned())?;
                let reader = FrameReader::map(fd).map_err(|e| format!("map frame region: {e}"))?;
                self.reader = Some(reader);
            }
            EventMsg::PaintReady { seq } => {
                // Upload path fires ONLY here, and only when the sequence actually
                // advanced — an idle page re-signalling the same frame is a no-op.
                if seq != self.last_seq {
                    if let Some(reader) = self.reader.as_ref() {
                        if let Some(snap) = reader.snapshot() {
                            self.pending = Some(snap.to_color_image());
                            self.last_seq = seq;
                            self.state = SessionState::Live;
                        }
                    }
                }
            }
            EventMsg::Title(t) => self.title = t,
            EventMsg::NavState {
                can_back,
                can_forward,
                loading,
                url,
            } => {
                // A committed navigation to a new page host: re-anchor the ad-filter
                // first-party (resetting the per-page block count) and push the fresh
                // cosmetic user-stylesheet (BOOKMARKS-7). Same-page nav-state churn
                // (loading true→false) leaves both untouched.
                if self.filter.set_page(&url) {
                    self.recent_resource_requests.clear();
                    let css = self.filter.cosmetic_stylesheet();
                    if !css.is_empty() {
                        self.send(&ControlMsg::CosmeticFilters(css));
                    }
                }
                self.nav = NavState {
                    url,
                    can_back,
                    can_forward,
                    loading,
                };
            }
            EventMsg::ResourceRequest { id, url, resource } => {
                // The helper asks whether to fetch a subresource — judge it against
                // the ad-filter engine and answer BEFORE it hits the network.
                let resource_type = filter::resource_from_wire(resource);
                let decision = self.filter.decide(&url, resource_type);
                let allowed = !decision.is_block();
                let blocked_by = decision.blocked_by().map(str::to_owned);
                // A blocked TOP-LEVEL document drives a full-page interstitial
                // instead of silently leaving the old page frame visible. The
                // block itself still drops the request before the network.
                if !allowed && matches!(resource_type, mde_adblock::ResourceType::Document) {
                    if blocked_by
                        .as_deref()
                        .is_some_and(|filter| filter.starts_with("safe-browsing"))
                    {
                        self.safe_browsing_block = Some(url.clone());
                    } else if blocked_by
                        .as_deref()
                        .is_some_and(|filter| filter.starts_with("managed-policy"))
                    {
                        self.managed_policy_block = Some(url.clone());
                    }
                }
                self.resource_seq = self.resource_seq.saturating_add(1);
                let seq = self.resource_seq;
                if self.recent_resource_requests.len() >= MAX_RECENT_RESOURCE_REQUESTS {
                    self.recent_resource_requests.pop_front();
                }
                self.recent_resource_requests
                    .push_back(ResourceRequestStatus {
                        seq,
                        url,
                        resource,
                        allowed,
                        blocked_by,
                    });
                self.send(&ControlMsg::ResourceVerdict { id, allow: allowed });
            }
            EventMsg::PdfSaved { path, ok } => {
                self.pdf_events.push_back(PdfSaveStatus { path, ok })
            }
            EventMsg::PageText { id, text } => {
                self.page_text_events.push_back(PageTextStatus { id, text });
            }
            EventMsg::PageScrape { id, body } => {
                self.page_scrape_events
                    .push_back(PageScrapeStatus { id, body });
            }
            EventMsg::PasskeyRequest { body } => {
                self.passkey_events.push_back(PasskeyRequestStatus { body });
            }
            EventMsg::LoginSubmitted { origin, body } => {
                const MAX_PENDING_LOGIN_CAPTURES: usize = 16;
                if self.login_captures.len() >= MAX_PENDING_LOGIN_CAPTURES {
                    self.login_captures.pop_front();
                }
                self.login_captures
                    .push_back(LoginCaptureStatus { origin, body });
            }
            EventMsg::Download {
                id,
                url,
                filename,
                received,
                total,
                done,
                canceled,
            } => {
                self.download_events.push_back(DownloadStatus {
                    id,
                    url,
                    filename,
                    received,
                    total,
                    done,
                    canceled,
                });
            }
            EventMsg::PopupRequested { url } => {
                self.popup_requests.push_back(PopupRequestStatus { url });
            }
            EventMsg::CursorChanged { kind } => self.cursor = kind,
            EventMsg::Fullscreen { enabled } => self.fullscreen = enabled,
            EventMsg::AudioState { audible } => self.audible = audible,
            EventMsg::MediaMetadata { body } => {
                let body = body.trim().to_owned();
                self.media_metadata = (!body.is_empty()).then_some(MediaMetadataStatus { body });
            }
            EventMsg::PermissionRequest { id, kind, origin } => {
                if self.pending_permissions.len() >= MAX_PENDING_PERMISSIONS {
                    // Overflow: deny the oldest so the engine releases its held CEF
                    // callback (never silently drop it), then enqueue the newcomer.
                    if let Some(dropped) = self.pending_permissions.pop_front() {
                        self.send(&ControlMsg::PermissionDecision {
                            id: dropped.id,
                            allow: false,
                        });
                    }
                }
                self.pending_permissions
                    .push_back(PermissionRequest { id, kind, origin });
            }
            EventMsg::Favicon { png } => self.favicon = Some(png),
            EventMsg::CertError { url, code, message } => {
                self.cert_error = Some(CertError { url, code, message });
            }
            EventMsg::JsDialog {
                kind,
                message,
                origin,
            } => {
                let dialog = JsDialog {
                    kind,
                    message,
                    origin,
                };
                self.pending_js_dialog = Some(dialog.clone());
                if self.js_dialog_events.len() >= MAX_PENDING_JS_DIALOGS {
                    self.js_dialog_events.pop_front();
                }
                self.js_dialog_events.push_back(dialog);
            }
            EventMsg::BeforeUnloadDialog {
                id,
                message,
                origin,
                is_reload,
            } => {
                if self.pending_before_unloads.len() >= MAX_PENDING_BEFORE_UNLOADS {
                    if let Some(dropped) = self.pending_before_unloads.pop_front() {
                        self.send(&ControlMsg::BeforeUnloadDecision {
                            id: dropped.id,
                            proceed: false,
                        });
                    }
                }
                self.pending_before_unloads.push_back(BeforeUnloadDialog {
                    id,
                    message,
                    origin,
                    is_reload,
                });
            }
            EventMsg::FindResult { count, active, .. } => {
                self.find_result = Some((active, count));
            }
            EventMsg::Crashed { reason } => self.mark_crashed(reason),
        }
        Ok(())
    }

    fn mark_crashed(&mut self, reason: String) {
        self.state = SessionState::Crashed { reason };
    }

    /// Take the frame decoded on the last paint-ready, if any. The panel uploads
    /// it to its texture; returns `None` when no fresh frame is pending (so the
    /// texture is not re-uploaded every frame).
    pub const fn take_frame(&mut self) -> Option<ColorImage> {
        self.pending.take()
    }

    /// Drain save-as-PDF completion events reported by the helper.
    pub fn drain_pdf_events(&mut self) -> Vec<PdfSaveStatus> {
        self.pdf_events.drain(..).collect()
    }

    /// Drain visible page-text extraction results reported by the helper.
    pub fn drain_page_text_events(&mut self) -> Vec<PageTextStatus> {
        self.page_text_events.drain(..).collect()
    }

    /// Drain structured active-page scrape results reported by the helper.
    pub fn drain_page_scrape_events(&mut self) -> Vec<PageScrapeStatus> {
        self.page_scrape_events.drain(..).collect()
    }

    /// Drain passkey/WebAuthn ceremony requests reported by the helper.
    pub fn drain_passkey_events(&mut self) -> Vec<PasskeyRequestStatus> {
        self.passkey_events.drain(..).collect()
    }

    /// Drain submitted-login JSON bodies (auto-capture) reported by the helper. The
    /// shell parses each and offers to save the credential (session-only).
    pub fn drain_login_captures(&mut self) -> Vec<LoginCaptureStatus> {
        self.login_captures.drain(..).collect()
    }

    /// Drain browser download progress/completion events (B2) — the shell folds
    /// each into its downloads drawer, keyed by [`DownloadStatus::id`].
    pub fn drain_download_events(&mut self) -> Vec<DownloadStatus> {
        self.download_events.drain(..).collect()
    }

    /// Drain page-initiated popup requests (window.open / target=_blank) — the
    /// shell opens each URL as a regular new tab.
    pub fn drain_popup_requests(&mut self) -> Vec<PopupRequestStatus> {
        self.popup_requests.drain(..).collect()
    }

    /// Recent subresource requests observed for the current page.
    #[must_use]
    pub fn recent_resource_requests(&self) -> Vec<ResourceRequestStatus> {
        self.recent_resource_requests.iter().cloned().collect()
    }

    /// Whether the recent-resource window contains rows newer than `seq`.
    ///
    /// Resource sequence numbers are monotonically appended, so hot poll paths can
    /// check the newest row before deciding whether to scan or clone the bounded
    /// request history.
    #[must_use]
    pub fn has_recent_resource_requests_after(&self, seq: u64) -> bool {
        self.recent_resource_requests
            .back()
            .is_some_and(|resource| resource.seq > seq)
    }

    /// Recent subresource requests observed after `seq`.
    ///
    /// The full recent list backs operator-facing site-info summaries. Hot shell
    /// poll paths that only need newly observed rows should use this sequence
    /// window so an unchanged active tab does not clone the full bounded history
    /// every frame.
    #[must_use]
    pub fn recent_resource_requests_after(&self, seq: u64) -> Vec<ResourceRequestStatus> {
        self.recent_resource_requests
            .iter()
            .filter(|resource| resource.seq > seq)
            .cloned()
            .collect()
    }

    /// The session's live status.
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    /// Whether the session has crashed.
    #[must_use]
    pub const fn is_crashed(&self) -> bool {
        matches!(self.state, SessionState::Crashed { .. })
    }

    /// The current navigation state (drives the chrome).
    #[must_use]
    pub const fn nav(&self) -> &NavState {
        &self.nav
    }

    /// The current page title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The engine's current cursor shape (hover over a link/text field/resize
    /// edge). The shell reflects it while the pointer is over the page canvas.
    #[must_use]
    pub const fn cursor(&self) -> CursorKind {
        self.cursor
    }

    /// Whether the page is currently in HTML5 fullscreen (the shell hides its chrome).
    #[must_use]
    pub const fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Whether the page is currently producing audio (drives the tab-strip 🔊 glyph).
    #[must_use]
    pub const fn audible(&self) -> bool {
        self.audible
    }

    /// Latest page/media-session now-playing metadata observed by the helper.
    #[must_use]
    pub const fn media_metadata(&self) -> Option<&MediaMetadataStatus> {
        self.media_metadata.as_ref()
    }

    /// The latest engine-fetched favicon as PNG bytes, if the page reported one.
    /// The shell uploads it as the tab-strip icon.
    #[must_use]
    pub fn favicon(&self) -> Option<&[u8]> {
        self.favicon.as_deref()
    }

    /// Navigate to `url`.
    pub fn load(&mut self, url: impl Into<String>) {
        // A fresh navigation clears any interstitial from the prior page.
        self.cert_error = None;
        self.safe_browsing_block = None;
        self.managed_policy_block = None;
        self.pending_js_dialog = None;
        self.js_dialog_events.clear();
        self.nav.loading = true;
        self.send(&ControlMsg::Load(url.into()));
    }

    /// Reload the current page.
    pub fn reload(&mut self) {
        self.send(&ControlMsg::Reload);
    }

    /// Stop the current page load.
    pub fn stop(&mut self) {
        self.nav.loading = false;
        self.send(&ControlMsg::Stop);
    }

    /// Go back one history entry.
    pub fn go_back(&mut self) {
        self.send(&ControlMsg::Back);
    }

    /// Go forward one history entry.
    pub fn go_forward(&mut self) {
        self.send(&ControlMsg::Forward);
    }

    /// Set page zoom to a percentage. `100` is normal size.
    pub fn set_zoom(&mut self, percent: u16) {
        self.send(&ControlMsg::SetZoom { percent });
    }

    /// IME preedit: push the in-progress composition string to the focused editable
    /// (empty clears it). Driven by egui `ImeEvent::Preedit`.
    pub fn ime_set_composition(&mut self, text: String) {
        self.send(&ControlMsg::ImeSetComposition { text });
    }

    /// IME commit: finalize the composition by inserting `text`. Driven by egui
    /// `ImeEvent::Commit`.
    pub fn ime_commit_text(&mut self, text: String) {
        self.send(&ControlMsg::ImeCommitText { text });
    }

    /// IME finish: finalize any pending composition in place. Driven by egui
    /// `ImeEvent::Disable`.
    pub fn ime_finish_composition(&mut self) {
        self.send(&ControlMsg::ImeFinishComposition);
    }

    /// Autofill a user-chosen saved login into the page's first login form (the engine
    /// injects a fill script). User-initiated; session-only credentials.
    pub fn fill_login(&mut self, expected_host: String, username: String, password: String) {
        self.send(&ControlMsg::FillLogin {
            expected_host,
            username,
            password,
        });
    }

    /// Run a clipboard/editing command on the page's focused element (in-page
    /// context menu). Reuses the engine's native frame edit commands.
    pub fn edit_command(&mut self, command: crate::wire::EditCommand) {
        self.send(&ControlMsg::EditCommand { command });
    }

    /// Find text on the current page.
    pub fn find_in_page(&mut self, query: impl Into<String>, backwards: bool, find_next: bool) {
        self.send(&ControlMsg::FindInPage {
            query: query.into(),
            backwards,
            find_next,
        });
    }

    /// The latest find-in-page match tally `(active, count)` reported by the
    /// engine (1-based active ordinal, 0 = no active match), or `None` before any
    /// search this session.
    #[must_use]
    pub const fn find_result(&self) -> Option<(u32, u32)> {
        self.find_result
    }

    /// The latest TLS/certificate error that blocked a load, if the current
    /// navigation was cancelled by one. The shell paints its "Not secure —
    /// blocked" interstitial from this; cleared on the next [`Self::load`].
    #[must_use]
    pub const fn cert_error(&self) -> Option<&CertError> {
        self.cert_error.as_ref()
    }

    /// The URL of a top-level navigation blocked by the safe-browsing list, if any —
    /// drives the shell's "unsafe site" interstitial. Cleared on a fresh navigation.
    #[must_use]
    pub fn safe_browsing_block(&self) -> Option<&str> {
        self.safe_browsing_block.as_deref()
    }

    /// The URL of a top-level navigation blocked by managed Browser policy, if any.
    #[must_use]
    pub fn managed_policy_block(&self) -> Option<&str> {
        self.managed_policy_block.as_deref()
    }

    /// Clear the managed-policy interstitial after the shell has handled it.
    pub fn clear_managed_policy_block(&mut self) {
        self.managed_policy_block = None;
    }

    /// The latest JavaScript dialog (alert/confirm/prompt) a page raised. The
    /// engine already auto-resolved it (the page did not block); the shell may
    /// surface this as a passive, non-blocking notice.
    #[must_use]
    pub const fn pending_js_dialog(&self) -> Option<&JsDialog> {
        self.pending_js_dialog.as_ref()
    }

    /// Drain JavaScript dialog notices the engine already auto-resolved. Each event
    /// is surfaced at most once to the shell; [`Self::pending_js_dialog`] remains
    /// the latest retained value for status accessors/tests.
    pub fn drain_js_dialog_events(&mut self) -> Vec<JsDialog> {
        self.js_dialog_events.drain(..).collect()
    }

    /// The oldest pending beforeunload prompt awaiting a leave/stay decision, if
    /// any. The shell renders this and replies via [`Self::answer_before_unload`].
    #[must_use]
    pub fn pending_before_unload(&self) -> Option<&BeforeUnloadDialog> {
        self.pending_before_unloads.front()
    }

    /// Answer the oldest pending beforeunload prompt: `proceed=true` leaves or
    /// reloads the page, `false` stays. A no-op when no prompt is pending.
    pub fn answer_before_unload(&mut self, proceed: bool) {
        if let Some(dialog) = self.pending_before_unloads.pop_front() {
            self.send(&ControlMsg::BeforeUnloadDecision {
                id: dialog.id,
                proceed,
            });
        }
    }

    /// The oldest pending permission request awaiting the user's allow/block, if any
    /// (the FRONT of the queue). The shell renders a prompt from this and replies via
    /// [`Self::answer_permission`]; answering reveals the next queued request.
    #[must_use]
    pub fn pending_permission(&self) -> Option<&PermissionRequest> {
        self.pending_permissions.front()
    }

    /// Answer the oldest pending permission request: pop it and send the engine a
    /// [`ControlMsg::PermissionDecision`] carrying its `id` (the engine continues the
    /// held CEF callback with accept/deny). A no-op when the queue is empty.
    /// Session-only — the client persists no permission state.
    pub fn answer_permission(&mut self, allow: bool) {
        if let Some(request) = self.pending_permissions.pop_front() {
            self.send(&ControlMsg::PermissionDecision {
                id: request.id,
                allow,
            });
        }
    }

    /// Clear the page-find selection/highlight where the helper supports it.
    pub fn clear_find(&mut self) {
        self.send(&ControlMsg::ClearFind);
    }

    /// Set whether tab audio is muted.
    pub fn set_audio_muted(&mut self, muted: bool) {
        self.send(&ControlMsg::SetAudioMuted { muted });
    }

    /// Tell the helper whether this tab is hidden (backgrounded/occluded). A
    /// hidden tab drives CEF `WasHidden(true)`, which stops its offscreen paint
    /// and shm readback so an unseen tab no longer burns decode/copy work.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.send(&ControlMsg::SetHidden { hidden });
    }

    /// Ask the helper to toggle media playback on the active page.
    pub fn toggle_media_playback(&mut self) {
        self.send(&ControlMsg::ToggleMediaPlayback);
    }

    /// Ask the helper to run one media transport action on the active page.
    pub fn media_transport(&mut self, action: MediaTransportAction) {
        self.send(&ControlMsg::MediaTransport { action });
    }

    /// Set whether page-initiated autoplay is blocked until user activation.
    pub fn set_autoplay_blocked(&mut self, blocked: bool) {
        self.send(&ControlMsg::SetAutoplayBlocked { blocked });
    }

    /// Set whether forced-dark styling is enabled for this tab.
    pub fn set_force_dark(&mut self, enabled: bool) {
        self.send(&ControlMsg::SetForceDark { enabled });
    }

    /// Set whether reader-mode styling is enabled for this tab.
    pub fn set_reader_mode(&mut self, enabled: bool) {
        self.send(&ControlMsg::SetReaderMode { enabled });
    }

    /// Set whether the shell-curated userscript bundle is enabled for this tab.
    pub fn set_user_scripts(&mut self, enabled: bool, bundle: impl Into<String>) {
        self.send(&ControlMsg::SetUserScripts {
            enabled,
            bundle: bundle.into(),
        });
    }

    /// Override page-visible User-Agent metadata for this tab. Empty restores the
    /// helper's engine default.
    pub fn set_user_agent(&mut self, user_agent: impl Into<String>) {
        self.send(&ControlMsg::SetUserAgent {
            user_agent: user_agent.into(),
        });
    }

    /// Override page-visible device metadata for this tab.
    pub fn set_device_profile(
        &mut self,
        profile: impl Into<String>,
        width: u16,
        height: u16,
        scale_percent: u16,
        touch: bool,
    ) {
        self.send(&ControlMsg::SetDeviceProfile {
            profile: profile.into(),
            width,
            height,
            scale_percent,
            touch,
        });
    }

    /// Ask the helper to print the current page.
    pub fn print_page(&mut self) {
        self.send(&ControlMsg::PrintPage);
    }

    /// Ask the helper to save the current page as a PDF at `path`.
    pub fn save_pdf(&mut self, path: impl Into<String>) {
        self.send(&ControlMsg::SavePdf { path: path.into() });
    }

    /// Ask the helper to extract bounded visible page text.
    pub fn request_page_text(&mut self, id: u64, max_bytes: u32) {
        self.send(&ControlMsg::RequestPageText { id, max_bytes });
    }

    /// Ask the helper to extract bounded active-page scrape data.
    pub fn request_page_scrape(
        &mut self,
        id: u64,
        max_bytes: u32,
        max_links: u16,
        max_headings: u16,
    ) {
        self.send(&ControlMsg::RequestPageScrape {
            id,
            max_bytes,
            max_links,
            max_headings,
        });
    }

    /// Resolve or reject one pending page WebAuthn/passkey request.
    pub fn complete_passkey(&mut self, body: impl Into<String>) {
        self.send(&ControlMsg::CompletePasskey { body: body.into() });
    }

    /// Apply shell-owned spellcheck highlights to the current page. An empty
    /// word list clears any prior helper-side highlights.
    pub fn set_spellcheck_highlights(&mut self, words: Vec<String>) {
        self.send(&ControlMsg::SetSpellcheckHighlights { words });
    }

    /// Ask the helper to replace one spelling miss with the selected suggestion.
    pub fn apply_spellcheck_correction(
        &mut self,
        word: impl Into<String>,
        replacement: impl Into<String>,
    ) {
        self.send(&ControlMsg::ApplySpellcheckCorrection {
            word: word.into(),
            replacement: replacement.into(),
        });
    }

    /// Ask the helper to replace all visible matches for one spelling miss with
    /// the selected suggestion.
    pub fn apply_spellcheck_correction_all(
        &mut self,
        word: impl Into<String>,
        replacement: impl Into<String>,
    ) {
        self.send(&ControlMsg::ApplySpellcheckCorrectionAll {
            word: word.into(),
            replacement: replacement.into(),
        });
    }

    /// Ask the helper to replace one indexed visible spelling miss with the
    /// selected suggestion.
    pub fn apply_spellcheck_correction_at(
        &mut self,
        word: impl Into<String>,
        replacement: impl Into<String>,
        occurrence: u16,
    ) {
        self.send(&ControlMsg::ApplySpellcheckCorrectionAt {
            word: word.into(),
            replacement: replacement.into(),
            occurrence,
        });
    }

    /// Tell the helper the view resized to `width` x `height` device pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.send(&ControlMsg::Resize { width, height });
    }

    /// Forward one egui input event. Pointer positions must already be in frame
    /// device pixels (the shell maps them); `pixels_per_point` scales only wheel
    /// scroll. A no-op once crashed, or for an event that does not map.
    pub fn send_input(&mut self, event: &egui::Event, pixels_per_point: f32) {
        if self.is_crashed() {
            return;
        }
        if let Some(ie) = input::map_event(event, pixels_per_point) {
            self.send(&ControlMsg::Input(ie));
        }
    }

    /// Encode + frame a control message and write it; a write failure crashes the
    /// session (the helper is gone). A no-op once crashed.
    fn send(&mut self, msg: &ControlMsg) {
        if self.is_crashed() {
            return;
        }
        let framed = wire::frame(&msg.encode());
        // `impl Write for &UnixStream`, so write through a `&UnixStream` binding.
        let mut sock: &UnixStream = &self.stream;
        if let Err(e) = sock.write_all(&framed) {
            self.mark_crashed(format!("failed to send to helper: {e}"));
        }
    }
}

impl Drop for WebSession {
    /// Reap the live helper child on teardown so a dropped session never leaks an
    /// orphaned `mde-web-preview` process. `std::process::Child`'s own drop
    /// deliberately does **not** signal the child (it detaches it) — which is how
    /// orphaned `tab` helper pairs accumulated live (BUG-BROWSER-4) across shell
    /// restarts, closed tabs, and respawn-on-reload swaps that drop the old
    /// session. So this first closes the session socket, giving a cooperative
    /// helper a short EOF-driven exit window, then KILLs and WAITs on failure.
    /// `wait` reaps the child (leaving no zombie either — an already-exited child
    /// was reaped by [`Self::poll`]'s `try_wait`, so `wait` is then a cheap cached
    /// read). CEF adds one wrapper hop (`mde-web-cef` → renderer bridge), so the
    /// live spawn starts a helper process group and teardown kills that whole
    /// group if the grace window does not exit cleanly.
    /// Best-effort: an already-gone child makes `kill` error, which is the goal
    /// state and is ignored, and `wait` never blocks on a reaped pid. A test /
    /// fake-helper session carries no child and this is a no-op.
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(mut child) = self.child.take() {
            if !wait_for_child_exit(&mut child, HELPER_GRACEFUL_SHUTDOWN) {
                kill_helper_process_group(&child);
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn kill_helper_process_group(child: &Child) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    // SAFETY: helpers spawned by `WebSession::spawn` are made process-group
    // leaders. Sending SIGKILL to `-pid` kills that helper tree; if the caller
    // supplied a non-group-leader test child, `kill` fails harmlessly.
    let _ = unsafe { kill(-pid, SIGKILL) };
}

/// Everything the live spawn needs to launch a sandboxed browser helper
/// (`live-helper`).
#[cfg(feature = "live-helper")]
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Path to the browser helper binary (`mde-web-preview` or `mde-web-cef`).
    pub helper_bin: std::path::PathBuf,
    /// Environment values to add to the helper process.
    pub env: Vec<(String, String)>,
    /// The first URL to load.
    pub url: String,
    /// Initial view width in device pixels.
    pub width: u32,
    /// Initial view height in device pixels.
    pub height: u32,
}

#[cfg(feature = "live-helper")]
impl WebSession {
    /// Spawn the real browser helper and wire it the session socket.
    ///
    /// The helper end is passed as the child's stdin — a connected `AF_UNIX`
    /// socket over which it reads control frames and `SCM_RIGHTS` its shm frame fd
    /// back. Honest-gated by the caller: it needs a GPU seat plus a helper whose
    /// `tab` mode speaks this socket contract.
    ///
    /// # Errors
    /// Fails if the socketpair or the child process cannot be created.
    pub fn spawn(spec: &SpawnSpec) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        let (shell_end, helper_end) = UnixStream::pair()?;
        let mut command = Command::new(&spec.helper_bin);
        command
            .arg("tab")
            .args([
                "--url",
                &spec.url,
                "--width",
                &spec.width.to_string(),
                "--height",
                &spec.height.to_string(),
            ])
            .envs(spec.env.iter().map(|(key, value)| (key, value)))
            .stdin(Stdio::from(OwnedFd::from(helper_end)))
            .process_group(0);
        let child = command.spawn()?;
        Self::from_stream(shell_end, Some(child))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;
    use crate::wire::{self as w};
    use mde_adblock::FilterListStore;
    use std::io::Read;

    /// A bundled-filter session over a bare socketpair (no shm/frame writer —
    /// these tests drive only the request-policy + cosmetic protocol). Returns the
    /// session (shell end) and the peer end that plays the helper.
    fn filtered_session() -> (WebSession, UnixStream) {
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let filter = RequestFilter::from_store(&FilterListStore::with_bundled());
        let session = WebSession::from_stream(shell, None)
            .expect("session")
            .with_filter(filter);
        (session, helper)
    }

    /// Write one framed helper event onto the peer socket.
    fn send_event(peer: &UnixStream, msg: &EventMsg) {
        let mut s: &UnixStream = peer;
        s.write_all(&w::frame(&msg.encode())).expect("write event");
    }

    /// Read exactly one framed control message the session wrote back (blocking).
    fn read_control(peer: &mut UnixStream) -> ControlMsg {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            if let Ok(Some(payload)) = w::take_frame(&mut buf) {
                return ControlMsg::decode(&payload).expect("decode control");
            }
            let n = peer.read(&mut chunk).expect("read");
            assert!(n > 0, "peer socket closed before a control frame arrived");
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    #[test]
    fn a_tracker_request_is_blocked_and_counted_over_the_seam() {
        let (mut session, mut peer) = filtered_session();
        // Commit a page so the first-party is set; the nav pushes a cosmetic
        // stylesheet. Drive the session per-message (send → poll → read) — the
        // established idiom in this file; a single poll over two queued frames
        // races the socketpair buffering and can block the follow-up read.
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            },
        );
        let first = read_control_after_poll(&mut session, &mut peer);
        assert!(matches!(first, ControlMsg::CosmeticFilters(_)));
        // A bundled EasyPrivacy tracker subresource is judged + answered.
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://www.google-analytics.com/collect".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
            },
        );
        let verdict = read_control_after_poll(&mut session, &mut peer);
        assert_eq!(
            verdict,
            ControlMsg::ResourceVerdict {
                id: 1,
                allow: false
            }
        );
        assert_eq!(session.blocked_count(), 1, "the blocked request is counted");
        let recent = session.recent_resource_requests();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].seq, 1);
        assert_eq!(recent[0].url, "https://www.google-analytics.com/collect");
        assert_eq!(
            recent[0].resource,
            filter::resource_to_wire(mde_adblock::ResourceType::Script)
        );
        assert!(!recent[0].allowed);
        assert!(
            recent[0]
                .blocked_by
                .as_deref()
                .is_some_and(|rule| rule.contains("google")),
            "tracker block should retain the matched rule: {:?}",
            recent[0].blocked_by
        );
    }

    #[test]
    fn recent_resource_requests_after_returns_only_newer_rows() {
        let (mut session, mut peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            },
        );
        let _cosmetic = read_control_after_poll(&mut session, &mut peer);

        for id in 1..=3 {
            send_event(
                &peer,
                &EventMsg::ResourceRequest {
                    id,
                    url: format!("https://cdn.example.com/{id}.js"),
                    resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
                },
            );
            let _verdict = read_control_after_poll(&mut session, &mut peer);
        }

        let all = session.recent_resource_requests();
        assert_eq!(all.len(), 3);
        assert!(
            session.has_recent_resource_requests_after(2),
            "the newest resource sequence should expose a cheap hot-path signal"
        );
        assert_eq!(
            session.recent_resource_requests_after(2),
            vec![ResourceRequestStatus {
                seq: 3,
                url: "https://cdn.example.com/3.js".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
                allowed: true,
                blocked_by: None,
            }],
            "hot poll paths should clone only requests beyond their sequence watermark"
        );
        assert!(
            !session.has_recent_resource_requests_after(3),
            "an unchanged watermark should avoid scanning or cloning the resource window"
        );
        assert!(
            session.recent_resource_requests_after(3).is_empty(),
            "an unchanged active tab should not clone its full recent-resource history"
        );
    }

    #[test]
    fn a_benign_first_party_request_passes_over_the_seam() {
        let (mut session, mut peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            },
        );
        let _cosmetic = read_control_after_poll(&mut session, &mut peer);
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 2,
                url: "https://news.example.com/app.js".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
            },
        );
        session.poll();
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ResourceVerdict { id: 2, allow: true }
        );
        assert_eq!(session.blocked_count(), 0);
    }

    #[test]
    fn mixed_content_subresource_is_blocked_over_the_seam() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://portal.example/".to_owned(),
            },
        );
        session.poll();
        assert_no_control_pending(&peer);

        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 7,
                url: "http://cdn.example.test/app.js".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
            },
        );
        assert_eq!(
            read_control_after_poll(&mut session, &mut peer),
            ControlMsg::ResourceVerdict {
                id: 7,
                allow: false
            }
        );
        assert_eq!(session.blocked_count(), 1);
        assert_eq!(
            session.recent_resource_requests(),
            vec![ResourceRequestStatus {
                seq: 1,
                url: "http://cdn.example.test/app.js".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
                allowed: false,
                blocked_by: Some("mixed-content:http".to_owned()),
            }]
        );
        assert!(session.safe_browsing_block().is_none());
        assert!(session.managed_policy_block().is_none());
    }

    #[test]
    fn a_mesh_request_is_exempt_over_the_seam() {
        let (mut session, mut peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 3,
                url: "https://media.mesh/pagead/x".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::XmlHttpRequest),
            },
        );
        session.poll();
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ResourceVerdict { id: 3, allow: true }
        );
        assert_eq!(
            session.blocked_count(),
            0,
            "*.mesh is never counted as blocked"
        );
    }

    #[test]
    fn a_committed_page_pushes_a_cosmetic_stylesheet() {
        let (mut session, mut peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            },
        );
        session.poll();
        let ctrl = read_control(&mut peer);
        assert!(
            matches!(ctrl, ControlMsg::CosmeticFilters(_)),
            "expected a cosmetic stylesheet, got {ctrl:?}"
        );
        if let ControlMsg::CosmeticFilters(css) = ctrl {
            assert!(css.contains("display: none !important"));
            assert!(css.contains(".advertisement"), "css = {css}");
        }
    }

    /// Poll once then read the single control frame the session emitted (used when
    /// a helper event triggers exactly one reply).
    fn read_control_after_poll(session: &mut WebSession, peer: &mut UnixStream) -> ControlMsg {
        session.poll();
        read_control(peer)
    }

    #[test]
    fn a_fullscreen_event_flips_the_session_flag() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(!session.fullscreen());
        send_event(&peer, &EventMsg::Fullscreen { enabled: true });
        session.poll();
        assert!(
            session.fullscreen(),
            "entering page fullscreen sets the flag"
        );
        send_event(&peer, &EventMsg::Fullscreen { enabled: false });
        session.poll();
        assert!(!session.fullscreen(), "leaving fullscreen clears it");
    }

    #[test]
    fn an_audio_state_event_flips_the_session_audible_flag() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(!session.audible(), "a fresh session is silent");
        send_event(&peer, &EventMsg::AudioState { audible: true });
        session.poll();
        assert!(session.audible(), "an audio stream start sets the flag");
        send_event(&peer, &EventMsg::AudioState { audible: false });
        session.poll();
        assert!(!session.audible(), "an audio stream stop clears it");
    }

    #[test]
    fn media_metadata_event_folds_to_the_latest_body() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(session.media_metadata().is_none());

        send_event(
            &peer,
            &EventMsg::MediaMetadata {
                body: r#"{"title":"First"}"#.to_owned(),
            },
        );
        session.poll();
        assert_eq!(
            session.media_metadata().map(|m| m.body.as_str()),
            Some(r#"{"title":"First"}"#)
        );

        send_event(
            &peer,
            &EventMsg::MediaMetadata {
                body: r#"{"title":"Second","paused":false}"#.to_owned(),
            },
        );
        session.poll();
        assert_eq!(
            session.media_metadata().map(|m| m.body.as_str()),
            Some(r#"{"title":"Second","paused":false}"#)
        );

        send_event(
            &peer,
            &EventMsg::MediaMetadata {
                body: "   ".to_owned(),
            },
        );
        session.poll();
        assert!(session.media_metadata().is_none());
    }

    #[test]
    fn a_permission_request_prompts_then_the_answer_replies_with_the_id() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(session.pending_permission().is_none());

        send_event(
            &peer,
            &EventMsg::PermissionRequest {
                id: 42,
                kind: 5,
                origin: "https://meet.example".to_owned(),
            },
        );
        session.poll();
        let pending = session.pending_permission().expect("a pending prompt");
        assert_eq!(pending.id, 42);
        assert_eq!(pending.kind, 5);
        assert_eq!(pending.origin, "https://meet.example");

        // Answering sends the engine a decision carrying the request's id and clears it.
        session.answer_permission(true);
        assert!(
            session.pending_permission().is_none(),
            "answering clears the pending prompt"
        );
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision {
                id: 42,
                allow: true
            }
        );
    }

    #[test]
    fn a_before_unload_prompt_roundtrips_the_user_decision() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(session.pending_before_unload().is_none());

        send_event(
            &peer,
            &EventMsg::BeforeUnloadDialog {
                id: 42,
                message: "You have unsaved changes".to_owned(),
                origin: "https://editor.example/doc/1".to_owned(),
                is_reload: false,
            },
        );
        session.poll();
        let pending = session
            .pending_before_unload()
            .expect("a pending beforeunload prompt");
        assert_eq!(pending.id, 42);
        assert_eq!(pending.message, "You have unsaved changes");
        assert_eq!(pending.origin, "https://editor.example/doc/1");
        assert!(!pending.is_reload);

        session.answer_before_unload(false);
        assert!(
            session.pending_before_unload().is_none(),
            "answering clears the pending prompt"
        );
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::BeforeUnloadDecision {
                id: 42,
                proceed: false
            }
        );
    }

    #[test]
    fn before_unload_queue_overflow_answers_stay_for_the_oldest() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        for id in 0..=MAX_PENDING_BEFORE_UNLOADS as u64 {
            send_event(
                &peer,
                &EventMsg::BeforeUnloadDialog {
                    id,
                    message: format!("draft {id}"),
                    origin: "https://editor.example".to_owned(),
                    is_reload: false,
                },
            );
        }

        session.poll();
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::BeforeUnloadDecision {
                id: 0,
                proceed: false
            }
        );
        assert_eq!(session.pending_before_unload().map(|p| p.id), Some(1));

        session.answer_before_unload(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::BeforeUnloadDecision {
                id: 1,
                proceed: true
            }
        );
    }

    #[test]
    fn concurrent_permission_requests_queue_and_answer_oldest_first() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        // Two prompts in quick succession — the engine holds a callback for EACH, so
        // BOTH must be retained (not overwritten), oldest surfaced first.
        for (id, kind) in [(1u64, 0u8), (2, 1)] {
            send_event(
                &peer,
                &EventMsg::PermissionRequest {
                    id,
                    kind,
                    origin: "https://x.example".to_owned(),
                },
            );
        }
        session.poll();
        assert_eq!(session.pending_permission().map(|r| r.id), Some(1));
        session.answer_permission(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision { id: 1, allow: true }
        );
        // Answering the first REVEALS the second (it was not lost).
        assert_eq!(session.pending_permission().map(|r| r.id), Some(2));
        session.answer_permission(false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision {
                id: 2,
                allow: false
            }
        );
        assert!(session.pending_permission().is_none());
    }

    #[test]
    fn permission_queue_overflow_auto_denies_the_oldest_so_no_callback_leaks() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        // One past the cap: the oldest (id 0) is auto-DENIED (not silently dropped)
        // so the engine releases its held callback; the rest stay queued.
        for id in 0..=(MAX_PENDING_PERMISSIONS as u64) {
            send_event(
                &peer,
                &EventMsg::PermissionRequest {
                    id,
                    kind: 0,
                    origin: "https://x.example".to_owned(),
                },
            );
        }
        session.poll();
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision {
                id: 0,
                allow: false
            },
            "the displaced oldest request is auto-denied, not orphaned"
        );
        assert_eq!(session.pending_permission().map(|r| r.id), Some(1));
    }

    #[test]
    fn a_top_level_safe_browsing_block_drives_the_interstitial_state() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        session.set_filter(
            RequestFilter::empty()
                .with_safe_browsing(filter::SafeBrowsingBlocklist::from_hosts(["malware.test"])),
        );
        assert!(session.safe_browsing_block().is_none());

        // A TOP-LEVEL Document navigation to the unsafe host arms the interstitial.
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://malware.test/".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Document),
            },
        );
        session.poll();
        assert_eq!(session.safe_browsing_block(), Some("https://malware.test/"));

        // A subresource block (not Document) does NOT arm the full-page interstitial.
        session.load("https://ok.example/");
        assert!(session.safe_browsing_block().is_none(), "nav clears it");
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 2,
                url: "https://malware.test/pixel.gif".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Image),
            },
        );
        session.poll();
        assert!(
            session.safe_browsing_block().is_none(),
            "a blocked subresource is not a full-page interstitial"
        );
    }

    #[test]
    fn a_top_level_managed_policy_block_drives_the_interstitial_state() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        session.set_filter(
            RequestFilter::empty()
                .with_managed_policy(filter::ManagedUrlPolicy::from_rules(["blocked.example"])),
        );
        assert!(session.managed_policy_block().is_none());

        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://blocked.example/".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Document),
            },
        );
        session.poll();
        assert_eq!(
            session.managed_policy_block(),
            Some("https://blocked.example/")
        );
        assert!(session.safe_browsing_block().is_none());

        session.load("https://ok.example/");
        assert!(
            session.managed_policy_block().is_none(),
            "fresh navigations clear the managed-policy interstitial"
        );
    }

    #[test]
    fn a_session_without_a_filter_allows_everything() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 4,
                url: "https://doubleclick.net/ad".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Image),
            },
        );
        session.poll();
        // The default (empty) filter blocks nothing.
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ResourceVerdict { id: 4, allow: true }
        );
        assert_eq!(session.blocked_count(), 0);
    }

    #[test]
    fn pdf_completion_events_are_queued_for_the_shell() {
        let (mut session, peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::PdfSaved {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: true,
            },
        );

        session.poll();

        assert_eq!(
            session.drain_pdf_events(),
            vec![PdfSaveStatus {
                path: "/tmp/mde-page.pdf".to_owned(),
                ok: true,
            }]
        );
        assert!(
            session.drain_pdf_events().is_empty(),
            "events are drained exactly once"
        );
    }

    #[test]
    fn favicon_events_expose_the_png_bytes() {
        let (mut session, peer) = filtered_session();
        assert!(session.favicon().is_none(), "no favicon before any event");
        send_event(
            &peer,
            &EventMsg::Favicon {
                png: vec![0x89, b'P', b'N', b'G'],
            },
        );

        session.poll();

        assert_eq!(session.favicon(), Some(&[0x89, b'P', b'N', b'G'][..]));
    }

    #[test]
    fn cert_error_is_exposed_then_cleared_on_a_fresh_load() {
        let (mut session, peer) = filtered_session();
        assert!(
            session.cert_error().is_none(),
            "no cert error before any event"
        );
        send_event(
            &peer,
            &EventMsg::CertError {
                url: "https://bad.example/".to_owned(),
                code: -202,
                message: "The certificate is not trusted (unknown authority)".to_owned(),
            },
        );

        session.poll();

        assert_eq!(
            session.cert_error(),
            Some(&CertError {
                url: "https://bad.example/".to_owned(),
                code: -202,
                message: "The certificate is not trusted (unknown authority)".to_owned(),
            })
        );

        // A fresh navigation clears the interstitial state.
        session.load("https://good.example/");
        assert!(
            session.cert_error().is_none(),
            "cert error cleared on the next load"
        );
    }

    #[test]
    fn js_dialog_notice_is_exposed_to_the_shell() {
        let (mut session, peer) = filtered_session();
        assert!(
            session.pending_js_dialog().is_none(),
            "no dialog before any event"
        );
        send_event(
            &peer,
            &EventMsg::JsDialog {
                kind: 1,
                message: "Delete this item?".to_owned(),
                origin: "https://app.example/".to_owned(),
            },
        );

        session.poll();

        assert_eq!(
            session.pending_js_dialog(),
            Some(&JsDialog {
                kind: 1,
                message: "Delete this item?".to_owned(),
                origin: "https://app.example/".to_owned(),
            })
        );
        assert_eq!(
            session.drain_js_dialog_events(),
            vec![JsDialog {
                kind: 1,
                message: "Delete this item?".to_owned(),
                origin: "https://app.example/".to_owned(),
            }]
        );
        assert!(
            session.drain_js_dialog_events().is_empty(),
            "dialog notices are drained exactly once"
        );
        assert!(
            session.pending_js_dialog().is_some(),
            "the latest dialog accessor remains available after draining notices"
        );
    }

    #[test]
    fn page_text_events_are_queued_for_the_shell() {
        let (mut session, peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::PageText {
                id: 7,
                text: "visible page words".to_owned(),
            },
        );

        session.poll();

        let events = session.drain_page_text_events();
        assert_eq!(
            events,
            vec![PageTextStatus {
                id: 7,
                text: "visible page words".to_owned(),
            }]
        );
        let debug = format!("{events:?}");
        assert!(!debug.contains("visible page words"));
        assert!(debug.contains("<redacted>"));
        assert!(
            session.drain_page_text_events().is_empty(),
            "page-text events are drained exactly once"
        );
    }

    #[test]
    fn passkey_requests_are_queued_for_the_shell() {
        let (mut session, peer) = filtered_session();
        let body = r#"{"ceremony":"get","origin":"https://login.example","rp_id":"login.example","challenge_b64url":"abcdefghijklmnopqrstuvwxyz","user_handle_b64url":"secret-handle"}"#;
        send_event(
            &peer,
            &EventMsg::PasskeyRequest {
                body: body.to_owned(),
            },
        );

        session.poll();

        let events = session.drain_passkey_events();
        assert_eq!(
            events,
            vec![PasskeyRequestStatus {
                body: body.to_owned(),
            }]
        );
        let debug = format!("{events:?}");
        assert!(!debug.contains("secret-handle"));
        assert!(debug.contains("<redacted>"));
        assert!(
            session.drain_passkey_events().is_empty(),
            "passkey events are drained exactly once"
        );
    }

    #[test]
    fn page_scrape_status_debug_redacts_the_dom_body() {
        let event = PageScrapeStatus {
            id: 11,
            body: r#"{"text":"private page body","links":["https://example.test/private"]}"#
                .to_owned(),
        };

        assert_eq!(event.id, 11);
        assert!(event.body.contains("private page body"));
        let debug = format!("{event:?}");
        assert!(debug.contains("PageScrapeStatus"));
        assert!(!debug.contains("private page body"));
        assert!(!debug.contains("https://example.test/private"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn login_capture_status_debug_redacts_the_credential_body() {
        let (mut session, peer) = filtered_session();
        let body = r#"{"username":"alice@example.com","password":"hunter2"}"#;
        send_event(
            &peer,
            &EventMsg::LoginSubmitted {
                origin: "https://login.example".to_owned(),
                body: body.to_owned(),
            },
        );

        session.poll();

        let captures = session.drain_login_captures();
        assert_eq!(
            captures,
            vec![LoginCaptureStatus {
                origin: "https://login.example".to_owned(),
                body: body.to_owned(),
            }]
        );
        let debug = format!("{captures:?}");
        assert!(debug.contains("https://login.example"));
        assert!(!debug.contains("alice@example.com"));
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn passkey_completion_is_sent_to_the_helper() {
        let (mut session, mut peer) = filtered_session();
        let body = r#"{"client_request_id":"pk-1","op":"browser_passkey_assertion"}"#;

        session.complete_passkey(body);

        assert_eq!(
            read_control(&mut peer),
            ControlMsg::CompletePasskey {
                body: body.to_owned()
            }
        );
    }

    #[test]
    fn resource_manifest_is_bounded_and_resets_on_page_host_change() {
        let (mut session, mut peer) = filtered_session();
        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://news.example.com/".to_owned(),
            },
        );
        let _cosmetic = read_control_after_poll(&mut session, &mut peer);
        for id in 1..=140 {
            send_event(
                &peer,
                &EventMsg::ResourceRequest {
                    id,
                    url: format!("https://cdn.example.com/{id}.js"),
                    resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
                },
            );
            let _verdict = read_control_after_poll(&mut session, &mut peer);
        }

        let resources = session.recent_resource_requests();
        assert_eq!(resources.len(), MAX_RECENT_RESOURCE_REQUESTS);
        assert_eq!(resources[0].url, "https://cdn.example.com/13.js");
        assert_eq!(
            resources.last().expect("last resource").url,
            "https://cdn.example.com/140.js"
        );

        send_event(
            &peer,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: false,
                url: "https://other.example.com/".to_owned(),
            },
        );
        let _cosmetic = read_control_after_poll(&mut session, &mut peer);
        assert!(session.recent_resource_requests().is_empty());
    }

    #[test]
    fn page_zoom_and_find_controls_are_framed_for_the_helper() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");

        session.set_zoom(125);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetZoom { percent: 125 }
        );

        session.find_in_page("mesh", false, false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::FindInPage {
                query: "mesh".to_owned(),
                backwards: false,
                find_next: false,
            }
        );

        session.find_in_page("mesh", true, true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::FindInPage {
                query: "mesh".to_owned(),
                backwards: true,
                find_next: true,
            }
        );

        session.clear_find();
        assert_eq!(read_control(&mut peer), ControlMsg::ClearFind);

        session.set_audio_muted(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetAudioMuted { muted: true }
        );
        session.set_audio_muted(false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetAudioMuted { muted: false }
        );

        session.toggle_media_playback();
        assert_eq!(read_control(&mut peer), ControlMsg::ToggleMediaPlayback);

        for action in [
            MediaTransportAction::PlayPause,
            MediaTransportAction::Play,
            MediaTransportAction::Pause,
            MediaTransportAction::Stop,
            MediaTransportAction::Next,
            MediaTransportAction::Previous,
            MediaTransportAction::VolumeUp,
            MediaTransportAction::VolumeDown,
        ] {
            session.media_transport(action);
            assert_eq!(
                read_control(&mut peer),
                ControlMsg::MediaTransport { action }
            );
        }

        session.set_autoplay_blocked(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetAutoplayBlocked { blocked: true }
        );
        session.set_autoplay_blocked(false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetAutoplayBlocked { blocked: false }
        );

        session.set_force_dark(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetForceDark { enabled: true }
        );
        session.set_force_dark(false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetForceDark { enabled: false }
        );

        session.set_reader_mode(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetReaderMode { enabled: true }
        );
        session.set_reader_mode(false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetReaderMode { enabled: false }
        );

        session.set_user_scripts(true, "console.log('mde');");
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetUserScripts {
                enabled: true,
                bundle: "console.log('mde');".to_owned(),
            }
        );
        session.set_user_scripts(false, "");
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetUserScripts {
                enabled: false,
                bundle: String::new(),
            }
        );
        session.set_user_agent("Mozilla/5.0 MDE-Test");
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetUserAgent {
                user_agent: "Mozilla/5.0 MDE-Test".to_owned(),
            }
        );
        session.set_user_agent("");
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetUserAgent {
                user_agent: String::new(),
            }
        );
        session.set_device_profile("phone", 390, 844, 300, true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetDeviceProfile {
                profile: "phone".to_owned(),
                width: 390,
                height: 844,
                scale_percent: 300,
                touch: true,
            }
        );
        session.set_device_profile("default", 0, 0, 100, false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SetDeviceProfile {
                profile: "default".to_owned(),
                width: 0,
                height: 0,
                scale_percent: 100,
                touch: false,
            }
        );

        session.print_page();
        assert_eq!(read_control(&mut peer), ControlMsg::PrintPage);
        session.save_pdf("/tmp/mde-page.pdf");
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::SavePdf {
                path: "/tmp/mde-page.pdf".to_owned()
            }
        );

        session.request_page_text(11, 4096);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::RequestPageText {
                id: 11,
                max_bytes: 4096,
            }
        );
    }

    /// Poll until a frame lands (the fake helper's initial burst is already in the
    /// socket buffer, so one poll is enough — the loop just guards scheduling).
    fn poll_for_frame(session: &mut WebSession) -> Option<ColorImage> {
        for _ in 0..50 {
            session.poll();
            if let Some(img) = session.take_frame() {
                return Some(img);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        None
    }

    fn poll_until_crashed(session: &mut WebSession) -> bool {
        for _ in 0..50 {
            session.poll();
            if session.is_crashed() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn a_paint_ready_yields_one_frame_and_goes_live() {
        let (mut session, _helper) = testkit::connect().expect("connect");
        let img = poll_for_frame(&mut session).expect("a frame arrived over the seam");
        assert_eq!(
            img.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize]
        );
        assert_eq!(session.state(), &SessionState::Live);
        // The frame is consumed — no re-upload until the next paint.
        assert!(session.take_frame().is_none());
        assert_eq!(session.nav().url, "about:blank");
        assert_eq!(session.title(), "about:blank");
    }

    #[test]
    fn a_mapped_frame_without_paint_ready_still_goes_live() {
        // Layer-A: the helper attaches the shm fd + publishes a frame but NEVER
        // sends a PaintReady. The session must still upload the frame and go Live
        // from the seqlock sequence alone (the fix for a helper whose paint-ready
        // is dropped/slow — the "stuck on Loading the page…" class of bug).
        use crate::frame::PixelFormat;
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let writer =
            testkit::FrameWriter::create(testkit::FAKE_W, testkit::FAKE_H).expect("shm writer");
        writer
            .emit(
                testkit::FAKE_W,
                testkit::FAKE_H,
                PixelFormat::Rgba8,
                &testkit::gradient(testkit::FAKE_W, testkit::FAKE_H),
            )
            .expect("emit a frame");
        // Attach the fd — but send NO PaintReady.
        scm::send_frame_with_fd(&helper, &EventMsg::AttachFrame.encode(), writer.raw_fd())
            .expect("attach fd");

        let mut session = WebSession::from_stream(shell, None).expect("session");
        session.poll();

        assert_eq!(
            session.state(),
            &SessionState::Live,
            "the shm-sequence fallback must promote to Live without a PaintReady"
        );
        let img = session
            .take_frame()
            .expect("a frame is available via the shm fallback");
        assert_eq!(
            img.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize]
        );
        // Keep the writer + helper end alive until the frame has been read.
        drop(helper);
        drop(writer);
    }

    #[test]
    fn a_paint_ready_before_attach_frame_still_reaches_live() {
        // The ordering hazard: PaintReady arrives BEFORE the fd is attached, so the
        // reader is still None and its upload is skipped. When AttachFrame then maps
        // the reader, Layer-A must self-heal and promote the already-published frame.
        use crate::frame::PixelFormat;
        let (shell, helper) = UnixStream::pair().expect("socketpair");
        let writer =
            testkit::FrameWriter::create(testkit::FAKE_W, testkit::FAKE_H).expect("shm writer");
        writer
            .emit(
                testkit::FAKE_W,
                testkit::FAKE_H,
                PixelFormat::Rgba8,
                &testkit::gradient(testkit::FAKE_W, testkit::FAKE_H),
            )
            .expect("emit a frame");
        let mut session = WebSession::from_stream(shell, None).expect("session");

        // PaintReady FIRST — no reader yet, so it must not go Live or lose the frame.
        send_event(
            &helper,
            &EventMsg::PaintReady {
                seq: writer.sequence(),
            },
        );
        session.poll();
        assert_ne!(
            session.state(),
            &SessionState::Live,
            "an early PaintReady with no mapped reader must not go Live"
        );
        assert!(session.take_frame().is_none(), "nothing uploaded yet");

        // Now the fd attaches; Layer-A promotes from the live shm sequence.
        scm::send_frame_with_fd(&helper, &EventMsg::AttachFrame.encode(), writer.raw_fd())
            .expect("attach fd");
        session.poll();
        assert_eq!(
            session.state(),
            &SessionState::Live,
            "AttachFrame + a live shm sequence self-heals the early PaintReady"
        );
        assert!(session.take_frame().is_some(), "the frame is now uploaded");
        drop(helper);
        drop(writer);
    }

    #[test]
    fn a_dead_helper_surfaces_as_crashed() {
        let (mut session, helper) = testkit::connect().expect("connect");
        poll_for_frame(&mut session).expect("frame");
        assert!(!session.is_crashed());

        helper.crash(); // closes the helper socket end
        assert!(
            poll_until_crashed(&mut session),
            "a dead helper must surface honestly"
        );
        assert!(matches!(session.state(), SessionState::Crashed { .. }));
    }

    #[test]
    fn two_sessions_are_isolated_across_a_crash() {
        let (mut a, helper_a) = testkit::connect().expect("connect a");
        let (mut b, _helper_b) = testkit::connect().expect("connect b");
        poll_for_frame(&mut a).expect("a frame");
        poll_for_frame(&mut b).expect("b frame");

        helper_a.crash();
        assert!(poll_until_crashed(&mut a), "tab A crashed");
        b.poll();
        assert!(!b.is_crashed(), "tab B is unaffected by A's crash");
        assert_eq!(b.state(), &SessionState::Live);
    }

    #[test]
    fn reload_drives_a_fresh_frame_over_the_seam() {
        let (mut session, _helper) = testkit::connect().expect("connect");
        poll_for_frame(&mut session).expect("first frame");
        // Ask for a reload; the fake helper answers with a new frame + PaintReady.
        session.reload();
        let next = poll_for_frame(&mut session).expect("a reloaded frame");
        assert_eq!(
            next.size,
            [testkit::FAKE_W as usize, testkit::FAKE_H as usize]
        );
    }

    #[test]
    fn stop_drives_a_real_control_frame_over_the_seam() {
        let (shell, mut helper) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &helper,
            &EventMsg::NavState {
                can_back: false,
                can_forward: false,
                loading: true,
                url: "https://example.test/".to_owned(),
            },
        );
        session.poll();
        assert!(session.nav().loading, "helper reported an in-flight load");

        session.stop();

        assert_eq!(read_control(&mut helper), ControlMsg::Stop);
        assert!(
            !session.nav().loading,
            "toolbar leaves loading state locally"
        );
    }

    #[test]
    fn input_after_a_crash_is_a_silent_no_op() {
        let (mut session, helper) = testkit::connect().expect("connect");
        poll_for_frame(&mut session).expect("frame");
        helper.crash();
        assert!(poll_until_crashed(&mut session));
        // Forwarding input / nav on a crashed session must not panic and stays a
        // no-op (the socket is gone).
        session.send_input(&egui::Event::PointerMoved(egui::pos2(1.0, 2.0)), 2.0);
        session.reload();
        assert!(session.is_crashed());
    }

    // ── BUG-BROWSER-4: the Drop reaps the live helper child (no orphan/zombie) ──

    /// Whether `pid` still has a process-table entry (Linux `/proc` — the platform
    /// this shell runs on). A live orphan keeps its `/proc/<pid>`; so does a
    /// killed-but-unwaited zombie — so a *missing* entry proves it was killed AND
    /// reaped.
    fn pid_alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    fn wait_until_pids_are_gone(pids: &[u32], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pids.iter().all(|pid| !pid_alive(*pid)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn drop_reaps_the_live_helper_child_leaving_no_orphan() {
        use std::process::Command;

        // Servo (the real helper) isn't spawnable in-test, so a long-lived `sleep`
        // stands in for the sandboxed helper child: its pid is unambiguously alive
        // until something reaps it. Wired into the session exactly as the live
        // spawn does — `from_stream(socket, Some(child))`.
        let (shell, _helper) = UnixStream::pair().expect("socketpair");
        let child = Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn a stand-in helper child");
        let pid = child.id();
        assert!(pid_alive(pid), "the stand-in helper should be running");

        let session = WebSession::from_stream(shell, Some(child)).expect("session");
        drop(session); // the Drop under test: kill + wait

        // The child is gone from the process table: a leaked orphan would still be
        // running `sleep 600`, and a killed-but-unwaited zombie would still hold
        // its `/proc` entry — both keep the path present. `wait` in Drop is
        // synchronous, so this settles at once; the short poll only guards against
        // scheduler jitter.
        let gone = wait_until_pids_are_gone(&[pid], Duration::from_secs(2));
        assert!(
            gone,
            "the helper child leaked past the session drop (orphan or zombie)"
        );
    }

    #[test]
    fn drop_kills_the_live_helper_process_group_leaving_no_renderer_child() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        // CEF is launched through an `mde-web-cef` wrapper which starts the real
        // renderer bridge beneath it. Model that shape with a shell parent and a
        // background child; dropping the session must not leave the child alive.
        let (shell, _helper) = UnixStream::pair().expect("socketpair");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 600 & printf '%s\\n' \"$!\"; wait")
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()
            .expect("spawn a stand-in helper process tree");
        let mut stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        let mut line = String::new();
        stdout
            .read_line(&mut line)
            .expect("read background child pid");
        let renderer_pid: u32 = line
            .trim()
            .parse()
            .expect("background child pid should be numeric");
        let wrapper_pid = child.id();
        assert!(pid_alive(wrapper_pid), "the wrapper should be running");
        assert!(
            pid_alive(renderer_pid),
            "the renderer stand-in should be running"
        );

        let session = WebSession::from_stream(shell, Some(child)).expect("session");
        drop(session);

        let gone = wait_until_pids_are_gone(&[wrapper_pid, renderer_pid], Duration::from_secs(2));
        assert!(gone, "the helper process group leaked past session drop");
    }

    #[test]
    fn dropping_a_childless_session_is_a_safe_no_op() {
        // A test / fake-helper session carries `child: None`; its Drop must not
        // panic or block — there is nothing to reap.
        let (shell, _peer) = UnixStream::pair().expect("socketpair");
        let session = WebSession::from_stream(shell, None).expect("session");
        drop(session);
    }

    // ── Adversarial event-fold + accessor robustness (audio / permission /
    //    safe-browsing / fullscreen / crash-precedence / IME) ──

    /// Assert the session wrote NO control frame back — a no-op sender or an
    /// already-cleared answer must be silent. Flips the peer non-blocking, reads,
    /// and requires an empty socket (`WouldBlock`); any buffered bytes are a stray
    /// frame the caller did not expect.
    fn assert_no_control_pending(peer: &UnixStream) {
        peer.set_nonblocking(true).expect("peer non-blocking");
        let mut s: &UnixStream = peer;
        let mut buf = [0u8; 64];
        let outcome = s.read(&mut buf);
        peer.set_nonblocking(false).expect("peer blocking");
        match outcome {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(0) => {}
            other => panic!("expected no control frame pending, got {other:?}"),
        }
    }

    #[test]
    fn audio_state_rapid_toggles_fold_to_the_last_value() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        for audible in [true, true, false, true] {
            send_event(&peer, &EventMsg::AudioState { audible });
        }
        session.poll();
        assert!(
            session.audible(),
            "a burst of toggles must fold to the LAST value (true)"
        );
        // A trailing stop still wins over the earlier starts.
        send_event(&peer, &EventMsg::AudioState { audible: false });
        session.poll();
        assert!(!session.audible(), "the final stop clears the flag");
    }

    #[test]
    fn answer_permission_with_nothing_pending_is_a_silent_no_op() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        assert!(session.pending_permission().is_none());
        // Nothing is pending — answering (either way) must not send a control frame.
        session.answer_permission(true);
        session.answer_permission(false);
        assert_no_control_pending(&peer);
        assert!(session.pending_permission().is_none());
    }

    #[test]
    fn answering_a_permission_twice_sends_exactly_one_decision() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &peer,
            &EventMsg::PermissionRequest {
                id: 7,
                kind: 2,
                origin: "https://clip.example".to_owned(),
            },
        );
        session.poll();
        session.answer_permission(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision { id: 7, allow: true }
        );
        // The prompt is already cleared; a second answer must be a silent no-op.
        session.answer_permission(true);
        assert_no_control_pending(&peer);
    }

    #[test]
    fn a_second_permission_request_queues_behind_the_first_no_leak() {
        // Regression guard: a second prompt used to OVERWRITE the first, orphaning
        // the first's held engine callback. It must now QUEUE behind it so both
        // resolve.
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &peer,
            &EventMsg::PermissionRequest {
                id: 1,
                kind: 0,
                origin: "https://first.example".to_owned(),
            },
        );
        session.poll();
        send_event(
            &peer,
            &EventMsg::PermissionRequest {
                id: 2,
                kind: 1,
                origin: "https://second.example".to_owned(),
            },
        );
        session.poll();
        // BOTH survive; the FIRST surfaces first (not overwritten).
        assert_eq!(session.pending_permission().map(|r| r.id), Some(1));
        session.answer_permission(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision { id: 1, allow: true }
        );
        // The second is revealed and resolvable — its callback is not orphaned.
        let pending = session.pending_permission().expect("second still queued");
        assert_eq!(pending.id, 2);
        assert_eq!(pending.origin, "https://second.example");
        session.answer_permission(true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::PermissionDecision { id: 2, allow: true }
        );
        assert_no_control_pending(&peer);
    }

    #[test]
    fn a_second_document_block_updates_the_interstitial_url() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        session.set_filter(RequestFilter::empty().with_safe_browsing(
            filter::SafeBrowsingBlocklist::from_hosts(["malware.test", "evil.test"]),
        ));
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 1,
                url: "https://malware.test/".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Document),
            },
        );
        session.poll();
        assert_eq!(session.safe_browsing_block(), Some("https://malware.test/"));
        // A second top-level Document block re-points the interstitial at the new URL.
        send_event(
            &peer,
            &EventMsg::ResourceRequest {
                id: 2,
                url: "https://evil.test/phish".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Document),
            },
        );
        session.poll();
        assert_eq!(
            session.safe_browsing_block(),
            Some("https://evil.test/phish")
        );
    }

    #[test]
    fn fullscreen_interleaved_with_audio_tracks_independently() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(&peer, &EventMsg::Fullscreen { enabled: true });
        send_event(&peer, &EventMsg::AudioState { audible: true });
        send_event(&peer, &EventMsg::Fullscreen { enabled: false });
        session.poll();
        assert!(
            !session.fullscreen(),
            "the LAST fullscreen event (exit) wins"
        );
        assert!(
            session.audible(),
            "the interleaved audio event survives fullscreen churn"
        );
    }

    #[test]
    fn an_audio_state_event_does_not_disturb_the_other_fields() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(&peer, &EventMsg::AudioState { audible: true });
        session.poll();
        assert!(session.audible());
        // Folding an AudioState must leave every unrelated field at its default.
        assert!(session.cert_error().is_none());
        assert!(session.safe_browsing_block().is_none());
        assert!(session.pending_permission().is_none());
        assert!(session.pending_js_dialog().is_none());
        assert!(!session.fullscreen());
        assert_eq!(session.nav(), &NavState::default());
        assert!(session.title().is_empty());
    }

    #[test]
    fn a_cert_error_does_not_disturb_the_audible_flag() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(&peer, &EventMsg::AudioState { audible: true });
        session.poll();
        send_event(
            &peer,
            &EventMsg::CertError {
                url: "https://bad.example/".to_owned(),
                code: -202,
                message: "unknown authority".to_owned(),
            },
        );
        session.poll();
        assert!(session.cert_error().is_some(), "cert error is recorded");
        assert!(
            session.audible(),
            "an unrelated cert error must not clear the audible flag"
        );
        assert!(session.pending_permission().is_none());
    }

    #[test]
    fn events_after_a_crash_in_the_same_batch_are_dropped() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        // One burst: a real state change, then a crash, then a change that must
        // NEVER be applied — drain stops at the crash and abandons the rest.
        send_event(&peer, &EventMsg::AudioState { audible: true });
        send_event(
            &peer,
            &EventMsg::Crashed {
                reason: "gpu process lost".to_owned(),
            },
        );
        send_event(&peer, &EventMsg::AudioState { audible: false });
        session.poll();
        assert!(session.is_crashed(), "the crash surfaced");
        assert!(
            session.audible(),
            "the post-crash AudioState(false) in the same batch was dropped"
        );
    }

    #[test]
    fn accessors_stay_sane_after_a_crash_event() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(&peer, &EventMsg::AudioState { audible: true });
        send_event(&peer, &EventMsg::Fullscreen { enabled: true });
        send_event(
            &peer,
            &EventMsg::PermissionRequest {
                id: 9,
                kind: 0,
                origin: "https://geo.example".to_owned(),
            },
        );
        session.poll();
        send_event(
            &peer,
            &EventMsg::Crashed {
                reason: "helper died".to_owned(),
            },
        );
        session.poll();
        assert!(session.is_crashed());
        // Every accessor still returns its last value without panicking.
        assert!(session.audible());
        assert!(session.fullscreen());
        assert_eq!(session.pending_permission().map(|p| p.id), Some(9));
        assert!(session.cert_error().is_none());
        assert!(session.find_result().is_none());
        // A further poll on a crashed session is a no-op that stays crashed.
        session.poll();
        assert!(session.is_crashed());
    }

    #[test]
    fn senders_on_a_crashed_session_write_nothing() {
        let (shell, peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        send_event(
            &peer,
            &EventMsg::Crashed {
                reason: "helper died".to_owned(),
            },
        );
        session.poll();
        assert!(session.is_crashed());
        // Every sender must no-op once crashed — no control frame, no panic.
        session.load("https://example.test/");
        session.reload();
        session.ime_commit_text("x".to_owned());
        session.answer_permission(true);
        session.answer_before_unload(true);
        session.set_zoom(150);
        assert_no_control_pending(&peer);
    }

    #[test]
    fn ime_senders_emit_exact_control_messages() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        session.ime_set_composition("にほ".to_owned());
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ImeSetComposition {
                text: "にほ".to_owned()
            }
        );
        session.ime_commit_text("日本語".to_owned());
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ImeCommitText {
                text: "日本語".to_owned()
            }
        );
        session.ime_finish_composition();
        assert_eq!(read_control(&mut peer), ControlMsg::ImeFinishComposition);
    }

    #[test]
    fn ime_senders_carry_empty_text_verbatim() {
        let (shell, mut peer) = UnixStream::pair().expect("socketpair");
        let mut session = WebSession::from_stream(shell, None).expect("session");
        // An empty preedit CLEARS the composition — the empty string must survive
        // the seam, not be dropped.
        session.ime_set_composition(String::new());
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ImeSetComposition {
                text: String::new()
            }
        );
        session.ime_commit_text(String::new());
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::ImeCommitText {
                text: String::new()
            }
        );
    }
}
