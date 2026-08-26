//! Host <-> guest display protocol (v1): framed messages over vsock.
//!
//! This is the pixel path that puts a graphical guest session into a window on
//! the host, on macOS, Windows and Linux alike.
//!
//! v1 is deliberately a *framebuffer* transport: CPU pixels, full frames, one
//! surface. That is the honest first step — it needs no scanout device, no
//! virtio-gpu changes, and no host GPU interop, so it works on every backend
//! we have today (including plain `vz` with no `--gpu` at all).
//!
//! It is shaped for the per-window evolution, though, and that shape is the
//! point: every message carries a `surface` id, frames carry damage rects, and
//! `FrameHeader::format` is a fourcc. Growing this into "one host window per
//! guest toplevel, zero-copy from a virtio-gpu blob" is then a change of
//! *producer* and *pixel source* — not a protocol redesign. See
//! `~/Projects/ideas/windowingsystem` for where that goes.
//!
//! Framing (little-endian): `[kind: u32][len: u32][payload; len]`
//!   - `KIND_CONTROL`: payload is one JSON [`DisplayMsg`].
//!   - `KIND_FRAME`:   payload is a 32-byte [`FrameHeader`], then raw pixels.
//!
//! Control messages flow both ways; frames only guest -> host.

use serde::{Deserialize, Serialize};

/// Guest vsock port the display agent listens on.
pub const VSOCK_PORT_DISPLAY: u32 = 1027;

/// Bumped on any wire change. Both ends refuse a mismatch.
pub const DISPLAY_PROTO_VERSION: u32 = 1;

pub const KIND_CONTROL: u32 = 1;
pub const KIND_FRAME: u32 = 2;

/// Guard against a stray connection to the wrong port speaking another
/// protocol: the first control message must be `Hello`.
pub const DISPLAY_MAGIC: u32 = 0x4e42_4453; // "NBDS"

/// `XRGB8888` — 4 bytes/pixel, blue in the low byte, high byte ignored.
///
/// Matches what `softbuffer` wants on the host (`0RGB` in a native-endian
/// u32) and what a Wayland `WL_SHM_FORMAT_XRGB8888` producer emits in the
/// guest, so the common case is a straight memcpy with no swizzle.
pub const FORMAT_XRGB8888: u32 = 0x3458_5238; // fourcc 'XR24'

/// Control-plane messages (JSON, one per `KIND_CONTROL` frame).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum DisplayMsg {
    // --- handshake -------------------------------------------------------
    /// Host -> guest, first message on the connection.
    Hello { magic: u32, version: u32 },
    /// Guest -> host, reply to `Hello`.
    HelloAck {
        magic: u32,
        version: u32,
        /// Human-readable frame source ("test-pattern", "shm:/run/...").
        source: String,
    },

    // --- guest -> host ---------------------------------------------------
    /// A surface appeared or changed geometry. Sent before its first frame,
    /// and again on every size change. v1 only ever uses `surface: 0`; the
    /// per-window bridge will use one id per xdg_toplevel.
    SurfaceConfig {
        surface: u32,
        width: u32,
        height: u32,
        /// Window title to show on the host. `None` keeps the current one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Surface destroyed; the host should close its window.
    SurfaceGone { surface: u32 },

    // --- host -> guest ---------------------------------------------------
    /// Pointer moved to a surface-local position, in surface pixels.
    PointerMotion { surface: u32, x: i32, y: i32 },
    /// `button` is a Linux evdev code (BTN_LEFT = 0x110, RIGHT = 0x111,
    /// MIDDLE = 0x112) so the guest can inject it into uinput unchanged.
    PointerButton {
        surface: u32,
        button: u32,
        pressed: bool,
    },
    /// Scroll, in surface pixels. Positive `dy` scrolls content down.
    PointerAxis { surface: u32, dx: f64, dy: f64 },
    /// `keycode` is a Linux evdev keycode (KEY_A = 30, ...), again so the
    /// guest can inject it directly. Host-specific keymaps are translated on
    /// the host side, where the platform keymap actually lives.
    Key {
        surface: u32,
        keycode: u32,
        pressed: bool,
    },
    /// The host window was resized; the guest should reconfigure its output
    /// and start producing frames at this size.
    Resize {
        surface: u32,
        width: u32,
        height: u32,
    },
    /// The host window was closed by the user.
    Close { surface: u32 },
}

/// Fixed 32-byte binary header preceding the pixels of a `KIND_FRAME` message.
///
/// `x/y/w/h` are the damage rect: the region of the surface these pixels
/// update. v1 producers send full-surface damage; the field exists so partial
/// updates need no wire change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub surface: u32,
    pub seq: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Bytes per row of the pixel payload (>= w * 4).
    pub stride: u32,
    pub format: u32,
}

pub const FRAME_HEADER_LEN: usize = 32;

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut b = [0u8; FRAME_HEADER_LEN];
        for (i, v) in [
            self.surface,
            self.seq,
            self.x,
            self.y,
            self.w,
            self.h,
            self.stride,
            self.format,
        ]
        .iter()
        .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < FRAME_HEADER_LEN {
            return None;
        }
        let f = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        Some(Self {
            surface: f(0),
            seq: f(1),
            x: f(2),
            y: f(3),
            w: f(4),
            h: f(5),
            stride: f(6),
            format: f(7),
        })
    }

    /// Pixel bytes that must follow this header.
    pub fn payload_len(&self) -> usize {
        self.stride as usize * self.h as usize
    }
}

// --- framing ---------------------------------------------------------------

/// Refuse absurd frames rather than trying to allocate them: a hostile or
/// confused peer must not be able to make us reserve gigabytes. 64 MiB is
/// comfortably above 4K XRGB (33 MiB).
pub const MAX_MSG_LEN: usize = 64 * 1024 * 1024;

/// Read one framed message. Returns `Ok(None)` on a clean EOF at a message
/// boundary (the peer hung up), which callers treat as a normal disconnect.
pub fn read_msg<R: std::io::Read>(r: &mut R) -> std::io::Result<Option<(u32, Vec<u8>)>> {
    let mut hdr = [0u8; 8];
    match r.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as usize;
    if len > MAX_MSG_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("display: message of {len} bytes exceeds cap"),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(Some((kind, payload)))
}

/// Write one framed message. Single `write_all` per part; callers that care
/// about latency should wrap the writer in a `BufWriter` and flush per frame.
pub fn write_msg<W: std::io::Write>(w: &mut W, kind: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut hdr = [0u8; 8];
    hdr[0..4].copy_from_slice(&kind.to_le_bytes());
    hdr[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&hdr)?;
    w.write_all(payload)
}

/// Write a control message.
pub fn write_control<W: std::io::Write>(w: &mut W, msg: &DisplayMsg) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)?;
    write_msg(w, KIND_CONTROL, &json)
}

/// Write a frame: header then pixels, as one `KIND_FRAME` message.
///
/// `pixels` must be exactly `hdr.payload_len()` bytes.
pub fn write_frame<W: std::io::Write>(
    w: &mut W,
    hdr: &FrameHeader,
    pixels: &[u8],
) -> std::io::Result<()> {
    debug_assert_eq!(pixels.len(), hdr.payload_len());
    let mut head = [0u8; 8];
    let len = (FRAME_HEADER_LEN + pixels.len()) as u32;
    head[0..4].copy_from_slice(&KIND_FRAME.to_le_bytes());
    head[4..8].copy_from_slice(&len.to_le_bytes());
    w.write_all(&head)?;
    w.write_all(&hdr.encode())?;
    w.write_all(pixels)
}

/// Split a `KIND_FRAME` payload into its header and pixel bytes.
pub fn parse_frame(payload: &[u8]) -> Option<(FrameHeader, &[u8])> {
    let hdr = FrameHeader::decode(payload)?;
    let pixels = payload.get(FRAME_HEADER_LEN..)?;
    // A truncated or over-long frame means the peer and we disagree about
    // geometry; refuse it instead of indexing off the end while blitting.
    if pixels.len() != hdr.payload_len() {
        return None;
    }
    Some((hdr, pixels))
}

// --- shared-memory frame source -------------------------------------------

/// Magic for the shm framebuffer header ("NBSH").
pub const SHM_MAGIC: u32 = 0x4e42_5348;
/// Bytes of header before the pixels in an shm framebuffer.
pub const SHM_HEADER_LEN: usize = 64;

/// Layout of the shared-memory framebuffer a guest compositor publishes for
/// the display agent to ship.
///
/// This is the contract an out-of-tree producer (solarflare's compositor)
/// implements to get its output into a host window, with no Wayland protocol
/// work on either side: create a file of `SHM_HEADER_LEN + stride * height`
/// bytes, mmap it shared, and drive `seq` as a **seqlock**:
///
/// 1. bump `seq` to an **odd** value (a write is in progress),
/// 2. write the pixels,
/// 3. bump `seq` to the next **even** value (the frame is stable).
///
/// The agent polls `seq`, copies only stable even generations, and retries if
/// `seq` moved while it read. That keeps a torn frame off the wire without
/// either side taking a lock, which matters because the producer is on a
/// frame deadline and the consumer is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShmHeader {
    pub magic: u32,
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    pub seq: u32,
    pub flags: u32,
}

impl ShmHeader {
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < SHM_HEADER_LEN {
            return None;
        }
        let f = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
        let h = Self {
            magic: f(0),
            version: f(1),
            width: f(2),
            height: f(3),
            stride: f(4),
            format: f(5),
            seq: f(6),
            flags: f(7),
        };
        (h.magic == SHM_MAGIC).then_some(h)
    }

    /// Total file size this header implies, header included.
    pub fn mapping_len(&self) -> usize {
        SHM_HEADER_LEN + self.stride as usize * self.height as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_roundtrips() {
        let h = FrameHeader {
            surface: 0,
            seq: 42,
            x: 1,
            y: 2,
            w: 640,
            h: 480,
            stride: 640 * 4,
            format: FORMAT_XRGB8888,
        };
        assert_eq!(FrameHeader::decode(&h.encode()), Some(h));
        assert_eq!(h.payload_len(), 640 * 480 * 4);
    }

    #[test]
    fn frame_roundtrips_through_the_wire() {
        let h = FrameHeader {
            surface: 0,
            seq: 1,
            x: 0,
            y: 0,
            w: 4,
            h: 2,
            stride: 16,
            format: FORMAT_XRGB8888,
        };
        let pixels = vec![0xABu8; h.payload_len()];
        let mut buf = Vec::new();
        write_frame(&mut buf, &h, &pixels).unwrap();

        let mut cur = std::io::Cursor::new(buf);
        let (kind, payload) = read_msg(&mut cur).unwrap().unwrap();
        assert_eq!(kind, KIND_FRAME);
        let (got, got_pixels) = parse_frame(&payload).unwrap();
        assert_eq!(got, h);
        assert_eq!(got_pixels, &pixels[..]);
        assert!(read_msg(&mut cur).unwrap().is_none(), "clean EOF");
    }

    #[test]
    fn control_roundtrips() {
        let mut buf = Vec::new();
        write_control(
            &mut buf,
            &DisplayMsg::Hello {
                magic: DISPLAY_MAGIC,
                version: DISPLAY_PROTO_VERSION,
            },
        )
        .unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let (kind, payload) = read_msg(&mut cur).unwrap().unwrap();
        assert_eq!(kind, KIND_CONTROL);
        let msg: DisplayMsg = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(msg, DisplayMsg::Hello { magic, .. } if magic == DISPLAY_MAGIC));
    }

    #[test]
    fn truncated_frame_is_rejected_not_panicked() {
        let h = FrameHeader {
            surface: 0,
            seq: 1,
            x: 0,
            y: 0,
            w: 4,
            h: 2,
            stride: 16,
            format: FORMAT_XRGB8888,
        };
        let mut payload = h.encode().to_vec();
        payload.extend_from_slice(&[0u8; 8]); // short: needs 32
        assert!(parse_frame(&payload).is_none());
    }
}
