//! The **read** side of the shared-memory frame channel — the shm half of the
//! BOOKMARKS-6 seam.
//!
//! The sandboxed helper (`mde-web-preview`, BOOKMARKS-5) owns the WRITE side: its
//! `FrameChannel` publishes each finished frame into a `memfd`-backed region with
//! a fixed 64-byte `MWP1` header and top-down RGBA/BGRA pixels, synchronised by a
//! seqlock (an atomic sequence at offset 0: odd = mid-write, even = stable). The
//! shell receives that region's fd over the session socket (`SCM_RIGHTS`, see
//! [`crate::scm`]) and hands it here.
//!
//! [`FrameReader::map`] maps the fd **read-only** (`PROT_READ`), validates the
//! header, and [`FrameReader::snapshot`] takes a tear-free seqlock snapshot — the
//! same discipline the writer's own reader uses (read the sequence, retry while
//! odd, copy, re-read; if unchanged the copy is consistent). The wire-layout
//! constants below MIRROR the writer's; they are the shared contract between the
//! two crates (the helper crate is workspace-excluded, so it cannot be a
//! dependency — only the format is shared).

use std::fmt;
use std::fs::File;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicU64, Ordering};

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

use crate::egui::{Color32, ColorImage};

/// Region magic: ASCII `"MWP1"` — must equal the writer's (`mde-web-preview`'s
/// `shm::MAGIC`). This is the compile-time anchor of the shared wire contract.
pub const MAGIC: u32 = u32::from_le_bytes(*b"MWP1");
/// Wire-layout version the reader understands.
pub const VERSION: u32 = 1;
/// Fixed header size in bytes; pixels start at this offset.
pub const HEADER_SIZE: usize = 64;

// Field byte offsets — identical to the writer's header table.
const OFF_SEQUENCE: usize = 0;
const OFF_MAGIC: usize = 8;
const OFF_VERSION: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_FORMAT: usize = 28;
// (offset 32 = the writer's `capacity`; the reader derives capacity from the
// mapping length instead, so it does not read that field.)
const OFF_PIXEL_LEN: usize = 40;

/// The pixel byte order of a published frame (mirrors the writer's `PixelFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// 8-bit RGBA, one byte per channel (Servo's read-back order).
    Rgba8 = 0,
    /// 8-bit BGRA, one byte per channel.
    Bgra8 = 1,
}

impl PixelFormat {
    const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Rgba8),
            1 => Some(Self::Bgra8),
            _ => None,
        }
    }
}

/// Why a frame region could not be mapped or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderError {
    /// The fd's region is smaller than the fixed header.
    RegionTooSmall(usize),
    /// The header magic did not match [`MAGIC`] — this is not an `MWP1` region.
    BadMagic(u32),
    /// The header version is one this reader does not understand.
    BadVersion(u32),
    /// A syscall (`fstat`/`mmap`) failed.
    Os(String),
}

impl fmt::Display for ReaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RegionTooSmall(n) => {
                write!(f, "shm region is {n} bytes, smaller than the header")
            }
            Self::BadMagic(m) => write!(f, "shm region magic {m:#010x} is not MWP1"),
            Self::BadVersion(v) => write!(f, "shm region version {v} is unsupported"),
            Self::Os(e) => write!(f, "shm mapping failed: {e}"),
        }
    }
}

impl std::error::Error for ReaderError {}

/// One tear-free snapshot of the latest published frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// Frame width in device pixels.
    pub width: u32,
    /// Frame height in device pixels.
    pub height: u32,
    /// The pixel byte order.
    pub format: PixelFormat,
    /// The frame pixels (`width * height * 4` bytes, top-down).
    pub pixels: Vec<u8>,
}

impl FrameSnapshot {
    /// Convert into an [`egui::ColorImage`] for upload to a `TextureHandle`.
    ///
    /// RGBA is uploaded directly; BGRA is swizzled to RGBA. The alpha is treated
    /// as unmultiplied (the read-back is straight alpha).
    ///
    /// # Performance
    /// `epaint` stores pixels as `Color32` internally, so building a
    /// `Vec<Color32>` is unavoidable on the CPU-readback path. The BGRA branch
    /// therefore fuses the channel reorder *into* that mandatory build in one
    /// linear pass — reading `px[2],px[1],px[0],px[3]` directly. The previous
    /// implementation instead `clone()`d the whole frame, ran a separate
    /// in-place B↔R swap pass, and *then* built the `Color32` buffer: for a
    /// 1080p frame that is one extra ~8 MB allocation and one extra full-frame
    /// scan eliminated on every published paint (see `docs/design/browser-perf-native.md`).
    #[must_use]
    pub fn to_color_image(&self) -> ColorImage {
        let size = [self.width as usize, self.height as usize];
        match self.format {
            PixelFormat::Rgba8 => ColorImage::from_rgba_unmultiplied(size, &self.pixels),
            PixelFormat::Bgra8 => {
                let pixels = self
                    .pixels
                    .chunks_exact(4)
                    .map(|px| Color32::from_rgba_unmultiplied(px[2], px[1], px[0], px[3]))
                    .collect();
                ColorImage { size, pixels }
            }
        }
    }
}

/// A read-only view of a helper's shm frame region.
pub struct FrameReader {
    // Owns the fd for the mapping's lifetime (the mapping itself keeps the
    // underlying object alive, but holding the fd keeps the accounting tidy and
    // mirrors the writer).
    _file: File,
    base: NonNull<u8>,
    len: usize,
    capacity: usize,
}

impl FrameReader {
    /// Map a received frame-region fd **read-only** and validate its header.
    ///
    /// # Errors
    /// [`ReaderError`] if the region is too small, is not an `MWP1` region of a
    /// supported version, or the `mmap` syscall fails.
    pub fn map(fd: OwnedFd) -> Result<Self, ReaderError> {
        let file = File::from(fd);
        let len = file
            .metadata()
            .map_err(|e| ReaderError::Os(e.to_string()))?
            .len();
        let len = usize::try_from(len).map_err(|_| ReaderError::RegionTooSmall(0))?;
        if len < HEADER_SIZE {
            return Err(ReaderError::RegionTooSmall(len));
        }
        let nz = NonZeroUsize::new(len).ok_or(ReaderError::RegionTooSmall(0))?;

        // SAFETY: `file` owns a valid fd sized to `len` (checked >= HEADER_SIZE);
        // mapping it PROT_READ / MAP_SHARED at a kernel-chosen address is sound.
        // We never write through this mapping (read-only view of the writer).
        let base = unsafe {
            mmap(
                None,
                nz,
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                &file,
                0,
            )
            .map_err(|e| ReaderError::Os(e.to_string()))?
        };
        let base = base.cast::<u8>();

        let reader = Self {
            _file: file,
            base,
            len,
            capacity: len - HEADER_SIZE,
        };

        let magic = reader.get_u32(OFF_MAGIC);
        if magic != MAGIC {
            return Err(ReaderError::BadMagic(magic));
        }
        let version = reader.get_u32(OFF_VERSION);
        if version != VERSION {
            return Err(ReaderError::BadVersion(version));
        }
        Ok(reader)
    }

    /// The current published sequence number (even + non-zero = a stable frame is
    /// present; zero = nothing published yet).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq().load(Ordering::Acquire)
    }

    /// Take a tear-free seqlock snapshot of the latest stable frame, if one has
    /// been published and its geometry fits the region.
    ///
    /// Returns `None` before the first frame, or if a consistent snapshot could
    /// not be taken within a bounded number of retries (a writer racing), or if
    /// the header describes an out-of-bounds frame.
    #[must_use]
    pub fn snapshot(&self) -> Option<FrameSnapshot> {
        let seq = self.seq();
        for _ in 0..64 {
            let s1 = seq.load(Ordering::Acquire);
            if s1 == 0 {
                return None; // nothing published yet
            }
            if s1 % 2 != 0 {
                continue; // odd sequence = writer mid-frame; retry
            }
            fence(Ordering::Acquire);
            let width = self.get_u32(OFF_WIDTH);
            let height = self.get_u32(OFF_HEIGHT);
            let format = PixelFormat::from_u32(self.get_u32(OFF_FORMAT))?;
            let plen = usize::try_from(self.get_u64(OFF_PIXEL_LEN)).ok()?;

            let expected = (width as usize)
                .checked_mul(height as usize)
                .and_then(|px| px.checked_mul(4))?;
            if plen == 0 || plen != expected || plen > self.capacity {
                return None;
            }

            let mut pixels = vec![0u8; plen];
            // SAFETY: `plen <= capacity`, so `[HEADER_SIZE, HEADER_SIZE+plen)` is
            // inside the mapping; `pixels` is a fresh, non-overlapping buffer.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.base.as_ptr().add(HEADER_SIZE),
                    pixels.as_mut_ptr(),
                    plen,
                );
            }
            if seq.load(Ordering::Acquire) == s1 {
                return Some(FrameSnapshot {
                    width,
                    height,
                    format,
                    pixels,
                });
            }
        }
        None
    }

    /// The atomic sequence counter at offset 0 of the mapping.
    // The `*const u8 -> *mut u64` cast is sound: `mmap` returns a page-aligned
    // base and `OFF_SEQUENCE` is 0, so the target is 8-byte aligned. We only ever
    // `load` (never store) through this handle — a read-only mapping permits that.
    #[allow(clippy::cast_ptr_alignment)]
    const fn seq(&self) -> &AtomicU64 {
        // SAFETY: offset 0 of a page-aligned mmap is 8-byte aligned and inside the
        // mapping; the region at offset 0 is used ONLY as an `AtomicU64`, and only
        // for atomic loads, so shared read access is sound for the mapping's life.
        // `NonNull::as_ptr` yields a `*mut u8`, so the target is already `*mut u64`.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(OFF_SEQUENCE).cast::<u64>()) }
    }

    fn get_u32(&self, off: usize) -> u32 {
        debug_assert!(off + 4 <= self.len);
        // SAFETY: `off + 4 <= len` (header offsets are all < HEADER_SIZE <= len);
        // the read is inside the mapping. Unaligned-safe.
        u32::from_le(unsafe { std::ptr::read_unaligned(self.base.as_ptr().add(off).cast::<u32>()) })
    }

    fn get_u64(&self, off: usize) -> u64 {
        debug_assert!(off + 8 <= self.len);
        // SAFETY: as `get_u32`, with an 8-byte field inside the header.
        u64::from_le(unsafe { std::ptr::read_unaligned(self.base.as_ptr().add(off).cast::<u64>()) })
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly what `mmap` returned and the mapping has
        // not been unmapped before; the fd is closed by `File`'s drop.
        unsafe {
            let _ = munmap(self.base.cast::<std::ffi::c_void>(), self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FrameWriter;

    #[test]
    fn magic_matches_the_writer_contract() {
        // The compile-time anchor: our MAGIC is the same "MWP1" the helper writes.
        assert_eq!(MAGIC.to_le_bytes(), *b"MWP1");
        assert_eq!(HEADER_SIZE, 64);
    }

    #[test]
    fn maps_and_reads_a_published_rgba_frame() {
        let writer = FrameWriter::create(4, 4).expect("create shm");
        let px: Vec<u8> = (0u8..64).collect(); // 4*4*4 distinct bytes
        writer.emit(4, 4, PixelFormat::Rgba8, &px).expect("emit");

        let reader = FrameReader::map(writer.dup_fd().expect("dup")).expect("map");
        assert!(reader.sequence() >= 2 && reader.sequence() % 2 == 0);
        let snap = reader.snapshot().expect("a frame is present");
        assert_eq!((snap.width, snap.height), (4, 4));
        assert_eq!(snap.format, PixelFormat::Rgba8);
        assert_eq!(snap.pixels, px);

        // And it uploads to an egui image of the right size.
        let img = snap.to_color_image();
        assert_eq!(img.size, [4, 4]);
    }

    #[test]
    fn empty_region_reports_no_frame() {
        let writer = FrameWriter::create(2, 2).expect("create shm");
        let reader = FrameReader::map(writer.dup_fd().expect("dup")).expect("map");
        assert_eq!(reader.sequence(), 0);
        assert!(reader.snapshot().is_none());
    }

    #[test]
    fn bgra_is_swizzled_to_rgba_on_upload() {
        let writer = FrameWriter::create(1, 1).expect("create shm");
        // One BGRA pixel: B=10 G=20 R=30 A=255.
        writer
            .emit(1, 1, PixelFormat::Bgra8, &[10, 20, 30, 255])
            .expect("emit");
        let reader = FrameReader::map(writer.dup_fd().expect("dup")).expect("map");
        let img = reader.snapshot().expect("frame").to_color_image();
        let p = img.pixels[0];
        assert_eq!((p.r(), p.g(), p.b()), (30, 20, 10), "R/B swizzled");
    }

    #[test]
    fn bgra_fused_conversion_swizzles_every_pixel() {
        // A 2x2 opaque BGRA frame with four distinct pixels, each stored B,G,R,A.
        // Guards the single-pass fused converter against a per-pixel indexing bug
        // that the 1x1 case above cannot catch.
        let writer = FrameWriter::create(2, 2).expect("create shm");
        let px: Vec<u8> = vec![
            1, 2, 3, 255, // px0: B=1 G=2 R=3
            4, 5, 6, 255, // px1: B=4 G=5 R=6
            7, 8, 9, 255, // px2: B=7 G=8 R=9
            10, 11, 12, 255, // px3: B=10 G=11 R=12
        ];
        writer.emit(2, 2, PixelFormat::Bgra8, &px).expect("emit");
        let reader = FrameReader::map(writer.dup_fd().expect("dup")).expect("map");
        let img = reader.snapshot().expect("frame").to_color_image();
        assert_eq!(img.size, [2, 2]);
        assert_eq!(img.pixels.len(), 4, "one Color32 per source pixel");
        // Every pixel: R and B swapped, G and A preserved (opaque passthrough).
        let expect = [(3, 2, 1), (6, 5, 4), (9, 8, 7), (12, 11, 10)];
        for (i, (r, g, b)) in expect.into_iter().enumerate() {
            let p = img.pixels[i];
            assert_eq!((p.r(), p.g(), p.b(), p.a()), (r, g, b, 255), "pixel {i}");
        }
    }

    #[test]
    fn a_non_mwp1_fd_is_rejected() {
        // A plain, correctly-sized-but-unwritten region has zero magic.
        let raw = nix::sys::memfd::memfd_create(
            c"not-mwp1",
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC,
        )
        .expect("memfd");
        nix::unistd::ftruncate(&raw, 128).expect("ftruncate");
        assert!(matches!(
            FrameReader::map(raw),
            Err(ReaderError::BadMagic(0))
        ));
    }
}
