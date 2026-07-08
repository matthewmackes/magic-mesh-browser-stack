//! Offscreen frame sink for the Chromium/CEF bridge.
//!
//! CEF's `OnPaint` callback provides a pixel buffer plus geometry. This module is
//! the engine-neutral target for that callback: it owns the `MWP1` shm channel,
//! attaches the fd to the shell once, then publishes each buffer and emits
//! `PaintReady` with the stable seqlock sequence.

use std::fmt;
use std::os::unix::net::UnixStream;

use crate::shm::{FrameChannel, FrameChannelError, PixelFormat};
use crate::wire::EventMsg;
use crate::{shm, sock};

/// Errors raised while publishing offscreen browser frames.
#[derive(Debug)]
pub enum OffscreenError {
    /// Shared-memory channel creation or publish failed.
    Frame(FrameChannelError),
    /// Socket send failed.
    Socket(std::io::Error),
}

impl fmt::Display for OffscreenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Frame(err) => write!(f, "{err}"),
            Self::Socket(err) => write!(f, "offscreen socket send failed: {err}"),
        }
    }
}

impl std::error::Error for OffscreenError {}

impl From<FrameChannelError> for OffscreenError {
    fn from(value: FrameChannelError) -> Self {
        Self::Frame(value)
    }
}

impl From<std::io::Error> for OffscreenError {
    fn from(value: std::io::Error) -> Self {
        Self::Socket(value)
    }
}

/// A shell-attached frame sink for one Chromium tab.
pub struct OffscreenFrameSink {
    channel: FrameChannel,
}

impl OffscreenFrameSink {
    /// Create the shm channel and send the `AttachFrame` fd over `stream`.
    ///
    /// # Errors
    /// Returns [`OffscreenError`] if channel creation or fd transfer fails.
    pub fn attach(
        stream: &UnixStream,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, OffscreenError> {
        let channel = shm::FrameChannel::create(max_width, max_height)?;
        sock::send_frame_with_fd(stream, &EventMsg::AttachFrame.encode(), channel.as_raw_fd())?;
        Ok(Self { channel })
    }

    /// Publish one CEF BGRA paint buffer and emit `PaintReady`.
    ///
    /// # Errors
    /// Returns [`OffscreenError`] if the buffer is invalid or the socket send fails.
    pub fn publish_bgra(
        &self,
        stream: &UnixStream,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<u64, OffscreenError> {
        self.channel
            .publish(width, height, PixelFormat::Bgra8, pixels)?;
        self.announce_paint(stream)
    }

    /// Publish one RGBA paint buffer and emit `PaintReady`.
    ///
    /// # Errors
    /// Returns [`OffscreenError`] if the buffer is invalid or the socket send fails.
    pub fn publish_rgba(
        &self,
        stream: &UnixStream,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> Result<u64, OffscreenError> {
        self.channel
            .publish(width, height, PixelFormat::Rgba8, pixels)?;
        self.announce_paint(stream)
    }

    /// Current stable sequence.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.channel.sequence()
    }

    /// Same-process channel view for diagnostics and tests.
    #[must_use]
    pub fn latest(&self) -> Option<shm::FrameView> {
        self.channel.read_latest()
    }

    fn announce_paint(&self, stream: &UnixStream) -> Result<u64, OffscreenError> {
        let seq = self.channel.sequence();
        sock::send_frame(stream, &EventMsg::PaintReady { seq }.encode())?;
        Ok(seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sock::{recv, RecvOutcome};
    use crate::wire::{take_frame, EventMsg};

    #[test]
    fn sink_attaches_frame_fd_and_announces_paint_ready() {
        let (helper, shell) = UnixStream::pair().expect("socketpair");
        let sink = OffscreenFrameSink::attach(&helper, 4, 4).expect("attach");

        let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("recv attach") else {
            panic!("expected attach data")
        };
        assert_eq!(fds.len(), 1);
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::AttachFrame
        );

        let seq = sink
            .publish_bgra(&helper, 2, 2, &[0x55; 2 * 2 * 4])
            .expect("publish");
        assert_eq!(seq % 2, 0);
        assert_eq!(seq, sink.sequence());
        let latest = sink.latest().expect("latest");
        assert_eq!((latest.width, latest.height), (2, 2));
        assert_eq!(latest.format, PixelFormat::Bgra8);

        let RecvOutcome::Data { bytes, fds } = recv(&shell).expect("recv paint") else {
            panic!("expected paint data")
        };
        assert!(fds.is_empty());
        let mut bytes = bytes;
        let payload = take_frame(&mut bytes).expect("frame").expect("payload");
        assert_eq!(
            EventMsg::decode(&payload).expect("event"),
            EventMsg::PaintReady { seq }
        );
    }
}
