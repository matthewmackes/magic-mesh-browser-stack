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
//! 3. Forwards input with [`send_input`](WebSession::send_input) (scaled by
//!    `pixels_per_point`) and drives navigation.
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
                self.send(&ControlMsg::ResourceVerdict {
                    id,
                    allow: !decision.is_block(),
                });
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

    /// Go back one history entry.
    pub fn go_back(&mut self) {
        self.send(&ControlMsg::Back);
    }

    /// Go forward one history entry.
    pub fn go_forward(&mut self) {
        self.send(&ControlMsg::Forward);
    }

    /// Tell the helper the view resized to `width` x `height` device pixels.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.send(&ControlMsg::Resize { width, height });
    }

    /// Forward one egui input event, scaling pointer geometry by
    /// `pixels_per_point`. A no-op once crashed, or for an event that does not map.
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

/// Everything the live spawn needs to launch the sandboxed helper (`live-helper`).
#[cfg(feature = "live-helper")]
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// Path to the `mde-web-preview` binary.
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
    /// Spawn the real `mde-web-preview` helper and wire it the session socket.
    ///
    /// The helper end is passed as the child's stdin — a connected `AF_UNIX`
    /// socket over which it reads control frames and `SCM_RIGHTS` its shm frame fd
    /// back. Honest-gated: it needs a GPU seat and the helper's `tab` mode taught
    /// to speak this socket (the BOOKMARKS-5 follow-up).
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
        session.poll();
        assert!(session.is_crashed(), "a dead helper must surface honestly");
        assert!(matches!(session.state(), SessionState::Crashed { .. }));
    }

    #[test]
    fn two_sessions_are_isolated_across_a_crash() {
        let (mut a, helper_a) = testkit::connect().expect("connect a");
        let (mut b, _helper_b) = testkit::connect().expect("connect b");
        poll_for_frame(&mut a).expect("a frame");
        poll_for_frame(&mut b).expect("b frame");

        helper_a.crash();
        a.poll();
        b.poll();
        assert!(a.is_crashed(), "tab A crashed");
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
    fn input_after_a_crash_is_a_silent_no_op() {
        let (mut session, helper) = testkit::connect().expect("connect");
        poll_for_frame(&mut session).expect("frame");
        helper.crash();
        session.poll();
        assert!(session.is_crashed());
        // Forwarding input / nav on a crashed session must not panic and stays a
        // no-op (the socket is gone).
        session.send_input(&egui::Event::PointerMoved(egui::pos2(1.0, 2.0)), 2.0);
        session.reload();
        assert!(session.is_crashed());
    }
}
