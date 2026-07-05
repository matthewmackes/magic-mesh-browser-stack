//! The helper (write) side of the per-session Unix socket — the socket half of the
//! BOOKMARKS-6 seam, as `mde-web-preview` speaks it.
//!
//! The shell (`mde-web-preview-client`) hands this process the session socket as
//! its stdin; the `tab` serve loop uses this module to:
//!
//! * send the shm frame-region fd to the shell exactly once, riding an
//!   [`crate::wire::EventMsg::AttachFrame`] as `SCM_RIGHTS` ancillary data
//!   ([`send_frame_with_fd`]);
//! * send plain framed events afterwards (paint-ready / nav-state / …) with
//!   [`send_frame`];
//! * drain the shell's control frames without blocking ([`recv`]).
//!
//! This is a faithful mirror of the client's `scm` module — the two ends MUST
//! agree on the `SCM_RIGHTS` mechanism. The only thing that could *byte*-drift is
//! the frame encoding, and that is not defined here: [`send_frame`] /
//! [`send_frame_with_fd`] frame their payload through the SHARED [`crate::wire`]
//! module, so the on-wire bytes have a single source of truth (pinned by the
//! `protocol_golden` test). Only the syscall plumbing lives here.

use std::io::{self, IoSlice, IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::cmsg_space;
use nix::errno::Errno;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};

use crate::wire;

/// The result of one non-blocking [`recv`] drain.
#[derive(Debug)]
pub enum RecvOutcome {
    /// Bytes (and possibly descriptors) were read. The helper never expects an fd
    /// from the shell, but the field is kept for parity with the client's `scm`.
    Data {
        /// The raw bytes read (may span/split wire frames — the caller buffers).
        bytes: Vec<u8>,
        /// Any descriptors delivered as `SCM_RIGHTS` ancillary data (normally none).
        fds: Vec<OwnedFd>,
    },
    /// A non-blocking socket with nothing to read right now.
    WouldBlock,
    /// The peer closed the socket (the shell went away) — stop serving.
    Eof,
}

/// Send a plain, length-prefixed event `payload` (no fd attached).
///
/// # Errors
/// Propagates the underlying socket write failure.
pub fn send_frame(stream: &UnixStream, payload: &[u8]) -> io::Result<()> {
    let mut s: &UnixStream = stream;
    s.write_all(&wire::frame(payload))
}

/// Send a length-prefixed `payload` framed on the wire, attaching `fd` as
/// `SCM_RIGHTS` ancillary data (the one-time frame-region hand-off that rides
/// [`crate::wire::EventMsg::AttachFrame`]).
///
/// # Errors
/// Propagates a `sendmsg` failure, or a short write (which cannot occur for the
/// tiny attach frame on a fresh socket).
pub fn send_frame_with_fd(stream: &UnixStream, payload: &[u8], fd: RawFd) -> io::Result<()> {
    let framed = wire::frame(payload);
    let iov = [IoSlice::new(&framed)];
    let fds = [fd];
    let cmsgs = [ControlMessage::ScmRights(&fds)];
    let sent = sendmsg::<UnixAddr>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
        .map_err(io::Error::from)?;
    if sent != framed.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short SCM_RIGHTS attach write",
        ));
    }
    Ok(())
}

/// Drain one batch of bytes (+ any descriptors) from the socket without blocking.
///
/// # Errors
/// Propagates an unexpected `recvmsg` failure (a genuine socket error, not the
/// benign would-block / interrupted cases, which are folded into the outcome).
pub fn recv(stream: &UnixStream) -> io::Result<RecvOutcome> {
    let mut buf = [0u8; 8192];
    let mut cmsg = cmsg_space!([RawFd; 4]);
    // Scope the `iov` (which borrows `buf` mutably) so it drops before we copy the
    // bytes out of `buf` below.
    let (n, fds) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        match recvmsg::<UnixAddr>(
            stream.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::empty(),
        ) {
            Ok(msg) => {
                if msg.bytes == 0 {
                    return Ok(RecvOutcome::Eof);
                }
                let mut fds = Vec::new();
                for c in msg.cmsgs()? {
                    if let ControlMessageOwned::ScmRights(raw) = c {
                        for fd in raw {
                            // SAFETY: `recvmsg` with SCM_RIGHTS just transferred
                            // ownership of `fd` to this process; wrapping it in an
                            // `OwnedFd` gives it a single owner that closes on drop.
                            fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                    }
                }
                (msg.bytes, fds)
            }
            // Non-blocking socket with nothing ready, or an interrupted syscall.
            Err(Errno::EAGAIN | Errno::EINTR) => return Ok(RecvOutcome::WouldBlock),
            Err(e) => return Err(io::Error::from(e)),
        }
    };
    Ok(RecvOutcome::Data {
        bytes: buf[..n].to_vec(),
        fds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{take_frame, ControlMsg, EventMsg};

    #[test]
    fn a_framed_event_crosses_a_socketpair() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        send_frame(&a, &EventMsg::PaintReady { seq: 4 }.encode()).expect("send");
        let outcome = recv(&b).expect("recv");
        assert!(
            matches!(&outcome, RecvOutcome::Data { .. }),
            "expected data"
        );
        let RecvOutcome::Data { bytes, fds } = outcome else {
            unreachable!("asserted Data above")
        };
        assert!(fds.is_empty(), "a plain event carries no fd");
        let mut buf = bytes;
        let payload = take_frame(&mut buf).expect("ok").expect("one frame");
        assert_eq!(
            EventMsg::decode(&payload).expect("decode"),
            EventMsg::PaintReady { seq: 4 }
        );
    }

    #[test]
    fn a_control_frame_is_read_back_off_the_socket() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        // The shell writes a Load; the helper drains + decodes it.
        send_frame(
            &a,
            &ControlMsg::Load("https://example.test/".to_owned()).encode(),
        )
        .expect("send");
        let outcome = recv(&b).expect("recv");
        assert!(
            matches!(&outcome, RecvOutcome::Data { .. }),
            "expected data"
        );
        let RecvOutcome::Data { bytes, .. } = outcome else {
            unreachable!("asserted Data above")
        };
        let mut buf = bytes;
        let payload = take_frame(&mut buf).expect("ok").expect("one frame");
        assert_eq!(
            ControlMsg::decode(&payload).expect("decode"),
            ControlMsg::Load("https://example.test/".to_owned())
        );
    }

    #[test]
    fn an_attached_fd_arrives_with_its_frame() {
        use crate::shm::FrameChannel;
        let (a, b) = UnixStream::pair().expect("socketpair");
        let channel = FrameChannel::create(2, 2).expect("shm");
        send_frame_with_fd(&a, &EventMsg::AttachFrame.encode(), channel.as_raw_fd())
            .expect("send fd");
        let outcome = recv(&b).expect("recv");
        assert!(
            matches!(&outcome, RecvOutcome::Data { fds, .. } if fds.len() == 1),
            "expected data + exactly one fd, got {outcome:?}"
        );
        let RecvOutcome::Data { bytes, .. } = outcome else {
            unreachable!("asserted Data above")
        };
        let mut buf = bytes;
        let payload = take_frame(&mut buf).expect("ok").expect("one frame");
        assert_eq!(
            EventMsg::decode(&payload).expect("decode"),
            EventMsg::AttachFrame
        );
    }

    #[test]
    fn a_closed_peer_reads_as_eof() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        b.set_nonblocking(true).expect("nonblocking");
        drop(a);
        assert!(matches!(recv(&b).expect("recv"), RecvOutcome::Eof));
    }

    #[test]
    fn an_empty_nonblocking_socket_would_block() {
        let (_a, b) = UnixStream::pair().expect("socketpair");
        b.set_nonblocking(true).expect("nonblocking");
        assert!(matches!(recv(&b).expect("recv"), RecvOutcome::WouldBlock));
    }
}
