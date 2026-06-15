//! Terminal raw-mode + window-size helpers for `-t` runs/execs.
//! Unix (macOS/Linux) uses termios; Windows gets no-op stubs (the console runs
//! in cooked mode — interactive `-it` is degraded but non-interactive works).

#[cfg(not(unix))]
pub struct RawGuard;
#[cfg(not(unix))]
pub fn enter_raw() -> Option<RawGuard> {
    None
}
#[cfg(not(unix))]
pub fn term_size() -> (u16, u16) {
    (80, 24)
}
#[cfg(not(unix))]
pub fn is_stdin_tty() -> bool {
    false
}

#[cfg(unix)]
use std::os::unix::io::RawFd;

#[cfg(unix)]
pub struct RawGuard {
    fd: RawFd,
    orig: libc::termios,
    active: bool,
}

#[cfg(unix)]
pub fn enter_raw() -> Option<RawGuard> {
    let fd = libc::STDIN_FILENO;
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        libc::cfmakeraw(&mut raw);
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        Some(RawGuard {
            fd,
            orig,
            active: true,
        })
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
            }
        }
    }
}

/// Current terminal size (cols, rows), defaulting to 80x24.
#[cfg(unix)]
pub fn term_size() -> (u16, u16) {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            (ws.ws_col, ws.ws_row)
        } else {
            (80, 24)
        }
    }
}

#[cfg(unix)]
pub fn is_stdin_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
}
