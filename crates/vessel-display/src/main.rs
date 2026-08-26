//! Nebula vessel-display: the guest half of the display bridge.
//!
//! Listens on vsock:1027, ships frames from a [`FrameSource`] to whichever
//! host is connected, and replays that host's input through `/dev/uinput`.
//!
//! Started by nebula-init alongside vessel-agent when the rootfs carries it.
//! One connection at a time: a second host connecting takes over, because the
//! common case is reconnecting after the viewer was closed and the alternative
//! is a stale connection wedging the port.

#[cfg(target_os = "linux")]
mod source;
#[cfg(target_os = "linux")]
mod uinput;

#[cfg(target_os = "linux")]
mod agent {
    use std::io::{BufWriter, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use nebula_core::display::*;

    use crate::source::{FrameSource, ShmSource, TestPattern};
    use crate::uinput::{Uinput, ABS_RANGE};

    /// Frame pacing when the source has nothing new. Fast enough that a fresh
    /// frame is picked up promptly, slow enough to idle at ~0% CPU.
    const IDLE_POLL: Duration = Duration::from_millis(8);

    pub struct VsockListener {
        fd: OwnedFd,
    }

    impl VsockListener {
        pub fn bind(port: u32) -> std::io::Result<Self> {
            unsafe {
                let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let fd = OwnedFd::from_raw_fd(fd);
                let mut addr: libc::sockaddr_vm = std::mem::zeroed();
                addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
                addr.svm_cid = libc::VMADDR_CID_ANY;
                addr.svm_port = port;
                if libc::bind(
                    fd.as_raw_fd(),
                    &addr as *const _ as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
                ) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::listen(fd.as_raw_fd(), 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(Self { fd })
            }
        }

        pub fn accept(&self) -> std::io::Result<std::fs::File> {
            let fd = unsafe {
                libc::accept(
                    self.fd.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        }
    }

    fn make_source() -> Box<dyn FrameSource> {
        // NEBULA_DISPLAY_SHM points at the compositor's published framebuffer.
        // Without it we serve the test pattern, so a fresh vessel shows
        // something in the host window instead of a blank one.
        match std::env::var("NEBULA_DISPLAY_SHM") {
            Ok(p) if !p.is_empty() => Box::new(ShmSource::new(p)),
            _ => {
                let w = env_u32("NEBULA_DISPLAY_WIDTH", 1280);
                let h = env_u32("NEBULA_DISPLAY_HEIGHT", 800);
                Box::new(TestPattern::new(w, h))
            }
        }
    }

    fn env_u32(key: &str, default: u32) -> u32 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(default)
    }

    /// Geometry the host last asked for, shared with the frame loop.
    struct Shared {
        want_w: AtomicU32,
        want_h: AtomicU32,
        resized: AtomicBool,
        closed: AtomicBool,
    }

    /// Serve one host connection until it goes away.
    fn serve(conn: std::fs::File, source: &Mutex<Box<dyn FrameSource>>) -> std::io::Result<()> {
        let mut reader = conn.try_clone()?;
        let mut writer = BufWriter::with_capacity(256 * 1024, conn);

        // Handshake: the host speaks first so a stray connection that says
        // nothing is rejected on its own timeout, not ours.
        let Some((kind, payload)) = read_msg(&mut reader)? else {
            return Ok(());
        };
        if kind != KIND_CONTROL {
            return Err(bad("first message was not control"));
        }
        match serde_json::from_slice::<DisplayMsg>(&payload) {
            Ok(DisplayMsg::Hello { magic, version })
                if magic == DISPLAY_MAGIC && version == DISPLAY_PROTO_VERSION => {}
            Ok(DisplayMsg::Hello { version, .. }) => {
                return Err(bad(&format!(
                    "protocol version {version}, we speak {DISPLAY_PROTO_VERSION}"
                )));
            }
            _ => return Err(bad("expected Hello")),
        }
        let name = source.lock().unwrap().name();
        write_control(
            &mut writer,
            &DisplayMsg::HelloAck {
                magic: DISPLAY_MAGIC,
                version: DISPLAY_PROTO_VERSION,
                source: name.clone(),
            },
        )?;
        writer.flush()?;
        eprintln!("vessel-display: host attached; source = {name}");

        let shared = Arc::new(Shared {
            want_w: AtomicU32::new(0),
            want_h: AtomicU32::new(0),
            resized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });

        // Input pump. Runs on its own thread so a slow uinput write can never
        // stall the frame loop (and vice versa).
        let input_shared = Arc::clone(&shared);
        let input = std::thread::Builder::new()
            .name("display-input".into())
            .spawn(move || {
                let ui = match Uinput::open() {
                    Ok(u) => Some(u),
                    Err(e) => {
                        eprintln!(
                            "vessel-display: /dev/uinput unavailable ({e}); \
                             frames will still stream, input is dropped"
                        );
                        None
                    }
                };
                // Surface size the host coordinates are relative to; needed to
                // scale into the absolute axis range uinput advertises.
                let (mut sw, mut sh) = (1u32, 1u32);
                loop {
                    match read_msg(&mut reader) {
                        Ok(Some((KIND_CONTROL, payload))) => {
                            let Ok(msg) = serde_json::from_slice::<DisplayMsg>(&payload) else {
                                continue;
                            };
                            match msg {
                                DisplayMsg::Resize { width, height, .. } => {
                                    sw = width.max(1);
                                    sh = height.max(1);
                                    input_shared.want_w.store(width, Ordering::Relaxed);
                                    input_shared.want_h.store(height, Ordering::Relaxed);
                                    input_shared.resized.store(true, Ordering::Release);
                                }
                                DisplayMsg::Close { .. } => {
                                    input_shared.closed.store(true, Ordering::Release);
                                    return;
                                }
                                _ => {
                                    let Some(ui) = &ui else { continue };
                                    let _ = dispatch(ui, msg, sw, sh);
                                }
                            }
                        }
                        Ok(Some(_)) => {}
                        // EOF or error: the host is gone.
                        Ok(None) | Err(_) => {
                            input_shared.closed.store(true, Ordering::Release);
                            return;
                        }
                    }
                }
            })?;

        // Frame loop.
        let mut buf: Vec<u8> = Vec::new();
        let mut seq: u32 = 0;
        let mut last_cfg: Option<(u32, u32)> = None;
        let mut last_send = Instant::now();

        while !shared.closed.load(Ordering::Acquire) {
            if shared.resized.swap(false, Ordering::AcqRel) {
                let (w, h) = (
                    shared.want_w.load(Ordering::Relaxed),
                    shared.want_h.load(Ordering::Relaxed),
                );
                source.lock().unwrap().resize(w, h);
            }

            let frame = source.lock().unwrap().next_frame(&mut buf)?;
            let Some(frame) = frame else {
                std::thread::sleep(IDLE_POLL);
                continue;
            };

            if last_cfg != Some((frame.width, frame.height)) {
                write_control(
                    &mut writer,
                    &DisplayMsg::SurfaceConfig {
                        surface: 0,
                        width: frame.width,
                        height: frame.height,
                        title: None,
                    },
                )?;
                last_cfg = Some((frame.width, frame.height));
            }

            let hdr = FrameHeader {
                surface: 0,
                seq,
                x: 0,
                y: 0,
                w: frame.width,
                h: frame.height,
                stride: frame.stride,
                format: frame.format,
            };
            // A source whose buffer disagrees with its own geometry is a bug
            // we would rather see than silently truncate on the host.
            if buf.len() != hdr.payload_len() {
                eprintln!(
                    "vessel-display: source produced {} bytes, header wants {}; skipping frame",
                    buf.len(),
                    hdr.payload_len()
                );
                std::thread::sleep(IDLE_POLL);
                continue;
            }
            write_frame(&mut writer, &hdr, &buf)?;
            writer.flush()?;
            seq = seq.wrapping_add(1);

            // The test pattern is always "ready", so without this it would
            // spin a core producing frames faster than anyone can show them.
            let elapsed = last_send.elapsed();
            if elapsed < IDLE_POLL {
                std::thread::sleep(IDLE_POLL - elapsed);
            }
            last_send = Instant::now();
        }

        let _ = input.join();
        eprintln!("vessel-display: host detached");
        Ok(())
    }

    fn dispatch(ui: &Uinput, msg: DisplayMsg, sw: u32, sh: u32) -> std::io::Result<()> {
        match msg {
            DisplayMsg::PointerMotion { x, y, .. } => {
                // Host sends surface pixels; uinput wants the abs range.
                let sx = (x as i64 * ABS_RANGE as i64 / sw.max(1) as i64) as i32;
                let sy = (y as i64 * ABS_RANGE as i64 / sh.max(1) as i64) as i32;
                ui.pointer_motion(sx, sy)
            }
            DisplayMsg::PointerButton {
                button, pressed, ..
            } => ui.key(button, pressed),
            DisplayMsg::PointerAxis { dx, dy, .. } => ui.scroll(dx, dy),
            DisplayMsg::Key {
                keycode, pressed, ..
            } => ui.key(keycode, pressed),
            _ => Ok(()),
        }
    }

    fn bad(msg: &str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("display: {msg}"))
    }

    pub fn main() {
        // Keep one source across connections so a compositor's shm mapping
        // survives the viewer being closed and reopened.
        let source: Mutex<Box<dyn FrameSource>> = Mutex::new(make_source());
        loop {
            let listener = match VsockListener::bind(VSOCK_PORT_DISPLAY) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!(
                        "vessel-display: bind vsock:{VSOCK_PORT_DISPLAY} failed: {e}; retrying"
                    );
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
            };
            eprintln!("vessel-display: listening on vsock:{VSOCK_PORT_DISPLAY}");
            loop {
                match listener.accept() {
                    Ok(conn) => {
                        if let Err(e) = serve(conn, &source) {
                            eprintln!("vessel-display: connection ended: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("vessel-display: accept failed: {e}");
                        break; // rebind
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    agent::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("vessel-display runs inside the guest (linux only)");
    std::process::exit(1);
}
