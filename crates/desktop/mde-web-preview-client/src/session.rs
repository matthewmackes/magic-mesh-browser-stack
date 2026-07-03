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
        })
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
                self.nav = NavState {
                    url,
                    can_back,
                    can_forward,
                    loading,
                };
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
