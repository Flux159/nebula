//! usernet: an in-process usermode NAT for libkrun's virtio-net device.
//!
//! Speaks the same protocol as an external network proxy (passt/qemu stream
//! netdev): 4-byte big-endian length-prefixed ethernet frames over a unix
//! STREAM socket — but lives in a thread inside the VMM, so there is no
//! host dependency, no AppArmor profile, and the same code path will serve
//! the Windows/WHP backend.
//!
//! Topology (mirrors passt defaults, RFC-7335-ish):
//!   guest  192.168.127.2/24   (assigned via our built-in DHCP responder)
//!   gw/DNS 192.168.127.1      (this NAT; DNS/anything to the gateway is
//!                              remapped to host loopback)
//!
//! Outbound flows are NATed through ordinary host sockets created on
//! demand: TCP by sniffing SYNs and standing up a smoltcp listener per
//! destination, UDP per (guest, destination) pair. Inbound published ports
//! are NOT handled here (nebula uses a guest-agent vsock proxy for those).

use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::socket::udp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    DhcpMessageType, DhcpPacket, DhcpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr, HardwareAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv4Packet,
    Ipv4Repr, TcpPacket, UdpPacket, UdpRepr,
};

const GW_IP: Ipv4Address = Ipv4Address::new(192, 168, 127, 1);
const GUEST_IP: Ipv4Address = Ipv4Address::new(192, 168, 127, 2);
const NETMASK: u8 = 24;
const GW_MAC: EthernetAddress = EthernetAddress([0x5a, 0x94, 0xef, 0x00, 0x00, 0x01]);
const MTU: usize = 1500;
const TCP_BUF: usize = 256 * 1024;
const UDP_IDLE: Duration = Duration::from_secs(60);

/// Spawn the NAT thread serving `fd` (one end of a socketpair whose other
/// end was handed to the virtio-net unixstream backend).
pub fn spawn(fd: OwnedFd, guest_mac: [u8; 6]) {
    std::thread::Builder::new()
        .name("usernet".into())
        .spawn(move || {
            let stream = unsafe { UnixStream::from_raw_fd(fd.as_raw_fd()) };
            std::mem::forget(fd); // stream owns it now
            if let Err(e) = run(stream, EthernetAddress(guest_mac)) {
                log::error!("usernet exited: {e}");
            }
        })
        .expect("spawn usernet thread");
}

// --- framed pipe <-> smoltcp device -----------------------------------------

struct Pipe {
    stream: UnixStream,
    rx_partial: Vec<u8>,
    /// Parsed inbound ethernet frames waiting for the interface.
    rx: VecDeque<Vec<u8>>,
    /// Outbound bytes (already length-prefixed) not yet written.
    tx_pending: Vec<u8>,
}

impl Pipe {
    fn new(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            rx_partial: Vec::new(),
            rx: VecDeque::new(),
            tx_pending: Vec::new(),
        })
    }

    /// Drain readable bytes into complete frames. Ok(false) = peer closed.
    fn pump_rx(&mut self) -> std::io::Result<bool> {
        let mut buf = [0u8; 65536];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return Ok(false),
                Ok(n) => self.rx_partial.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        loop {
            if self.rx_partial.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes(self.rx_partial[..4].try_into().unwrap()) as usize;
            if len > 65535 {
                return Err(std::io::Error::new(ErrorKind::InvalidData, "frame too big"));
            }
            if self.rx_partial.len() < 4 + len {
                break;
            }
            self.rx.push_back(self.rx_partial[4..4 + len].to_vec());
            self.rx_partial.drain(..4 + len);
        }
        Ok(true)
    }

    fn send_frame(&mut self, frame: &[u8]) {
        self.tx_pending
            .extend_from_slice(&(frame.len() as u32).to_be_bytes());
        self.tx_pending.extend_from_slice(frame);
    }

    fn pump_tx(&mut self) -> std::io::Result<()> {
        while !self.tx_pending.is_empty() {
            match self.stream.write(&self.tx_pending) {
                Ok(n) => {
                    self.tx_pending.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// smoltcp Device backed by the Pipe queues.
struct PipeDevice<'a> {
    pipe: &'a mut Pipe,
}

struct PipeRx(Vec<u8>);
struct PipeTx<'a>(&'a mut Pipe);

impl RxToken for PipeRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl<'a> TxToken for PipeTx<'a> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.send_frame(&buf);
        r
    }
}

impl<'a> Device for PipeDevice<'a> {
    type RxToken<'b>
        = PipeRx
    where
        Self: 'b;
    type TxToken<'b>
        = PipeTx<'b>
    where
        Self: 'b;

    fn receive(&mut self, _ts: SmolInstant) -> Option<(PipeRx, PipeTx<'_>)> {
        // Split borrow: pop a frame first, then hand out the tx side.
        let frame = self.pipe.rx.pop_front()?;
        Some((PipeRx(frame), PipeTx(self.pipe)))
    }

    fn transmit(&mut self, _ts: SmolInstant) -> Option<PipeTx<'_>> {
        Some(PipeTx(self.pipe))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }
}

// --- NAT flows ---------------------------------------------------------------

struct TcpFlow {
    handle: SocketHandle,
    host: Option<TcpStream>,
    /// Destination as the guest saw it (pre-mapping), for logging.
    dst: SocketAddrV4,
    connecting: bool,
    dead_at: Option<Instant>,
    /// host->guest bytes read but not yet accepted by the smoltcp socket.
    /// NEVER drop these — partial sends otherwise corrupt the stream.
    pending: Vec<u8>,
    pending_off: usize,
}

struct UdpFlow {
    host: UdpSocket,
    guest: SocketAddrV4,
    last_used: Instant,
}

/// Gateway-destined traffic lands on the host loopback (DNS relay etc.).
fn map_dst(ip: Ipv4Address, port: u16) -> SocketAddrV4 {
    if ip == GW_IP {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
    } else {
        SocketAddrV4::new(Ipv4Addr::from(ip.octets()), port)
    }
}

fn run(stream: UnixStream, guest_mac: EthernetAddress) -> std::io::Result<()> {
    let mut pipe = Pipe::new(stream)?;
    let mut config = Config::new(HardwareAddress::Ethernet(GW_MAC));
    config.random_seed = 0x6e6562756c61; // deterministic is fine here
    let mut device = PipeDevice { pipe: &mut pipe };
    let mut iface = Interface::new(config, &mut device, SmolInstant::now());
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(IpAddress::Ipv4(GW_IP), NETMASK));
    });
    // NAT mode: accept packets for ANY destination (our TCP/UDP listeners
    // bind foreign endpoints like 52.x.x.x:443). any_ip alone is not enough:
    // smoltcp additionally requires the destination to be "routed locally" —
    // a route whose gateway is an address the interface itself owns — so a
    // default route via our own gateway IP completes the trick.
    iface.set_any_ip(true);
    iface
        .routes_mut()
        .add_default_ipv4_route(GW_IP)
        .expect("add default route");

    let mut sockets = SocketSet::new(vec![]);
    let mut tcp_flows: Vec<TcpFlow> = Vec::new();
    let mut tcp_listeners: HashMap<(Ipv4Address, u16), SocketHandle> = HashMap::new();
    let mut udp_flows: HashMap<(SocketAddrV4, SocketAddrV4), UdpFlow> = HashMap::new();
    let mut udp_guest_socks: HashMap<(Ipv4Address, u16), SocketHandle> = HashMap::new();

    loop {
        // 1. Read frames from the VMM side.
        if !pipe.pump_rx()? {
            return Ok(()); // VM gone
        }

        // 2. Pre-scan frames: DHCP server + on-demand socket creation.
        let mut keep = VecDeque::new();
        while let Some(frame) = pipe.rx.pop_front() {
            match prescan(
                &frame,
                guest_mac,
                &mut pipe,
                &mut sockets,
                &mut tcp_listeners,
                &mut udp_guest_socks,
            ) {
                Prescan::Consumed => {}
                Prescan::Deliver => keep.push_back(frame),
            }
        }
        pipe.rx = keep;

        // 3. Drive the stack.
        let mut device = PipeDevice { pipe: &mut pipe };
        iface.poll(SmolInstant::now(), &mut device, &mut sockets);

        // 4. Adopt newly-established TCP flows (listener got its SYN).
        tcp_listeners.retain(|(dst_ip, dst_port), handle| {
            let sock = sockets.get_mut::<tcp::Socket>(*handle);
            if sock.is_active() && !matches!(sock.state(), tcp::State::Listen) {
                tcp_flows.push(TcpFlow {
                    handle: *handle,
                    host: None,
                    dst: SocketAddrV4::new(Ipv4Addr::from(dst_ip.octets()), *dst_port),
                    connecting: false,
                    dead_at: None,
                    pending: Vec::new(),
                    pending_off: 0,
                });
                false
            } else {
                true
            }
        });

        // 5. Pump TCP flows.
        let mut i = 0;
        while i < tcp_flows.len() {
            let done = pump_tcp(&mut tcp_flows[i], &mut sockets);
            if done {
                let f = tcp_flows.swap_remove(i);
                sockets.remove(f.handle);
            } else {
                i += 1;
            }
        }

        // 6. Pump UDP guest sockets <-> host sockets.
        pump_udp(&mut sockets, &mut udp_guest_socks, &mut udp_flows);
        udp_flows.retain(|_, f| f.last_used.elapsed() < UDP_IDLE);

        // 7. Flush egress + wait for work.
        let mut device = PipeDevice { pipe: &mut pipe };
        iface.poll(SmolInstant::now(), &mut device, &mut sockets);
        pipe.pump_tx()?;

        wait_readable(&pipe, &tcp_flows, &udp_flows, Duration::from_millis(20))?;
    }
}

enum Prescan {
    Consumed,
    Deliver,
}

/// Look at a guest frame BEFORE the stack: answer DHCP ourselves, and make
/// sure a smoltcp socket exists for any new TCP/UDP destination so the
/// stack can accept the flow.
fn prescan(
    frame: &[u8],
    guest_mac: EthernetAddress,
    pipe: &mut Pipe,
    sockets: &mut SocketSet<'static>,
    tcp_listeners: &mut HashMap<(Ipv4Address, u16), SocketHandle>,
    udp_guest_socks: &mut HashMap<(Ipv4Address, u16), SocketHandle>,
) -> Prescan {
    let Ok(eth) = EthernetFrame::new_checked(frame) else {
        return Prescan::Deliver;
    };
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return Prescan::Deliver; // ARP etc. -> smoltcp
    }
    let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
        return Prescan::Deliver;
    };
    let (src_ip, dst_ip) = (ip.src_addr(), ip.dst_addr());

    match ip.next_header() {
        IpProtocol::Udp => {
            let Ok(udp_pkt) = UdpPacket::new_checked(ip.payload()) else {
                return Prescan::Deliver;
            };
            // DHCP?
            if udp_pkt.dst_port() == 67 {
                if let Ok(dhcp) = DhcpPacket::new_checked(udp_pkt.payload()) {
                    if let Ok(repr) = DhcpRepr::parse(&dhcp) {
                        dhcp_reply(pipe, guest_mac, &repr);
                    }
                }
                return Prescan::Consumed;
            }
            // Ensure a guest-side UDP socket bound to this destination.
            let key = (dst_ip, udp_pkt.dst_port());
            udp_guest_socks.entry(key).or_insert_with(|| {
                let mut s = udp::Socket::new(
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 256 * 1024]),
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 256 * 1024]),
                );
                let _ = s.bind((IpAddress::Ipv4(dst_ip), udp_pkt.dst_port()));
                sockets.add(s)
            });
            let _ = src_ip;
            Prescan::Deliver
        }
        IpProtocol::Tcp => {
            let Ok(tcp_pkt) = TcpPacket::new_checked(ip.payload()) else {
                return Prescan::Deliver;
            };
            if tcp_pkt.syn() && !tcp_pkt.ack() {
                let key = (dst_ip, tcp_pkt.dst_port());
                if !tcp_listeners.contains_key(&key) {
                    let mut s = tcp::Socket::new(
                        tcp::SocketBuffer::new(vec![0; TCP_BUF]),
                        tcp::SocketBuffer::new(vec![0; TCP_BUF]),
                    );
                    s.set_nagle_enabled(false);
                    if s.listen((IpAddress::Ipv4(dst_ip), tcp_pkt.dst_port())).is_ok() {
                        tcp_listeners.insert(key, sockets.add(s));
                    }
                }
            }
            Prescan::Deliver
        }
        _ => Prescan::Deliver,
    }
}

/// Hand-built DHCP OFFER/ACK straight onto the wire.
fn dhcp_reply(pipe: &mut Pipe, guest_mac: EthernetAddress, req: &DhcpRepr) {
    let mtype = match req.message_type {
        DhcpMessageType::Discover => DhcpMessageType::Offer,
        DhcpMessageType::Request => DhcpMessageType::Ack,
        _ => return,
    };
    let gw = Ipv4Addr::from(GW_IP.octets());
    let reply = DhcpRepr {
        message_type: mtype,
        transaction_id: req.transaction_id,
        secs: 0,
        client_hardware_address: guest_mac,
        client_ip: Ipv4Address::UNSPECIFIED,
        your_ip: GUEST_IP,
        server_ip: GW_IP,
        router: Some(GW_IP),
        subnet_mask: Some(Ipv4Address::new(255, 255, 255, 0)),
        relay_agent_ip: Ipv4Address::UNSPECIFIED,
        broadcast: false,
        requested_ip: None,
        client_identifier: None,
        server_identifier: Some(GW_IP),
        parameter_request_list: None,
        dns_servers: Some(heapless_dns(gw)),
        max_size: None,
        lease_duration: Some(86400),
        renew_duration: None,
        rebind_duration: None,
        additional_options: &[],
    };

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };
    let ip_repr = Ipv4Repr {
        src_addr: GW_IP,
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + reply.buffer_len(),
        hop_limit: 64,
    };
    let eth_repr = EthernetRepr {
        src_addr: GW_MAC,
        dst_addr: EthernetAddress::BROADCAST,
        ethertype: EthernetProtocol::Ipv4,
    };

    let total = eth_repr.buffer_len() + ip_repr.buffer_len() + ip_repr.payload_len;
    let mut buf = vec![0u8; total];
    let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
    eth_repr.emit(&mut eth);
    let mut ip = Ipv4Packet::new_unchecked(eth.payload_mut());
    ip_repr.emit(&mut ip, &smoltcp::phy::ChecksumCapabilities::default());
    let mut udp_pkt = UdpPacket::new_unchecked(ip.payload_mut());
    udp_repr.emit(
        &mut udp_pkt,
        &IpAddress::Ipv4(GW_IP),
        &IpAddress::Ipv4(Ipv4Address::BROADCAST),
        reply.buffer_len(),
        |p| {
            let mut dhcp = DhcpPacket::new_unchecked(p);
            let _ = reply.emit(&mut dhcp);
        },
        &smoltcp::phy::ChecksumCapabilities::default(),
    );
    pipe.send_frame(&buf);
}

fn heapless_dns(gw: Ipv4Addr) -> heapless::Vec<Ipv4Address, 3> {
    let mut v = heapless::Vec::new();
    let _ = v.push(Ipv4Address::from(gw.octets()));
    v
}

/// Returns true when the flow is finished and the socket can be removed.
fn pump_tcp(flow: &mut TcpFlow, sockets: &mut SocketSet<'static>) -> bool {
    let sock = sockets.get_mut::<tcp::Socket>(flow.handle);

    // Connect to the mapped host destination on first activity.
    if flow.host.is_none() && !flow.connecting {
        flow.connecting = true;
        let mapped = map_dst(*flow.dst.ip(), flow.dst.port());
        match TcpStream::connect_timeout(&SocketAddr::V4(mapped.into()), Duration::from_secs(10)) {
            Ok(s) => {
                let _ = s.set_nonblocking(true);
                let _ = s.set_nodelay(true);
                flow.host = Some(s);
            }
            Err(_) => {
                sock.abort();
                return true;
            }
        }
    }
    let Some(host) = flow.host.as_mut() else {
        return false;
    };

    // guest -> host
    let mut buf = [0u8; 65536];
    while sock.can_recv() {
        match sock.recv(|data| {
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            (n, n)
        }) {
            Ok(0) => break,
            Ok(n) => {
                if write_all_nb(host, &buf[..n]).is_err() {
                    sock.abort();
                    return true;
                }
            }
            Err(_) => break,
        }
    }

    // host -> guest. Bytes go through flow.pending so a full smoltcp send
    // buffer can never drop data mid-chunk (that corrupts TLS streams).
    loop {
        // 1. Flush whatever is already staged.
        while flow.pending_off < flow.pending.len() {
            match sock.send_slice(&flow.pending[flow.pending_off..]) {
                Ok(0) => break,
                Ok(sent) => flow.pending_off += sent,
                Err(_) => {
                    flow.pending_off = flow.pending.len();
                    break;
                }
            }
        }
        if flow.pending_off < flow.pending.len() {
            break; // smoltcp buffer full; resume next poll
        }
        flow.pending.clear();
        flow.pending_off = 0;

        // 2. Pull more from the host only when nothing is staged.
        if !sock.can_send() {
            break;
        }
        match host.read(&mut buf) {
            Ok(0) => {
                sock.close(); // FIN to guest
                break;
            }
            Ok(n) => {
                flow.pending.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => {
                sock.close();
                break;
            }
        }
    }

    // Guest closed and drained -> shut down host write side; fully closed
    // sockets linger briefly so FINs flush.
    if !sock.is_active() {
        if flow.dead_at.is_none() {
            flow.dead_at = Some(Instant::now());
            if let Some(h) = flow.host.as_ref() {
                let _ = h.shutdown(std::net::Shutdown::Both);
            }
        }
        return flow.dead_at.unwrap().elapsed() > Duration::from_millis(200);
    }
    false
}

fn write_all_nb(s: &mut TcpStream, mut buf: &[u8]) -> std::io::Result<()> {
    while !buf.is_empty() {
        match s.write(buf) {
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Host socket backpressure: block briefly (rare; keeps the
                // single-threaded loop simple).
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn pump_udp(
    sockets: &mut SocketSet<'static>,
    guest_socks: &mut HashMap<(Ipv4Address, u16), SocketHandle>,
    flows: &mut HashMap<(SocketAddrV4, SocketAddrV4), UdpFlow>,
) {
    // guest -> host
    for ((dst_ip, dst_port), handle) in guest_socks.iter() {
        let sock = sockets.get_mut::<udp::Socket>(*handle);
        let mut buf = [0u8; 65536];
        while let Ok((n, meta)) = sock.recv_slice(&mut buf) {
            let IpAddress::Ipv4(src_v4) = meta.endpoint.addr;
            let guest_ep = SocketAddrV4::new(Ipv4Addr::from(src_v4.octets()), meta.endpoint.port);
            let dst_ep = SocketAddrV4::new(Ipv4Addr::from(dst_ip.octets()), *dst_port);
            let flow = flows.entry((guest_ep, dst_ep)).or_insert_with(|| {
                let host = UdpSocket::bind("0.0.0.0:0").expect("bind udp");
                let _ = host.set_nonblocking(true);
                let _ = host.connect(SocketAddr::V4(map_dst(*dst_ip, *dst_port).into()));
                UdpFlow {
                    host,
                    guest: guest_ep,
                    last_used: Instant::now(),
                }
            });
            flow.last_used = Instant::now();
            let _ = flow.host.send(&buf[..n]);
        }
    }
    // host -> guest
    for ((_guest_ep, dst_ep), flow) in flows.iter_mut() {
        let key = (
            Ipv4Address::from(dst_ep.ip().octets()),
            dst_ep.port(),
        );
        let Some(handle) = guest_socks.get(&key) else {
            continue;
        };
        let sock = sockets.get_mut::<udp::Socket>(*handle);
        let mut buf = [0u8; 65536];
        while let Ok(n) = flow.host.recv(&mut buf) {
            flow.last_used = Instant::now();
            let meta = udp::UdpMetadata::from(smoltcp::wire::IpEndpoint {
                addr: IpAddress::Ipv4(Ipv4Address::from(flow.guest.ip().octets())),
                port: flow.guest.port(),
            });
            let _ = sock.send_slice(&buf[..n], meta);
        }
    }
}

/// poll(2) on the pipe + all host sockets so the loop sleeps when idle.
fn wait_readable(
    pipe: &Pipe,
    tcp_flows: &[TcpFlow],
    udp_flows: &HashMap<(SocketAddrV4, SocketAddrV4), UdpFlow>,
    timeout: Duration,
) -> std::io::Result<()> {
    let mut fds: Vec<libc::pollfd> = Vec::with_capacity(1 + tcp_flows.len() + udp_flows.len());
    let mut events = libc::POLLIN;
    if !pipe.tx_pending.is_empty() {
        events |= libc::POLLOUT;
    }
    fds.push(libc::pollfd {
        fd: pipe.stream.as_raw_fd(),
        events,
        revents: 0,
    });
    for f in tcp_flows {
        if let Some(h) = &f.host {
            fds.push(libc::pollfd {
                fd: h.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
    }
    for f in udp_flows.values() {
        fds.push(libc::pollfd {
            fd: f.host.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as _,
            timeout.as_millis() as i32,
        );
    }
    Ok(())
}
