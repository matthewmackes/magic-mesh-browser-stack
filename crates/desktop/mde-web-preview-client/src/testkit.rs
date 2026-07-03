//! An in-process **fake helper** — the write side of the seam.
//!
//! It stands in for the sandboxed `mde-web-preview` process so the full socket +
//! shm + texture-upload path is exercised headlessly (no Servo, no GPU).
//!
//! [`FrameWriter`] is a faithful mirror of the helper's `FrameChannel`: a
//! `memfd`-backed `MWP1` region with the same header layout + seqlock write
//! discipline. [`connect`] wires a `UnixStream` socketpair, publishes an initial
//! frame, hands the region's fd to the shell end over `SCM_RIGHTS`, and spawns a
//! thread that plays the helper — answering `Reload`/`Load` with a fresh frame +
//! `PaintReady`, and closing the socket (an honest crash) when the returned
//! [`FakeHelper`] is dropped.
//!
//! Gated behind `cfg(test)` for this crate's own tests and the `testkit` feature
//! for the shell's Browser-surface tests, so no scaffolding ships in release.

use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};

use crate::frame::{PixelFormat, HEADER_SIZE, MAGIC, VERSION};
use crate::scm::{self, RecvOutcome};
use crate::session::WebSession;
use crate::wire::{self, ControlMsg, EventMsg};

// Writer-side header offsets (the full set the writer touches — the reader in
// `frame.rs` only reads the subset it needs).
const OFF_SEQUENCE: usize = 0;
const OFF_MAGIC: usize = 8;
const OFF_VERSION: usize = 12;
const OFF_WIDTH: usize = 16;
const OFF_HEIGHT: usize = 20;
const OFF_STRIDE: usize = 24;
const OFF_FORMAT: usize = 28;
const OFF_CAPACITY: usize = 32;
const OFF_PIXEL_LEN: usize = 40;

/// The fake frame geometry `connect` publishes.
pub const FAKE_W: u32 = 8;
/// The fake frame geometry `connect` publishes.
pub const FAKE_H: u32 = 6;

/// A `memfd`-backed `MWP1` frame writer — the test/dev mirror of the helper's
/// `FrameChannel` write side.
pub struct FrameWriter {
    fd: OwnedFd,
    base: NonNull<u8>,
    len: usize,
    capacity: usize,
}

// SAFETY: the mapping is owned exclusively by this struct; cross-thread sharing is
// only through the seqlock `AtomicU64`, so it is sound to move between threads
// (the writer is moved into the helper thread by `connect`).
unsafe impl Send for FrameWriter {}

impl FrameWriter {
    /// Create a region sized for a `max_width` x `max_height` RGBA/BGRA frame.
    ///
    /// # Errors
    /// Fails if the dimensions are zero or a `memfd`/`ftruncate`/`mmap` syscall
    /// fails.
    pub fn create(max_width: u32, max_height: u32) -> io::Result<Self> {
        if max_width == 0 || max_height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero dimension",
            ));
        }
        let capacity = (max_width as usize)
            .checked_mul(max_height as usize)
            .and_then(|px| px.checked_mul(4))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "capacity overflow"))?;
        let len = HEADER_SIZE + capacity;

        let fd = nix::sys::memfd::memfd_create(
            c"mwp-testkit-frame",
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC,
        )
        .map_err(io::Error::from)?;
        nix::unistd::ftruncate(&fd, i64::try_from(len).unwrap_or(i64::MAX))
            .map_err(io::Error::from)?;

        let nz = NonZeroUsize::new(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "zero region"))?;
        // SAFETY: `fd` is a fresh writable memfd sized to `len`; mapping it SHARED
        // read/write at a kernel-chosen address is sound.
        let base = unsafe {
            mmap(
                None,
                nz,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )
            .map_err(io::Error::from)?
        };
        let writer = Self {
            fd,
            base: base.cast::<u8>(),
            len,
            capacity,
        };
        writer.put_u32(OFF_MAGIC, MAGIC);
        writer.put_u32(OFF_VERSION, VERSION);
        writer.put_u64(OFF_CAPACITY, capacity as u64);
        Ok(writer)
    }

    /// The region fd (for `SCM_RIGHTS` hand-off; the writer keeps ownership).
    #[must_use]
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// A duplicate of the region fd (for a reader that maps it independently).
    ///
    /// # Errors
    /// Fails if `dup` fails.
    pub fn dup_fd(&self) -> io::Result<OwnedFd> {
        self.fd.try_clone()
    }

    /// The current published sequence (even = a stable frame is present).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.seq().load(Ordering::Acquire)
    }

    /// Publish a frame (seqlock write). `pixels` must be `width*height*4` bytes.
    ///
    /// # Errors
    /// Fails if the pixel buffer size is wrong or exceeds capacity.
    pub fn emit(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
        pixels: &[u8],
    ) -> io::Result<()> {
        let expected = (width as usize) * (height as usize) * 4;
        if pixels.len() != expected || pixels.len() > self.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "wrong pixel buffer size",
            ));
        }
        let seq = self.seq();
        seq.fetch_add(1, Ordering::AcqRel); // -> odd (writing)
        self.put_u32(OFF_WIDTH, width);
        self.put_u32(OFF_HEIGHT, height);
        self.put_u32(OFF_STRIDE, width.saturating_mul(4));
        self.put_u32(OFF_FORMAT, format as u32);
        self.put_u64(OFF_PIXEL_LEN, pixels.len() as u64);
        // SAFETY: `pixels.len() <= capacity`; the destination is inside the mapping
        // and non-overlapping with the source.
        unsafe {
            std::ptr::copy_nonoverlapping(
                pixels.as_ptr(),
                self.base.as_ptr().add(HEADER_SIZE),
                pixels.len(),
            );
        }
        fence(Ordering::Release);
        seq.fetch_add(1, Ordering::AcqRel); // -> even (stable)
        Ok(())
    }

    #[allow(clippy::cast_ptr_alignment)]
    const fn seq(&self) -> &AtomicU64 {
        // SAFETY: offset 0 of a page-aligned mmap is 8-byte aligned and inside the
        // mapping; used only as an `AtomicU64`.
        unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(OFF_SEQUENCE).cast::<u64>()) }
    }

    fn put_u32(&self, off: usize, val: u32) {
        debug_assert!(off + 4 <= self.len);
        // SAFETY: `off + 4 <= len`; write inside the mapping, unaligned-safe.
        unsafe { std::ptr::write_unaligned(self.base.as_ptr().add(off).cast::<u32>(), val.to_le()) }
    }

    fn put_u64(&self, off: usize, val: u64) {
        debug_assert!(off + 8 <= self.len);
        // SAFETY: `off + 8 <= len`; write inside the mapping, unaligned-safe.
        unsafe { std::ptr::write_unaligned(self.base.as_ptr().add(off).cast::<u64>(), val.to_le()) }
    }
}

impl Drop for FrameWriter {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are exactly what `mmap` returned; not yet unmapped.
        unsafe {
            let _ = munmap(self.base.cast::<std::ffi::c_void>(), self.len);
        }
    }
}

/// A running fake helper. Dropping it (or calling [`FakeHelper::crash`]) stops the
/// helper thread and closes its socket end — the shell reads that as a crash.
pub struct FakeHelper {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl FakeHelper {
    /// Stop the helper immediately (close the socket → the shell sees a crash).
    pub fn crash(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FakeHelper {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// A deterministic RGBA gradient of the given geometry.
#[must_use]
pub fn gradient(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let g = u8::try_from(y * 255 / h.max(1)).unwrap_or(0);
        for x in 0..w {
            let r = u8::try_from(x * 255 / w.max(1)).unwrap_or(0);
            rgba.extend_from_slice(&[r, g, 128, 255]);
        }
    }
    rgba
}

fn write_frame(mut stream: &UnixStream, payload: &[u8]) -> io::Result<()> {
    stream.write_all(&wire::frame(payload))
}

/// Wire a session to a fresh fake helper that has already published one frame.
///
/// The initial burst (attach-fd, nav-state, title, paint-ready) is written
/// synchronously before returning, so the shell's very first
/// [`WebSession::poll`] observes a complete, uploadable frame.
///
/// # Errors
/// Fails if the socketpair or the shm region cannot be created.
pub fn connect() -> io::Result<(WebSession, FakeHelper)> {
    let (shell_end, helper_end) = UnixStream::pair()?;
    let writer = FrameWriter::create(FAKE_W, FAKE_H)?;
    writer.emit(
        FAKE_W,
        FAKE_H,
        PixelFormat::Rgba8,
        &gradient(FAKE_W, FAKE_H),
    )?;

    scm::send_frame_with_fd(
        &helper_end,
        &EventMsg::AttachFrame.encode(),
        writer.raw_fd(),
    )?;
    write_frame(
        &helper_end,
        &EventMsg::NavState {
            can_back: false,
            can_forward: false,
            loading: false,
            url: "about:blank".to_owned(),
        }
        .encode(),
    )?;
    write_frame(
        &helper_end,
        &EventMsg::Title("about:blank".to_owned()).encode(),
    )?;
    write_frame(
        &helper_end,
        &EventMsg::PaintReady {
            seq: writer.sequence(),
        }
        .encode(),
    )?;

    helper_end.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || helper_loop(&helper_end, &writer, &stop_thread));

    let session = WebSession::from_stream(shell_end, None)?;
    Ok((
        session,
        FakeHelper {
            stop,
            handle: Some(handle),
        },
    ))
}

/// The fake helper's control loop: answer `Reload`/`Load` with a fresh frame +
/// `PaintReady`; exit on stop or a closed socket.
fn helper_loop(stream: &UnixStream, writer: &FrameWriter, stop: &AtomicBool) {
    let mut rbuf = Vec::new();
    let mut tick: u8 = 0;
    while !stop.load(Ordering::SeqCst) {
        match scm::recv(stream) {
            Ok(RecvOutcome::Data { bytes, .. }) => {
                rbuf.extend_from_slice(&bytes);
                while let Ok(Some(payload)) = wire::take_frame(&mut rbuf) {
                    if let Ok(ControlMsg::Reload | ControlMsg::Load(_)) =
                        ControlMsg::decode(&payload)
                    {
                        tick = tick.wrapping_add(1);
                        let px = vec![tick; (FAKE_W * FAKE_H * 4) as usize];
                        if writer.emit(FAKE_W, FAKE_H, PixelFormat::Rgba8, &px).is_ok() {
                            let _ = write_frame(
                                stream,
                                &EventMsg::PaintReady {
                                    seq: writer.sequence(),
                                }
                                .encode(),
                            );
                        }
                    }
                }
            }
            Ok(RecvOutcome::WouldBlock) => std::thread::sleep(Duration::from_millis(2)),
            Ok(RecvOutcome::Eof) | Err(_) => break,
        }
    }
}
