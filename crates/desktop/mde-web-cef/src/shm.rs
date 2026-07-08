//! Shared-memory frame writer for the Chromium/CEF bridge.
//!
//! This is the helper-side half of the BOOKMARKS-6 frame contract. CEF's
//! offscreen paint callback will publish BGRA/RGBA pixels here; the shell maps
//! the same `MWP1` region with `mde-web-preview-client::frame::FrameReader`.

use std::ffi::CString;
use std::fmt;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicU64, Ordering};

use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

/// Region magic: ASCII `"MWP1"`; must match `mde-web-preview-client`.
pub const MAGIC: u32 = u32::from_le_bytes(*b"MWP1");
/// Wire-layout version.
pub const VERSION: u32 = 1;
/// Fixed header size in bytes; pixels start at this offset.
pub const HEADER_SIZE: usize = 64;

const OFF_SEQUENCE: usize = 0;
const OFF_MAGIC: usize = 8;
const OFF_VERSION: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_STRIDE: usize = 24;
const OFF_FORMAT: usize = 28;
const OFF_CAPACITY: usize = 32;
const OFF_PIXEL_LEN: usize = 40;

/// The pixel byte order published in the frame region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// 8-bit RGBA, one byte per channel.
    Rgba8 = 0,
    /// 8-bit BGRA, one byte per channel. CEF offscreen paint commonly provides BGRA.
    Bgra8 = 1,
}

impl PixelFormat {
    const fn as_u32(self) -> u32 {
        self as u32
    }

    const fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Rgba8),
            1 => Some(Self::Bgra8),
            _ => None,
        }
    }
}

/// One same-process readback of the latest frame, used by tests and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameView {
    /// Frame width in device pixels.
    pub width: u32,
    /// Frame height in device pixels.
    pub height: u32,
    /// Pixel byte order.
    pub format: PixelFormat,
    /// Pixel bytes, top-down.
    pub pixels: Vec<u8>,
}

/// Why a frame channel operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameChannelError {
    /// Dimensions were zero.
    EmptyDimensions,
    /// Capacity or mapping length overflowed.
    SizeOverflow,
    /// A Linux syscall failed.
    Os(String),
    /// Pixel bytes did not match `width * height * 4`.
    WrongPixelLen {
        /// Actual bytes supplied.
        actual: usize,
        /// Expected bytes.
        expected: usize,
    },
    /// Frame did not fit the channel capacity.
    TooLarge {
        /// Actual bytes supplied.
        actual: usize,
        /// Channel capacity.
        capacity: usize,
    },
}

impl fmt::Display for FrameChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDimensions => f.write_str("frame dimensions must be non-zero"),
            Self::SizeOverflow => f.write_str("frame region size overflow"),
            Self::Os(err) => write!(f, "frame channel syscall failed: {err}"),
            Self::WrongPixelLen { actual, expected } => {
                write!(f, "pixel buffer is {actual} bytes, expected {expected}")
            }
            Self::TooLarge { actual, capacity } => {
                write!(
                    f,
                    "frame is {actual} bytes, larger than capacity {capacity}"
                )
            }
        }
    }
}

impl std::error::Error for FrameChannelError {}

/// Writer end of one browser frame region.
pub struct FrameChannel {
    fd: OwnedFd,
    base: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: the mapping is owned by this struct; cross-thread visibility is guarded
// by the seqlock sequence at offset 0.
unsafe impl Send for FrameChannel {}

impl FrameChannel {
    /// Create a frame channel large enough for `max_width` x `max_height` pixels.
    ///
    /// # Errors
    /// Returns [`FrameChannelError`] if dimensions are invalid, size arithmetic
    /// overflows, or `memfd`/`ftruncate`/`mmap` fails.
    pub fn create(max_width: u32, max_height: u32) -> Result<Self, FrameChannelError> {
        if max_width == 0 || max_height == 0 {
            return Err(FrameChannelError::EmptyDimensions);
        }
        let capacity = (max_width as usize)
            .checked_mul(max_height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(FrameChannelError::SizeOverflow)?;
        let len = HEADER_SIZE
            .checked_add(capacity)
            .ok_or(FrameChannelError::SizeOverflow)?;

        let name = CString::new("mde-web-cef-frame")
            .map_err(|err| FrameChannelError::Os(err.to_string()))?;
        let fd = memfd_create(name.as_c_str(), MemFdCreateFlag::MFD_CLOEXEC)
            .map_err(|err| FrameChannelError::Os(err.to_string()))?;
        nix::unistd::ftruncate(
            &fd,
            i64::try_from(len).map_err(|_| FrameChannelError::SizeOverflow)?,
        )
        .map_err(|err| FrameChannelError::Os(err.to_string()))?;

        let nz = NonZeroUsize::new(len).ok_or(FrameChannelError::SizeOverflow)?;
        // SAFETY: `fd` is a fresh memfd sized to `len`, and the mapping is shared
        // read/write so the shell can observe updates after receiving the fd.
        let base = unsafe {
            mmap(
                None,
                nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
            .map_err(|err| FrameChannelError::Os(err.to_string()))?
        }
        .cast::<u8>();

        let channel = Self {
            fd,
            base,
            len,
            capacity,
        };
        channel.write_static_header();
        Ok(channel)
    }

    /// File descriptor to transfer with `SCM_RIGHTS`.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Current seqlock sequence. Even non-zero values are stable frames.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq().load(Ordering::Acquire)
    }

    /// Publish one top-down frame into the channel.
    ///
    /// # Errors
    /// Returns [`FrameChannelError`] if the pixel length is inconsistent or does
    /// not fit the channel capacity.
    pub fn publish(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: &[u8],
    ) -> Result<(), FrameChannelError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or(FrameChannelError::SizeOverflow)?;
        if pixels.len() != expected {
            return Err(FrameChannelError::WrongPixelLen {
                actual: pixels.len(),
                expected,
            });
        }
        if pixels.len() > self.capacity {
            return Err(FrameChannelError::TooLarge {
                actual: pixels.len(),
                capacity: self.capacity,
            });
        }

        let seq = self.seq();
        let start = seq.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(start % 2, 0, "overlapping frame writers");
        self.put_u32(OFF_WIDTH, width);
        self.put_u32(OFF_HEIGHT, height);
        self.put_u32(OFF_STRIDE, width.saturating_mul(4));
        self.put_u32(OFF_FORMAT, format.as_u32());
        self.put_u64(OFF_PIXEL_LEN, pixels.len() as u64);
        // SAFETY: `pixels.len() <= capacity`; the destination is inside the mmap
        // after the fixed header and does not overlap with `pixels`.
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                self.base.as_ptr().add(HEADER_SIZE),
                pixels.len(),
            );
        }
        fence(Ordering::Release);
        seq.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Read the latest stable frame from the same process.
    #[must_use]
    pub fn read_latest(&self) -> Option<FrameView> {
        let seq = self.seq();
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None;
            }
            if s1 % 2 != 0 {
                continue;
            }
            fence(Ordering::Acquire);
            let width = self.get_u32(OFF_WIDTH);
            let height = self.get_u32(OFF_HEIGHT);
            let format = PixelFormat::from_u32(self.get_u32(OFF_FORMAT))?;
            let pixel_len = usize::try_from(self.get_u64(OFF_PIXEL_LEN)).ok()?;
            if pixel_len == 0 || pixel_len > self.capacity {
                return None;
            }
            let mut pixels = vec![0u8; pixel_len];
            // SAFETY: `pixel_len <= capacity`; the source range is inside the mmap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.base.as_ptr().add(HEADER_SIZE),
                    pixels.as_mut_ptr(),
                    pixel_len,
                );
            }
            if seq.load(Ordering::Acquire) == s1 {
                return Some(FrameView {
                    width,
                    height,
                    format,
                    pixels,
                });
            }
        }
        None
    }

    fn write_static_header(&self) {
        self.put_u32(OFF_MAGIC, MAGIC);
        self.put_u32(OFF_VERSION, VERSION);
        self.put_u64(OFF_CAPACITY, self.capacity as u64);
    }

    #[allow(clippy::cast_ptr_alignment)]
    fn seq(&self) -> &AtomicU64 {
        // SAFETY: mmap returns a page-aligned base, offset 0 is 8-byte aligned,
        // and this location is only used as the seqlock atomic.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(OFF_SEQUENCE).cast::<u64>()) }
    }

    fn put_u32(&self, offset: usize, value: u32) {
        debug_assert!(offset + 4 <= self.len);
        // SAFETY: fixed header offset inside mapping; unaligned-safe write.
        unsafe {
            std::ptr::write_unaligned(self.base.as_ptr().add(offset).cast::<u32>(), value.to_le());
        }
    }

    fn put_u64(&self, offset: usize, value: u64) {
        debug_assert!(offset + 8 <= self.len);
        // SAFETY: fixed header offset inside mapping; unaligned-safe write.
        unsafe {
            std::ptr::write_unaligned(self.base.as_ptr().add(offset).cast::<u64>(), value.to_le());
        }
    }

    fn get_u32(&self, offset: usize) -> u32 {
        debug_assert!(offset + 4 <= self.len);
        // SAFETY: fixed header offset inside mapping; unaligned-safe read.
        u32::from_le(unsafe {
            std::ptr::read_unaligned(self.base.as_ptr().add(offset).cast::<u32>())
        })
    }

    fn get_u64(&self, offset: usize) -> u64 {
        debug_assert!(offset + 8 <= self.len);
        // SAFETY: fixed header offset inside mapping; unaligned-safe read.
        u64::from_le(unsafe {
            std::ptr::read_unaligned(self.base.as_ptr().add(offset).cast::<u64>())
        })
    }
}

impl Drop for FrameChannel {
    fn drop(&mut self) {
        // SAFETY: `base` and `len` come from `mmap` and have not been unmapped.
        unsafe {
            let _ = munmap(self.base.cast::<std::ffi::c_void>(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_the_shell_frame_contract() {
        assert_eq!(MAGIC.to_le_bytes(), *b"MWP1");
        assert_eq!(VERSION, 1);
        assert_eq!(HEADER_SIZE, 64);
    }

    #[test]
    fn empty_channel_has_no_frame() {
        let channel = FrameChannel::create(4, 4).expect("channel");
        assert_eq!(channel.sequence(), 0);
        assert!(channel.read_latest().is_none());
    }

    #[test]
    fn published_frame_round_trips_with_even_sequence() {
        let channel = FrameChannel::create(4, 4).expect("channel");
        let pixels = vec![0x7a; 2 * 2 * 4];
        channel
            .publish(2, 2, PixelFormat::Bgra8, &pixels)
            .expect("publish");
        assert!(channel.sequence() >= 2);
        assert_eq!(channel.sequence() % 2, 0);
        let frame = channel.read_latest().expect("frame");
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.format, PixelFormat::Bgra8);
        assert_eq!(frame.pixels, pixels);
    }

    #[test]
    fn publish_rejects_wrong_pixel_length() {
        let channel = FrameChannel::create(4, 4).expect("channel");
        assert!(matches!(
            channel.publish(2, 2, PixelFormat::Rgba8, &[1, 2, 3]),
            Err(FrameChannelError::WrongPixelLen { .. })
        ));
    }
}
