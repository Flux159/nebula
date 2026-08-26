//! Where guest pixels come from.
//!
//! Two sources ship today, and the split is the point: `Shm` is the real one —
//! the contract a guest compositor implements to get on screen — and
//! `TestPattern` exists so the whole path (vsock framing, host window,
//! present, input) can be brought up and measured before any compositor
//! exists. Adding a wlr-screencopy source later is another impl of this trait
//! and nothing else changes.

use std::io;

use nebula_core::display::{ShmHeader, FORMAT_XRGB8888, SHM_HEADER_LEN, SHM_MAGIC};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
}

pub trait FrameSource: Send {
    /// Human-readable name, reported in the handshake.
    fn name(&self) -> String;

    /// Produce the next frame into `buf`, or `Ok(None)` if nothing changed
    /// since the last call (the agent then waits and asks again, rather than
    /// re-sending an identical frame).
    fn next_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<Frame>>;

    /// The host window was resized. Sources that can follow it should; the
    /// default is to ignore it and keep their own geometry, which the host
    /// letterboxes.
    fn resize(&mut self, _width: u32, _height: u32) {}
}

// --- test pattern ----------------------------------------------------------

/// An animated gradient with a box that moves one step per frame.
///
/// The motion is what makes it useful: a static pattern proves bytes arrive,
/// a moving one shows dropped frames, tearing and latency at a glance.
pub struct TestPattern {
    width: u32,
    height: u32,
    tick: u32,
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            tick: 0,
        }
    }
}

impl FrameSource for TestPattern {
    fn name(&self) -> String {
        format!("test-pattern {}x{}", self.width, self.height)
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.width = width;
            self.height = height;
        }
    }

    fn next_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<Frame>> {
        let (w, h) = (self.width, self.height);
        let stride = w * 4;
        buf.clear();
        buf.resize((stride * h) as usize, 0);

        let t = self.tick;
        // Box position: a Lissajous-ish walk so it visits the whole surface
        // instead of bouncing along one axis.
        let bw = (w / 8).max(8);
        let bh = (h / 8).max(8);
        let bx = ((t * 3) % (w.saturating_sub(bw)).max(1)) as i64;
        let by = ((t * 2) % (h.saturating_sub(bh)).max(1)) as i64;

        for y in 0..h {
            let row = (y * stride) as usize;
            for x in 0..w {
                let inside = (x as i64) >= bx
                    && (x as i64) < bx + bw as i64
                    && (y as i64) >= by
                    && (y as i64) < by + bh as i64;
                let (r, g, b) = if inside {
                    (0xffu32, 0xffu32, 0xffu32)
                } else {
                    (
                        (x * 255 / w.max(1)) & 0xff,
                        (y * 255 / h.max(1)) & 0xff,
                        (t.wrapping_mul(2)) & 0xff,
                    )
                };
                let px = (r << 16) | (g << 8) | b;
                let o = row + (x * 4) as usize;
                buf[o..o + 4].copy_from_slice(&px.to_le_bytes());
            }
        }
        self.tick = self.tick.wrapping_add(1);
        Ok(Some(Frame {
            width: w,
            height: h,
            stride,
            format: FORMAT_XRGB8888,
        }))
    }
}

// --- shared memory ---------------------------------------------------------

/// Reads frames a guest compositor publishes into a shared file.
///
/// See [`nebula_core::display::ShmHeader`] for the producer contract. We hold
/// the mapping open across frames and only re-map when the geometry in the
/// header changes, so the steady state is a seqlock read plus one memcpy.
pub struct ShmSource {
    path: String,
    map: Option<Mapping>,
    last_seq: u32,
}

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

// The pointer is only ever read through `&self`-style accessors on the single
// source thread; it is Send because the mapping is process-wide, not
// thread-local.
unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

impl ShmSource {
    pub fn new(path: String) -> Self {
        Self {
            path,
            map: None,
            last_seq: 0,
        }
    }

    /// (Re)map the file. Returns false if it does not exist yet — the
    /// compositor may simply not have started, which is not an error.
    fn ensure_mapped(&mut self, want_len: usize) -> io::Result<bool> {
        if let Some(m) = &self.map {
            if m.len >= want_len {
                return Ok(true);
            }
        }
        self.map = None;
        let c_path = std::ffi::CString::new(self.path.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "shm path has a NUL"))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(e);
        }
        let len = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            st.st_size as usize
        };
        if len < want_len.max(SHM_HEADER_LEN) {
            unsafe { libc::close(fd) };
            return Ok(false); // producer still growing the file
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        self.map = Some(Mapping {
            ptr: ptr as *mut u8,
            len,
        });
        Ok(true)
    }

    fn header(&self) -> Option<ShmHeader> {
        let m = self.map.as_ref()?;
        let bytes = unsafe { std::slice::from_raw_parts(m.ptr, SHM_HEADER_LEN.min(m.len)) };
        ShmHeader::decode(bytes)
    }
}

impl FrameSource for ShmSource {
    fn name(&self) -> String {
        format!("shm:{}", self.path)
    }

    fn next_frame(&mut self, buf: &mut Vec<u8>) -> io::Result<Option<Frame>> {
        if !self.ensure_mapped(SHM_HEADER_LEN)? {
            return Ok(None);
        }
        let Some(hdr) = self.header() else {
            // File exists but has no valid magic yet — the producer is mid
            // initialisation. Wait rather than shipping garbage.
            return Ok(None);
        };
        if hdr.magic != SHM_MAGIC || hdr.width == 0 || hdr.height == 0 {
            return Ok(None);
        }
        if !self.ensure_mapped(hdr.mapping_len())? {
            return Ok(None);
        }
        // Re-read after a possible re-map: the geometry we sized against must
        // be the geometry we copy.
        let Some(hdr) = self.header() else {
            return Ok(None);
        };

        // Seqlock: odd means a write is in flight, and seq must be unchanged
        // across the copy for the frame to be coherent.
        if hdr.seq % 2 != 0 || hdr.seq == self.last_seq {
            return Ok(None);
        }
        let m = self.map.as_ref().expect("mapped above");
        let need = hdr.mapping_len();
        if need > m.len {
            return Ok(None);
        }
        let pixels =
            unsafe { std::slice::from_raw_parts(m.ptr.add(SHM_HEADER_LEN), need - SHM_HEADER_LEN) };
        buf.clear();
        buf.extend_from_slice(pixels);

        let after = self.header().map(|h| h.seq).unwrap_or(u32::MAX);
        if after != hdr.seq {
            return Ok(None); // torn; try again next tick
        }
        self.last_seq = hdr.seq;
        Ok(Some(Frame {
            width: hdr.width,
            height: hdr.height,
            stride: hdr.stride,
            format: hdr.format,
        }))
    }
}
