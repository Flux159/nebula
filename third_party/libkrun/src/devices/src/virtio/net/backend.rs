use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use utils::windows::RawFd;

/// Platform error carried by backend failures: errno-style on unix
/// (the backends speak nix), io::Error on Windows.
#[cfg(unix)]
pub type BackendError = nix::Error;
#[cfg(windows)]
pub type BackendError = io::Error;

#[allow(dead_code)]
#[derive(Debug)]
pub enum ConnectError {
    InvalidAddress(BackendError),
    CreateSocket(BackendError),
    Binding(BackendError),
    SendingMagic(BackendError),
    // Tap backend errors.
    OpenNetTun(BackendError),
    TunSetIff(io::Error),
    TunSetVnetHdrSz(io::Error),
    TunSetOffload(io::Error),
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum ReadError {
    /// Nothing was written
    NothingRead,
    /// Another internal error occurred
    Internal(BackendError),
}

#[allow(dead_code)]
#[derive(Debug)]
pub enum WriteError {
    /// Nothing was written, you can drop the frame or try to resend it later
    NothingWritten,
    /// Part of the buffer was written, the write has to be finished using try_finish_write
    PartialWrite,
    /// Passt doesnt seem to be running (received EPIPE)
    ProcessNotRunning,
    /// Another internal error occurred
    Internal(BackendError),
}

pub trait NetBackend {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError>;
    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError>;
    fn has_unfinished_write(&self) -> bool;
    fn try_finish_write(&mut self, hdr_len: usize, buf: &[u8]) -> Result<(), WriteError>;
    /// Handle the worker's epoll watches for backend readiness: the socket
    /// fd on unix, a WSAEVENT handle on Windows.
    fn raw_socket_fd(&self) -> RawFd;

    /// Acknowledge/reset the readiness event source. Required on Windows
    /// (WSAEnumNetworkEvents resets the manual-reset event); no-op on unix.
    fn ack_events(&self) {}

    /// Delay in microseconds before retrying after NothingWritten.
    /// Returns 0 if no delay-based retry is needed (e.g. on Linux where
    /// EAGAIN + EPOLLET handles retries via writable events).
    #[allow(dead_code)]
    fn write_retry_delay_us(&self) -> u64 {
        0
    }
}
