//! The connection to a vessel's display agent.
//!
//! Runs on its own thread: blocking reads on the socket, decoded frames handed
//! to the UI through a double buffer, input sent back. The UI thread never
//! blocks on the network, which is the whole point — a stalled guest should
//! leave the window responsive (and resizable, and closeable), not beachballed.

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use nebula_core::display::*;
use nebula_core::ipc;

/// The most recent complete frame, plus the geometry it was drawn at.
#[derive(Default)]
pub struct FrameBuf {
    pub width: u32,
    pub height: u32,
    /// XRGB8888 pixels, tightly packed at `width * 4` (stride is normalised
    /// away on receipt so the UI never has to think about it).
    pub pixels: Vec<u32>,
    /// Bumped on every new frame; the UI redraws only when it changes.
    pub generation: u64,
}

pub struct Shared {
    pub frame: Mutex<FrameBuf>,
    /// Set when the agent goes away, so the UI can say so instead of showing
    /// a frozen last frame with no explanation.
    pub disconnected: Mutex<Option<String>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            frame: Mutex::new(FrameBuf::default()),
            disconnected: Mutex::new(None),
        })
    }
}

/// Run the connection until it ends. `on_frame` is called after each frame
/// lands so the UI can be woken; `input` carries messages to send guest-ward.
pub fn run(
    socket: &Path,
    shared: Arc<Shared>,
    input: Receiver<DisplayMsg>,
    on_frame: impl Fn() + Send + 'static,
) -> anyhow::Result<()> {
    let stream = ipc::connect(socket).map_err(|e| {
        anyhow::anyhow!(
            "cannot reach the display agent at {}: {e}\n\
             (is the vessel running, and does its rootfs carry vessel-display?)",
            socket.display()
        )
    })?;
    let mut reader = stream.try_clone()?;
    let mut writer = BufWriter::new(stream);

    write_control(
        &mut writer,
        &DisplayMsg::Hello {
            magic: DISPLAY_MAGIC,
            version: DISPLAY_PROTO_VERSION,
        },
    )?;
    writer.flush()?;

    let Some((kind, payload)) = read_msg(&mut reader)? else {
        anyhow::bail!("display agent closed the connection during the handshake");
    };
    anyhow::ensure!(kind == KIND_CONTROL, "expected a control message");
    match serde_json::from_slice::<DisplayMsg>(&payload)? {
        DisplayMsg::HelloAck {
            magic,
            version,
            source,
        } => {
            anyhow::ensure!(magic == DISPLAY_MAGIC, "not a nebula display agent");
            anyhow::ensure!(
                version == DISPLAY_PROTO_VERSION,
                "agent speaks display protocol v{version}, this viewer speaks \
                 v{DISPLAY_PROTO_VERSION} — rebuild the rootfs"
            );
            eprintln!("nebula-display: connected; guest source = {source}");
        }
        other => anyhow::bail!("expected HelloAck, got {other:?}"),
    }

    // Sender thread: input is low-rate and must not wait behind a frame read.
    let mut in_writer = writer;
    std::thread::Builder::new()
        .name("display-send".into())
        .spawn(move || {
            while let Ok(msg) = input.recv() {
                if write_control(&mut in_writer, &msg).is_err() || in_writer.flush().is_err() {
                    return;
                }
            }
        })?;

    let result = read_loop(&mut reader, &shared, &on_frame);
    let note = match &result {
        Ok(()) => "guest closed the display connection".to_string(),
        Err(e) => e.to_string(),
    };
    *shared.disconnected.lock().unwrap() = Some(note);
    on_frame();
    result
}

fn read_loop(
    reader: &mut impl std::io::Read,
    shared: &Arc<Shared>,
    on_frame: &(impl Fn() + Send + 'static),
) -> anyhow::Result<()> {
    // Geometry the guest last announced. Frames are trusted only insofar as
    // they are self-consistent; we re-derive size from the frame header.
    while let Some((kind, payload)) = read_msg(reader)? {
        match kind {
            KIND_FRAME => {
                let Some((hdr, pixels)) = parse_frame(&payload) else {
                    anyhow::bail!("malformed frame from the guest");
                };
                if hdr.format != FORMAT_XRGB8888 {
                    anyhow::bail!(
                        "guest sent format {:#x}; this viewer only decodes XRGB8888",
                        hdr.format
                    );
                }
                let mut fb = shared.frame.lock().unwrap();
                // Full-surface frames define the buffer; partial damage
                // updates a sub-rect of the buffer already there.
                let full = hdr.x == 0 && hdr.y == 0;
                if full && (fb.width != hdr.w || fb.height != hdr.h) {
                    fb.width = hdr.w;
                    fb.height = hdr.h;
                    fb.pixels = vec![0u32; (hdr.w as usize) * (hdr.h as usize)];
                }
                blit(&mut fb, &hdr, pixels);
                fb.generation = fb.generation.wrapping_add(1);
                drop(fb);
                on_frame();
            }
            KIND_CONTROL => {
                let msg: DisplayMsg = serde_json::from_slice(&payload)?;
                if let DisplayMsg::SurfaceConfig { width, height, .. } = msg {
                    let mut fb = shared.frame.lock().unwrap();
                    if fb.width != width || fb.height != height {
                        fb.width = width;
                        fb.height = height;
                        fb.pixels = vec![0u32; (width as usize) * (height as usize)];
                    }
                }
            }
            _ => {} // forward-compatible: ignore kinds we do not know
        }
    }
    Ok(())
}

/// Copy a damage rect into the frame buffer, converting stride to packed.
fn blit(fb: &mut FrameBuf, hdr: &FrameHeader, pixels: &[u8]) {
    let (fw, fh) = (fb.width as usize, fb.height as usize);
    let stride = hdr.stride as usize;
    for row in 0..hdr.h as usize {
        let dy = hdr.y as usize + row;
        if dy >= fh {
            break;
        }
        let src = &pixels[row * stride..];
        for col in 0..hdr.w as usize {
            let dx = hdr.x as usize + col;
            if dx >= fw {
                break;
            }
            let o = col * 4;
            // Little-endian XRGB8888 == softbuffer's 0RGB u32 on every
            // platform it supports, so this is a widen, not a swizzle.
            let px = u32::from_le_bytes([src[o], src[o + 1], src[o + 2], 0]);
            fb.pixels[dy * fw + dx] = px;
        }
    }
}
