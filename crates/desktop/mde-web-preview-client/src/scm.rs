//! The file-descriptor hand-off over the per-session Unix socket (`SCM_RIGHTS`).
//!
//! The helper passes its shm frame-region fd to the shell exactly once, riding an
//! [`crate::wire::EventMsg::AttachFrame`] control message as ancillary data
//! (`SCM_RIGHTS`). Regular control/event frames carry no fd and go over the plain
//! socket write; only the attach uses [`send_frame_with_fd`]. The shell drains the
//! socket with [`recv`], which returns any bytes read plus any descriptors the
//! kernel handed over (already wrapped in [`OwnedFd`], so they close on drop).
//!
//! `recv` is non-blocking-friendly: on an empty non-blocking socket it reports
//! [`RecvOutcome::WouldBlock`] rather than erroring, and a peer close is the
//! typed [`RecvOutcome::Eof`] (which the session reads as a crash).

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::cmsg_space;
use nix::errno::Errno;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};

use crate::wire;

/// The result of one non-blocking [`recv`] drain.
#[derive(Debug)]
pub enum RecvOutcome {
    /// Bytes (and possibly descriptors) were read.
    Data {
        /// The raw bytes read (may span/split wire frames — the caller buffers).
        bytes: Vec<u8>,
        /// Any descriptors delivered as `SCM_RIGHTS` ancillary data.
        fds: Vec<OwnedFd>,
    },
    /// The socket is a non-blocking socket with nothing to read right now.
    WouldBlock,
    /// The peer closed the socket (helper exit) — read this as a crash.
    Eof,
}

/// Send a length-prefixed `payload` framed on the wire, attaching `fd` as
/// `SCM_RIGHTS` ancillary data (used for the one-time frame-region hand-off).
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

/// Drain one batch of bytes + descriptors from the socket without blocking.
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
            // Non-blocking socket with nothing ready, or an interrupted syscall —
            // the caller simply polls again next frame.
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
    use crate::testkit::FrameWriter;
    use crate::wire::{take_frame, EventMsg};

    #[test]
    fn an_fd_and_its_frame_cross_the_socket_together() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        let writer = FrameWriter::create(2, 2).expect("shm");
        writer
            .emit(2, 2, crate::frame::PixelFormat::Rgba8, &[7u8; 2 * 2 * 4])
            .expect("emit");

        // Helper side: send AttachFrame carrying the shm fd.
        send_frame_with_fd(&a, &EventMsg::AttachFrame.encode(), writer.raw_fd()).expect("send");

        // Shell side: recv the bytes + the fd, then map + read the frame.
        let outcome = recv(&b).expect("recv");
        assert!(
            matches!(&outcome, RecvOutcome::Data { fds, .. } if fds.len() == 1),
            "expected data + exactly one fd, got {outcome:?}"
        );
        let RecvOutcome::Data { bytes, mut fds } = outcome else {
            unreachable!("asserted Data above")
        };

        let mut rbuf = bytes;
        let payload = take_frame(&mut rbuf).expect("ok").expect("one frame");
        assert_eq!(
            EventMsg::decode(&payload).expect("decode"),
            EventMsg::AttachFrame
        );

        let reader = crate::frame::FrameReader::map(fds.remove(0)).expect("map");
        let snap = reader.snapshot().expect("frame");
        assert_eq!((snap.width, snap.height), (2, 2));
        assert!(snap.pixels.iter().all(|&p| p == 7));
    }

    #[test]
    fn a_closed_peer_reads_as_eof() {
        let (a, b) = UnixStream::pair().expect("socketpair");
        b.set_nonblocking(true).expect("nonblocking");
        drop(a); // helper died
        assert!(matches!(recv(&b).expect("recv"), RecvOutcome::Eof));
    }

    #[test]
    fn an_empty_nonblocking_socket_would_block() {
        let (_a, b) = UnixStream::pair().expect("socketpair");
        b.set_nonblocking(true).expect("nonblocking");
        assert!(matches!(recv(&b).expect("recv"), RecvOutcome::WouldBlock));
    }
}
