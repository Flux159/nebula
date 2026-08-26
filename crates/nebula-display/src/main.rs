//! `nebula-display` — show a vessel's framebuffer in a native host window.
//!
//! This is the host half of the display bridge, and it is deliberately the
//! *portable* half: winit gives us a real NSWindow on macOS, an HWND on
//! Windows and a Wayland/X11 surface on Linux, and softbuffer presents CPU
//! pixels into whichever of those it is. So the same binary is the viewer on
//! every host Nebula runs on, which is what lets a guest GUI be developed on
//! one and demoed on another.
//!
//! Scope, stated plainly: **one window showing one guest surface**. That is
//! the first step, not the destination. The destination — one host window per
//! guest toplevel, with the guest's GPU buffer handed to the compositor
//! without a copy — is a different pixel path (virtio-gpu blob export ->
//! CAMetalLayer / DXGI / dmabuf) behind this same protocol and this same
//! window/input plumbing. Every message here already carries a surface id for
//! that reason.

mod keymap;
mod net;

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

use nebula_core::display::DisplayMsg;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// Woken by the network thread when a frame lands.
#[derive(Debug, Clone, Copy)]
struct FrameReady;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut socket: Option<PathBuf> = None;
    let mut title = "Nebula display".to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from),
            "--title" => title = args.next().unwrap_or(title),
            "-h" | "--help" => {
                eprintln!(
                    "usage: nebula-display --socket <path> [--title <name>]\n\
                     \n\
                     <path> is a vessel's display.sock, normally\n\
                     ~/.nebula/vessels/<name>/display.sock.\n\
                     Prefer `nebula vessels display <name>`, which finds it for you."
                );
                return Ok(());
            }
            other => anyhow::bail!("unexpected argument {other:?} (try --help)"),
        }
    }
    let socket = socket.ok_or_else(|| anyhow::anyhow!("--socket is required (try --help)"))?;

    let event_loop = EventLoop::<FrameReady>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let shared = net::Shared::new();
    let (tx, rx) = channel::<DisplayMsg>();

    {
        let shared = Arc::clone(&shared);
        let socket = socket.clone();
        std::thread::Builder::new()
            .name("display-net".into())
            .spawn(move || {
                if let Err(e) = net::run(&socket, shared, rx, move || {
                    let _ = proxy.send_event(FrameReady);
                }) {
                    eprintln!("nebula-display: {e}");
                }
            })?;
    }

    let mut app = App {
        title,
        shared,
        input: tx,
        window: None,
        surface: None,
        last_generation: u64::MAX,
        // Where the guest surface sits inside the window after letterboxing:
        // (origin x, origin y, drawn width, drawn height).
        placement: (0, 0, 1, 1),
        cursor: (0.0, 0.0),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct App {
    title: String,
    shared: Arc<net::Shared>,
    input: Sender<DisplayMsg>,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
    last_generation: u64,
    placement: (i32, i32, u32, u32),
    cursor: (f64, f64),
}

impl App {
    fn send(&self, msg: DisplayMsg) {
        // A closed channel means the net thread is gone; the window will be
        // told about that through `disconnected`, so dropping input is right.
        let _ = self.input.send(msg);
    }

    /// Map a window-space position to guest surface pixels, or `None` when the
    /// pointer is over the letterbox rather than the guest surface.
    fn to_surface(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        let (ox, oy, dw, dh) = self.placement;
        let fb = self.shared.frame.lock().unwrap();
        if fb.width == 0 || fb.height == 0 || dw == 0 || dh == 0 {
            return None;
        }
        let rx = x - ox as f64;
        let ry = y - oy as f64;
        if rx < 0.0 || ry < 0.0 || rx >= dw as f64 || ry >= dh as f64 {
            return None;
        }
        Some((
            (rx * fb.width as f64 / dw as f64) as i32,
            (ry * fb.height as f64 / dh as f64) as i32,
        ))
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (self.window.as_ref(), self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return; // minimised
        };
        if let Err(e) = surface.resize(w, h) {
            eprintln!("nebula-display: surface resize failed: {e}");
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };

        let fb = self.shared.frame.lock().unwrap();
        let (ww, wh) = (size.width, size.height);

        if fb.width == 0 || fb.height == 0 {
            buffer.fill(0x0011_1114);
            self.placement = (0, 0, 0, 0);
            drop(fb);
            let _ = buffer.present();
            return; // nothing received yet, so there is no seq to ack
        }

        // Letterbox: preserve the guest's aspect ratio inside whatever the
        // user resized the window to. Sources that can follow a resize get one
        // (see the Resized handler), so this is usually a no-op scale of 1.
        let scale = (ww as f64 / fb.width as f64).min(wh as f64 / fb.height as f64);
        let dw = ((fb.width as f64 * scale) as u32).max(1).min(ww);
        let dh = ((fb.height as f64 * scale) as u32).max(1).min(wh);
        let ox = ((ww - dw) / 2) as i32;
        let oy = ((wh - dh) / 2) as i32;
        self.placement = (ox, oy, dw, dh);

        buffer.fill(0x0011_1114);
        // Nearest-neighbour, and intentionally so: this path exists to show
        // the guest honestly, and a smoothing filter would hide exactly the
        // scaling and alignment bugs a windowing system needs to see.
        for row in 0..dh {
            let sy = (row as f64 / scale) as usize;
            if sy >= fb.height as usize {
                break;
            }
            let src = &fb.pixels[sy * fb.width as usize..];
            let dst_row = (oy as u32 + row) as usize * ww as usize;
            for col in 0..dw {
                let sx = (col as f64 / scale) as usize;
                if sx >= fb.width as usize {
                    break;
                }
                buffer[dst_row + (ox as u32 + col) as usize] = src[sx];
            }
        }
        let presented = fb.seq;
        drop(fb);
        let _ = buffer.present();
        // Return credit only now: the guest's next frame is paced by what we
        // actually put on screen, so an occluded or slow window throttles the
        // producer instead of letting frames pile up in the vsock proxy.
        self.send(DisplayMsg::FrameAck {
            surface: 0,
            seq: presented,
        });
    }
}

impl ApplicationHandler<FrameReady> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(1280.0, 800.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("nebula-display: cannot create a window: {e}");
                event_loop.exit();
                return;
            }
        };
        match softbuffer::Context::new(Arc::clone(&window))
            .and_then(|ctx| softbuffer::Surface::new(&ctx, Arc::clone(&window)))
        {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                eprintln!("nebula-display: cannot create a drawing surface: {e}");
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: FrameReady) {
        let gen = self.shared.frame.lock().unwrap().generation;
        if gen != self.last_generation {
            self.last_generation = gen;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        if let Some(reason) = self.shared.disconnected.lock().unwrap().take() {
            eprintln!("nebula-display: {reason}");
            if let Some(w) = &self.window {
                w.set_title(&format!("{} — disconnected", self.title));
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.send(DisplayMsg::Close { surface: 0 });
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => {
                // Tell the guest the new size in *physical* pixels: it renders
                // real pixels, and HiDPI scaling is the host's business.
                self.send(DisplayMsg::Resize {
                    surface: 0,
                    width: size.width,
                    height: size.height,
                });
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                if let Some((x, y)) = self.to_surface(position.x, position.y) {
                    self.send(DisplayMsg::PointerMotion { surface: 0, x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let code = match button {
                    MouseButton::Left => keymap::BTN_LEFT,
                    MouseButton::Right => keymap::BTN_RIGHT,
                    MouseButton::Middle => keymap::BTN_MIDDLE,
                    _ => return,
                };
                self.send(DisplayMsg::PointerButton {
                    surface: 0,
                    button: code,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Normalise both delta flavours to pixels; the guest converts
                // to evdev's 1/120-of-a-notch units.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64 * 53.0, y as f64 * 53.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                };
                self.send(DisplayMsg::PointerAxis { surface: 0, dx, dy });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(evdev) = keymap::to_evdev(code) else {
                    return;
                };
                self.send(DisplayMsg::Key {
                    surface: 0,
                    keycode: evdev,
                    pressed: event.state == ElementState::Pressed,
                });
            }
            WindowEvent::Focused(false) => {
                // Releasing modifiers on focus loss stops the classic "guest
                // thinks Shift is still down" wedge after a cmd-tab.
                for code in [29u32, 42, 54, 56, 97, 100, 125, 126] {
                    self.send(DisplayMsg::Key {
                        surface: 0,
                        keycode: code,
                        pressed: false,
                    });
                }
            }
            _ => {}
        }
    }
}
