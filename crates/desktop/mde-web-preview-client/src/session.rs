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

use crate::egui::{self, ColorImage};
use crate::filter::{self, RequestFilter};
use crate::frame::FrameReader;
use crate::scm::{self, RecvOutcome};
use crate::wire::{ControlMsg, EventMsg};
use crate::{input, wire};

/// How many `recvmsg` batches one [`WebSession::poll`] drains before yielding
/// (a bound so a flooding helper can't spin the UI thread).
const MAX_RECV_PER_POLL: usize = 64;
const MAX_RECENT_RESOURCE_REQUESTS: usize = 128;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageTextStatus {
    /// Request id originally supplied by the shell.
    pub id: u64,
    /// Bounded visible text extracted by the helper.
    pub text: String,
}

/// One structured active-page scrape result from the helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageScrapeStatus {
    /// Request id originally supplied by the shell.
    pub id: u64,
    /// Bounded helper JSON body with visible text plus DOM links/headings.
    pub body: String,
}

/// One helper-observed passkey/WebAuthn ceremony request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasskeyRequestStatus {
    /// Bounded helper JSON body with public ceremony metadata.
    pub body: String,
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

/// One subresource request observed by the shell-side request filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRequestStatus {
    /// Requested subresource URL.
    pub url: String,
    /// Compact resource-type discriminant from [`crate::resource_to_wire`].
    pub resource: u8,
    /// Whether the shell allowed the request to continue.
    pub allowed: bool,
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
    last_seq: u64,
    pending: Option<ColorImage>,
    pdf_events: VecDeque<PdfSaveStatus>,
    page_text_events: VecDeque<PageTextStatus>,
    page_scrape_events: VecDeque<PageScrapeStatus>,
    passkey_events: VecDeque<PasskeyRequestStatus>,
    download_events: VecDeque<DownloadStatus>,
    popup_requests: VecDeque<PopupRequestStatus>,
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
            last_seq: 0,
            pending: None,
            pdf_events: VecDeque::new(),
            page_text_events: VecDeque::new(),
            page_scrape_events: VecDeque::new(),
            passkey_events: VecDeque::new(),
            download_events: VecDeque::new(),
            popup_requests: VecDeque::new(),
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
                let decision = self
                    .filter
                    .decide(&url, filter::resource_from_wire(resource));
                let allowed = !decision.is_block();
                if self.recent_resource_requests.len() >= MAX_RECENT_RESOURCE_REQUESTS {
                    self.recent_resource_requests.pop_front();
                }
                self.recent_resource_requests
                    .push_back(ResourceRequestStatus {
                        url,
                        resource,
                        allowed,
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

    /// Navigate to `url`.
    pub fn load(&mut self, url: impl Into<String>) {
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

    /// Find text on the current page.
    pub fn find_in_page(&mut self, query: impl Into<String>, backwards: bool) {
        self.send(&ControlMsg::FindInPage {
            query: query.into(),
            backwards,
        });
    }

    /// Clear the page-find selection/highlight where the helper supports it.
    pub fn clear_find(&mut self) {
        self.send(&ControlMsg::ClearFind);
    }

    /// Set whether tab audio is muted.
    pub fn set_audio_muted(&mut self, muted: bool) {
        self.send(&ControlMsg::SetAudioMuted { muted });
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
    /// session. So this KILLs then WAITs: `kill` stops a still-running helper and
    /// `wait` reaps it (leaving no zombie either — an already-exited child was
    /// reaped by [`Self::poll`]'s `try_wait`, so `wait` is then a cheap cached
    /// read). Best-effort: an already-gone child makes `kill` error, which is the
    /// goal state and is ignored, and `wait` never blocks on a reaped pid. A
    /// test / fake-helper session carries no child and this is a no-op.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Everything the live spawn needs to launch a sandboxed browser helper
/// (`live-helper`).
#[cfg(feature = "live-helper")]
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Path to the browser helper binary (`mde-web-preview` or `mde-web-cef`).
    pub helper_bin: std::path::PathBuf,
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
        use std::process::{Command, Stdio};
        let (shell_end, helper_end) = UnixStream::pair()?;
        let child = Command::new(&spec.helper_bin)
            .arg("tab")
            .args([
                "--url",
                &spec.url,
                "--width",
                &spec.width.to_string(),
                "--height",
                &spec.height.to_string(),
            ])
            .stdin(Stdio::from(OwnedFd::from(helper_end)))
            .spawn()?;
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
        assert_eq!(
            session.recent_resource_requests(),
            vec![ResourceRequestStatus {
                url: "https://www.google-analytics.com/collect".to_owned(),
                resource: filter::resource_to_wire(mde_adblock::ResourceType::Script),
                allowed: false,
            }]
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

        assert_eq!(
            session.drain_page_text_events(),
            vec![PageTextStatus {
                id: 7,
                text: "visible page words".to_owned(),
            }]
        );
        assert!(
            session.drain_page_text_events().is_empty(),
            "page-text events are drained exactly once"
        );
    }

    #[test]
    fn passkey_requests_are_queued_for_the_shell() {
        let (mut session, peer) = filtered_session();
        let body = r#"{"ceremony":"get","origin":"https://login.example","rp_id":"login.example","challenge_b64url":"abcdefghijklmnopqrstuvwxyz"}"#;
        send_event(
            &peer,
            &EventMsg::PasskeyRequest {
                body: body.to_owned(),
            },
        );

        session.poll();

        assert_eq!(
            session.drain_passkey_events(),
            vec![PasskeyRequestStatus {
                body: body.to_owned(),
            }]
        );
        assert!(
            session.drain_passkey_events().is_empty(),
            "passkey events are drained exactly once"
        );
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

        session.find_in_page("mesh", false);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::FindInPage {
                query: "mesh".to_owned(),
                backwards: false,
            }
        );

        session.find_in_page("mesh", true);
        assert_eq!(
            read_control(&mut peer),
            ControlMsg::FindInPage {
                query: "mesh".to_owned(),
                backwards: true,
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

    #[test]
    fn drop_reaps_the_live_helper_child_leaving_no_orphan() {
        use std::process::Command;
        use std::time::{Duration, Instant};

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
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut gone = !pid_alive(pid);
        while !gone && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            gone = !pid_alive(pid);
        }
        assert!(
            gone,
            "the helper child leaked past the session drop (orphan or zombie)"
        );
    }

    #[test]
    fn dropping_a_childless_session_is_a_safe_no_op() {
        // A test / fake-helper session carries `child: None`; its Drop must not
        // panic or block — there is nothing to reap.
        let (shell, _peer) = UnixStream::pair().expect("socketpair");
        let session = WebSession::from_stream(shell, None).expect("session");
        drop(session);
    }
}
