//! Windows host-side vsock backend.
//!
//! On unix hosts, `krun_add_vsock_port` maps a guest vsock port to a unix
//! socket path. Windows has no unix sockets, so the same path becomes a
//! *port file*: for listening ports libkrun binds a loopback `TcpListener`
//! and writes the chosen port number (decimal text) to the file; for
//! outgoing ports libkrun reads the port number from the file and connects
//! to `127.0.0.1:<port>`. This matches nebula's `ipc` module convention so
//! the host side needs no changes beyond using TCP.
//!
//! The proxies plug into the existing muxer state machine: each socket is
//! registered with a manual-reset `WSAEVENT` via `WSAEventSelect`, and that
//! event handle is what the muxer epoll (IOCP wait-completion-packet
//! bridge) watches. `process_event` decodes the actual readiness with
//! `WSAEnumNetworkEvents` (which also resets the event).
//!
//! WSAEventSelect subtlety: `FD_READ` is only re-recorded when new data
//! arrives or a `recv` leaves data behind. If we stop reading because the
//! guest is out of credit, buffered data will never re-signal the event —
//! so every path that re-enables read interest (credit update, op
//! response) immediately drains the socket instead of waiting for the
//! event.

use super::{
    defs,
    defs::uapi,
    proxy::{ProxyRemoval, RecvPkt},
};

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::num::Wrapping;
use std::os::windows::io::AsRawSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::super::Queue as VirtQueue;
use super::super::linux_errno::linux_error;
use super::muxer::{MuxerRx, push_packet};
use super::muxer_rxq::MuxerRxQ;
use super::packet::{TsiAcceptReq, TsiConnectReq, TsiListenReq, TsiSendtoAddr, VsockPacket};
use super::proxy::{NewProxyType, Proxy, ProxyError, ProxyStatus, ProxyUpdate};
use utils::epoll::EventSet;
use utils::windows::{AsRawFd, RawFd, SendHandle};

use vm_memory::GuestMemoryMmap;

use windows_sys::Win32::Networking::WinSock::{
    FD_ACCEPT, FD_CLOSE, FD_CLOSE_BIT, FD_READ, SOCKET, WSACloseEvent, WSACreateEvent,
    WSAEnumNetworkEvents, WSAEventSelect, WSANETWORKEVENTS,
};

/// windows-sys models WSAEVENT as `isize`; our epoll/SendHandle side uses
/// `HANDLE` (`*mut c_void`). Same kernel handle, two spellings.
fn wsaevent_to_handle(ev: isize) -> RawFd {
    ev as RawFd
}

fn handle_to_wsaevent(h: RawFd) -> isize {
    h as isize
}

/// Convert an io::Error into a negative Linux errno for the guest.
fn neg_linux_errno(e: std::io::Error) -> i32 {
    -linux_error(e).raw_os_error().unwrap_or(5 /* EIO */)
}

/// Create a manual-reset WSAEVENT and associate it with `socket` for the
/// given network-event mask.
fn wsa_event_for(socket: SOCKET, mask: u32) -> std::io::Result<SendHandle> {
    let event = unsafe { WSACreateEvent() };
    if event == 0 || event == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let rc = unsafe { WSAEventSelect(socket, event, mask as i32) };
    if rc != 0 {
        unsafe { WSACloseEvent(event) };
        return Err(std::io::Error::last_os_error());
    }
    Ok(SendHandle::new(wsaevent_to_handle(event)))
}

/// Decoded result of WSAEnumNetworkEvents.
struct NetEvents {
    readable: bool,
    closed: bool,
    close_error: i32,
}

fn enum_net_events(socket: SOCKET, event: SendHandle) -> NetEvents {
    let mut ev: WSANETWORKEVENTS = unsafe { std::mem::zeroed() };
    let rc =
        unsafe { WSAEnumNetworkEvents(socket, handle_to_wsaevent(event.as_raw_handle()), &mut ev) };
    if rc != 0 {
        warn!(
            "WSAEnumNetworkEvents failed: {}",
            std::io::Error::last_os_error()
        );
        return NetEvents {
            readable: false,
            closed: false,
            close_error: 0,
        };
    }
    NetEvents {
        readable: ev.lNetworkEvents as u32 & (FD_READ | FD_ACCEPT) != 0,
        closed: ev.lNetworkEvents as u32 & FD_CLOSE != 0,
        close_error: ev.iErrorCode[FD_CLOSE_BIT as usize],
    }
}

/// Read the loopback TCP port from a port file written by the host process.
fn read_port_file(path: &Path) -> std::io::Result<u16> {
    let text = std::fs::read_to_string(path)?;
    text.trim().parse::<u16>().map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid port file {path:?}"),
        )
    })
}

pub struct TcpProxy {
    /// Host peer half-closed its write side; guest already got SHUTDOWN(SEND).
    half_closed: bool,
    id: u64,
    cid: u64,
    stream: Option<TcpStream>,
    event: Option<SendHandle>,
    pub status: ProxyStatus,
    mem: GuestMemoryMmap,
    queue: Arc<Mutex<VirtQueue>>,
    rxq: Arc<Mutex<MuxerRxQ>>,
    path: PathBuf,
    peer_port: u32,
    local_port: u32,
    control_port: u32,
    peer_fwd_cnt: Wrapping<u32>,
    peer_buf_alloc: u32,
    tx_cnt: Wrapping<u32>,
    last_tx_cnt_sent: Wrapping<u32>,
    push_cnt: Wrapping<u32>,
    rx_cnt: Wrapping<u32>,
}

impl TcpProxy {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u64,
        cid: u64,
        local_port: u32,
        control_port: u32,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
        path: PathBuf,
    ) -> Result<Self, ProxyError> {
        Ok(TcpProxy {
            id,
            cid,
            local_port,
            peer_port: 0,
            control_port,
            stream: None,
            event: None,
            half_closed: false,
            status: ProxyStatus::Idle,
            mem,
            queue,
            rxq,
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            path,
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            push_cnt: Wrapping(0),
            rx_cnt: Wrapping(0),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_reverse(
        id: u64,
        cid: u64,
        local_port: u32,
        peer_port: u32,
        stream: TcpStream,
        mem: GuestMemoryMmap,
        queue: Arc<Mutex<VirtQueue>>,
        rxq: Arc<Mutex<MuxerRxQ>>,
    ) -> Self {
        debug!("new_reverse: id={id} local_port={local_port} peer_port={peer_port}");
        let _ = stream.set_nonblocking(true);
        let event = wsa_event_for(stream.as_raw_socket() as SOCKET, FD_READ | FD_CLOSE)
            .map_err(|e| warn!("new_reverse: event create failed: {e}"))
            .ok();
        TcpProxy {
            id,
            cid,
            local_port,
            peer_port,
            control_port: 0,
            stream: Some(stream),
            event,
            half_closed: false,
            status: ProxyStatus::ReverseInit,
            mem,
            queue,
            rxq,
            rx_cnt: Wrapping(0),
            tx_cnt: Wrapping(0),
            last_tx_cnt_sent: Wrapping(0),
            peer_buf_alloc: 0,
            peer_fwd_cnt: Wrapping(0),
            push_cnt: Wrapping(0),
            path: Default::default(),
        }
    }

    fn push_connect_rsp(&self, result: i32) {
        debug!(
            "push_connect_rsp: id: {}, control_port: {}, result: {}",
            self.id, self.control_port, result
        );

        // This response goes to the control port (DGRAM).
        let rx = MuxerRx::ConnResponse {
            local_port: 1025,
            peer_port: self.control_port,
            result,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn push_reset(&self) {
        debug!(
            "push_reset: id: {}, peer_port: {}, local_port: {}",
            self.id, self.peer_port, self.local_port
        );

        let rx = MuxerRx::Reset {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };

        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn push_shutdown_send(&self) {
        debug!(
            "push_shutdown_send: id: {}, peer_port: {}, local_port: {}",
            self.id, self.peer_port, self.local_port
        );
        let rx = MuxerRx::ShutdownSend {
            local_port: self.local_port,
            peer_port: self.peer_port,
            buf_alloc: defs::CONN_TX_BUF_SIZE as u32,
            fwd_cnt: self.tx_cnt.0,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn peer_avail_credit(&self) -> usize {
        (Wrapping(self.peer_buf_alloc) - (self.rx_cnt - self.peer_fwd_cnt)).0 as usize
    }

    fn recv_to_pkt(&self, pkt: &mut VsockPacket) -> RecvPkt {
        if let Some(buf) = pkt.buf_mut() {
            let peer_credit = self.peer_avail_credit();
            let max_len = std::cmp::min(buf.len(), peer_credit);

            if max_len == 0 {
                return RecvPkt::WaitForCredit;
            }

            let stream = match self.stream.as_ref() {
                Some(s) => s,
                None => return RecvPkt::Error,
            };

            match (&mut (&*stream)).read(&mut buf[..max_len]) {
                Ok(0) => RecvPkt::Close,
                Ok(cnt) => RecvPkt::Read(cnt),
                Err(e) => {
                    if e.kind() != ErrorKind::WouldBlock {
                        debug!("recv_pkt: recv error: {e:?}");
                    }
                    RecvPkt::Error
                }
            }
        } else {
            debug!("recv_pkt: pkt without buf");
            RecvPkt::Error
        }
    }

    fn recv_pkt(&mut self) -> (bool, bool) {
        let mut have_used = false;
        let mut wait_credit = false;
        let queue_mutex = self.queue.clone();
        let mut queue = queue_mutex.lock().unwrap();

        while let Some(head) = queue.pop(&self.mem) {
            let len = match VsockPacket::from_rx_virtq_head(&head) {
                Ok(mut pkt) => match self.recv_to_pkt(&mut pkt) {
                    RecvPkt::WaitForCredit => {
                        wait_credit = true;
                        0
                    }
                    RecvPkt::Read(cnt) => {
                        self.rx_cnt += Wrapping(cnt as u32);
                        self.init_data_pkt(&mut pkt);
                        pkt.set_len(cnt as u32);
                        pkt.hdr().len() + cnt
                    }
                    RecvPkt::Close => {
                        self.status = ProxyStatus::Closed;
                        0
                    }
                    RecvPkt::Error => 0,
                },
                Err(e) => {
                    debug!("recv_pkt: RX queue error: {e:?}");
                    0
                }
            };

            if len == 0 {
                queue.undo_pop();
                break;
            } else {
                have_used = true;
                self.push_cnt += Wrapping(len as u32);
                if let Err(e) = queue.add_used(&self.mem, head.index, len as u32) {
                    error!("failed to add used elements to the queue: {e:?}");
                }
            }
        }

        (have_used, wait_credit)
    }

    fn init_data_pkt(&self, pkt: &mut VsockPacket) {
        pkt.set_op(uapi::VSOCK_OP_RW)
            .set_src_cid(uapi::VSOCK_HOST_CID)
            .set_dst_cid(self.cid)
            .set_src_port(self.local_port)
            .set_dst_port(self.peer_port)
            .set_type(uapi::VSOCK_TYPE_STREAM)
            .set_buf_alloc(defs::CONN_TX_BUF_SIZE as u32)
            .set_fwd_cnt(self.tx_cnt.0);
    }

    /// Drain readable host data into the guest, mirroring the unix
    /// process_event(IN) body. Called both on socket events and whenever
    /// read interest is re-enabled (see module docs for why).
    fn drain_into_guest(&mut self, update: &mut ProxyUpdate) {
        if self.status != ProxyStatus::Connected || self.half_closed {
            return;
        }

        let (signal_queue, wait_credit) = self.recv_pkt();
        update.signal_queue |= signal_queue;

        if wait_credit && self.status != ProxyStatus::WaitingCreditUpdate {
            self.status = ProxyStatus::WaitingCreditUpdate;
            // Push the credit request directly: the muxer-side
            // process_proxy_update does not forward push_credit_req.
            let rx = MuxerRx::CreditRequest {
                local_port: self.local_port,
                peer_port: self.peer_port,
                fwd_cnt: self.tx_cnt.0,
            };
            push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
            update.signal_queue = true;
        }

        if self.status == ProxyStatus::Closed {
            // Host read EOF: half close, keep guest->host direction open.
            debug!("drain: host read EOF, half-closing: id={}", self.id);
            self.status = ProxyStatus::Connected;
            self.half_closed = true;
            self.push_shutdown_send();
            update.signal_queue = true;
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::empty()));
        } else if self.status == ProxyStatus::WaitingCreditUpdate {
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::empty()));
        }
    }
}

impl Proxy for TcpProxy {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self) -> ProxyStatus {
        self.status
    }

    fn connect(&mut self, _pkt: &VsockPacket, _req: TsiConnectReq) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        // Loopback connect is effectively instant, do it synchronously.
        let result = match read_port_file(&self.path)
            .and_then(|port| TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))))
        {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let _ = stream.set_nonblocking(true);
                match wsa_event_for(stream.as_raw_socket() as SOCKET, FD_READ | FD_CLOSE) {
                    Ok(event) => {
                        self.stream = Some(stream);
                        self.event = Some(event);
                        self.status = ProxyStatus::Connected;
                        0
                    }
                    Err(e) => neg_linux_errno(e),
                }
            }
            Err(e) => {
                debug!("Error connecting: {e}");
                neg_linux_errno(e)
            }
        };

        if self.status == ProxyStatus::Connected {
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::IN));
        }
        self.push_connect_rsp(result);

        update
    }

    fn confirm_connect(&mut self, pkt: &VsockPacket) -> Option<ProxyUpdate> {
        debug!(
            "confirm_connect: local_port={} peer_port={}",
            pkt.dst_port(),
            pkt.src_port(),
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.local_port = pkt.dst_port();
        self.peer_port = pkt.src_port();

        // This response goes to the connection.
        let rx = MuxerRx::OpResponse {
            local_port: pkt.dst_port(),
            peer_port: pkt.src_port(),
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);

        None
    }

    fn getpeername(&mut self, _pkt: &VsockPacket) {
        unreachable!("TSI is not supported on Windows");
    }

    fn sendmsg(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let ret = if let (Some(buf), Some(stream)) = (pkt.buf(), self.stream.as_ref()) {
            // The unix backend switches the socket to blocking mode once
            // connected, so a full write here matches its semantics. The
            // socket stays non-blocking for reads; spin on WouldBlock.
            let mut written = 0usize;
            let mut result: i32 = 0;
            while written < buf.len() {
                match (&mut (&*stream)).write(&buf[written..]) {
                    Ok(n) => written += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(e) => {
                        result = neg_linux_errno(e);
                        break;
                    }
                }
            }
            if result == 0 {
                self.tx_cnt += Wrapping(written as u32);
                written as i32
            } else {
                result
            }
        } else {
            -22 // LINUX_EINVAL
        };

        if ret > 0 && (self.tx_cnt - self.last_tx_cnt_sent).0 >= self.peer_buf_alloc / 2 {
            self.last_tx_cnt_sent = self.tx_cnt;

            let rx = MuxerRx::CreditUpdate {
                local_port: pkt.dst_port(),
                peer_port: pkt.src_port(),
                fwd_cnt: self.tx_cnt.0,
            };

            push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
            update.signal_queue = true;
        }

        debug!("sendmsg ret={ret}");

        update
    }

    fn sendto_addr(&mut self, _req: TsiSendtoAddr) -> ProxyUpdate {
        unreachable!("TSI is not supported on Windows");
    }

    fn listen(
        &mut self,
        _pkt: &VsockPacket,
        _req: TsiListenReq,
        _host_port_map: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        unreachable!("TSI is not supported on Windows");
    }

    fn accept(&mut self, _req: TsiAcceptReq) -> ProxyUpdate {
        unreachable!("TSI is not supported on Windows");
    }

    fn update_peer_credit(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "update_credit: buf_alloc={} rx_cnt={} fwd_cnt={}",
            pkt.buf_alloc(),
            self.rx_cnt,
            pkt.fwd_cnt()
        );
        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.status = ProxyStatus::Connected;

        let mut update = ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), EventSet::IN)),
            ..Default::default()
        };
        // Buffered data won't re-signal the WSA event; drain now.
        self.drain_into_guest(&mut update);
        update
    }

    fn push_op_request(&self) {
        // This packet goes to the connection.
        let rx = MuxerRx::OpRequest {
            local_port: self.local_port,
            peer_port: self.peer_port,
        };
        push_packet(self.cid, rx, &self.rxq, &self.queue, &self.mem);
    }

    fn process_op_response(&mut self, pkt: &VsockPacket) -> ProxyUpdate {
        debug!(
            "process_op_response: id={} src_port={} dst_port={}",
            self.id,
            pkt.src_port(),
            pkt.dst_port()
        );

        self.peer_buf_alloc = pkt.buf_alloc();
        self.peer_fwd_cnt = Wrapping(pkt.fwd_cnt());

        self.status = ProxyStatus::Connected;

        let mut update = ProxyUpdate {
            polling: Some((self.id, self.as_raw_fd(), EventSet::IN)),
            ..Default::default()
        };
        // Data may already be buffered from before the guest was ready.
        self.drain_into_guest(&mut update);
        update
    }

    fn enqueue_accept(&mut self) {
        unreachable!("TSI is not supported on Windows");
    }

    fn shutdown(&mut self, pkt: &VsockPacket) {
        let send_off = pkt.flags() & uapi::VSOCK_FLAGS_SHUTDOWN_SEND != 0;

        // SHUTDOWN_RCV is intentionally ignored: SD_RECEIVE with queued
        // inbound data makes winsock RST the connection, destroying
        // guest output the host client hasn't read yet (exec replies
        // were lost race-dependently). Only propagate the FIN.
        if send_off
            && let Some(stream) = self.stream.as_ref()
            && let Err(e) = stream.shutdown(Shutdown::Write)
        {
            warn!("error sending shutdown to socket: {e}");
        }
    }

    fn release(&mut self) -> ProxyUpdate {
        debug!(
            "release: id={}, tx_cnt={}, last_tx_cnt={}",
            self.id, self.tx_cnt, self.last_tx_cnt_sent
        );

        ProxyUpdate {
            remove_proxy: ProxyRemoval::Deferred,
            ..Default::default()
        }
    }

    fn process_event(&mut self, _evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let (socket, event) = match (self.stream.as_ref(), self.event) {
            (Some(s), Some(e)) => (s.as_raw_socket() as SOCKET, e),
            _ => return update,
        };
        let net = enum_net_events(socket, event);

        if net.closed && net.close_error != 0 {
            // Abortive close (RST): tear the connection down like the unix
            // backend does on epoll HANG_UP.
            debug!("process_event: abortive close: id={}", self.id);
            self.push_reset();
            self.status = ProxyStatus::Closed;
            update.polling = Some((self.id, self.as_raw_fd(), EventSet::empty()));
            update.signal_queue = true;
            update.remove_proxy = ProxyRemoval::Deferred;
            return update;
        }

        if net.readable || net.closed {
            // Graceful FD_CLOSE is discovered by recv() returning 0 after
            // the buffered data drains, which feeds the half-close path.
            self.drain_into_guest(&mut update);
        }

        update
    }
}

impl AsRawFd for TcpProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.event
            .map(|e| e.as_raw_handle())
            .unwrap_or(std::ptr::null_mut())
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        if let Some(event) = self.event.take() {
            unsafe { WSACloseEvent(handle_to_wsaevent(event.as_raw_handle())) };
        }
    }
}

pub struct TcpAcceptorProxy {
    id: u64,
    listener: TcpListener,
    event: SendHandle,
    peer_port: u32,
}

impl TcpAcceptorProxy {
    pub fn new(id: u64, path: &PathBuf, peer_port: u32) -> Result<Self, ProxyError> {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .map_err(ProxyError::CreatingSocket)?;
        let port = listener
            .local_addr()
            .map_err(ProxyError::CreatingSocket)?
            .port();
        // Publish the port for host clients (nebula's ipc convention).
        std::fs::write(path, format!("{port}")).map_err(ProxyError::CreatingSocket)?;
        listener
            .set_nonblocking(true)
            .map_err(ProxyError::CreatingSocket)?;
        let event = wsa_event_for(listener.as_raw_socket() as SOCKET, FD_ACCEPT)
            .map_err(ProxyError::CreatingSocket)?;
        debug!("TcpAcceptorProxy: vsock port {peer_port} -> 127.0.0.1:{port} ({path:?})");
        Ok(TcpAcceptorProxy {
            id,
            listener,
            event,
            peer_port,
        })
    }
}

impl Proxy for TcpAcceptorProxy {
    fn id(&self) -> u64 {
        self.id
    }
    fn status(&self) -> ProxyStatus {
        ProxyStatus::WaitingOnAccept
    }
    fn connect(&mut self, _: &VsockPacket, _: TsiConnectReq) -> ProxyUpdate {
        unreachable!()
    }
    fn getpeername(&mut self, _: &VsockPacket) {
        unreachable!()
    }
    fn sendmsg(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn sendto_addr(&mut self, _: TsiSendtoAddr) -> ProxyUpdate {
        unreachable!()
    }
    fn listen(
        &mut self,
        _: &VsockPacket,
        _: TsiListenReq,
        _: &Option<HashMap<u16, u16>>,
    ) -> ProxyUpdate {
        unreachable!()
    }
    fn accept(&mut self, _: TsiAcceptReq) -> ProxyUpdate {
        unreachable!()
    }
    fn update_peer_credit(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn process_op_response(&mut self, _: &VsockPacket) -> ProxyUpdate {
        unreachable!()
    }
    fn release(&mut self) -> ProxyUpdate {
        unreachable!()
    }
    fn process_event(&mut self, _evset: EventSet) -> ProxyUpdate {
        let mut update = ProxyUpdate::default();

        let net = enum_net_events(self.listener.as_raw_socket() as SOCKET, self.event);
        if !net.readable {
            return update;
        }

        // Accept everything that's pending; FD_ACCEPT is recorded per
        // accept call, so stopping early is safe but draining avoids
        // spurious wakeup ordering issues.
        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_nonblocking(true);
                    update.new_proxy = Some((self.peer_port, stream, 0, NewProxyType::Unix));
                    update.signal_queue = true;
                    // The muxer can only carry one new proxy per update.
                    break;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => {
                    warn!("error accepting connection: id={}, err={}", self.id, e);
                    break;
                }
            }
        }
        update
    }
}

impl AsRawFd for TcpAcceptorProxy {
    fn as_raw_fd(&self) -> RawFd {
        self.event.as_raw_handle()
    }
}

impl Drop for TcpAcceptorProxy {
    fn drop(&mut self) {
        unsafe { WSACloseEvent(handle_to_wsaevent(self.event.as_raw_handle())) };
    }
}
