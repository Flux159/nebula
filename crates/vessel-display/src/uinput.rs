//! Replay host input into the guest through `/dev/uinput`.
//!
//! The host sends Linux evdev codes (it does the platform-keymap translation,
//! because that is where the platform keymap lives). We create one virtual
//! keyboard+pointer here, so anything in the guest that reads input the normal
//! way — libinput, and therefore any Wayland compositor — sees a real device
//! and needs no knowledge of Nebula at all.
//!
//! Absolute pointer positioning (`ABS_X`/`ABS_Y` rather than relative motion)
//! is deliberate: the host already knows where the cursor is inside the
//! window, and relative motion would desynchronise the two cursors on every
//! dropped event.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

// linux/uinput.h + linux/input-event-codes.h. Hand-declared rather than
// pulling a crate in: the guest binary ships in the rootfs and every
// dependency is bytes we pay for on every boot.
const UI_SET_EVBIT: libc::Ioctl = 0x4004_5564;
const UI_SET_KEYBIT: libc::Ioctl = 0x4004_5565;
const UI_SET_ABSBIT: libc::Ioctl = 0x4004_5567;
const UI_SET_RELBIT: libc::Ioctl = 0x4004_5566;
const UI_DEV_SETUP: libc::Ioctl = 0x4055_5503;
const UI_ABS_SETUP: libc::Ioctl = 0x401c_5504;
const UI_DEV_CREATE: libc::Ioctl = 0x0000_5501;
const UI_DEV_DESTROY: libc::Ioctl = 0x0000_5502;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

/// Highest key code we enable. Covers the full keyboard plus the BTN_* range
/// (BTN_LEFT = 0x110) that pointer buttons live in.
const KEY_MAX_ENABLED: u16 = 0x2ff;

/// Absolute axis range we advertise. The host scales window coordinates into
/// this space, so the guest resolution can change without renegotiating.
pub const ABS_RANGE: i32 = 65535;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    // The kernel struct is `__u16 code; struct input_absinfo absinfo;` and
    // input_absinfo is 4-byte aligned, so there are 2 bytes of padding here.
    _pad: u16,
    absinfo: InputAbsinfo,
}

#[repr(C)]
struct InputEvent {
    // 64-bit time_t on every arch we target (arm64/x86_64 Linux).
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

pub struct Uinput {
    fd: File,
}

impl Uinput {
    /// Create the virtual device. Fails if the kernel lacks
    /// `CONFIG_INPUT_UINPUT` or we are not root.
    pub fn open() -> io::Result<Self> {
        let fd = OpenOptions::new()
            .write(true)
            .read(false)
            .open("/dev/uinput")?;
        let raw = fd.as_raw_fd();

        let set = |req: libc::Ioctl, bit: libc::c_ulong| -> io::Result<()> {
            if unsafe { libc::ioctl(raw, req, bit) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };

        set(UI_SET_EVBIT, EV_KEY as libc::c_ulong)?;
        set(UI_SET_EVBIT, EV_ABS as libc::c_ulong)?;
        set(UI_SET_EVBIT, EV_REL as libc::c_ulong)?;
        set(UI_SET_EVBIT, EV_SYN as libc::c_ulong)?;
        for code in 1..=KEY_MAX_ENABLED {
            set(UI_SET_KEYBIT, code as libc::c_ulong)?;
        }
        set(UI_SET_ABSBIT, ABS_X as libc::c_ulong)?;
        set(UI_SET_ABSBIT, ABS_Y as libc::c_ulong)?;
        set(UI_SET_RELBIT, REL_WHEEL_HI_RES as libc::c_ulong)?;
        set(UI_SET_RELBIT, REL_HWHEEL_HI_RES as libc::c_ulong)?;

        for code in [ABS_X, ABS_Y] {
            let abs = UinputAbsSetup {
                code,
                _pad: 0,
                absinfo: InputAbsinfo {
                    value: 0,
                    minimum: 0,
                    maximum: ABS_RANGE,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            if unsafe { libc::ioctl(raw, UI_ABS_SETUP, &abs as *const _) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let mut name = [0u8; 80];
        let label = b"nebula-display";
        name[..label.len()].copy_from_slice(label);
        let setup = UinputSetup {
            id: InputId {
                bustype: 0x06, // BUS_VIRTUAL
                vendor: 0x1af4,
                product: 0x0001,
                version: 1,
            },
            name,
            ff_effects_max: 0,
        };
        if unsafe { libc::ioctl(raw, UI_DEV_SETUP, &setup as *const _) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::ioctl(raw, UI_DEV_CREATE, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }

    fn emit(&self, type_: u16, code: u16, value: i32) -> io::Result<()> {
        let ev = InputEvent {
            tv_sec: 0,
            tv_usec: 0,
            type_,
            code,
            value,
        };
        let n = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &ev as *const _ as *const libc::c_void,
                std::mem::size_of::<InputEvent>(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        self.emit(EV_SYN, SYN_REPORT, 0)
    }

    /// Move the pointer. `x`/`y` are already scaled into `0..=ABS_RANGE`.
    pub fn pointer_motion(&self, x: i32, y: i32) -> io::Result<()> {
        self.emit(EV_ABS, ABS_X, x.clamp(0, ABS_RANGE))?;
        self.emit(EV_ABS, ABS_Y, y.clamp(0, ABS_RANGE))?;
        self.sync()
    }

    pub fn key(&self, code: u32, pressed: bool) -> io::Result<()> {
        if code == 0 || code > KEY_MAX_ENABLED as u32 {
            return Ok(()); // unmapped on the host side; drop rather than error
        }
        self.emit(EV_KEY, code as u16, i32::from(pressed))?;
        self.sync()
    }

    /// High-res scroll: the kernel's unit is 1/120 of a notch, and the host
    /// sends pixels, so 1 notch = 120 units = ~53px matches the usual
    /// libinput convention closely enough for a dev loop.
    pub fn scroll(&self, dx: f64, dy: f64) -> io::Result<()> {
        let to_units = |v: f64| (v * 120.0 / 53.0).round() as i32;
        let (hx, hy) = (to_units(dx), to_units(-dy));
        if hy != 0 {
            self.emit(EV_REL, REL_WHEEL_HI_RES, hy)?;
        }
        if hx != 0 {
            self.emit(EV_REL, REL_HWHEEL_HI_RES, hx)?;
        }
        if hx != 0 || hy != 0 {
            self.sync()?;
        }
        Ok(())
    }
}

impl Drop for Uinput {
    fn drop(&mut self) {
        unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}
