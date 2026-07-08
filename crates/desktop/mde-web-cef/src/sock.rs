//! Helper-side Unix socket transport for the Chromium/CEF bridge.
//!
//! The shell already understands this BOOKMARKS-6 protocol through
//! `mde-web-preview-client`: the helper sends `AttachFrame` with the shm fd over
//! `SCM_RIGHTS`, then ordinary framed events such as `PaintReady`.

use std::io::{self, IoSlice, IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

use nix::cmsg_space;
use nix::errno::Errno;
use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};

use crate::wire;

/// One non-blocking receive result.
#[derive(Debug)]
pub enum RecvOutcome {
    /// Bytes and optional descriptors were read.
    Data {
        /// Raw framed bytes.
        bytes: Vec<u8>,
        /// Descriptors transferred with `SCM_RIGHTS`.
        fds: Vec<OwnedFd>,
    },
    /// Nothing is available on a non-blocking socket.
    WouldBlock,
    /// Peer closed the socket.
    Eof,
}

/// Send one framed payload without any descriptor.
///
/// # Errors
/// Propagates socket write errors.
pub fn send_frame(stream: &UnixStream, payload: &[u8]) -> io::Result<()> {
    let mut stream = stream;
    stream.write_all(&wire::frame(payload))
}

/// Send one framed payload with a single fd attached through `SCM_RIGHTS`.
///
/// # Errors
/// Propagates `sendmsg` errors or reports a short write.
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

/// Receive one non-blocking batch of bytes and descriptors.
///
/// # Errors
/// Propagates unexpected socket errors.
pub fn recv(stream: &UnixStream) -> io::Result<RecvOutcome> {
    let mut buf = [0u8; 8192];
    let mut cmsg = cmsg_space!([RawFd; 4]);
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
                for cmsg in msg.cmsgs()? {
                    if let ControlMessageOwned::ScmRights(raw_fds) = cmsg {
                        for fd in raw_fds {
                            // SAFETY: `recvmsg` transferred ownership of this fd.
                            fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                    }
                }
                (msg.bytes, fds)
            }
            Err(Errno::EAGAIN | Errno::EINTR) => return Ok(RecvOutcome::WouldBlock),
            Err(err) => return Err(io::Error::from(err)),
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
    use crate::shm::{FrameChannel, PixelFormat};
    use crate::wire::{take_frame, ControlMsg, EventMsg};

    #[test]
    fn event_frame_crosses_the_socket() {
        let (a, b) = UnixStream::pair().expect("pair");
        send_frame(&a, &EventMsg::PaintReady { seq: 2 }.encode()).expect("send");
        let RecvOutcome::Data { bytes, fds } = recv(&b).expect("recv") else {
            panic!("expected data")
        };
        assert!(fds.is_empty());
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::PaintReady { seq: 2 }
        );
    }

    #[test]
    fn control_frame_can_be_drained_by_the_bridge() {
        let (a, b) = UnixStream::pair().expect("pair");
        send_frame(
            &a,
            &ControlMsg::Load("https://example.test/".to_owned()).encode(),
        )
        .expect("send");
        let RecvOutcome::Data { bytes, .. } = recv(&b).expect("recv") else {
            panic!("expected data")
        };
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            ControlMsg::decode(&payload).expect("control"),
            ControlMsg::Load("https://example.test/".to_owned())
        );
    }

    #[test]
    fn attach_frame_transfers_a_shell_readable_fd() {
        let (a, b) = UnixStream::pair().expect("pair");
        let channel = FrameChannel::create(2, 2).expect("channel");
        channel
            .publish(2, 2, PixelFormat::Rgba8, &[8u8; 2 * 2 * 4])
            .expect("publish");
        send_frame_with_fd(&a, &EventMsg::AttachFrame.encode(), channel.as_raw_fd())
            .expect("send fd");
        let RecvOutcome::Data { bytes, fds } = recv(&b).expect("recv") else {
            panic!("expected data")
        };
        assert_eq!(fds.len(), 1);
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::AttachFrame
        );
    }
}
