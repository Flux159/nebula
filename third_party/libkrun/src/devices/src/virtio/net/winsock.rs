//! Windows virtio-net backend: a connected loopback `TcpStream` speaking the
//! same 4-byte big-endian length-prefixed ethernet framing as the unixstream
//! backend (the peer is the in-process usernet NAT thread).
//!
//! Readiness for the worker epoll comes from a manual-reset `WSAEVENT`
//! associated with `FD_READ | FD_CLOSE` — the IOCP epoll bridge watches the
//! event handle. The worker must call [`NetBackend::ack_events`] before
//! draining reads (WSAEnumNetworkEvents is what resets the event).
//!
//! Writes are nonblocking with a bounded sleep-retry on `WouldBlock`: the
//! peer is the usernet thread on the same host, which drains promptly, so
//! the deferred-write machinery (EPOLLOUT on Linux, retry timers on macOS)
//! isn't needed.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::os::windows::io::AsRawSocket;

use utils::windows::{RawFd, SendHandle};
use windows_sys::Win32::Networking::WinSock::{
    FD_CLOSE, FD_READ, SOCKET, WSACloseEvent, WSACreateEvent, WSAEnumNetworkEvents,
    WSAEventSelect, WSANETWORKEVENTS,
};

use super::backend::{ConnectError, NetBackend, ReadError, WriteError};
use super::write_virtio_net_hdr;

const FRAME_HEADER_LEN: usize = 4;

pub struct Winsock {
    stream: TcpStream,
    event: SendHandle,
    // 0 when a frame length has not been read
    expecting_frame_length: u32,
    // 0 if last write is fully complete, otherwise the length that was written
    last_partial_write_length: usize,
}

impl Winsock {
    pub fn new(stream: TcpStream) -> Result<Self, ConnectError> {
        stream.set_nodelay(true).map_err(ConnectError::Binding)?;
        stream
            .set_nonblocking(true)
            .map_err(ConnectError::Binding)?;

        let event = unsafe { WSACreateEvent() };
        if event == 0 || event == -1 {
            return Err(ConnectError::CreateSocket(std::io::Error::last_os_error()));
        }
        let rc = unsafe {
            WSAEventSelect(
                stream.as_raw_socket() as SOCKET,
                event,
                (FD_READ | FD_CLOSE) as i32,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            unsafe { WSACloseEvent(event) };
            return Err(ConnectError::CreateSocket(err));
        }

        Ok(Self {
            stream,
            event: SendHandle::new(event as RawFd),
            expecting_frame_length: 0,
            last_partial_write_length: 0,
        })
    }

    /// Try to read until filling the whole slice (mirrors unixstream).
    fn read_loop(&mut self, buf: &mut [u8], block_until_has_data: bool) -> Result<(), ReadError> {
        let mut bytes_read = 0;

        if !block_until_has_data {
            match (&self.stream).read(buf) {
                Ok(0) => return Err(ReadError::Internal(ErrorKind::ConnectionReset.into())),
                Ok(size) => bytes_read += size,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    return Err(ReadError::NothingRead);
                }
                Err(e) => return Err(ReadError::Internal(e)),
            }
        }

        // Rest of the frame: the peer writes whole frames, so any remainder
        // is in flight; spin on WouldBlock (loopback, sub-ms).
        while bytes_read < buf.len() {
            match (&self.stream).read(&mut buf[bytes_read..]) {
                Ok(0) => return Err(ReadError::Internal(ErrorKind::ConnectionReset.into())),
                Ok(size) => bytes_read += size,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::yield_now();
                }
                Err(e) => return Err(ReadError::Internal(e)),
            }
        }

        Ok(())
    }

    fn write_loop(&mut self, buf: &[u8]) -> Result<(), WriteError> {
        let mut bytes_send = 0;

        while bytes_send < buf.len() {
            match (&self.stream).write(&buf[bytes_send..]) {
                Ok(size) => bytes_send += size,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // The usernet thread drains this socket continuously;
                    // a short sleep is enough and keeps the worker simple
                    // (no deferred-write plumbing on Windows).
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    return Err(WriteError::ProcessNotRunning);
                }
                Err(e) => return Err(WriteError::Internal(e)),
            }
        }
        self.last_partial_write_length = 0;
        Ok(())
    }
}

impl NetBackend for Winsock {
    /// Try to read a frame; ReadError::NothingRead when the socket is dry.
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        if self.expecting_frame_length == 0 {
            self.expecting_frame_length = {
                let mut frame_length_buf = [0u8; FRAME_HEADER_LEN];
                self.read_loop(&mut frame_length_buf, false)?;
                u32::from_be_bytes(frame_length_buf)
            };
        }

        let hdr_len = write_virtio_net_hdr(buf);
        let buf = &mut buf[hdr_len..];
        let frame_length = self.expecting_frame_length as usize;
        self.read_loop(&mut buf[..frame_length], false)?;
        self.expecting_frame_length = 0;
        Ok(hdr_len + frame_length)
    }

    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        assert!(
            hdr_len >= FRAME_HEADER_LEN,
            "Not enough space to write the frame header"
        );
        assert!(buf.len() > hdr_len);
        let frame_length = buf.len() - hdr_len;

        buf[hdr_len - FRAME_HEADER_LEN..hdr_len]
            .copy_from_slice(&(frame_length as u32).to_be_bytes());

        self.write_loop(&buf[hdr_len - FRAME_HEADER_LEN..])
    }

    fn has_unfinished_write(&self) -> bool {
        // write_loop never leaves a partial frame behind.
        false
    }

    fn try_finish_write(&mut self, _hdr_len: usize, _buf: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }

    fn raw_socket_fd(&self) -> RawFd {
        self.event.as_raw_handle()
    }

    fn ack_events(&self) {
        let mut ev: WSANETWORKEVENTS = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            WSAEnumNetworkEvents(
                self.stream.as_raw_socket() as SOCKET,
                self.event.as_raw_handle() as isize,
                &mut ev,
            )
        };
        if rc != 0 {
            log::warn!(
                "winsock net backend: WSAEnumNetworkEvents failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

impl Drop for Winsock {
    fn drop(&mut self) {
        unsafe { WSACloseEvent(self.event.as_raw_handle() as isize) };
    }
}
