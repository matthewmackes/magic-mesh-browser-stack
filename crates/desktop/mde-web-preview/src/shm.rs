//! The shared-memory frame channel — the seam BOOKMARKS-6 consumes.
//!
//! The sandboxed engine renders offscreen and publishes each finished frame
//! into a single shared-memory region. BOOKMARKS-6 (the shell side) receives the
//! region's file descriptor over the per-session Unix socket (`SCM_RIGHTS`),
//! maps it read-only, and uploads the pixels to an egui texture on paint-ready.
//! This module is only the **writer** + the wire layout; BOOKMARKS-6 owns the
//! socket, the fd hand-off, and the input path.
//!
//! ## Wire layout (little-endian, fixed 64-byte header, then pixels)
//!
//! | offset | field       | type            | notes                              |
//! |-------:|-------------|-----------------|------------------------------------|
//! | 0      | `sequence`  | `u64` (atomic)  | seqlock: odd = mid-write, even = stable |
//! | 8      | `magic`     | `u32`           | [`MAGIC`] (`"MWP1"`)                |
//! | 12     | `version`   | `u32`           | [`VERSION`]                        |
//! | 16     | `width`     | `u32`           | frame width in device pixels       |
//! | 20     | `height`    | `u32`           | frame height in device pixels      |
//! | 24     | `stride`    | `u32`           | bytes per row (`width * 4`)         |
//! | 28     | `format`    | `u32`           | [`PixelFormat`] discriminant       |
//! | 32     | `capacity`  | `u64`           | pixel bytes the region can hold     |
//! | 40     | `pixel_len` | `u64`           | valid pixel bytes in the last frame |
//! | 48     | *reserved*  | 16 bytes        | zero                               |
//! | 64     | `pixels`    | `capacity` bytes| the frame, top-down                |
//!
//! A reader takes a **seqlock** snapshot: read `sequence` (retry while odd),
//! `Acquire`; read the header + pixels; re-read `sequence`; if unchanged the
//! snapshot is torn-free. Single writer, single reader, lock-free.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicU64, Ordering};

use anyhow::{ensure, Context, Result};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

/// Region magic: ASCII `"MWP1"` (mde-web-preview, layout v1).
pub const MAGIC: u32 = u32::from_le_bytes(*b"MWP1");
/// Wire-layout version.
pub const VERSION: u32 = 1;
/// Fixed header size in bytes; pixels start here.
pub const HEADER_SIZE: usize = 64;

// Field byte offsets (documented in the module table).
const OFF_SEQUENCE: usize = 0;
const OFF_MAGIC: usize = 8;
const OFF_VERSION: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_STRIDE: usize = 24;
const OFF_FORMAT: usize = 28;
const OFF_CAPACITY: usize = 32;
const OFF_PIXEL_LEN: usize = 40;

/// The pixel byte order of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// 8-bit RGBA, one byte per channel (Servo's `read_to_image` order).
    Rgba8 = 0,
    /// 8-bit BGRA, one byte per channel.
    Bgra8 = 1,
}

impl PixelFormat {
    #[must_use]
    const fn as_u32(self) -> u32 {
        self as u32
    }

    #[must_use]
    const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Rgba8),
            1 => Some(Self::Bgra8),
            _ => None,
        }
    }
}

/// An owned, read-back copy of the latest published frame (used by the headless
/// test and by any same-process reader).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameView {
    /// Frame width in device pixels.
    pub width: u32,
    /// Frame height in device pixels.
    pub height: u32,
    /// The pixel byte order.
    pub format: PixelFormat,
    /// The frame pixels (`width * height * 4` bytes, top-down).
    pub pixels: Vec<u8>,
}

/// The writer end of the shared-memory frame channel.
///
/// Backed by an anonymous `memfd`, so the fd is passable to BOOKMARKS-6 over a
/// Unix socket without a filesystem name. One channel per browser session.
pub struct FrameChannel {
    fd: OwnedFd,
    base: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: the mapping is owned exclusively by this struct for its lifetime; the
// only cross-thread sharing is through the `AtomicU64` sequence + the seqlock
// discipline, so the raw pointer is sound to move between threads.
unsafe impl Send for FrameChannel {}

impl FrameChannel {
    /// Create a channel large enough for a `max_width` x `max_height` RGBA/BGRA
    /// frame.
    ///
    /// # Errors
    /// Fails if the dimensions are zero, the `memfd`/`ftruncate`/`mmap` syscalls
    /// fail, or the size overflows.
    pub fn create(max_width: u32, max_height: u32) -> Result<Self> {
        ensure!(
            max_width > 0 && max_height > 0,
            "frame dimensions must be non-zero"
        );
        let capacity = (max_width as usize)
            .checked_mul(max_height as usize)
            .and_then(|px| px.checked_mul(4))
            .context("frame capacity overflow")?;
        let len = HEADER_SIZE
            .checked_add(capacity)
            .context("region size overflow")?;

        let fd = memfd_create(c"mde-web-preview-frame", MemFdCreateFlag::MFD_CLOEXEC)
            .context("memfd_create")?;
        nix::unistd::ftruncate(&fd, i64::try_from(len).context("region too large")?)
            .context("ftruncate")?;

        let nz = std::num::NonZeroUsize::new(len).context("zero region")?;
        // SAFETY: `fd` is a fresh, writable memfd sized to exactly `len`; mapping
        // it SHARED read/write at a kernel-chosen address is sound.
        let base = unsafe {
            mmap(
                None,
                nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
            .context("mmap frame region")?
        };
        let base = base.cast::<u8>();

        let chan = Self {
            fd,
            base,
            len,
            capacity,
        };
        chan.write_static_header(capacity);
        Ok(chan)
    }

    /// The region's file descriptor, for BOOKMARKS-6 to receive via `SCM_RIGHTS`.
    #[must_use]
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The current published sequence number (even = a stable frame is present).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq().load(Ordering::Acquire)
    }

    /// Publish a frame. `pixels` must be exactly `width * height * 4` bytes and
    /// fit the channel capacity.
    ///
    /// # Errors
    /// Fails if the pixel buffer size is wrong or exceeds the channel capacity.
    pub fn emit(&self, width: u32, height: u32, format: PixelFormat, pixels: &[u8]) -> Result<()> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(4))
            .context("pixel length overflow")?;
        ensure!(
            pixels.len() == expected,
            "pixel buffer is {} bytes, expected {expected}",
            pixels.len()
        );
        ensure!(
            pixels.len() <= self.capacity,
            "frame larger than channel capacity"
        );

        // seqlock write: bump to odd (writing), fill, bump to even (stable).
        let seq = self.seq();
        let start = seq.fetch_add(1, Ordering::AcqRel);
        debug_assert_eq!(start % 2, 0, "overlapping writers");

        self.put_u32(OFF_WIDTH, width);
        self.put_u32(OFF_HEIGHT, height);
        self.put_u32(OFF_STRIDE, width.saturating_mul(4));
        self.put_u32(OFF_FORMAT, format.as_u32());
        self.put_u64(OFF_PIXEL_LEN, pixels.len() as u64);
        // SAFETY: `pixels.len() <= capacity` (checked); the destination
        // [HEADER_SIZE, HEADER_SIZE+len) is inside the mapping and non-overlapping
        // with the source (a distinct borrow).
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

    /// Read back the latest stable frame, if one has been published.
    ///
    /// Returns `None` before the first `emit`, or if a consistent snapshot could
    /// not be taken within a bounded number of retries (a writer racing).
    #[must_use]
    pub fn read_latest(&self) -> Option<FrameView> {
        let seq = self.seq();
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 || !s1.is_multiple_of(2) {
                if s1 == 0 {
                    return None; // nothing published yet
                }
                continue; // writer mid-frame; retry
            }
            fence(Ordering::Acquire);
            let width = self.get_u32(OFF_WIDTH);
            let height = self.get_u32(OFF_HEIGHT);
            let format = PixelFormat::from_u32(self.get_u32(OFF_FORMAT))?;
            let Ok(plen) = usize::try_from(self.get_u64(OFF_PIXEL_LEN)) else {
                return None;
            };
            if plen == 0 || plen > self.capacity {
                return None;
            }
            let mut pixels = vec![0u8; plen];
            // SAFETY: `plen <= capacity`, so the source range is inside the
            // mapping; `pixels` is a fresh, non-overlapping buffer of `plen`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.base.as_ptr().add(HEADER_SIZE),
                    pixels.as_mut_ptr(),
                    plen,
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

    /// Write the fields that never change after creation.
    fn write_static_header(&self, capacity: usize) {
        self.put_u32(OFF_MAGIC, MAGIC);
        self.put_u32(OFF_VERSION, VERSION);
        self.put_u64(OFF_CAPACITY, capacity as u64);
        // sequence starts at 0 (memfd is zero-filled) == "no frame yet".
    }

    /// The atomic sequence counter living at offset 0 of the mapping.
    // The `*mut u8 -> *mut u64` cast is sound: `mmap` returns a page-aligned base
    // and `OFF_SEQUENCE` is 0, so the target is 8-byte aligned.
    #[allow(clippy::cast_ptr_alignment)]
    const fn seq(&self) -> &AtomicU64 {
        // SAFETY: offset 0 of a page-aligned mmap is 8-byte aligned and inside
        // the mapping; the region is used ONLY as an `AtomicU64` here, so shared
        // atomic access is sound for the mapping's lifetime.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(OFF_SEQUENCE).cast::<u64>()) }
    }

    fn put_u32(&self, off: usize, val: u32) {
        debug_assert!(off + 4 <= self.len);
        // SAFETY: `off + 4 <= len` (header offsets are all < HEADER_SIZE <= len);
        // the write is inside the mapping. Unaligned-safe.
        unsafe { std::ptr::write_unaligned(self.base.as_ptr().add(off).cast::<u32>(), val.to_le()) }
    }

    fn put_u64(&self, off: usize, val: u64) {
        debug_assert!(off + 8 <= self.len);
        // SAFETY: as `put_u32`, with an 8-byte field inside the header.
        unsafe { std::ptr::write_unaligned(self.base.as_ptr().add(off).cast::<u64>(), val.to_le()) }
    }

    fn get_u32(&self, off: usize) -> u32 {
        debug_assert!(off + 4 <= self.len);
        // SAFETY: read inside the mapping; unaligned-safe.
        u32::from_le(unsafe { std::ptr::read_unaligned(self.base.as_ptr().add(off).cast::<u32>()) })
    }

    fn get_u64(&self, off: usize) -> u64 {
        debug_assert!(off + 8 <= self.len);
        // SAFETY: read inside the mapping; unaligned-safe.
        u64::from_le(unsafe { std::ptr::read_unaligned(self.base.as_ptr().add(off).cast::<u64>()) })
    }
}

impl Drop for FrameChannel {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly what `mmap` returned and the mapping
        // has not been unmapped before; the fd is closed by `OwnedFd`'s drop.
        unsafe {
            let _ = munmap(self.base.cast::<std::ffi::c_void>(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_is_ascii_mwp1() {
        assert_eq!(MAGIC.to_le_bytes(), *b"MWP1");
    }

    #[test]
    fn empty_channel_reports_no_frame() {
        let chan = FrameChannel::create(4, 4).expect("create");
        assert_eq!(chan.sequence(), 0);
        assert!(chan.read_latest().is_none());
    }

    #[test]
    fn emit_then_read_round_trips_a_frame() {
        let chan = FrameChannel::create(8, 8).expect("create");
        let px: Vec<u8> = (0..64u8).collect(); // 4*4*4 bytes
        chan.emit(4, 4, PixelFormat::Rgba8, &px).expect("emit");

        // A frame has arrived on the shm channel.
        assert!(chan.sequence() >= 2 && chan.sequence().is_multiple_of(2));
        let view = chan.read_latest().expect("a frame is present");
        assert_eq!(view.width, 4);
        assert_eq!(view.height, 4);
        assert_eq!(view.format, PixelFormat::Rgba8);
        assert_eq!(view.pixels, px);
    }

    #[test]
    fn emit_rejects_a_wrongly_sized_buffer() {
        let chan = FrameChannel::create(4, 4).expect("create");
        assert!(chan.emit(4, 4, PixelFormat::Rgba8, &[0u8; 10]).is_err());
    }

    #[test]
    fn later_frame_supersedes_the_earlier_one() {
        let chan = FrameChannel::create(4, 4).expect("create");
        chan.emit(2, 2, PixelFormat::Rgba8, &[1u8; 2 * 2 * 4])
            .expect("emit 1");
        chan.emit(2, 2, PixelFormat::Bgra8, &[9u8; 2 * 2 * 4])
            .expect("emit 2");
        let view = chan.read_latest().expect("frame");
        assert_eq!(view.format, PixelFormat::Bgra8);
        assert!(view.pixels.iter().all(|&b| b == 9));
    }
}
